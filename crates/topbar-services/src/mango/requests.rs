//! One fresh socket per Mango command; mutations are never retried.
//! Public action sequences share one gate and one deadline, including gate wait.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};
use topbar_core::config::MangoConfig;

use super::snapshot::{self, Client, Monitor, Registry, decode};
use crate::compositor::{WorkspaceId, same_app, same_launcher_application};
use crate::error::SvcError;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LINE: usize = 1024 * 1024;

/// Cheap action handle to exactly one selected Mango instance.
#[derive(Clone)]
pub struct MangoHandle {
    inner: Arc<Inner>,
}

struct Inner {
    socket: Option<PathBuf>,
    config: Arc<MangoConfig>,
    registry: Arc<Mutex<Registry>>,
    actions: tokio::sync::Mutex<()>,
}

impl MangoHandle {
    pub(crate) fn new(
        socket: Option<PathBuf>,
        config: Arc<MangoConfig>,
        registry: Arc<Mutex<Registry>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                socket,
                config,
                registry,
                actions: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Replace the exact connector's selection with one captured numeric tag.
    pub async fn focus_workspace(&self, id: WorkspaceId) -> Result<(), SvcError> {
        deadline(async {
            let _action = self.inner.actions.lock().await;
            let (connector, tag) = self.inner.registry.lock()
                .map_err(|_| SvcError::Protocol("Mango output registry is poisoned".into()))?
                .resolve(id)
                .ok_or_else(|| SvcError::Rejected("workspace identity is no longer known".into()))?;
            let selector = monitor_selector(&connector)?;
            let monitor = self.monitor(&connector).await?;
            if !monitor.tags.iter().any(|candidate| candidate.index == tag) {
                return Err(SvcError::Rejected("tag no longer exists on the captured output".into()));
            }
            self.leave_overview(&monitor, &selector).await?;
            self.dispatch(&format!("viewcrossmon,{tag},{selector}")).await?;
            // Stock viewcrossmon can fall back to the focused output if hot-unplug
            // races dispatch. Report it, never retry or compensate on another output.
            let result = self.monitor(&connector).await?;
            if !result.active || result.active_tags != [tag] {
                return Err(SvcError::Rejected(format!(
                    "Mango did not activate tag {tag} on {connector}; the output may have changed during dispatch (stock IPC cannot make hot-unplug atomic)"
                )));
            }
            Ok(())
        }).await
    }

    /// Focus an exact native client, leaving overview on its own output first.
    pub async fn focus_window(&self, id: u64) -> Result<(), SvcError> {
        deadline(async {
            let _action = self.inner.actions.lock().await;
            self.focus_window_inner(id).await
        })
        .await
    }

    async fn focus_window_inner(&self, id: u64) -> Result<(), SvcError> {
        let client = self.client(id).await?;
        let selector = monitor_selector(&client.monitor)?;
        let monitor = self.monitor(&client.monitor).await?;
        self.leave_overview(&monitor, &selector).await?;
        self.dispatch(&format!("focusid,client,{id}")).await?;
        let current = self.client(id).await?;
        let output = self.monitor(&client.monitor).await?;
        if !current.is_focused
            || current.monitor != client.monitor
            || !output.active
            || output.active_tags == [0]
            || output.active_client.id.map(u64::from) != Some(id)
        {
            return Err(SvcError::Rejected(format!(
                "Mango did not focus client {id} on {}; the client or output changed during activation",
                client.monitor
            )));
        }
        Ok(())
    }

    /// Notification identities retain priority, then urgency, focus and lowest ID.
    pub async fn focus_app(&self, identities: &[&str]) -> Result<bool, SvcError> {
        deadline(async {
            let _action = self.inner.actions.lock().await;
            let clients = self.clients().await?;
            let selected = identities.iter().find_map(|identity| {
                clients
                    .iter()
                    .filter(|client| same_app(&client.appid, identity))
                    .min_by_key(|client| (!client.is_urgent, !client.is_focused, client.id))
            });
            let Some(client) = selected else {
                return Ok(false);
            };
            self.focus_window_inner(u64::from(client.id)).await?;
            Ok(true)
        })
        .await
    }

    /// Launcher applications compete by focus then lowest native ID, not fake MRU.
    pub async fn focus_application(&self, identities: &[&str]) -> Result<bool, SvcError> {
        deadline(async {
            let _action = self.inner.actions.lock().await;
            let clients = self.clients().await?;
            let selected = clients
                .iter()
                .filter(|client| {
                    identities
                        .iter()
                        .any(|identity| same_launcher_application(&client.appid, identity))
                })
                .min_by_key(|client| (!client.is_focused, client.id));
            let Some(client) = selected else {
                return Ok(false);
            };
            self.focus_window_inner(u64::from(client.id)).await?;
            Ok(true)
        })
        .await
    }

    /// Select the next keyboard language using Mango's native relative action.
    pub async fn switch_layout_next(&self) -> Result<(), SvcError> {
        deadline(async {
            let _action = self.inner.actions.lock().await;
            self.require_layouts()?;
            self.dispatch("switch_keyboard_layout,0").await
        })
        .await
    }

    /// Query the live full language description before selecting absolute previous.
    pub async fn switch_layout_prev(&self) -> Result<(), SvcError> {
        deadline(async {
            let _action = self.inner.actions.lock().await;
            self.require_layouts()?;
            #[derive(Deserialize)]
            struct Layout { layout: String }
            let response: Layout = decode(&self.request("get keyboardlayout").await?)?;
            let layouts = &self.inner.config.keyboard_layouts;
            let index = layouts.iter().position(|name| name == &response.layout)
                .ok_or_else(|| SvcError::KeyboardLayout(format!(
                    "live Mango layout {:?} does not match advanced.mango.keyboard_layouts; configure full descriptions in XKB order and restart topbar",
                    response.layout
                )))?;
            let previous = ((index + layouts.len() - 1) % layouts.len()) + 1;
            self.dispatch(&format!("switch_keyboard_layout,{previous}")).await
        }).await
    }

    fn require_layouts(&self) -> Result<(), SvcError> {
        if !(2..=100).contains(&self.inner.config.keyboard_layouts.len()) {
            return Err(SvcError::KeyboardLayout(
                "configure 2–100 full descriptions in advanced.mango.keyboard_layouts and restart topbar".into()
            ));
        }
        Ok(())
    }

    /// Quit only the Mango socket selected at panel startup.
    pub async fn quit_compositor(&self) -> Result<(), SvcError> {
        deadline(async {
            let _action = self.inner.actions.lock().await;
            self.dispatch("quit").await
        })
        .await
    }

    async fn leave_overview(&self, monitor: &Monitor, selector: &str) -> Result<(), SvcError> {
        if monitor.active_tags != [0] {
            return Ok(());
        }
        self.dispatch(&format!("focusmon,{selector}")).await?;
        let focused = self.monitor(&monitor.name).await?;
        if !focused.active {
            return Err(SvcError::Rejected(format!(
                "cannot leave overview: {} is no longer the focused output",
                monitor.name
            )));
        }
        self.dispatch("leaveoverview").await?;
        let normal = self.monitor(&monitor.name).await?;
        if !normal.active || normal.active_tags == [0] {
            return Err(SvcError::Rejected(format!(
                "Mango did not leave overview on {}",
                monitor.name
            )));
        }
        Ok(())
    }

    async fn monitor(&self, connector: &str) -> Result<Monitor, SvcError> {
        monitor_selector(connector)?;
        let monitor =
            snapshot::parse_monitor(&self.request(&format!("get monitor {connector}")).await?)?;
        if monitor.name != connector {
            return Err(SvcError::Protocol(format!(
                "Mango returned the wrong monitor for {connector}"
            )));
        }
        // Stock focusmon silently skips disabled outputs. viewcrossmon would
        // then change the still-focused output's tag, so reject before dispatch.
        if monitor.width <= 0 || monitor.height <= 0 {
            return Err(SvcError::Rejected(format!(
                "cannot activate disabled Mango output {connector}"
            )));
        }
        Ok(monitor)
    }

    async fn client(&self, id: u64) -> Result<Client, SvcError> {
        if id == 0 || id > i32::MAX as u64 {
            return Err(SvcError::Rejected(
                "native Mango client ID is outside the signed dispatch range 1..=2147483647".into(),
            ));
        }
        let client = snapshot::parse_client(&self.request(&format!("get client {id}")).await?)?;
        if u64::from(client.id) != id {
            return Err(SvcError::Protocol("Mango returned the wrong client".into()));
        }
        Ok(client)
    }

    async fn clients(&self) -> Result<Vec<Client>, SvcError> {
        snapshot::parse_clients(&self.request("get all-clients").await?)
    }

    async fn dispatch(&self, command: &str) -> Result<(), SvcError> {
        #[derive(Deserialize)]
        struct Accepted {
            success: bool,
        }
        let accepted: Accepted = decode(&self.request(&format!("dispatch {command}")).await?)?;
        if !accepted.success {
            return Err(SvcError::Rejected("Mango reported success=false".into()));
        }
        Ok(())
    }

    async fn request(&self, payload: &str) -> Result<String, SvcError> {
        let socket = self.inner.socket.as_deref().ok_or_else(|| {
            SvcError::CompositorSelection(
                "MANGO_INSTANCE_SIGNATURE is not set; cannot reach the selected Mango instance"
                    .into(),
            )
        })?;
        exchange(socket, payload).await
    }
}

/// Monitor specs tokenize ':' and '&&'; commas tokenize dispatch and whitespace
/// can alter native argument parsing. PCRE metacharacters themselves are escaped.
fn monitor_selector(connector: &str) -> Result<String, SvcError> {
    if connector.is_empty()
        || connector
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, ',' | ':' | '&'))
    {
        return Err(SvcError::Protocol(
            "unsafe or empty Mango output connector".into(),
        ));
    }
    let mut selector = String::with_capacity(connector.len() * 2 + 7);
    selector.push_str("name:^");
    for ch in connector.chars() {
        if matches!(
            ch,
            '\\' | '.' | '^' | '$' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
        ) {
            selector.push('\\');
        }
        selector.push(ch);
    }
    selector.push('$');
    Ok(selector)
}

async fn deadline<T>(operation: impl Future<Output = Result<T, SvcError>>) -> Result<T, SvcError> {
    tokio::time::timeout(REQUEST_TIMEOUT, operation)
        .await
        .map_err(|_| SvcError::Timeout(REQUEST_TIMEOUT))?
}

async fn exchange(socket: &Path, payload: &str) -> Result<String, SvcError> {
    // Keep the payload plus newline below Mango's native 1024-byte input limit.
    if payload.len() >= 1023 || payload.chars().any(|ch| matches!(ch, '\r' | '\n' | '\0')) {
        return Err(SvcError::Protocol(
            "Mango command exceeds the native input limit or contains a line delimiter".into(),
        ));
    }
    deadline(async {
        let stream = UnixStream::connect(socket).await.map_err(SvcError::Io)?;
        let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE));
        framed.send(payload.to_owned()).await
            .map_err(|error| SvcError::Protocol(format!("could not send Mango command: {error}")))?;
        let line = framed.next().await
            .ok_or_else(|| SvcError::Protocol("Mango closed its one-shot socket without a reply; mutation status is unknown (not retried)".into()))?
            .map_err(|error| SvcError::Protocol(format!("could not read Mango reply: {error}")))?;
        Ok(line)
    }).await
}

pub(crate) async fn focused_output_connector(
    socket: Option<PathBuf>,
) -> Result<Option<String>, SvcError> {
    deadline(async {
        let socket = socket.ok_or_else(|| {
            SvcError::CompositorSelection("MANGO_INSTANCE_SIGNATURE is not set".into())
        })?;
        let monitors = snapshot::parse_monitors(&exchange(&socket, "get all-monitors").await?)?;
        Ok(monitors
            .into_iter()
            .find(|monitor| monitor.active)
            .map(|monitor| monitor.name))
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::*;

    struct Step {
        command: String,
        reply: Option<String>,
        delay: Duration,
    }

    fn reply(command: impl Into<String>, response: impl ToString) -> Step {
        Step {
            command: command.into(),
            reply: Some(response.to_string()),
            delay: Duration::ZERO,
        }
    }

    /// Every step accepts a new socket and compares the complete, untrimmed line.
    struct Server {
        path: PathBuf,
        finished: Option<oneshot::Sender<()>>,
        task: JoinHandle<Vec<String>>,
    }

    impl Server {
        fn bind() -> (PathBuf, UnixListener) {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "topbar-mango-request-{}-{}.sock",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind disposable Mango socket");
            (path, listener)
        }

        fn new(steps: Vec<Step>) -> Self {
            let (path, listener) = Self::bind();
            let (finished, done) = oneshot::channel();
            let task = tokio::spawn(async move {
                let mut commands = Vec::new();
                for step in steps {
                    let (stream, _) =
                        tokio::time::timeout(Duration::from_secs(3), listener.accept())
                            .await
                            .expect("action connects")
                            .expect("accept command socket");
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    tokio::time::timeout(Duration::from_secs(3), stream.read_line(&mut line))
                        .await
                        .expect("action sends a line")
                        .expect("read command");
                    assert_eq!(line, format!("{}\n", step.command));
                    commands.push(line);
                    tokio::time::sleep(step.delay).await;
                    if let Some(response) = step.reply {
                        // A timed-out action may already have closed its socket.
                        let _ = stream
                            .get_mut()
                            .write_all(format!("{response}\n").as_bytes())
                            .await;
                    }
                }
                done.await.expect("test finished its actions");
                assert!(
                    tokio::time::timeout(Duration::from_millis(40), listener.accept())
                        .await
                        .is_err(),
                    "unexpected connection: mutation retry, compensation, or invalid input dispatch"
                );
                commands
            });
            Self {
                path,
                finished: Some(finished),
                task,
            }
        }

        fn handle(&self, registry: Arc<Mutex<Registry>>, layouts: &[&str]) -> MangoHandle {
            MangoHandle::new(
                Some(self.path.clone()),
                Arc::new(MangoConfig {
                    keyboard_layouts: layouts.iter().map(|name| (*name).to_owned()).collect(),
                    ..MangoConfig::default()
                }),
                registry,
            )
        }

        async fn finish(mut self) -> Vec<String> {
            self.finished
                .take()
                .expect("finish once")
                .send(())
                .expect("server waiting");
            tokio::time::timeout(Duration::from_secs(4), &mut self.task)
                .await
                .expect("fixture finishes")
                .expect("fixture assertions")
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn monitor(
        name: &str,
        active: bool,
        tags: &[u32],
        active_tags: &[u32],
        active_client: Option<u32>,
    ) -> Value {
        json!({
            "name": name,
            "active": active,
            "width": 1920,
            "height": 1080,
            "tags": tags.iter().map(|index| json!({
                "index": index,
                "is_active": active_tags.contains(index),
                "is_urgent": false,
                "client_count": 0,
            })).collect::<Vec<_>>(),
            "active_tags": active_tags,
            "active_client": match active_client {
                Some(id) => json!({"id": id, "title": "fixture", "appid": "app"}),
                None => json!({"id": null, "title": null, "appid": null}),
            },
            "keyboardlayout": "English (US)",
        })
    }

    fn client(id: u32, appid: &str, focused: bool, urgent: bool) -> Value {
        json!({
            "id": id,
            "title": "fixture",
            "appid": appid,
            "monitor": "DP-1",
            "tags": [1],
            "is_focused": focused,
            "is_urgent": urgent,
        })
    }

    fn workspaces(
        registry: &Arc<Mutex<Registry>>,
        monitors: &[Value],
    ) -> crate::compositor::WorkspacesSnapshot {
        let monitors = snapshot::parse_monitors(&json!({"monitors": monitors}).to_string())
            .expect("native monitor fixtures");
        registry
            .lock()
            .unwrap()
            .workspaces(&monitors, &MangoConfig::default(), true)
            .expect("workspace projection")
    }

    fn capture(registry: &Arc<Mutex<Registry>>, monitors: &[Value], name: &str, tag: u32) -> u64 {
        workspaces(registry, monitors).outputs[name]
            .iter()
            .find(|workspace| workspace.idx == tag as usize)
            .expect("captured workspace")
            .id
    }

    fn activation_steps(mut selected: Value, overview: bool) -> Vec<Step> {
        let id = selected["id"].as_u64().unwrap() as u32;
        let mut steps = vec![
            reply(format!("get client {id}"), selected.clone()),
            reply(
                "get monitor DP-1",
                monitor(
                    "DP-1",
                    !overview,
                    &[1],
                    if overview { &[0] } else { &[1] },
                    None,
                ),
            ),
        ];
        if overview {
            steps.extend([
                reply("dispatch focusmon,name:^DP-1$", json!({"success": true})),
                reply("get monitor DP-1", monitor("DP-1", true, &[1], &[0], None)),
                reply("dispatch leaveoverview", json!({"success": true})),
                reply("get monitor DP-1", monitor("DP-1", true, &[1], &[1], None)),
            ]);
        }
        selected["is_focused"] = json!(true);
        steps.extend([
            reply(
                format!("dispatch focusid,client,{id}"),
                json!({"success": true}),
            ),
            reply(format!("get client {id}"), selected),
            reply(
                "get monitor DP-1",
                monitor("DP-1", true, &[1], &[1], Some(id)),
            ),
        ]);
        steps
    }

    #[tokio::test]
    async fn each_action_uses_a_fresh_one_shot_socket_and_exact_native_line() {
        let server = Server::new(vec![
            reply("dispatch quit", json!({"success": true, "future_field": 1})),
            reply("dispatch quit", json!({"success": true})),
            reply(
                "dispatch switch_keyboard_layout,0",
                json!({"success": true}),
            ),
        ]);
        let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &["US", "RU"]);
        handle.quit_compositor().await.unwrap();
        handle.quit_compositor().await.unwrap();
        handle.switch_layout_next().await.unwrap();
        assert_eq!(
            server.finish().await,
            [
                "dispatch quit\n",
                "dispatch quit\n",
                "dispatch switch_keyboard_layout,0\n",
            ]
        );
    }

    #[tokio::test]
    async fn dispatch_rejects_error_envelopes_malformed_oversized_and_false_success() {
        let cases = [
            (
                json!({"success": true, "error": "native refusal"}).to_string(),
                true,
            ),
            (
                json!({"error": {"reason": "native refusal"}}).to_string(),
                false,
            ),
            ("not JSON".into(), false),
            (json!({"success": "true"}).to_string(), false),
            (json!({"ok": true}).to_string(), false),
            (json!({"success": false}).to_string(), true),
            (
                json!({"success": true, "padding": "x".repeat(MAX_LINE)}).to_string(),
                false,
            ),
        ];
        for (response, rejected) in cases {
            let server = Server::new(vec![reply("dispatch quit", response)]);
            let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &[]);
            let error = handle.quit_compositor().await.unwrap_err();
            if rejected {
                assert!(matches!(error, SvcError::Rejected(_)), "{error}");
            } else {
                assert!(matches!(error, SvcError::Protocol(_)), "{error}");
            }
            assert_eq!(server.finish().await, ["dispatch quit\n"]);
        }
    }

    #[tokio::test]
    async fn monitor_selector_escapes_every_pcre_metacharacter_on_the_wire() {
        let connector = r"DP.\^$|?*+()[]{}";
        let selector = r"name:^DP\.\\\^\$\|\?\*\+\(\)\[\]\{\}$";
        let registry = Arc::new(Mutex::new(Registry::default()));
        let initial = monitor(connector, false, &[1, 2], &[1], None);
        let id = capture(&registry, std::slice::from_ref(&initial), connector, 2);
        let server = Server::new(vec![
            reply(format!("get monitor {connector}"), initial),
            reply(
                format!("dispatch viewcrossmon,2,{selector}"),
                json!({"success": true}),
            ),
            reply(
                format!("get monitor {connector}"),
                monitor(connector, true, &[1, 2], &[2], None),
            ),
        ]);
        server
            .handle(registry, &[])
            .focus_workspace(id)
            .await
            .unwrap();
        server.finish().await;
    }

    #[tokio::test]
    async fn selector_delimiters_controls_and_whitespace_never_connect() {
        assert!(matches!(monitor_selector(""), Err(SvcError::Protocol(_))));
        let server = Server::new(vec![]);
        for connector in [
            "DP,1",
            "DP:1",
            "DP&1",
            "DP&&1",
            "DP 1",
            "DP\t1",
            "DP\n1",
            "DP\r1",
            "DP\0",
            "DP\u{1b}1",
            "DP\u{7f}1",
            "DP\u{a0}1",
        ] {
            let registry = Arc::new(Mutex::new(Registry::default()));
            let id = capture(
                &registry,
                &[monitor(connector, true, &[1], &[1], None)],
                connector,
                1,
            );
            let error = server
                .handle(registry, &[])
                .focus_workspace(id)
                .await
                .unwrap_err();
            assert!(
                matches!(error, SvcError::Protocol(_)),
                "{connector:?}: {error}"
            );
        }
        assert!(server.finish().await.is_empty());
    }

    #[tokio::test]
    async fn commands_at_or_above_1024_bytes_including_newline_never_connect() {
        let server = Server::new(vec![]);
        for connector in [
            "x".repeat(1011),
            "x".repeat(1012),
            "x".repeat(1013),
            "é".repeat(506),
        ] {
            assert!(format!("get monitor {connector}\n").len() >= 1024);
            let registry = Arc::new(Mutex::new(Registry::default()));
            let id = capture(
                &registry,
                &[monitor(&connector, true, &[1], &[1], None)],
                &connector,
                1,
            );
            let error = server
                .handle(registry, &[])
                .focus_workspace(id)
                .await
                .unwrap_err();
            assert!(matches!(error, SvcError::Protocol(_)), "{error}");
        }
        assert!(server.finish().await.is_empty());
    }

    #[tokio::test]
    async fn a_1023_byte_command_including_newline_reaches_the_native_socket() {
        let connector = "x".repeat(1010);
        assert_eq!(format!("get monitor {connector}\n").len(), 1023);
        let registry = Arc::new(Mutex::new(Registry::default()));
        let id = capture(
            &registry,
            &[monitor(&connector, true, &[1], &[1], None)],
            &connector,
            1,
        );
        let server = Server::new(vec![reply(
            format!("get monitor {connector}"),
            json!({"error": "native monitor lookup refused"}),
        )]);
        let error = server
            .handle(registry, &[])
            .focus_workspace(id)
            .await
            .unwrap_err();
        assert!(matches!(error, SvcError::Rejected(_)), "{error}");
        assert_eq!(server.finish().await.len(), 1);
    }

    #[tokio::test]
    async fn captured_workspace_identity_survives_monitor_reordering() {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let target = monitor("DP-1", false, &[1, 2], &[1], None);
        let other = monitor("DP-10", true, &[1, 2], &[1], None);
        let id = capture(&registry, &[target.clone(), other.clone()], "DP-1", 2);
        workspaces(&registry, &[other, target.clone()]);
        assert_eq!(
            registry.lock().unwrap().resolve(id),
            Some(("DP-1".into(), 2))
        );
        let server = Server::new(vec![
            reply("get monitor DP-1", target),
            reply(
                "dispatch viewcrossmon,2,name:^DP-1$",
                json!({"success": true}),
            ),
            reply(
                "get monitor DP-1",
                monitor("DP-1", true, &[1, 2], &[2], None),
            ),
        ]);
        server
            .handle(registry, &[])
            .focus_workspace(id)
            .await
            .unwrap();
        server.finish().await;
    }

    #[tokio::test]
    async fn removed_monitor_or_tag_never_dispatches_the_captured_workspace() {
        for removed_monitor in [true, false] {
            let registry = Arc::new(Mutex::new(Registry::default()));
            let id = capture(
                &registry,
                &[monitor("DP-1", true, &[1, 2], &[2], None)],
                "DP-1",
                2,
            );
            let current = monitor("DP-1", true, &[1], &[1], None);
            let response = if removed_monitor {
                workspaces(&registry, &[]);
                json!({"error": "monitor no longer exists"})
            } else {
                workspaces(&registry, std::slice::from_ref(&current));
                current
            };
            let server = Server::new(vec![reply("get monitor DP-1", response)]);
            let error = server
                .handle(registry, &[])
                .focus_workspace(id)
                .await
                .unwrap_err();
            assert!(matches!(error, SvcError::Rejected(_)), "{error}");
            assert_eq!(server.finish().await, ["get monitor DP-1\n"]);
        }
    }

    #[tokio::test]
    async fn disabled_output_rejects_workspace_and_overview_activation_without_dispatch() {
        for (width, height) in [(0, 1080), (1920, 0), (-1, 1080), (1920, -1)] {
            for overview in [false, true] {
                let registry = Arc::new(Mutex::new(Registry::default()));
                let captured = monitor("DP-1", true, &[1, 2], &[2], None);
                let id = capture(&registry, &[captured], "DP-1", 2);
                let mut disabled = monitor(
                    "DP-1",
                    false,
                    &[1, 2],
                    if overview { &[0] } else { &[2] },
                    None,
                );
                disabled["width"] = json!(width);
                disabled["height"] = json!(height);
                let server = Server::new(vec![reply("get monitor DP-1", disabled)]);
                let error = server
                    .handle(registry, &[])
                    .focus_workspace(id)
                    .await
                    .unwrap_err();
                assert!(matches!(error, SvcError::Rejected(_)), "{error}");
                assert!(error.to_string().contains("disabled Mango output DP-1"));
                assert_eq!(server.finish().await, ["get monitor DP-1\n"]);
            }
        }
    }

    #[tokio::test]
    async fn disabled_output_rejects_all_window_routes_before_focus_or_leaving_overview() {
        for (width, height) in [(0, 1080), (1920, 0), (-1, 1080), (1920, -1)] {
            for overview in [false, true] {
                for route in 0..3 {
                    let selected = client(42, "app", false, false);
                    let mut disabled = monitor(
                        "DP-1",
                        false,
                        &[1, 2],
                        if overview { &[0] } else { &[2] },
                        None,
                    );
                    disabled["width"] = json!(width);
                    disabled["height"] = json!(height);
                    let mut steps = Vec::new();
                    if route != 0 {
                        steps.push(reply(
                            "get all-clients",
                            json!({"clients": [selected.clone()]}),
                        ));
                    }
                    steps.extend([
                        reply("get client 42", selected),
                        reply("get monitor DP-1", disabled),
                    ]);
                    let server = Server::new(steps);
                    let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &[]);
                    let error = match route {
                        0 => handle.focus_window(42).await.map(|()| true),
                        1 => handle.focus_app(&["app"]).await,
                        _ => handle.focus_application(&["app.desktop"]).await,
                    }
                    .unwrap_err();
                    assert!(matches!(error, SvcError::Rejected(_)), "{error}");
                    assert!(error.to_string().contains("disabled Mango output DP-1"));
                    let commands = server.finish().await;
                    let expected = if route == 0 {
                        vec!["get client 42\n", "get monitor DP-1\n"]
                    } else {
                        vec!["get all-clients\n", "get client 42\n", "get monitor DP-1\n"]
                    };
                    assert_eq!(commands, expected);
                }
            }
        }
    }

    #[tokio::test]
    async fn overview_current_pill_focuses_exact_monitor_exits_then_restores_captured_tag() {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let remembered = capture(
            &registry,
            &[monitor("DP-1", true, &[1, 2], &[2], None)],
            "DP-1",
            2,
        );
        let overview = monitor("DP-1", false, &[1, 2], &[0], None);
        let snapshot = workspaces(&registry, std::slice::from_ref(&overview));
        let current_pill = snapshot.outputs["DP-1"]
            .iter()
            .find(|view| view.is_active)
            .unwrap()
            .id;
        assert_eq!(current_pill, remembered);
        let server = Server::new(vec![
            reply("get monitor DP-1", overview),
            reply("dispatch focusmon,name:^DP-1$", json!({"success": true})),
            reply(
                "get monitor DP-1",
                monitor("DP-1", true, &[1, 2], &[0], None),
            ),
            reply("dispatch leaveoverview", json!({"success": true})),
            reply(
                "get monitor DP-1",
                monitor("DP-1", true, &[1, 2], &[1], None),
            ),
            reply(
                "dispatch viewcrossmon,2,name:^DP-1$",
                json!({"success": true}),
            ),
            reply(
                "get monitor DP-1",
                monitor("DP-1", true, &[1, 2], &[2], None),
            ),
        ]);
        server
            .handle(registry, &[])
            .focus_workspace(current_pill)
            .await
            .unwrap();
        server.finish().await;
    }

    #[tokio::test]
    async fn overview_transition_must_be_observed_before_the_next_mutation() {
        for focus_failed in [true, false] {
            let registry = Arc::new(Mutex::new(Registry::default()));
            let initial = monitor("DP-1", false, &[1], &[0], None);
            let id = capture(&registry, std::slice::from_ref(&initial), "DP-1", 1);
            let mut steps = vec![
                reply("get monitor DP-1", initial),
                reply("dispatch focusmon,name:^DP-1$", json!({"success": true})),
                reply(
                    "get monitor DP-1",
                    monitor("DP-1", !focus_failed, &[1], &[0], None),
                ),
            ];
            if !focus_failed {
                steps.extend([
                    reply("dispatch leaveoverview", json!({"success": true})),
                    reply("get monitor DP-1", monitor("DP-1", true, &[1], &[0], None)),
                ]);
            }
            let server = Server::new(steps);
            let error = server
                .handle(registry, &[])
                .focus_workspace(id)
                .await
                .unwrap_err();
            assert!(matches!(error, SvcError::Rejected(_)), "{error}");
            let commands = server.finish().await;
            assert!(
                !commands
                    .iter()
                    .any(|line| line.starts_with("dispatch viewcrossmon,"))
            );
        }
    }

    #[tokio::test]
    async fn workspace_disappearance_or_false_postcondition_has_one_mutation_no_compensation() {
        for postcondition in [
            json!({"error": "monitor disappeared during dispatch"}),
            monitor("DP-1", false, &[1, 2], &[2], None),
            monitor("DP-1", true, &[1, 2], &[1], None),
            monitor("DP-1", true, &[1, 2], &[1, 2], None),
        ] {
            let registry = Arc::new(Mutex::new(Registry::default()));
            let initial = monitor("DP-1", false, &[1, 2], &[1], None);
            let id = capture(&registry, std::slice::from_ref(&initial), "DP-1", 2);
            let server = Server::new(vec![
                reply("get monitor DP-1", initial),
                reply(
                    "dispatch viewcrossmon,2,name:^DP-1$",
                    json!({"success": true}),
                ),
                reply("get monitor DP-1", postcondition),
            ]);
            let error = server
                .handle(registry, &[])
                .focus_workspace(id)
                .await
                .unwrap_err();
            assert!(matches!(error, SvcError::Rejected(_)), "{error}");
            let commands = server.finish().await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|line| line.starts_with("dispatch "))
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn window_notification_and_launcher_routes_exit_overview_before_focus_and_postcheck() {
        for route in 0..3 {
            let selected = client(42, "app", false, false);
            let mut steps = Vec::new();
            if route != 0 {
                steps.push(reply(
                    "get all-clients",
                    json!({"clients": [selected.clone()]}),
                ));
            }
            steps.extend(activation_steps(selected, true));
            let server = Server::new(steps);
            let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &[]);
            let activated = match route {
                0 => handle.focus_window(42).await.map(|()| true),
                1 => handle.focus_app(&["app"]).await,
                _ => handle.focus_application(&["app.desktop"]).await,
            };
            assert!(activated.unwrap());
            server.finish().await;
        }
    }

    #[tokio::test]
    async fn window_focus_success_requires_native_client_and_monitor_postconditions() {
        for failure in 0..5 {
            let selected = client(42, "app", false, false);
            let mut steps = activation_steps(selected, false);
            let mut focused = client(42, "app", failure != 0, false);
            if failure == 1 {
                focused["monitor"] = json!("DP-2");
            }
            let output = monitor(
                "DP-1",
                failure != 2,
                &[1],
                if failure == 3 { &[0] } else { &[1] },
                Some(if failure == 4 { 43 } else { 42 }),
            );
            steps[3] = reply("get client 42", focused);
            steps[4] = reply("get monitor DP-1", output);
            let server = Server::new(steps);
            let error = server
                .handle(Arc::new(Mutex::new(Registry::default())), &[])
                .focus_window(42)
                .await
                .unwrap_err();
            assert!(matches!(error, SvcError::Rejected(_)), "{error}");
            let commands = server.finish().await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|line| line.starts_with("dispatch "))
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn notification_selection_is_identity_priority_then_urgent_focused_lowest_id() {
        let cases = [
            (
                vec!["@ORG.CHAT", "Chat"],
                vec![
                    client(9, "org.chat", false, false),
                    client(1, "Chat", true, true),
                ],
                9,
            ),
            (
                vec!["app"],
                vec![client(1, "app", true, false), client(9, "app", false, true)],
                9,
            ),
            (
                vec!["app"],
                vec![client(1, "app", false, true), client(9, "app", true, true)],
                9,
            ),
            (
                vec!["app"],
                vec![client(9, "app", true, true), client(2, "app", true, true)],
                2,
            ),
        ];
        for (identities, clients, expected) in cases {
            let selected = clients
                .iter()
                .find(|item| item["id"] == json!(expected))
                .unwrap()
                .clone();
            let mut steps = vec![reply("get all-clients", json!({"clients": clients}))];
            steps.extend(activation_steps(selected, false));
            let server = Server::new(steps);
            assert!(
                server
                    .handle(Arc::new(Mutex::new(Registry::default())), &[])
                    .focus_app(&identities)
                    .await
                    .unwrap()
            );
            server.finish().await;
        }
    }

    #[tokio::test]
    async fn launcher_selection_is_global_focus_then_lowest_id_with_desktop_comparison() {
        let cases = [
            (
                vec!["org.example.Editor.desktop", "EditorWindow"],
                vec![
                    client(9, "org.example.Editor", false, true),
                    client(5, "EditorWindow", true, false),
                ],
                5,
            ),
            (
                vec!["org.example.Editor.desktop", "EditorWindow"],
                vec![
                    client(9, "org.example.Editor", true, true),
                    client(2, "EditorWindow", true, false),
                ],
                2,
            ),
            (
                vec!["org.example.Editor.desktop", "EditorWindow"],
                vec![
                    client(8, "org.example.Editor", false, true),
                    client(3, "EditorWindow", false, false),
                ],
                3,
            ),
            (
                vec!["@ORG.EXAMPLE.EDITOR.DESKTOP"],
                vec![
                    client(1, "org.example.Edit", true, true),
                    client(2, "org.example.Editor", false, false),
                ],
                2,
            ),
        ];
        for (identities, clients, expected) in cases {
            let selected = clients
                .iter()
                .find(|item| item["id"] == json!(expected))
                .unwrap()
                .clone();
            let mut steps = vec![reply("get all-clients", json!({"clients": clients}))];
            steps.extend(activation_steps(selected, false));
            let server = Server::new(steps);
            assert!(
                server
                    .handle(Arc::new(Mutex::new(Registry::default())), &[])
                    .focus_application(&identities)
                    .await
                    .unwrap()
            );
            server.finish().await;
        }
    }

    #[tokio::test]
    async fn application_routes_with_no_exact_identity_match_do_not_mutate() {
        for launcher in [false, true] {
            let server = Server::new(vec![reply(
                "get all-clients",
                json!({"clients": [client(1, "org.example.Editor", true, true)]}),
            )]);
            let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &[]);
            let found = if launcher {
                handle.focus_application(&["org.example.Edit"]).await
            } else {
                handle.focus_app(&["org.example.Edit"]).await
            };
            assert!(!found.unwrap());
            assert_eq!(server.finish().await, ["get all-clients\n"]);
        }
    }

    #[tokio::test]
    async fn previous_layout_queries_live_description_before_absolute_switch() {
        for (layout, previous) in [("English (US)", 2), ("Russian", 1)] {
            let server = Server::new(vec![
                reply("get keyboardlayout", json!({"layout": layout})),
                reply(
                    format!("dispatch switch_keyboard_layout,{previous}"),
                    json!({"success": true}),
                ),
            ]);
            server
                .handle(
                    Arc::new(Mutex::new(Registry::default())),
                    &["English (US)", "Russian"],
                )
                .switch_layout_prev()
                .await
                .unwrap();
            server.finish().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_previous_layout_actions_take_two_live_steps_not_the_same_step_twice() {
        let (path, listener) = Server::bind();
        let (finished, done) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut language = "English (US)";
            let mut commands = Vec::new();
            // A native one-shot server handles queued connections in order.
            // Without action serialization both queries observe the initial US layout.
            for _ in 0..4 {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                    .await
                    .expect("action connects")
                    .expect("accept command socket");
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(3), stream.read_line(&mut line))
                    .await
                    .expect("action sends a line")
                    .expect("read command");
                let response = match line.as_str() {
                    "get keyboardlayout\n" => json!({"layout": language}),
                    "dispatch switch_keyboard_layout,2\n" => {
                        language = "Russian";
                        json!({"success": true})
                    }
                    "dispatch switch_keyboard_layout,1\n" => {
                        language = "English (US)";
                        json!({"success": true})
                    }
                    _ => panic!("unexpected native keyboard command: {line:?}"),
                };
                commands.push(line);
                stream
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("native keyboard reply");
            }
            assert_eq!(
                language, "English (US)",
                "two previous steps restore the original layout"
            );
            done.await.expect("both actions complete");
            assert!(
                tokio::time::timeout(Duration::from_millis(40), listener.accept())
                    .await
                    .is_err(),
                "no mutation retries"
            );
            commands
        });
        let server = Server {
            path,
            finished: Some(finished),
            task,
        };
        let handle = server.handle(
            Arc::new(Mutex::new(Registry::default())),
            &["English (US)", "Russian"],
        );
        let other = handle.clone();
        let (first, second) = tokio::join!(handle.switch_layout_prev(), other.switch_layout_prev());
        first.unwrap();
        second.unwrap();
        assert_eq!(
            server.finish().await,
            [
                "get keyboardlayout\n",
                "dispatch switch_keyboard_layout,2\n",
                "get keyboardlayout\n",
                "dispatch switch_keyboard_layout,1\n",
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_for_another_action_uses_the_same_two_second_deadline() {
        let server = Server::new(vec![
            reply("get keyboardlayout", json!({"layout": "English (US)"})),
            Step {
                delay: Duration::from_millis(1100),
                ..reply(
                    "dispatch switch_keyboard_layout,2",
                    json!({"success": true}),
                )
            },
            Step {
                delay: Duration::from_millis(1100),
                ..reply("get keyboardlayout", json!({"layout": "Russian"}))
            },
        ]);
        let handle = server.handle(
            Arc::new(Mutex::new(Registry::default())),
            &["English (US)", "Russian"],
        );
        let other = handle.clone();
        let (first, second) = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(handle.switch_layout_prev(), other.switch_layout_prev())
        })
        .await
        .expect("actions finish within the outer safety guard");
        first.expect("first action completes within its deadline");
        assert!(matches!(second, Err(SvcError::Timeout(duration)) if duration == REQUEST_TIMEOUT));
        assert_eq!(
            server.finish().await,
            [
                "get keyboardlayout\n",
                "dispatch switch_keyboard_layout,2\n",
                "get keyboardlayout\n",
            ]
        );
    }

    #[tokio::test]
    async fn unknown_live_layout_and_query_error_do_not_switch() {
        for native_error in [false, true] {
            let response = if native_error {
                json!({"layout": "English (US)", "error": "keyboard unavailable"})
            } else {
                json!({"layout": "French"})
            };
            let server = Server::new(vec![reply("get keyboardlayout", response)]);
            let error = server
                .handle(
                    Arc::new(Mutex::new(Registry::default())),
                    &["English (US)", "Russian"],
                )
                .switch_layout_prev()
                .await
                .unwrap_err();
            if native_error {
                assert!(matches!(error, SvcError::Rejected(_)), "{error}");
            } else {
                assert!(matches!(error, SvcError::KeyboardLayout(_)), "{error}");
            }
            assert_eq!(server.finish().await, ["get keyboardlayout\n"]);
        }
    }

    #[tokio::test]
    async fn empty_keyboard_metadata_rejects_both_directions_even_without_a_socket() {
        let handle = MangoHandle::new(
            None,
            Arc::new(MangoConfig::default()),
            Arc::new(Mutex::new(Registry::default())),
        );
        assert!(matches!(
            handle.switch_layout_next().await,
            Err(SvcError::KeyboardLayout(_))
        ));
        assert!(matches!(
            handle.switch_layout_prev().await,
            Err(SvcError::KeyboardLayout(_))
        ));
        let server = Server::new(vec![]);
        let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &[]);
        assert!(matches!(
            handle.switch_layout_next().await,
            Err(SvcError::KeyboardLayout(_))
        ));
        assert!(matches!(
            handle.switch_layout_prev().await,
            Err(SvcError::KeyboardLayout(_))
        ));
        assert!(server.finish().await.is_empty());
    }

    #[tokio::test]
    async fn ambiguous_mutation_eof_is_reported_without_retry() {
        let server = Server::new(vec![Step {
            command: "dispatch quit".into(),
            reply: None,
            delay: Duration::ZERO,
        }]);
        let error = server
            .handle(Arc::new(Mutex::new(Registry::default())), &[])
            .quit_compositor()
            .await
            .unwrap_err();
        assert!(matches!(&error, SvcError::Protocol(message) if message.contains("unknown")));
        assert_eq!(server.finish().await, ["dispatch quit\n"]);
    }

    #[tokio::test]
    async fn native_ids_outside_the_signed_dispatch_range_never_connect() {
        let server = Server::new(vec![]);
        let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &[]);
        for id in [0, i32::MAX as u64 + 1, u64::from(u32::MAX), u64::MAX] {
            let error = handle.focus_window(id).await.unwrap_err();
            assert!(matches!(error, SvcError::Rejected(_)), "{id}: {error}");
        }
        assert!(server.finish().await.is_empty());
    }

    #[tokio::test]
    async fn a_multi_command_action_shares_one_two_second_deadline_and_never_retries() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(2));
        let selected = client(42, "app", false, false);
        let server = Server::new(vec![
            Step {
                delay: Duration::from_millis(750),
                ..reply("get client 42", selected)
            },
            Step {
                delay: Duration::from_millis(750),
                ..reply("get monitor DP-1", monitor("DP-1", true, &[1], &[1], None))
            },
            Step {
                delay: Duration::from_millis(900),
                ..reply("dispatch focusid,client,42", json!({"success": true}))
            },
        ]);
        let handle = server.handle(Arc::new(Mutex::new(Registry::default())), &[]);
        let error = tokio::time::timeout(Duration::from_secs(8), handle.focus_window(42))
            .await
            .expect("action finishes within the outer safety guard")
            .unwrap_err();
        assert!(matches!(error, SvcError::Timeout(duration) if duration == REQUEST_TIMEOUT));
        let commands = server.finish().await;
        assert_eq!(
            commands,
            [
                "get client 42\n",
                "get monitor DP-1\n",
                "dispatch focusid,client,42\n",
            ]
        );
    }
}
