//! Two native Mango watches form one live panel connection.
//!
//! Mango 0.18.2 `handle_watch_command` in `src/ipc/ipc.c` registers each watch
//! on its own socket and immediately sends its full replacement snapshot.
//! Neither half is published live before both initial replacements arrive.
//! Any framing, parsing, or socket failure drops the pair and starts afresh.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio_util::codec::{Framed, LinesCodec};
use topbar_core::config::MangoConfig;
use tracing::{debug, info, warn};

use crate::compositor::{KeyboardLayoutSnapshot, WindowsSnapshot, WorkspacesSnapshot};
use crate::error::SvcError;

use super::snapshot::{self, Client, Monitor, Registry};

const MAX_LINE: usize = 1024 * 1024;
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

pub(crate) struct Publishers {
    pub workspaces: watch::Sender<Arc<WorkspacesSnapshot>>,
    pub keyboard_layout: watch::Sender<Arc<KeyboardLayoutSnapshot>>,
    pub windows: watch::Sender<Arc<WindowsSnapshot>>,
}

pub(crate) async fn run(
    socket: PathBuf,
    config: Arc<MangoConfig>,
    registry: Arc<Mutex<Registry>>,
    publishers: Publishers,
    mut kicks: mpsc::Receiver<()>,
) {
    let mut backoff = BACKOFF_START;
    loop {
        let mut handshaken = false;
        let result = tokio::select! {
            result = session(&socket, &config, &registry, &publishers, &mut handshaken) => Some(result),
            () = kicked(&mut kicks) => None,
        };
        publishers.publish_disconnected();
        match result {
            None => {
                info!("reconnecting to Mango to refresh this session's state");
                backoff = BACKOFF_START;
                continue;
            }
            Some(Ok(())) => info!("Mango closed a watch socket; reconnecting"),
            Some(Err(error)) => warn!("Mango watch stream: {error}"),
        }
        if handshaken {
            backoff = BACKOFF_START;
        }
        debug!("reconnecting to Mango in {backoff:?}");
        tokio::select! {
            () = tokio::time::sleep(backoff) => {
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            () = kicked(&mut kicks) => {
                backoff = BACKOFF_START;
            }
        }
    }
}

/// A closed kick channel must not turn either select into a busy loop.
async fn kicked(kicks: &mut mpsc::Receiver<()>) {
    if kicks.recv().await.is_none() {
        pending::<()>().await;
    }
}

async fn session(
    socket: &Path,
    config: &MangoConfig,
    registry: &Mutex<Registry>,
    publishers: &Publishers,
    handshaken: &mut bool,
) -> Result<(), SvcError> {
    let monitor_socket = UnixStream::connect(socket).await.map_err(SvcError::Io)?;
    let client_socket = UnixStream::connect(socket).await.map_err(SvcError::Io)?;
    let mut monitor_watch = Framed::new(monitor_socket, LinesCodec::new_with_max_length(MAX_LINE));
    let mut client_watch = Framed::new(client_socket, LinesCodec::new_with_max_length(MAX_LINE));
    monitor_watch
        .send("watch all-monitors".to_owned())
        .await
        .map_err(|error| {
            SvcError::Protocol(format!("could not start Mango monitor watch: {error}"))
        })?;
    client_watch
        .send("watch all-clients".to_owned())
        .await
        .map_err(|error| {
            SvcError::Protocol(format!("could not start Mango client watch: {error}"))
        })?;

    let mut monitors: Option<Vec<Monitor>> = None;
    let mut clients: Option<Vec<Client>> = None;
    loop {
        tokio::select! {
            line = monitor_watch.next() => {
                let Some(line) = line else { return Ok(()) };
                let line = line.map_err(|error| {
                    SvcError::Protocol(format!("could not read Mango monitor watch: {error}"))
                })?;
                let replacement = snapshot::parse_monitors(&line)?;
                if let Some(clients) = clients.as_ref() {
                    publishers.publish_monitors(&replacement, config, registry)?;
                    if !*handshaken {
                        publishers.publish_clients(clients);
                        *handshaken = true;
                    }
                }
                monitors = Some(replacement);
            }
            line = client_watch.next() => {
                let Some(line) = line else { return Ok(()) };
                let line = line.map_err(|error| {
                    SvcError::Protocol(format!("could not read Mango client watch: {error}"))
                })?;
                let replacement = snapshot::parse_clients(&line)?;
                if let Some(monitors) = monitors.as_ref() {
                    if !*handshaken {
                        publishers.publish_monitors(monitors, config, registry)?;
                    }
                    publishers.publish_clients(&replacement);
                    *handshaken = true;
                }
                clients = Some(replacement);
            }
        }
    }
}

impl Publishers {
    fn publish_monitors(
        &self,
        monitors: &[Monitor],
        config: &MangoConfig,
        registry: &Mutex<Registry>,
    ) -> Result<(), SvcError> {
        let workspaces = registry
            .lock()
            .map_err(|_| SvcError::Protocol("Mango connector registry lock poisoned".into()))?
            .workspaces(monitors, config, true)?;
        send_if_changed(&self.workspaces, workspaces);
        send_if_changed(
            &self.keyboard_layout,
            snapshot::keyboard_layout(monitors, config, true),
        );
        Ok(())
    }

    fn publish_clients(&self, clients: &[Client]) {
        send_if_changed(&self.windows, snapshot::windows(clients, true));
    }

    fn publish_disconnected(&self) {
        // Drop watch read guards before send_if_changed acquires the write lock.
        let workspaces = self
            .workspaces
            .borrow()
            .as_ref()
            .clone()
            .with_connected(false);
        send_if_changed(&self.workspaces, workspaces);
        let keyboard = self
            .keyboard_layout
            .borrow()
            .as_ref()
            .clone()
            .with_connected(false);
        send_if_changed(&self.keyboard_layout, keyboard);
        send_if_changed(&self.windows, WindowsSnapshot::default());
    }
}

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
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::*;

    // Source-derived JSON, not a capture: Mango 0.18.2 src/ipc/ipc.c,
    // build_monitor_json/build_tags_json/monitor_active_client and
    // build_client_json. Each watch line is a whole replacement object.
    const MONITORS: &str = r#"{"monitors":[{"name":"eDP-1","active":true,"width":1920,"height":1080,"tags":[{"index":1,"is_active":true,"is_urgent":false,"client_count":1},{"index":2,"is_active":false,"is_urgent":true,"client_count":0}],"active_tags":[1],"active_client":{"id":4,"title":"Editor","appid":"emacs"},"keyboardlayout":"English (US)"}]}"#;
    const CLIENTS: &str = r#"{"clients":[{"id":4,"title":"Editor","appid":"emacs","monitor":"eDP-1","tags":[1],"is_focused":true,"is_urgent":false}]}"#;
    const EMPTY_CLIENTS: &str = r#"{"clients":[]}"#;
    const WAIT: Duration = Duration::from_secs(3);

    struct Socket {
        directory: PathBuf,
        path: PathBuf,
        listener: UnixListener,
    }

    impl Socket {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "topbar-mango-watch-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("ipc.sock");
            let listener = UnixListener::bind(&path).unwrap();
            Self {
                directory,
                path,
                listener,
            }
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    struct Running {
        task: JoinHandle<()>,
        kicks: mpsc::Sender<()>,
        workspaces: watch::Receiver<Arc<WorkspacesSnapshot>>,
        keyboard: watch::Receiver<Arc<KeyboardLayoutSnapshot>>,
        windows: watch::Receiver<Arc<WindowsSnapshot>>,
    }

    impl Running {
        fn start(socket: &Socket) -> Self {
            let (workspaces_tx, workspaces) =
                watch::channel(Arc::new(WorkspacesSnapshot::default()));
            let (keyboard_tx, keyboard) =
                watch::channel(Arc::new(KeyboardLayoutSnapshot::default()));
            let (windows_tx, windows) = watch::channel(Arc::new(WindowsSnapshot::default()));
            let (kicks, kicks_rx) = mpsc::channel(1);
            let config = Arc::new(MangoConfig {
                persistent_tags: vec![2],
                keyboard_layouts: vec!["us".into(), "de".into()],
            });
            let task = tokio::spawn(run(
                socket.path.clone(),
                config,
                Arc::new(Mutex::new(Registry::default())),
                Publishers {
                    workspaces: workspaces_tx,
                    keyboard_layout: keyboard_tx,
                    windows: windows_tx,
                },
                kicks_rx,
            ));
            Self {
                task,
                kicks,
                workspaces,
                keyboard,
                windows,
            }
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn accept_pair(
        socket: &Socket,
    ) -> (
        Framed<UnixStream, LinesCodec>,
        Framed<UnixStream, LinesCodec>,
    ) {
        timeout(WAIT, async {
            let (monitors, _) = socket.listener.accept().await.unwrap();
            let mut monitors = Framed::new(monitors, LinesCodec::new());
            assert_eq!(
                monitors.next().await.unwrap().unwrap(),
                "watch all-monitors"
            );
            let (clients, _) = socket.listener.accept().await.unwrap();
            let mut clients = Framed::new(clients, LinesCodec::new());
            assert_eq!(clients.next().await.unwrap().unwrap(), "watch all-clients");
            (monitors, clients)
        })
        .await
        .unwrap()
    }

    async fn live(running: &mut Running) {
        workspace_connection(&mut running.workspaces, true).await;
        assert!(running.keyboard.borrow().connected);
        assert!(running.windows.borrow().connected);
    }

    async fn workspace_connection(
        receiver: &mut watch::Receiver<Arc<WorkspacesSnapshot>>,
        connected: bool,
    ) {
        timeout(WAIT, async {
            loop {
                if receiver.borrow_and_update().connected == connected {
                    return;
                }
                receiver.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    async fn window_count(receiver: &mut watch::Receiver<Arc<WindowsSnapshot>>, count: usize) {
        timeout(WAIT, async {
            loop {
                let matches = {
                    let snapshot = receiver.borrow_and_update();
                    snapshot.connected && snapshot.windows.len() == count
                };
                if matches {
                    return;
                }
                receiver.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn both_initial_replacements_are_required_and_split_lines_are_reassembled() {
        let socket = Socket::new();
        let mut running = Running::start(&socket);
        let (mut monitors, mut clients) = accept_pair(&socket).await;
        let middle = MONITORS.len() / 2;
        monitors
            .get_mut()
            .write_all(&MONITORS.as_bytes()[..middle])
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(30), running.workspaces.changed())
                .await
                .is_err()
        );
        monitors
            .get_mut()
            .write_all(format!("{}\n", &MONITORS[middle..]).as_bytes())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(30), running.workspaces.changed())
                .await
                .is_err()
        );
        assert!(!running.keyboard.borrow().connected);
        assert!(!running.windows.borrow().connected);
        clients.send(CLIENTS.to_owned()).await.unwrap();
        live(&mut running).await;
        assert_eq!(running.workspaces.borrow().for_output("eDP-1").len(), 2);
        assert_eq!(running.keyboard.borrow().current(), Some("English (US)"));
        assert_eq!(running.keyboard.borrow().layout_count, 2);
        assert_eq!(running.windows.borrow().windows[0].id, 4);
    }

    #[tokio::test]
    async fn client_first_initialization_and_coalesced_lines_replace_instead_of_merge() {
        let socket = Socket::new();
        let mut running = Running::start(&socket);
        let (mut monitors, mut clients) = accept_pair(&socket).await;
        clients.send(CLIENTS.to_owned()).await.unwrap();
        assert!(
            timeout(Duration::from_millis(30), running.windows.changed())
                .await
                .is_err()
        );
        monitors.send(MONITORS.to_owned()).await.unwrap();
        live(&mut running).await;
        clients
            .get_mut()
            .write_all(format!("{CLIENTS}\n{EMPTY_CLIENTS}\n").as_bytes())
            .await
            .unwrap();
        window_count(&mut running.windows, 0).await;
        // Cross-watch transitions may temporarily leave monitor active_client
        // absent from the latest clients list. Native watches are not atomic.
        assert!(running.workspaces.borrow().connected);
        monitors
            .send(r#"{"monitors":[]}"#.to_owned())
            .await
            .unwrap();
        timeout(WAIT, async {
            loop {
                if running.workspaces.borrow_and_update().outputs.is_empty() {
                    break;
                }
                running.workspaces.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(running.workspaces.borrow().connected);
        assert!(running.keyboard.borrow().connected);
    }

    #[tokio::test]
    async fn disabled_native_output_geometry_does_not_remove_watch_presence_or_tags() {
        let socket = Socket::new();
        let mut running = Running::start(&socket);
        let (mut monitors, mut clients) = accept_pair(&socket).await;
        monitors.send(MONITORS.to_owned()).await.unwrap();
        clients.send(CLIENTS.to_owned()).await.unwrap();
        live(&mut running).await;
        let id = running.workspaces.borrow().for_output("eDP-1")[0].id;
        let mut disabled: serde_json::Value = serde_json::from_str(MONITORS).unwrap();
        disabled["monitors"][0]["width"] = serde_json::json!(0);
        disabled["monitors"][0]["height"] = serde_json::json!(0);
        disabled["monitors"][0]["keyboardlayout"] = serde_json::json!("German");
        monitors.send(disabled.to_string()).await.unwrap();
        timeout(WAIT, async {
            loop {
                if running.keyboard.borrow_and_update().current() == Some("German") {
                    break;
                }
                running.keyboard.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let workspaces = running.workspaces.borrow();
        assert!(workspaces.connected);
        assert_eq!(workspaces.for_output("eDP-1").len(), 2);
        assert_eq!(workspaces.for_output("eDP-1")[0].id, id);
        assert!(running.windows.borrow().connected);
    }

    #[tokio::test]
    async fn equivalent_replacements_do_not_notify_any_projection() {
        let socket = Socket::new();
        let mut running = Running::start(&socket);
        let (mut monitors, mut clients) = accept_pair(&socket).await;
        monitors.send(MONITORS.to_owned()).await.unwrap();
        clients.send(CLIENTS.to_owned()).await.unwrap();
        live(&mut running).await;
        running.workspaces.mark_unchanged();
        running.keyboard.mark_unchanged();
        running.windows.mark_unchanged();
        let monitors_with_unknown = MONITORS.replacen("{", "{\"future\":42,", 1);
        let clients_with_unknown = CLIENTS.replacen("{", "{\"future\":true,", 1);
        monitors.send(monitors_with_unknown).await.unwrap();
        clients.send(clients_with_unknown).await.unwrap();
        assert!(
            timeout(Duration::from_millis(50), running.workspaces.changed())
                .await
                .is_err()
        );
        assert!(!running.keyboard.has_changed().unwrap());
        assert!(!running.windows.has_changed().unwrap());
    }

    #[tokio::test]
    async fn either_socket_disconnect_dims_retained_state_clears_windows_and_reconnects_pair() {
        for disconnect_monitors in [true, false] {
            let socket = Socket::new();
            let mut running = Running::start(&socket);
            let (mut monitors, mut clients) = accept_pair(&socket).await;
            monitors.send(MONITORS.to_owned()).await.unwrap();
            clients.send(CLIENTS.to_owned()).await.unwrap();
            live(&mut running).await;
            let id = running.workspaces.borrow().for_output("eDP-1")[0].id;
            let mut surviving = if disconnect_monitors {
                drop(monitors);
                clients
            } else {
                drop(clients);
                monitors
            };
            workspace_connection(&mut running.workspaces, false).await;
            assert_eq!(running.workspaces.borrow().for_output("eDP-1")[0].id, id);
            assert!(!running.keyboard.borrow().connected);
            assert_eq!(running.keyboard.borrow().current(), Some("English (US)"));
            assert!(!running.windows.borrow().connected);
            assert!(running.windows.borrow().windows.is_empty());
            assert!(timeout(WAIT, surviving.next()).await.unwrap().is_none());
            let (mut monitors, mut clients) = accept_pair(&socket).await;
            clients.send(CLIENTS.to_owned()).await.unwrap();
            assert!(!running.workspaces.borrow().connected);
            monitors.send(MONITORS.to_owned()).await.unwrap();
            live(&mut running).await;
            assert_eq!(running.workspaces.borrow().for_output("eDP-1")[0].id, id);
        }
    }

    #[tokio::test]
    async fn malformed_inconsistent_and_oversized_lines_drop_the_pair_without_panicking() {
        let inconsistent = MONITORS.replace("\"active_tags\":[1]", "\"active_tags\":[7]");
        let oversized = "x".repeat(MAX_LINE + 1);
        for (monitor_side, bad) in [
            (true, "not JSON"),
            (false, "{}"),
            (true, inconsistent.as_str()),
            (false, oversized.as_str()),
        ] {
            let socket = Socket::new();
            let mut running = Running::start(&socket);
            let (mut monitors, mut clients) = accept_pair(&socket).await;
            monitors.send(MONITORS.to_owned()).await.unwrap();
            clients.send(CLIENTS.to_owned()).await.unwrap();
            live(&mut running).await;
            let failed = if monitor_side {
                &mut monitors
            } else {
                &mut clients
            };
            // A bounded decoder may close while the oversized write is still
            // in progress. Either successful delivery or BrokenPipe is valid.
            let _ = failed
                .get_mut()
                .write_all(format!("{bad}\n").as_bytes())
                .await;
            workspace_connection(&mut running.workspaces, false).await;
            assert!(!running.task.is_finished());
            let (mut monitors, mut clients) = accept_pair(&socket).await;
            monitors.send(MONITORS.to_owned()).await.unwrap();
            clients.send(EMPTY_CLIENTS.to_owned()).await.unwrap();
            live(&mut running).await;
            assert!(running.windows.borrow().windows.is_empty());
        }
    }

    #[tokio::test]
    async fn health_check_interrupts_live_session_and_reconnect_backoff() {
        let socket = Socket::new();
        let mut running = Running::start(&socket);
        let (mut monitors, mut clients) = accept_pair(&socket).await;
        monitors.send(MONITORS.to_owned()).await.unwrap();
        clients.send(CLIENTS.to_owned()).await.unwrap();
        live(&mut running).await;
        running.kicks.send(()).await.unwrap();
        assert!(timeout(WAIT, monitors.next()).await.unwrap().is_none());
        assert!(timeout(WAIT, clients.next()).await.unwrap().is_none());
        let (mut monitors, mut clients) = accept_pair(&socket).await;
        monitors.send(MONITORS.to_owned()).await.unwrap();
        clients.send(CLIENTS.to_owned()).await.unwrap();
        live(&mut running).await;
        drop(monitors);
        workspace_connection(&mut running.workspaces, false).await;
        running.kicks.send(()).await.unwrap();
        // This is below BACKOFF_START: the kick must cancel the sleep, not be
        // saved until the old deadline has elapsed.
        let (mut monitors, mut clients) = timeout(Duration::from_millis(200), accept_pair(&socket))
            .await
            .unwrap();
        monitors.send(MONITORS.to_owned()).await.unwrap();
        clients.send(CLIENTS.to_owned()).await.unwrap();
        live(&mut running).await;
    }
}
