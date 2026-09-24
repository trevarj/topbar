//! Socket2 invalidates complete snapshots; it is deliberately not another reducer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, interval_at, sleep, sleep_until, timeout};
use tokio_util::codec::{FramedRead, LinesCodec};

use super::requests::{DEADLINE, HyprlandHandle, Live, Peer, valid_address};
use super::snapshot::{SNAPSHOT_REQUEST, project};
use crate::SvcError;
use crate::compositor::{KeyboardLayoutSnapshot, WorkspacesSnapshot};

const COALESCE: Duration = Duration::from_millis(35);
const RECONCILE: Duration = Duration::from_secs(30);
const BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const LINE_LIMIT: usize = 1024 * 1024;

pub(super) async fn run(
    directory: PathBuf,
    handle: HyprlandHandle,
    live: watch::Sender<Live>,
    workspaces: watch::Sender<Arc<WorkspacesSnapshot>>,
    keyboard: watch::Sender<Arc<KeyboardLayoutSnapshot>>,
    mut kicks: mpsc::Receiver<()>,
) {
    let mut backoff = BACKOFF;
    loop {
        let mut published = false;
        let result = tokio::select! {
            result = session(&directory, &handle, &live, &workspaces, &keyboard, &mut published) => Some(result),
            kick = kicks.recv() => {
                if kick.is_none() { return; }
                None
            },
        };
        live.send_modify(|live| {
            live.peer = None;
            live.urgent.clear();
        });
        workspaces.send_if_modified(|last| {
            if !last.connected {
                return false;
            }
            *last = Arc::new((**last).clone().with_connected(false));
            true
        });
        keyboard.send_if_modified(|last| {
            if !last.connected {
                return false;
            }
            *last = Arc::new((**last).clone().with_connected(false));
            true
        });
        if let Some(Err(error)) = &result {
            tracing::warn!("Hyprland disconnected: {error}");
        }
        if result.is_none() {
            backoff = BACKOFF;
            continue;
        }
        if published {
            backoff = BACKOFF;
        }
        tokio::select! {
            _ = sleep(backoff) => {},
            kick = kicks.recv() => {
                if kick.is_none() { return; }
                backoff = BACKOFF;
                continue;
            },
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn session(
    directory: &std::path::Path,
    handle: &HyprlandHandle,
    live: &watch::Sender<Live>,
    workspaces: &watch::Sender<Arc<WorkspacesSnapshot>>,
    keyboard: &watch::Sender<Arc<KeyboardLayoutSnapshot>>,
    published: &mut bool,
) -> Result<(), SvcError> {
    // socket2 is connected FIRST: events occurring during every query below
    // are drained and request a trailing refresh, including during bootstrap.
    let socket = timeout(
        DEADLINE,
        UnixStream::connect(directory.join(".socket2.sock")),
    )
    .await
    .map_err(|_| SvcError::Timeout(DEADLINE))?
    .map_err(SvcError::Io)?;
    let pid = socket
        .peer_cred()
        .map_err(SvcError::Io)?
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| SvcError::Protocol("Hyprland event socket has no peer PID".into()))?;
    let peer = Peer::read(std::path::Path::new("/proc"), pid).await?;
    live.send_modify(|live| live.peer = Some(peer));
    let mut events = FramedRead::new(socket, LinesCodec::new_with_max_length(LINE_LIMIT));
    let mut reconcile = interval_at(Instant::now() + RECONCILE, RECONCILE);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut bootstrap = true;
    loop {
        while !dirty {
            tokio::select! {
                line = events.next() => dirty |= event(line, live)?,
                _ = reconcile.tick() => dirty = true,
            }
        }
        if !bootstrap {
            let until = Instant::now() + COALESCE;
            loop {
                tokio::select! {
                    _ = sleep_until(until) => break,
                    line = events.next() => { event(line, live)?; },
                }
            }
        }
        bootstrap = false;
        dirty = false;
        let query = handle.request(SNAPSHOT_REQUEST);
        tokio::pin!(query);
        let reply = loop {
            tokio::select! {
                result = &mut query => break result?,
                line = events.next() => dirty |= event(line, live)?,
                _ = reconcile.tick() => dirty = true,
            }
        };
        let projection = project(&reply)?;
        live.send_modify(|live| {
            live.urgent.retain(|address| {
                projection
                    .clients
                    .iter()
                    .any(|client| &client.address == address && client.focus_history_id != 0)
            });
        });
        workspaces.send_if_modified(|last| {
            if **last == projection.workspaces {
                return false;
            }
            *last = Arc::new(projection.workspaces);
            true
        });
        keyboard.send_if_modified(|last| {
            if **last == projection.keyboard {
                return false;
            }
            *last = Arc::new(projection.keyboard);
            true
        });
        *published = true;
    }
}

fn event(
    line: Option<Result<String, tokio_util::codec::LinesCodecError>>,
    live: &watch::Sender<Live>,
) -> Result<bool, SvcError> {
    let line = line
        .ok_or_else(|| SvcError::Protocol("Hyprland closed socket2".into()))?
        .map_err(|error| SvcError::Protocol(format!("Hyprland socket2: {error}")))?;
    let (kind, data) = line.split_once(">>").unwrap_or((&line, ""));
    // Prefer v2 when Hyprland emits a paired legacy event. Unknown events do
    // nothing; even malformed known payloads invalidate and resync from source.
    let relevant = matches!(
        kind,
        "workspacev2"
            | "focusedmonv2"
            | "activewindowv2"
            | "monitorremovedv2"
            | "monitoraddedv2"
            | "createworkspacev2"
            | "destroyworkspacev2"
            | "moveworkspacev2"
            | "renameworkspace"
            | "activespecialv2"
            | "openwindow"
            | "closewindow"
            | "movewindowv2"
            | "urgent"
            | "activelayout"
            | "configreloaded"
            | "fullscreen"
            | "togglegroup"
    );
    if matches!(kind, "urgent" | "activewindowv2" | "closewindow") {
        let address = if data.starts_with("0x") {
            data.to_string()
        } else {
            format!("0x{data}")
        };
        live.send_modify(|live| {
            if valid_address(&address) {
                if kind == "urgent" {
                    live.urgent.insert(address);
                } else {
                    live.urgent.remove(&address);
                }
            } else {
                // Missed/corrupt events cannot establish per-window urgency.
                live.urgent.clear();
            }
        });
    }
    Ok(relevant)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    fn fixture(name: &str) -> String {
        format!(
            r#"[{{"name":"DP-1","activeWorkspace":{{"id":1}},"focused":true}}]


[{{"id":1,"name":"{name}","monitor":"DP-1","windows":0,"ispersistent":true}}]


[]


{{"keyboards":[]}}


[{{"id":1,"urgent":false}}]"#
        )
    }

    async fn answer(listener: &UnixListener, reply: &str) {
        let (mut request, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 4096];
        let n = request.read(&mut bytes).await.unwrap();
        assert_eq!(&bytes[..n], SNAPSHOT_REQUEST.as_bytes());
        request.write_all(reply.as_bytes()).await.unwrap();
    }

    async fn wait_for(
        rx: &mut watch::Receiver<Arc<WorkspacesSnapshot>>,
        check: impl Fn(&WorkspacesSnapshot) -> bool,
    ) {
        timeout(Duration::from_secs(5), async {
            loop {
                if check(&rx.borrow_and_update()) {
                    return;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn bootstrap_and_inflight_events_get_trailing_refresh_resume_and_reconnect() {
        let directory = super::super::requests::tests::directory();
        let events = UnixListener::bind(directory.join(".socket2.sock")).unwrap();
        let commands = UnixListener::bind(directory.join(".socket.sock")).unwrap();
        let service = super::super::Hyprland::start(Some(directory.clone()));
        let mut rx = service.workspaces();
        let server = tokio::spawn(async move {
            let (mut stream, _) = events.accept().await.unwrap();
            // Query cannot begin before the event socket has been accepted.
            let (mut request, _) = commands.accept().await.unwrap();
            let mut bytes = vec![0; SNAPSHOT_REQUEST.len()];
            request.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, SNAPSHOT_REQUEST.as_bytes());
            stream.write_all(b"workspacev2>>1,after\n").await.unwrap();
            request
                .write_all(fixture("before").as_bytes())
                .await
                .unwrap();
            drop(request);
            answer(&commands, &fixture("after")).await;
            // A health-check kick must drop the live socket, not leave stale state.
            let mut eof = [0];
            assert_eq!(stream.read(&mut eof).await.unwrap(), 0);
            drop(stream);
            let (stream, _) = events.accept().await.unwrap();
            answer(&commands, &fixture("resumed")).await;
            drop(stream);
            let (_stream, _) = events.accept().await.unwrap();
            answer(&commands, &fixture("reconnected")).await;
            std::future::pending::<()>().await;
        });
        wait_for(&mut rx, |s| {
            s.connected && s.for_output("DP-1")[0].name.as_deref() == Some("after")
        })
        .await;
        service.health_check();
        wait_for(&mut rx, |s| {
            s.connected && s.for_output("DP-1")[0].name.as_deref() == Some("reconnected")
        })
        .await;
        server.abort();
        wait_for(&mut rx, |s| !s.connected).await;
        assert_eq!(
            rx.borrow().for_output("DP-1")[0].name.as_deref(),
            Some("reconnected")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn bad_query_and_oversized_event_keep_the_last_complete_projection() {
        let directory = super::super::requests::tests::directory();
        let events = UnixListener::bind(directory.join(".socket2.sock")).unwrap();
        let commands = UnixListener::bind(directory.join(".socket.sock")).unwrap();
        let service = super::super::Hyprland::start(Some(directory.clone()));
        let mut rx = service.workspaces();
        let (ready, wait) = tokio::sync::oneshot::channel();
        let (ready_again, wait_again) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = events.accept().await.unwrap();
            answer(&commands, &fixture("retained")).await;
            wait.await.unwrap();
            stream.write_all(b"configreloaded>>\n").await.unwrap();
            answer(&commands, "not a complete JSON batch").await;
            drop(stream);
            let (mut stream, _) = events.accept().await.unwrap();
            answer(&commands, &fixture("after-decode-error")).await;
            wait_again.await.unwrap();
            // A giant line must end the connection rather than allocate forever.
            let _ = stream.write_all(&vec![b'x'; LINE_LIMIT + 1]).await;
            std::future::pending::<()>().await;
        });
        wait_for(&mut rx, |s| {
            s.connected && s.for_output("DP-1")[0].name.as_deref() == Some("retained")
        })
        .await;
        ready.send(()).unwrap();
        wait_for(&mut rx, |s| !s.connected).await;
        assert_eq!(
            rx.borrow().for_output("DP-1")[0].name.as_deref(),
            Some("retained")
        );
        wait_for(&mut rx, |s| {
            s.connected && s.for_output("DP-1")[0].name.as_deref() == Some("after-decode-error")
        })
        .await;
        ready_again.send(()).unwrap();
        wait_for(&mut rx, |s| !s.connected).await;
        assert_eq!(
            rx.borrow().for_output("DP-1")[0].name.as_deref(),
            Some("after-decode-error")
        );
        server.abort();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn framing_prefers_v2_and_malformed_known_events_resync() {
        let live = watch::channel(Live::default()).0;
        for (line, expected) in [
            ("workspace>>a", false),
            ("workspacev2>>-9,a>>b", true),
            ("workspacev2", true),
            ("unknown>>whatever", false),
        ] {
            assert_eq!(event(Some(Ok(line.into())), &live).unwrap(), expected);
        }
        event(Some(Ok("urgent>>aabb".into())), &live).unwrap();
        assert!(live.borrow().urgent.contains("0xaabb"));
        event(Some(Ok("activewindowv2>>aabb".into())), &live).unwrap();
        assert!(live.borrow().urgent.is_empty());
    }
}
