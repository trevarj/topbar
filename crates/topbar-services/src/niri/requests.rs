//! Sending actions to niri.
//!
//! Actions cannot share the event-stream connection: asking for an event
//! stream consumes the socket and shuts down its write half, so a click on a
//! workspace would have nowhere to go. This is a second, separate connection.
//!
//! niri answers any number of sequential requests on one connection, so the
//! handle keeps a single one alive behind a mutex instead of paying a connect
//! per click. A broken connection is dropped and re-established once, which
//! covers the common case of the compositor having restarted since the last
//! action.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use niri_ipc::{
    Action, LayoutSwitchTarget, Reply, Request, Response, Window, WorkspaceReferenceArg,
};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::codec::{Framed, LinesCodec};
use tracing::debug;

use crate::error::SvcError;

/// How long any single action may take, connection included.
///
/// Short on purpose: these run from a click, and a compositor that has not
/// answered in two seconds is not going to.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// A request connection to niri, cheap to clone and safe to share.
#[derive(Clone)]
pub struct NiriHandle {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None` when `$NIRI_SOCKET` was not set at start-up.
    socket: Option<PathBuf>,
    connection: Mutex<Option<Connection>>,
}

type Connection = Framed<UnixStream, LinesCodec>;

impl NiriHandle {
    /// Focus one exact window, rejecting IDs that disappeared since rendering.
    pub async fn focus_window(&self, id: u64) -> Result<(), SvcError> {
        let windows = match self.request(Request::Windows).await? {
            Response::Windows(windows) => windows,
            other => {
                return Err(SvcError::Protocol(format!(
                    "expected Windows, got {other:?}"
                )));
            }
        };
        if !windows.iter().any(|window| window.id == id) {
            return Err(SvcError::Rejected("window is no longer open".into()));
        }
        self.act(Action::FocusWindow { id }).await
    }
    /// Create a handle for `socket`, or a handle that always fails if there is
    /// none.
    pub(crate) fn new(socket: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                socket,
                connection: Mutex::new(None),
            }),
        }
    }

    /// Make `id` the active workspace on its output.
    ///
    /// Addressed by id rather than index: indices renumber when workspaces are
    /// re-ordered, so an index captured when the bar was drawn can point at a
    /// different workspace by the time the click lands.
    pub async fn focus_workspace(&self, id: u64) -> Result<(), SvcError> {
        self.act(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(id),
        })
        .await
    }

    /// Raise the window belonging to one of `identities`, if there is one.
    ///
    /// This is what makes clicking a notification focus the application that
    /// sent it. A notification names its sender more than one way — the
    /// `desktop-entry` hint, the display name — and either may be what the
    /// window calls itself, so every candidate is offered and the first that
    /// matches wins.
    ///
    /// Returns whether a window matched: an application can perfectly well
    /// notify with no window open, and that is not a failure to report.
    pub async fn focus_app(&self, identities: &[&str]) -> Result<bool, SvcError> {
        let windows = match self.request(Request::Windows).await? {
            Response::Windows(windows) => windows,
            other => {
                return Err(SvcError::Protocol(format!(
                    "expected Windows, got {other:?}"
                )));
            }
        };

        let Some(id) = pick_window(&windows, identities) else {
            debug!("no window matches {identities:?}; nothing to raise");
            return Ok(false);
        };

        self.act(Action::FocusWindow { id }).await?;
        Ok(true)
    }

    /// Raise the most recently focused window for one launcher application.
    ///
    /// Launcher activation must consult niri directly instead of trusting the
    /// event stream: it can be cold or reconnecting while an existing window
    /// is still open. Every supplied desktop-entry or `StartupWMClass`
    /// identity participates in one global MRU choice. Unlike notification
    /// activation, urgency has no bearing on this choice.
    ///
    /// Returns `Ok(false)` only after a live window query found no match.
    pub async fn focus_application(&self, identities: &[&str]) -> Result<bool, SvcError> {
        let windows = match self.request(Request::Windows).await? {
            Response::Windows(windows) => windows,
            other => {
                return Err(SvcError::Protocol(format!(
                    "expected Windows, got {other:?}"
                )));
            }
        };

        let Some(id) = pick_launcher_window(&windows, identities) else {
            debug!("no live window matches launcher application {identities:?}");
            return Ok(false);
        };

        self.act(Action::FocusWindow { id }).await?;
        Ok(true)
    }

    /// Switch to the next configured keyboard layout.
    pub async fn switch_layout_next(&self) -> Result<(), SvcError> {
        self.act(Action::SwitchLayout {
            layout: LayoutSwitchTarget::Next,
        })
        .await
    }

    /// Switch to the previous configured keyboard layout.
    pub async fn switch_layout_prev(&self) -> Result<(), SvcError> {
        self.act(Action::SwitchLayout {
            layout: LayoutSwitchTarget::Prev,
        })
        .await
    }

    /// Ask niri to exit. Used by the Quick Settings log-out action (M9).
    pub async fn quit_compositor(&self) -> Result<(), SvcError> {
        self.act(Action::Quit {
            skip_confirmation: true,
        })
        .await
    }

    /// Perform `action`, mapping a non-`Handled` answer to an error.
    async fn act(&self, action: Action) -> Result<(), SvcError> {
        match self.request(Request::Action(action)).await? {
            Response::Handled => Ok(()),
            other => Err(SvcError::Protocol(format!(
                "expected Handled, got {other:?}"
            ))),
        }
    }

    /// Send `request` and return niri's response.
    async fn request(&self, request: Request) -> Result<Response, SvcError> {
        let payload =
            serde_json::to_string(&request).map_err(|e| SvcError::Protocol(e.to_string()))?;
        match timeout(REQUEST_TIMEOUT, self.exchange(payload)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                // A timed-out connection is in an unknown state: whatever the
                // reply was, it is still queued on it. Throw it away.
                *self.inner.connection.lock().await = None;
                Err(SvcError::Timeout(REQUEST_TIMEOUT))
            }
        }
    }

    /// One request/reply round trip, retried once on a fresh connection.
    async fn exchange(&self, payload: String) -> Result<Response, SvcError> {
        let socket = self.inner.socket.as_ref().ok_or(SvcError::NoNiriSocket)?;
        let mut guard = self.inner.connection.lock().await;

        let mut reply = None;
        if let Some(connection) = guard.as_mut() {
            match round_trip(connection, &payload).await {
                Ok(value) => reply = Some(value),
                Err(error) => {
                    debug!("the niri request connection went away ({error}); reconnecting");
                    *guard = None;
                }
            }
        }

        let reply = match reply {
            Some(reply) => reply,
            None => {
                let stream = UnixStream::connect(socket).await.map_err(SvcError::Io)?;
                let mut connection = Framed::new(stream, LinesCodec::new());
                let reply = round_trip(&mut connection, &payload).await?;
                *guard = Some(connection);
                reply
            }
        };

        reply.map_err(SvcError::Rejected)
    }
}

/// The window to raise for an application named by `identities`.
///
/// Candidates are tried in order, so the `desktop-entry` hint beats the display
/// name rather than merely tying with it. Within one candidate the window
/// asking for attention wins — it is the one that sent the notification — and
/// otherwise the most recently focused one does, which is where the user last
/// left that application.
fn pick_window(windows: &[Window], identities: &[&str]) -> Option<u64> {
    identities.iter().find_map(|identity| {
        windows
            .iter()
            .filter(|window| {
                window
                    .app_id
                    .as_deref()
                    .is_some_and(|app_id| same_app(app_id, identity))
            })
            // Through `Duration`: niri's own `Timestamp` is not orderable.
            .max_by_key(|window| (window.is_urgent, window.focus_timestamp.map(Duration::from)))
            .map(|window| window.id)
    })
}

/// The launcher uses desktop IDs and explicit `StartupWMClass` aliases as one
/// identity set. All candidates compete by focus time, rather than identity
/// order or urgency, so reopening an application always raises where the user
/// last left it.
fn pick_launcher_window(windows: &[Window], identities: &[&str]) -> Option<u64> {
    windows
        .iter()
        .filter(|window| {
            window.app_id.as_deref().is_some_and(|app_id| {
                identities
                    .iter()
                    .any(|identity| same_launcher_application(app_id, identity))
            })
        })
        // Through `Duration`: niri's own `Timestamp` is not orderable.
        // The native ID makes otherwise equal timestamps deterministic.
        .max_by_key(|window| (window.focus_timestamp.map(Duration::from), window.id))
        .map(|window| window.id)
}

/// Whether a window's app id and a notification's idea of its sender are the
/// same application.
///
/// Case-insensitive, and a leading `@` — which some senders put on the display
/// name — is not part of the name.
fn same_app(app_id: &str, identity: &str) -> bool {
    fn normalise(value: &str) -> String {
        value.trim().trim_start_matches('@').to_lowercase()
    }

    normalise(app_id) == normalise(identity)
}

/// Normalise only forms a desktop entry can explicitly name. This accepts
/// niri's bare desktop ID beside its `.desktop` form while retaining exact
/// equality for `StartupWMClass` aliases.
fn same_launcher_application(app_id: &str, identity: &str) -> bool {
    fn normalise(value: &str) -> String {
        let value = value.trim().trim_start_matches('@').to_ascii_lowercase();
        value.strip_suffix(".desktop").unwrap_or(&value).to_owned()
    }

    normalise(app_id) == normalise(identity)
}

/// Write one request and read exactly one reply line.
async fn round_trip(connection: &mut Connection, payload: &str) -> Result<Reply, SvcError> {
    connection
        .send(payload.to_string())
        .await
        .map_err(|e| SvcError::Protocol(format!("could not send the request: {e}")))?;

    let line = connection
        .next()
        .await
        .ok_or_else(|| SvcError::Protocol("niri closed the connection".into()))?
        .map_err(|e| SvcError::Protocol(format!("could not read the reply: {e}")))?;

    serde_json::from_str(&line).map_err(|e| SvcError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form matters: these strings are what niri parses.
    #[test]
    fn actions_serialise_the_way_niri_expects() {
        let focus = serde_json::to_string(&Request::Action(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(7),
        }))
        .expect("serialisable");
        assert_eq!(
            focus,
            r#"{"Action":{"FocusWorkspace":{"reference":{"Id":7}}}}"#
        );

        let layout = serde_json::to_string(&Request::Action(Action::SwitchLayout {
            layout: LayoutSwitchTarget::Next,
        }))
        .expect("serialisable");
        assert_eq!(layout, r#"{"Action":{"SwitchLayout":{"layout":"Next"}}}"#);

        // niri answers actions with `Handled`; anything else is a bug on one
        // side or the other.
        let reply: Reply = serde_json::from_str(r#"{"Ok":"Handled"}"#).expect("parsable");
        assert!(matches!(reply, Ok(Response::Handled)));
    }

    #[test]
    fn a_rejection_carries_niris_own_words() {
        let reply: Reply =
            serde_json::from_str(r#"{"Err":"no workspace with id 999"}"#).expect("parsable");
        let error = reply.map_err(SvcError::Rejected).unwrap_err();
        assert!(error.to_string().contains("no workspace with id 999"));
        assert_eq!(error.user_message(), "The compositor refused the request");
    }

    fn window(id: u64, app_id: &str) -> Window {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": "a window",
            "app_id": app_id,
            "pid": null,
            "workspace_id": 1,
            "is_focused": false,
            "is_floating": false,
            "is_urgent": false,
            "layout": {
                "pos_in_scrolling_layout": [1, 1],
                "tile_size": [100.0, 100.0],
                "window_size": [100, 100],
                "tile_pos_in_workspace_view": null,
                "window_offset_in_tile": [0.0, 0.0],
            },
            "focus_timestamp": null,
        }))
        .expect("a window niri could have sent")
    }

    fn focused_at(mut window: Window, secs: u64) -> Window {
        window.focus_timestamp = Some(Duration::from_secs(secs).into());
        window
    }

    #[test]
    fn the_desktop_entry_is_preferred_over_the_display_name() {
        let windows = [
            window(1, "Telegram Desktop"),
            window(2, "org.telegram.desktop"),
        ];

        assert_eq!(
            pick_window(&windows, &["org.telegram.desktop", "Telegram Desktop"]),
            Some(2)
        );
    }

    #[test]
    fn an_app_id_matches_however_the_sender_capitalised_it() {
        let windows = [window(1, "org.telegram.desktop")];

        assert_eq!(pick_window(&windows, &["@Org.Telegram.Desktop"]), Some(1));
        assert_eq!(pick_window(&windows, &["Telegram"]), None);
    }

    #[test]
    fn the_urgent_window_wins_over_the_recent_one() {
        let mut urgent = focused_at(window(1, "chat"), 10);
        urgent.is_urgent = true;
        let windows = [focused_at(window(2, "chat"), 500), urgent];

        assert_eq!(pick_window(&windows, &["chat"]), Some(1));
    }

    #[test]
    fn otherwise_the_most_recently_focused_window_is_raised() {
        let windows = [
            focused_at(window(1, "chat"), 10),
            focused_at(window(2, "chat"), 500),
        ];

        assert_eq!(pick_window(&windows, &["chat"]), Some(2));
    }

    #[test]
    fn launcher_uses_global_mru_across_desktop_id_and_wm_class() {
        let mut urgent = focused_at(window(1, "org.example.Editor"), 10);
        urgent.is_urgent = true;
        let windows = [
            urgent,
            focused_at(window(2, "EditorWindow"), 500),
            focused_at(window(3, "unrelated"), 900),
        ];

        assert_eq!(
            pick_launcher_window(&windows, &["org.example.Editor.desktop", "EditorWindow"]),
            Some(2)
        );
    }

    #[test]
    fn launcher_identity_matching_rejects_partial_names() {
        let windows = [window(1, "org.example.Editor")];

        assert_eq!(pick_launcher_window(&windows, &["org.example.Edit"]), None);
    }

    #[test]
    fn launcher_normalises_desktop_suffix_case_before_exact_comparison() {
        let windows = [window(1, "ORG.EXAMPLE.EDITOR.DESKTOP")];

        assert_eq!(
            pick_launcher_window(&windows, &["org.example.Editor"]),
            Some(1)
        );
    }

    #[test]
    fn a_sender_with_nothing_open_matches_no_window() {
        assert_eq!(pick_window(&[window(1, "chat")], &["mail"]), None);
        assert_eq!(pick_window(&[], &["chat"]), None);
    }

    #[tokio::test]
    async fn without_a_socket_every_action_fails_clearly() {
        let handle = NiriHandle::new(None);
        let error = handle.focus_workspace(1).await.unwrap_err();
        assert!(matches!(error, SvcError::NoNiriSocket));
        assert_eq!(error.user_message(), "Could not reach the compositor");
    }

    #[tokio::test]
    async fn a_missing_socket_file_is_an_io_error_not_a_panic() {
        let handle = NiriHandle::new(Some(PathBuf::from("/nonexistent/topbar/niri-test.sock")));
        let error = handle.switch_layout_next().await.unwrap_err();
        assert!(matches!(error, SvcError::Io(_)), "{error}");
    }
}
