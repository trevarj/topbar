//! The niri compositor service: one event stream in, actions out.

mod requests;
mod snapshot;
mod stream;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;
use tracing::error;

use crate::compositor::{KeyboardLayoutSnapshot, WindowsSnapshot, WorkspacesSnapshot};
pub use requests::NiriHandle;

/// Everything the panel needs from niri.
///
/// Cloning is cheap: the handle is reference-counted and the receivers are
/// watch subscriptions, so every bar and widget can hold its own copy.
#[derive(Clone)]
pub struct Niri {
    handle: NiriHandle,
    workspaces: watch::Receiver<Arc<WorkspacesSnapshot>>,
    keyboard_layout: watch::Receiver<Arc<KeyboardLayoutSnapshot>>,
    windows: watch::Receiver<Arc<WindowsSnapshot>>,
    /// Asks the stream task to start a fresh connection. See
    /// [`Niri::health_check`].
    kicks: tokio::sync::mpsc::Sender<()>,
}

impl Niri {
    /// Connect to niri at `socket` and start the event-stream task.
    ///
    /// Must be called from inside the service runtime. `None` — no
    /// `$NIRI_SOCKET` — is not fatal: the snapshots stay disconnected and the
    /// widgets that need them dim, which is also what a compositor restart
    /// looks like.
    pub(crate) fn start(socket: Option<PathBuf>) -> Self {
        let (workspaces_tx, workspaces) = watch::channel(Arc::new(WorkspacesSnapshot::default()));
        let (keyboard_tx, keyboard_layout) =
            watch::channel(Arc::new(KeyboardLayoutSnapshot::default()));
        let (windows_tx, windows) = watch::channel(Arc::new(WindowsSnapshot::default()));

        let (kicks, kick_queue) = tokio::sync::mpsc::channel(1);
        match socket.clone() {
            Some(path) => {
                tokio::spawn(stream::run(
                    path,
                    stream::Publishers {
                        workspaces: workspaces_tx,
                        keyboard_layout: keyboard_tx,
                        windows: windows_tx,
                    },
                    kick_queue,
                ));
            }
            None => {
                error!("no niri socket; the workspace and keyboard-layout widgets will stay dimmed")
            }
        }

        Self {
            handle: NiriHandle::new(socket),
            workspaces,
            keyboard_layout,
            windows,
            kicks,
        }
    }

    /// Reconnect the event stream, whatever state it thinks it is in.
    ///
    /// [`crate::lifecycle`] calls this on resume. A socket that was open when
    /// the machine went to sleep can come back open and silent, and a workspace
    /// strip showing the state from before the lid closed is exactly the defect
    /// v1 shipped. Reconnecting costs one round trip and a full replay.
    ///
    /// Dropped rather than queued when a reconnect is already pending: two
    /// reconnects in a row would be one reconnect and one wasted connection.
    pub fn health_check(&self) {
        let _ = self.kicks.try_send(());
    }

    /// The handle actions are sent through.
    pub fn handle(&self) -> &NiriHandle {
        &self.handle
    }

    /// Subscribe to workspace state.
    pub fn workspaces(&self) -> watch::Receiver<Arc<WorkspacesSnapshot>> {
        self.workspaces.clone()
    }

    /// Subscribe to keyboard-layout state.
    pub fn keyboard_layout(&self) -> watch::Receiver<Arc<KeyboardLayoutSnapshot>> {
        self.keyboard_layout.clone()
    }

    /// Subscribe to the window projection.
    pub fn windows(&self) -> watch::Receiver<Arc<WindowsSnapshot>> {
        self.windows.clone()
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end tests over a real Unix socket with a stand-in compositor.
    //!
    //! These exercise the whole service — framing, handshake, reducer,
    //! projection, watch publishing, reconnect — without needing niri, and
    //! they are where the "rapid switching leaves no stale state" claim is
    //! actually proven. A nested compositor cannot do it: its workspace
    //! switches are gated on redraws, so it will not produce a burst.

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::*;

    /// How long a test waits for the service to catch up before failing.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// The first event of any connection: two workspaces on one output.
    fn workspaces_line(names: [&str; 2], active: u64) -> String {
        let workspace = |id: u64, idx: u8, name: &str| {
            format!(
                r#"{{"id":{id},"idx":{idx},"name":"{name}","output":"eDP-1","is_urgent":false,"is_active":{active_flag},"is_focused":{active_flag},"active_window_id":null}}"#,
                active_flag = id == active
            )
        };
        format!(
            r#"{{"WorkspacesChanged":{{"workspaces":[{},{}]}}}}"#,
            workspace(1, 1, names[0]),
            workspace(2, 2, names[1])
        )
    }

    fn activate(id: u64) -> String {
        format!(r#"{{"WorkspaceActivated":{{"id":{id},"focused":true}}}}"#)
    }

    /// A unique socket path for one test.
    fn socket_path() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "topbar-test-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Accept one connection, answer the handshake, and write `lines`.
    ///
    /// Returns when the client hangs up or the writes fail, which is what
    /// lets a test hand the same path to a second server and watch the
    /// service resynchronise.
    async fn serve_once(listener: &UnixListener, lines: Vec<String>) {
        let (stream, _) = listener.accept().await.expect("a client should connect");
        let (read, mut write) = stream.into_split();

        let mut request = String::new();
        BufReader::new(read)
            .read_line(&mut request)
            .await
            .expect("the service should send a request");
        assert_eq!(request.trim(), r#""EventStream""#);

        write
            .write_all(b"{\"Ok\":\"Handled\"}\n")
            .await
            .expect("handshake reply");
        for line in lines {
            if write
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
        // Hold the connection open; dropping it would look like a compositor
        // restart and send the service round the reconnect loop.
        std::future::pending::<()>().await;
    }

    /// Wait until `predicate` accepts a published snapshot.
    async fn wait_for(
        receiver: &mut watch::Receiver<Arc<WorkspacesSnapshot>>,
        what: &str,
        predicate: impl Fn(&WorkspacesSnapshot) -> bool,
    ) -> Arc<WorkspacesSnapshot> {
        let wait = async {
            loop {
                // Clone out before testing: holding one read guard while
                // taking another deadlocks against a writer waiting in
                // between, and the burst test has a writer waiting constantly.
                let snapshot = receiver.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                receiver.changed().await.expect("the service is alive");
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    fn active_idx(snapshot: &WorkspacesSnapshot) -> Option<usize> {
        snapshot
            .for_output("eDP-1")
            .iter()
            .find(|view| view.is_active)
            .map(|view| view.idx)
    }

    /// Running outside niri — a TTY login, a CI runner, `cargo test` — has to
    /// produce a quiet, disconnected service rather than a failure.
    #[tokio::test]
    async fn without_a_socket_the_service_starts_disconnected() {
        let niri = Niri::start(None);

        let workspaces = niri.workspaces();
        assert!(!workspaces.borrow().connected);
        assert!(workspaces.borrow().outputs.is_empty());
        assert_eq!(workspaces.borrow().focused_output, None);

        let layouts = niri.keyboard_layout();
        assert!(!layouts.borrow().connected);
        assert!(!layouts.borrow().is_switchable());
        assert_eq!(layouts.borrow().current(), None);

        assert!(niri.handle().focus_workspace(1).await.is_err());
    }

    /// The M2 acceptance run: a burst of switches far faster than any
    /// compositor would send them, ending in exactly the right state.
    #[tokio::test]
    async fn a_burst_of_switches_leaves_no_stale_state() {
        const SWITCHES: usize = 600;

        let path = socket_path();
        let listener = UnixListener::bind(&path).expect("bind the stand-in socket");

        let mut lines = vec![workspaces_line(["one", "two"], 1)];
        // Alternate between the two workspaces, ending on the second.
        lines.extend((0..SWITCHES).map(|switch| activate(if switch % 2 == 0 { 2 } else { 1 })));
        assert_eq!(lines.len(), SWITCHES + 1);

        tokio::spawn(async move { serve_once(&listener, lines).await });

        let niri = Niri::start(Some(path.clone()));
        let mut workspaces = niri.workspaces();

        let settled = wait_for(&mut workspaces, "the burst to land", |snapshot| {
            snapshot.connected && active_idx(snapshot) == Some(1)
        })
        .await;

        // Whatever intermediate frames were coalesced away, the state that
        // survives is the last event's, and it is self-consistent.
        let views = settled.for_output("eDP-1");
        assert_eq!(views.len(), 2);
        assert_eq!(
            views.iter().filter(|view| view.is_active).count(),
            1,
            "exactly one active workspace"
        );
        assert_eq!(
            views.iter().filter(|view| view.is_focused).count(),
            1,
            "exactly one focused workspace"
        );
        assert_eq!(settled.focused_output.as_deref(), Some("eDP-1"));

        let _ = std::fs::remove_file(&path);
    }

    /// Losing the compositor and getting it back: the panel dims, then
    /// rebuilds from the new connection's own full state.
    #[tokio::test]
    async fn a_dropped_connection_resynchronises_from_scratch() {
        let path = socket_path();

        // First compositor: workspaces "one"/"two", second one active.
        let listener = UnixListener::bind(&path).expect("bind");
        let first = tokio::spawn(async move {
            serve_once(&listener, vec![workspaces_line(["one", "two"], 2)]).await
        });

        let niri = Niri::start(Some(path.clone()));
        let mut workspaces = niri.workspaces();
        let before = wait_for(&mut workspaces, "the first connection", |snapshot| {
            snapshot.connected && !snapshot.outputs.is_empty()
        })
        .await;
        assert_eq!(active_idx(&before), Some(2));

        // The compositor goes away. The last state stays on screen, dimmed.
        first.abort();
        let _ = std::fs::remove_file(&path);
        let gone = wait_for(&mut workspaces, "the drop to show up", |snapshot| {
            !snapshot.connected
        })
        .await;
        assert_eq!(
            gone.for_output("eDP-1").len(),
            2,
            "a disconnect dims the widget, it does not empty it"
        );

        // A different compositor comes back on the same path with different
        // workspaces. Nothing of the old state may survive.
        let listener = UnixListener::bind(&path).expect("rebind");
        tokio::spawn(async move {
            serve_once(&listener, vec![workspaces_line(["alpha", "beta"], 1)]).await
        });

        let after = wait_for(&mut workspaces, "the reconnect", |snapshot| {
            snapshot.connected && active_idx(snapshot) == Some(1)
        })
        .await;
        assert_eq!(
            after
                .for_output("eDP-1")
                .iter()
                .map(|view| view.name.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"],
            "the new connection's state replaces the old one wholesale"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A line the service cannot read must not cost it the connection.
    #[tokio::test]
    async fn garbage_in_the_middle_of_the_stream_is_stepped_over() {
        let path = socket_path();
        let listener = UnixListener::bind(&path).expect("bind");

        let lines = vec![
            workspaces_line(["one", "two"], 1),
            "}{ not json".to_string(),
            r#"{"WorkspacesChanged":{"workspaces":[{"id":1,"#.to_string(),
            r#"{"AnEventFromAFutureNiri":{"whatever":true}}"#.to_string(),
            activate(2),
        ];
        tokio::spawn(async move { serve_once(&listener, lines).await });

        let niri = Niri::start(Some(path.clone()));
        let mut workspaces = niri.workspaces();
        let settled = wait_for(&mut workspaces, "the event after the garbage", |snapshot| {
            active_idx(snapshot) == Some(2)
        })
        .await;
        assert!(settled.connected, "the connection survived the garbage");

        let _ = std::fs::remove_file(&path);
    }

    /// Actions go out on their own connection, so they still work while the
    /// event stream is busy being an event stream.
    #[tokio::test]
    async fn actions_use_a_second_connection() {
        let path = socket_path();
        let listener = UnixListener::bind(&path).expect("bind");

        tokio::spawn(async move {
            // First connection: the event stream.
            let (stream, _) = listener.accept().await.expect("event stream connects");
            let (read, mut write) = stream.into_split();
            let mut request = String::new();
            BufReader::new(read)
                .read_line(&mut request)
                .await
                .expect("request");
            assert_eq!(request.trim(), r#""EventStream""#);
            write
                .write_all(b"{\"Ok\":\"Handled\"}\n")
                .await
                .expect("ok");
            write
                .write_all(format!("{}\n", workspaces_line(["one", "two"], 1)).as_bytes())
                .await
                .expect("state");

            // Second connection: two sequential actions, one connection.
            let (stream, _) = listener.accept().await.expect("actions connect");
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let mut seen = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                seen.push(line);
                let reply = if seen.len() == 1 {
                    "{\"Ok\":\"Handled\"}\n"
                } else {
                    "{\"Err\":\"no such workspace\"}\n"
                };
                if write.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
            }
            seen
        });

        let niri = Niri::start(Some(path.clone()));
        let mut workspaces = niri.workspaces();
        wait_for(&mut workspaces, "the event stream", |snapshot| {
            snapshot.connected && !snapshot.outputs.is_empty()
        })
        .await;

        niri.handle()
            .focus_workspace(2)
            .await
            .expect("the first action is handled");
        let rejected = niri
            .handle()
            .focus_workspace(99)
            .await
            .expect_err("the second is refused");
        assert!(rejected.to_string().contains("no such workspace"));
        assert_eq!(
            rejected.user_message(),
            "The compositor refused the request"
        );

        let _ = std::fs::remove_file(&path);
    }
}
