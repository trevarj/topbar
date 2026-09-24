//! The niri event-stream task.
//!
//! One task owns one connection and one [`EventStreamState`]. Three rules make
//! it impossible for the panel to show stale or torn workspace state:
//!
//! 1. **Framed lines.** `Framed<_, LinesCodec>` hands us whole lines, so a
//!    reply split across two reads is reassembled rather than dropped (v1 read
//!    a fixed buffer and lost the tail of long `WindowsChanged` payloads).
//! 2. **Fresh state per connection.** [`EventStreamState::apply`] documents
//!    itself as panicking on inconsistent input, and it means it: activating a
//!    workspace it has never seen is an `.expect`. State is therefore never
//!    carried across a gap — every connection starts from `default()` and niri
//!    re-sends the full state up front.
//! 3. **Per-line tolerance.** A line that will not parse is logged and skipped;
//!    a line that makes `apply` panic is caught and ends the *connection*, not
//!    the task. Neither can silently freeze the widget.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use niri_ipc::state::{EventStreamState, EventStreamStatePart};
use niri_ipc::{Event, Reply, Request, Response};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{debug, info, warn};

use crate::compositor::{KeyboardLayoutSnapshot, WorkspacesSnapshot};
use crate::error::SvcError;
use crate::niri::snapshot;

/// First reconnect delay after the stream drops.
const BACKOFF_START: Duration = Duration::from_millis(250);
/// Ceiling for the reconnect delay.
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// How much of an unparsable line to put in the log.
const LOG_LINE_LIMIT: usize = 200;

/// The two channels the stream publishes into.
pub(crate) struct Publishers {
    pub workspaces: watch::Sender<Arc<WorkspacesSnapshot>>,
    pub keyboard_layout: watch::Sender<Arc<KeyboardLayoutSnapshot>>,
}

/// Run the event stream until the process exits.
///
/// `kicks` asks for a reconnect from outside. The one caller is
/// [`crate::lifecycle`]: a socket that was open when the machine went to sleep
/// is often still open when it wakes, and still silent — the compositor's end
/// went away and the kernel has not noticed yet. The only way to be sure the
/// workspace strip is showing this session's workspaces is to start again, and
/// niri replays its whole state on every connection, so starting again is
/// cheap and self-correcting.
pub(crate) async fn run(socket: PathBuf, publishers: Publishers, mut kicks: mpsc::Receiver<()>) {
    let mut backoff = BACKOFF_START;
    loop {
        let mut handshaken = false;
        match session(&socket, &publishers, &mut handshaken, &mut kicks).await {
            Ok(()) => info!("niri closed the event stream; reconnecting"),
            Err(error) => warn!("niri event stream: {error}"),
        }

        publishers.publish_disconnected();

        if handshaken {
            // The connection worked at least once, so this is a compositor
            // restart rather than a configuration problem: retry promptly.
            backoff = BACKOFF_START;
        }
        debug!("reconnecting to niri in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connection, from handshake to end of stream.
async fn session(
    socket: &Path,
    publishers: &Publishers,
    handshaken: &mut bool,
    kicks: &mut mpsc::Receiver<()>,
) -> Result<(), SvcError> {
    // Anything asked for while there was no connection is already answered by
    // the one about to be made.
    while kicks.try_recv().is_ok() {}

    let stream = UnixStream::connect(socket).await.map_err(SvcError::Io)?;
    let mut framed = Framed::new(stream, LinesCodec::new());

    let request = serde_json::to_string(&Request::EventStream)
        .map_err(|e| SvcError::Protocol(e.to_string()))?;
    framed
        .send(request)
        .await
        .map_err(|e| SvcError::Protocol(format!("could not send the event-stream request: {e}")))?;

    let handshake = framed
        .next()
        .await
        .ok_or_else(|| SvcError::Protocol("niri closed the socket before replying".into()))?
        .map_err(|e| SvcError::Protocol(format!("could not read the handshake: {e}")))?;
    match serde_json::from_str::<Reply>(&handshake) {
        Ok(Ok(Response::Handled)) => {}
        Ok(Ok(other)) => {
            return Err(SvcError::Protocol(format!(
                "expected Handled for EventStream, got {other:?}"
            )));
        }
        Ok(Err(message)) => return Err(SvcError::Rejected(message)),
        Err(error) => return Err(SvcError::Protocol(error.to_string())),
    }
    *handshaken = true;
    info!("niri event stream connected");

    // niri replays the full state as the first events on every connection, so
    // starting from `default()` costs nothing and is the only safe choice.
    let mut state = EventStreamState::default();
    publishers.publish(&state);

    // Every sender gone means nothing will ever ask again — and a channel in
    // that state answers `recv` immediately for ever, which in a `select!`
    // would spin. The branch takes itself out of the race instead.
    let mut listening = true;
    loop {
        let line = tokio::select! {
            line = framed.next() => match line {
                Some(line) => line
                    .map_err(|e| SvcError::Protocol(format!("could not read an event line: {e}")))?,
                None => break,
            },
            asked = kicks.recv(), if listening => {
                if asked.is_none() {
                    listening = false;
                    continue;
                }
                info!("reconnecting to niri to make sure the state is this session's");
                return Ok(());
            }
        };
        match apply_line(&mut state, &line) {
            LineOutcome::Applied => publishers.publish(&state),
            LineOutcome::Skipped => {}
            LineOutcome::Poisoned => {
                return Err(SvcError::Protocol(
                    "the event-stream reducer rejected an event; resynchronising".into(),
                ));
            }
        }
    }

    Ok(())
}

/// What feeding one line to the reducer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineOutcome {
    /// The line parsed and the state changed.
    Applied,
    /// The line was blank or unparsable; the state is untouched.
    Skipped,
    /// The reducer panicked. The state must be thrown away.
    Poisoned,
}

/// Parse one line and fold it into `state`.
///
/// The `catch_unwind` is not defensive programming for its own sake:
/// `EventStreamState::apply` panics by design on an event that does not follow
/// from the state it has, which is exactly what a resumed-but-gapped stream
/// looks like. Catching it here turns a dead panel into a reconnect.
pub(crate) fn apply_line(state: &mut EventStreamState, line: &str) -> LineOutcome {
    let line = line.trim();
    if line.is_empty() {
        return LineOutcome::Skipped;
    }

    let event: Event = match serde_json::from_str(line) {
        Ok(event) => event,
        Err(error) => {
            warn!(
                "skipping an unreadable niri event ({error}): {}",
                clip(line)
            );
            return LineOutcome::Skipped;
        }
    };

    match panic::catch_unwind(AssertUnwindSafe(|| state.apply(event))) {
        Ok(_unhandled) => LineOutcome::Applied,
        Err(_) => {
            warn!("the niri event reducer panicked on: {}", clip(line));
            LineOutcome::Poisoned
        }
    }
}

/// Shorten a line for the log, on a character boundary.
fn clip(line: &str) -> &str {
    match line.char_indices().nth(LOG_LINE_LIMIT) {
        Some((index, _)) => &line[..index],
        None => line,
    }
}

impl Publishers {
    /// Publish the projections of `state`, skipping channels nothing changed in.
    fn publish(&self, state: &EventStreamState) {
        send_if_changed(&self.workspaces, snapshot::workspaces(state, true));
        send_if_changed(
            &self.keyboard_layout,
            snapshot::keyboard_layout(state, true),
        );
    }

    /// Keep the last state on screen but mark it as no longer live.
    ///
    /// The two `let` bindings are load-bearing. Passing `borrow()` straight
    /// into `send_if_changed` keeps the read guard alive across the call —
    /// which then asks the same lock for write, and the task deadlocks on
    /// itself the first time the compositor goes away, so the panel never dims
    /// and never reconnects. Binding first drops the guard at the semicolon.
    fn publish_disconnected(&self) {
        let workspaces = self
            .workspaces
            .borrow()
            .as_ref()
            .clone()
            .with_connected(false);
        send_if_changed(&self.workspaces, workspaces);

        let layouts = self
            .keyboard_layout
            .borrow()
            .as_ref()
            .clone()
            .with_connected(false);
        send_if_changed(&self.keyboard_layout, layouts);
    }
}

/// Publish `next` only if it differs from what subscribers already have.
///
/// Most niri events (window layout, focus timestamps, casts) project to an
/// identical snapshot; without this the panel would re-render on every one.
fn send_if_changed<T: PartialEq>(sender: &watch::Sender<Arc<T>>, next: T) {
    sender.send_if_modified(|current| {
        if **current == next {
            return false;
        }
        *current = Arc::new(next);
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real connection burst, captured from niri 26.04 over `$NIRI_SOCKET`.
    const BURST: &str = include_str!("../../tests/fixtures/niri-event-burst.jsonl");

    /// Feed a whole fixture through the reducer the way the task does.
    fn replay(lines: &str) -> EventStreamState {
        let mut state = EventStreamState::default();
        for line in lines.lines() {
            assert_ne!(
                apply_line(&mut state, line),
                LineOutcome::Poisoned,
                "fixture line poisoned the reducer: {line}"
            );
        }
        state
    }

    fn snapshot(state: &EventStreamState) -> WorkspacesSnapshot {
        snapshot::workspaces(state, true)
    }

    #[test]
    fn the_captured_burst_parses_into_state() {
        let state = replay(BURST);
        let snapshot = snapshot(&state);

        assert!(snapshot.connected);
        assert_eq!(snapshot.outputs.len(), 2, "{:?}", snapshot.outputs);
        assert_eq!(snapshot.focused_output.as_deref(), Some("eDP-1"));

        let internal = snapshot.for_output("eDP-1");
        assert_eq!(
            internal.iter().map(|view| view.idx).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(internal[0].name.as_deref(), Some("browser"));
        assert!(internal[0].is_focused && internal[0].is_active);
    }

    #[test]
    fn occupancy_comes_from_the_window_map() {
        let snapshot = snapshot(&replay(BURST));
        let occupied: Vec<usize> = snapshot
            .for_output("eDP-1")
            .iter()
            .filter(|view| view.has_windows)
            .map(|view| view.idx)
            .collect();
        // Workspace 3 ("chat") is empty in the fixture; 1 and 2 hold windows.
        assert_eq!(occupied, vec![1, 2]);
    }

    #[test]
    fn closing_the_last_window_empties_its_workspace() {
        let mut state = replay(BURST);
        assert!(apply_line(&mut state, r#"{"WindowClosed":{"id":7}}"#) == LineOutcome::Applied);

        let snapshot = snapshot(&state);
        let workspace_two = snapshot
            .for_output("eDP-1")
            .iter()
            .find(|view| view.idx == 2)
            .expect("workspace 2 is in the fixture");
        assert!(!workspace_two.has_windows);
    }

    #[test]
    fn activation_transfers_active_and_focused() {
        let mut state = replay(BURST);
        assert_eq!(
            apply_line(
                &mut state,
                r#"{"WorkspaceActivated":{"id":2,"focused":true}}"#
            ),
            LineOutcome::Applied
        );

        let snapshot = snapshot(&state);
        let views = snapshot.for_output("eDP-1");
        let active: Vec<usize> = views
            .iter()
            .filter(|view| view.is_active)
            .map(|view| view.idx)
            .collect();
        let focused: Vec<usize> = views
            .iter()
            .filter(|view| view.is_focused)
            .map(|view| view.idx)
            .collect();

        assert_eq!(active, vec![2], "exactly one active workspace per output");
        assert_eq!(focused, vec![2], "exactly one focused workspace overall");
        // The other output keeps its own active workspace.
        assert!(
            snapshot
                .for_output("DP-2")
                .iter()
                .any(|view| view.is_active)
        );
    }

    #[test]
    fn activating_another_output_does_not_move_this_ones_active_workspace() {
        let mut state = replay(BURST);
        assert_eq!(
            apply_line(
                &mut state,
                r#"{"WorkspaceActivated":{"id":11,"focused":true}}"#
            ),
            LineOutcome::Applied
        );

        let snapshot = snapshot(&state);
        let internal_active: Vec<usize> = snapshot
            .for_output("eDP-1")
            .iter()
            .filter(|view| view.is_active)
            .map(|view| view.idx)
            .collect();
        assert_eq!(internal_active, vec![1], "eDP-1 keeps its active workspace");
        assert_eq!(snapshot.focused_output.as_deref(), Some("DP-2"));
    }

    #[test]
    fn urgency_is_set_and_cleared() {
        let mut state = replay(BURST);
        let urgent = |state: &EventStreamState| {
            snapshot(state)
                .for_output("eDP-1")
                .iter()
                .filter(|view| view.is_urgent)
                .map(|view| view.idx)
                .collect::<Vec<_>>()
        };
        assert!(urgent(&state).is_empty());

        apply_line(
            &mut state,
            r#"{"WorkspaceUrgencyChanged":{"id":3,"urgent":true}}"#,
        );
        assert_eq!(urgent(&state), vec![3]);

        apply_line(
            &mut state,
            r#"{"WorkspaceUrgencyChanged":{"id":3,"urgent":false}}"#,
        );
        assert!(urgent(&state).is_empty());
    }

    #[test]
    fn an_urgent_window_makes_its_workspace_urgent() {
        let mut state = replay(BURST);
        apply_line(
            &mut state,
            r#"{"WindowUrgencyChanged":{"id":7,"urgent":true}}"#,
        );

        let snapshot = snapshot(&state);
        let workspace_two = snapshot
            .for_output("eDP-1")
            .iter()
            .find(|view| view.idx == 2)
            .expect("workspace 2 is in the fixture");
        assert!(
            workspace_two.is_urgent,
            "window urgency has to reach the workspace the panel draws"
        );
    }

    #[test]
    fn keyboard_layout_switching_tracks_the_index() {
        let mut state = replay(BURST);
        let layouts = snapshot::keyboard_layout(&state, true);
        assert_eq!(layouts.layout_count, 2);
        assert_eq!(layouts.current(), Some("English (US)"));
        assert!(layouts.is_switchable());

        assert_eq!(
            apply_line(&mut state, r#"{"KeyboardLayoutSwitched":{"idx":1}}"#),
            LineOutcome::Applied
        );
        assert_eq!(
            snapshot::keyboard_layout(&state, true).current(),
            Some("Russian")
        );
        state
            .keyboard_layouts
            .keyboard_layouts
            .as_mut()
            .unwrap()
            .current_idx = 200;
        let invalid = snapshot::keyboard_layout(&state, true);
        assert_eq!(invalid.current(), None);
        assert_eq!(
            invalid.layout_count, 2,
            "an invalid current index must not lose configured choices"
        );
    }

    #[test]
    fn garbage_lines_are_skipped_without_killing_the_stream() {
        let mut state = replay(BURST);
        let before = snapshot(&state);

        for line in [
            "",
            "   ",
            "not json at all",
            r#"{"WorkspacesChanged":{"workspaces":[{"id":1,"#, // torn mid-object
            r#"{"WorkspacesChanged":{}}"#,                     // right variant, wrong shape
            r#"{"SomeFutureEvent":{"whatever":1}}"#,           // an event we do not know
            "{}",
            "null",
        ] {
            assert_eq!(
                apply_line(&mut state, line),
                LineOutcome::Skipped,
                "{line:?} should be skipped"
            );
        }

        assert_eq!(snapshot(&state), before, "a skipped line changes nothing");

        // And the stream keeps working afterwards.
        assert_eq!(
            apply_line(
                &mut state,
                r#"{"WorkspaceActivated":{"id":2,"focused":true}}"#
            ),
            LineOutcome::Applied
        );
    }

    #[test]
    fn an_inconsistent_event_poisons_rather_than_panics() {
        // Activating a workspace the reducer has never seen is exactly what a
        // resumed-but-gapped stream looks like, and niri-ipc `.expect()`s on it.
        let mut state = EventStreamState::default();
        assert_eq!(
            apply_line(
                &mut state,
                r#"{"WorkspaceActivated":{"id":999,"focused":true}}"#
            ),
            LineOutcome::Poisoned
        );

        // A layout switch before the layouts are known does the same.
        let mut state = EventStreamState::default();
        assert_eq!(
            apply_line(&mut state, r#"{"KeyboardLayoutSwitched":{"idx":1}}"#),
            LineOutcome::Poisoned
        );
    }

    #[test]
    fn a_fresh_state_absorbs_a_full_resync() {
        // What reconnecting does: throw the state away, replay the burst.
        let mut state = replay(BURST);
        apply_line(
            &mut state,
            r#"{"WorkspaceActivated":{"id":2,"focused":true}}"#,
        );

        let resynced = replay(BURST);
        assert_eq!(
            snapshot(&resynced),
            snapshot(&replay(BURST)),
            "a resync is deterministic"
        );
        assert_ne!(snapshot(&state), snapshot(&resynced));
    }

    #[test]
    fn only_real_changes_reach_subscribers() {
        let (sender, mut receiver) = watch::channel(Arc::new(WorkspacesSnapshot::default()));
        receiver.mark_unchanged();

        let state = replay(BURST);
        send_if_changed(&sender, snapshot::workspaces(&state, true));
        assert!(receiver.has_changed().expect("sender is alive"));
        receiver.mark_unchanged();

        // The same state again: nothing to tell anyone.
        send_if_changed(&sender, snapshot::workspaces(&state, true));
        assert!(!receiver.has_changed().expect("sender is alive"));
    }

    #[test]
    fn events_the_panel_does_not_draw_do_not_wake_it() {
        let mut state = replay(BURST);
        let (sender, mut receiver) = watch::channel(Arc::new(snapshot::workspaces(&state, true)));
        receiver.mark_unchanged();

        for line in [
            r#"{"WindowFocusTimestampChanged":{"id":7,"focus_timestamp":{"secs":10,"nanos":0}}}"#,
            r#"{"OverviewOpenedOrClosed":{"is_open":true}}"#,
            r#"{"CastsChanged":{"casts":[]}}"#,
        ] {
            assert_eq!(apply_line(&mut state, line), LineOutcome::Applied);
            send_if_changed(&sender, snapshot::workspaces(&state, true));
        }

        assert!(
            !receiver.has_changed().expect("sender is alive"),
            "the workspaces widget must not re-render for unrelated events"
        );
    }

    #[test]
    fn clip_cuts_on_a_character_boundary() {
        let line = "é".repeat(LOG_LINE_LIMIT + 10);
        assert_eq!(clip(&line).chars().count(), LOG_LINE_LIMIT);
        assert_eq!(clip("short"), "short");
    }
}
