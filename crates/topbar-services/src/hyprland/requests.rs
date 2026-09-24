//! Short-lived request sockets and instance-scoped logout.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::timeout;

use super::snapshot::{Client, decode};
use crate::SvcError;

pub(super) const DEADLINE: Duration = Duration::from_secs(2);
const REPLY_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Live {
    pub peer: Option<Peer>,
    // Only socket2 urgency we actually observed; workspace urgency is queried
    // separately. Upstream exposes no reconstructible per-window urgent order.
    pub urgent: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Peer {
    pub pid: u32,
    ppid: u32,
    start_time: u64,
    exe: PathBuf,
}

impl Peer {
    pub(super) async fn read(root: &Path, pid: u32) -> Result<Self, SvcError> {
        let directory = root.join(pid.to_string());
        let stat = tokio::fs::read_to_string(directory.join("stat"))
            .await
            .map_err(SvcError::Io)?;
        let (ppid, start_time) = parse_stat(&stat, pid)?;
        let exe = tokio::fs::read_link(directory.join("exe"))
            .await
            .map_err(SvcError::Io)?;
        Ok(Self {
            pid,
            ppid,
            start_time,
            exe,
        })
    }
}

// The comm field may contain spaces and closing parentheses. Fields after its
// LAST ')' are unambiguous; never identify the watchdog by its process title.
fn parse_stat(stat: &str, pid: u32) -> Result<(u32, u64), SvcError> {
    let invalid = || SvcError::Protocol("invalid compositor /proc stat identity".into());
    let (leading, _) = stat.split_once(" (").ok_or_else(invalid)?;
    if leading.parse::<u32>().ok() != Some(pid) {
        return Err(invalid());
    }
    let (_, fields) = stat.rsplit_once(')').ok_or_else(invalid)?;
    let mut fields = fields.split_whitespace();
    let ppid = fields
        .nth(1)
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let start = fields
        .nth(17)
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    Ok((ppid, start))
}

/// Actions to the selected Hyprland instance; never retries a mutation.
#[derive(Clone)]
pub struct HyprlandHandle {
    directory: Option<PathBuf>,
    live: watch::Sender<Live>,
}

impl HyprlandHandle {
    pub(super) fn new(directory: Option<PathBuf>, live: watch::Sender<Live>) -> Self {
        Self { directory, live }
    }

    pub(super) async fn request(&self, payload: &str) -> Result<String, SvcError> {
        timeout(DEADLINE, async {
            let directory = self.directory.as_ref().ok_or(SvcError::NoHyprlandSocket)?;
            let mut socket = UnixStream::connect(directory.join(".socket.sock"))
                .await
                .map_err(SvcError::Io)?;
            // Hyprland reads the request immediately and closes after replying.
            // It does not permit an idle pooled command connection.
            socket
                .write_all(payload.as_bytes())
                .await
                .map_err(SvcError::Io)?;
            let mut bytes = Vec::new();
            socket
                .take(REPLY_LIMIT + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(SvcError::Io)?;
            if bytes.len() as u64 > REPLY_LIMIT {
                return Err(SvcError::Protocol("Hyprland reply exceeds 16 MiB".into()));
            }
            String::from_utf8(bytes).map_err(|error| SvcError::Protocol(error.to_string()))
        })
        .await
        .map_err(|_| SvcError::Timeout(DEADLINE))?
    }

    async fn act(&self, payload: &str) -> Result<(), SvcError> {
        let reply = self.request(payload).await?;
        if reply.trim() == "ok" {
            Ok(())
        } else {
            Err(SvcError::Rejected(reply))
        }
    }

    /// Resolve the native ID at execution time; named negative IDs are not relative moves.
    pub async fn focus_workspace(&self, id: i64) -> Result<(), SvcError> {
        self.act(&format!("/eval for _,w in ipairs(hl.get_workspaces()) do if w.id == {id} then if w.monitor == nil then error('workspace has no monitor') end if w.active then hl.dsp.focus({{monitor=w.monitor}}) else hl.dsp.focus({{workspace=w.config_name}}) end return end end error('workspace no longer exists')")).await
    }

    /// Find a notification sender by normalized class, urgent then MRU within identity preference.
    pub async fn focus_app(&self, identities: &[&str]) -> Result<bool, SvcError> {
        timeout(DEADLINE, async {
            let clients: Vec<Client> = decode(&self.request("j/clients").await?)?;
            let address = {
                let live = self.live.borrow();
                pick_client(&clients, identities, &live.urgent).map(|client| client.address.clone())
            };
            let Some(address) = address else {
                return Ok(false);
            };
            if !valid_address(&address) {
                return Err(SvcError::Protocol(
                    "Hyprland returned an invalid window address".into(),
                ));
            }
            self.act(&format!(
                "/dispatch hl.dsp.focus({{window='address:{address}'}})"
            ))
            .await?;
            Ok(true)
        })
        .await
        .map_err(|_| SvcError::Timeout(DEADLINE))?
    }

    /// Switch the main keyboard to its next configured layout.
    pub async fn switch_layout_next(&self) -> Result<(), SvcError> {
        self.act("/switchxkblayout current next").await
    }

    /// Switch the main keyboard to its previous configured layout.
    pub async fn switch_layout_prev(&self) -> Result<(), SvcError> {
        self.act("/switchxkblayout current prev").await
    }

    /// Stop the owning UWSM unit only after verifying the live socket's process identity.
    pub async fn quit_compositor(&self) -> Result<(), SvcError> {
        timeout(DEADLINE, async {
            let bus = match zbus::Connection::session().await {
                Ok(bus) => Some(bus),
                Err(zbus::Error::InputOutput(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    None
                }
                Err(error) => return Err(bus_error(error)),
            };
            self.quit_on(bus.as_ref(), Path::new("/proc")).await
        })
        .await
        .map_err(|_| SvcError::Timeout(DEADLINE))?
    }

    async fn quit_on(
        &self,
        bus: Option<&zbus::Connection>,
        proc_root: &Path,
    ) -> Result<(), SvcError> {
        let peer = self.live.borrow().peer.clone().ok_or_else(|| {
            SvcError::Protocol(
                "Hyprland event socket is disconnected; cannot identify logout target".into(),
            )
        })?;
        verify_peer(&peer, proc_root).await?;
        let unit = match bus {
            Some(bus) => owning_unit(bus, &peer, proc_root).await?,
            None => None, // no user manager: a standalone compositor
        };
        if self.live.borrow().peer.as_ref() != Some(&peer) {
            return Err(SvcError::Protocol(
                "Hyprland event connection changed during logout".into(),
            ));
        }
        verify_peer(&peer, proc_root).await?;
        if let Some(unit) = unit {
            let bus = bus.expect("managed unit has a bus");
            // Re-read all manager and process identities immediately before stopping.
            if owning_unit(bus, &peer, proc_root).await?.as_deref() != Some(&unit)
                || self.live.borrow().peer.as_ref() != Some(&peer)
            {
                return Err(SvcError::Protocol(
                    "Hyprland unit identity changed during logout".into(),
                ));
            }
            ManagerProxy::new(bus)
                .await
                .map_err(bus_error)?
                .stop_unit(&unit, "fail")
                .await
                .map_err(bus_error)?;
            Ok(()) // managed-stop failure NEVER falls back to native exit
        } else {
            self.act("/dispatch hl.dsp.exit()").await
        }
    }
}

pub(super) fn valid_address(address: &str) -> bool {
    address.strip_prefix("0x").is_some_and(|hex| {
        !hex.is_empty() && hex.len() <= 16 && hex.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

fn pick_client<'a>(
    clients: &'a [Client],
    identities: &[&str],
    urgent: &HashSet<String>,
) -> Option<&'a Client> {
    let normalise = |value: &str| value.trim().trim_start_matches('@').to_lowercase();
    identities.iter().find_map(|identity| {
        let identity = normalise(identity);
        clients
            .iter()
            .filter(|client| {
                normalise(&client.class) == identity || normalise(&client.initial_class) == identity
            })
            .min_by(|a, b| {
                let rank = |client: &Client| {
                    (
                        !urgent.contains(&client.address),
                        if client.focus_history_id < 0 {
                            i64::MAX
                        } else {
                            client.focus_history_id
                        },
                    )
                };
                rank(a)
                    .cmp(&rank(b))
                    .then_with(|| a.address.cmp(&b.address))
            })
    })
}

fn bus_error(error: zbus::Error) -> SvcError {
    SvcError::Bus(error.to_string())
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait Manager {
    #[zbus(name = "GetUnitByPID")]
    fn get_unit_by_pid(&self, pid: u32) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
trait Unit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Service",
    default_service = "org.freedesktop.systemd1"
)]
trait Service {
    #[zbus(property, name = "MainPID")]
    fn main_pid(&self) -> zbus::Result<u32>;
}

async fn verify_peer(peer: &Peer, root: &Path) -> Result<(), SvcError> {
    if Peer::read(root, peer.pid).await? != *peer {
        return Err(SvcError::Protocol(
            "Hyprland process identity changed".into(),
        ));
    }
    Ok(())
}

fn owns_instance(peer: &Peer, main: &Peer) -> bool {
    peer.pid == main.pid
        || (peer.ppid == main.pid
            && main
                .exe
                .file_name()
                .is_some_and(|name| name == "start-hyprland" || name == ".start-hyprland-wrapped"))
}

async fn owning_unit(
    bus: &zbus::Connection,
    peer: &Peer,
    proc_root: &Path,
) -> Result<Option<String>, SvcError> {
    let dbus = zbus::fdo::DBusProxy::new(bus).await.map_err(bus_error)?;
    let name =
        zbus::names::BusName::try_from("org.freedesktop.systemd1").expect("constant bus name");
    if !dbus
        .name_has_owner(name)
        .await
        .map_err(|error| SvcError::Bus(error.to_string()))?
    {
        return Ok(None);
    }
    let manager = ManagerProxy::new(bus).await.map_err(bus_error)?;
    let path = match manager.get_unit_by_pid(peer.pid).await {
        Ok(path) => path,
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
        {
            return Ok(None);
        }
        Err(error) => return Err(bus_error(error)),
    };
    let unit = UnitProxy::builder(bus)
        .path(path.clone())
        .map_err(bus_error)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(bus_error)?;
    let id = unit.id().await.map_err(bus_error)?;
    if !id.starts_with("wayland-wm@") || !id.ends_with(".service") {
        return Ok(None);
    }
    let active = unit.active_state().await.map_err(bus_error)?;
    if !matches!(active.as_str(), "active" | "activating") {
        return Err(SvcError::Protocol(format!(
            "Hyprland owner {id} is {active}"
        )));
    }
    let service = ServiceProxy::builder(bus)
        .path(path)
        .map_err(bus_error)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(bus_error)?;
    let pid = service.main_pid().await.map_err(bus_error)?;
    if pid == 0 {
        return Err(SvcError::Protocol(
            "Hyprland unit has no stable MainPID".into(),
        ));
    }
    let main = Peer::read(proc_root, pid).await?;
    verify_peer(peer, proc_root).await?;
    verify_peer(&main, proc_root).await?;
    Ok(owns_instance(peer, &main).then_some(id))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::net::UnixListener;

    const UNIT_PATH: &str = "/org/freedesktop/systemd1/unit/wayland";
    const UNIT_ID: &str = "wayland-wm@hyprland.desktop.service";

    struct FakeManager {
        stops: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
        fail: Arc<AtomicBool>,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl FakeManager {
        #[zbus(name = "GetUnitByPID")]
        fn get_unit_by_pid(&self, _pid: u32) -> zbus::zvariant::OwnedObjectPath {
            UNIT_PATH.try_into().unwrap()
        }

        async fn stop_unit(
            &self,
            name: &str,
            mode: &str,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
            self.stops.lock().await.push((name.into(), mode.into()));
            if self.fail.load(Ordering::SeqCst) {
                return Err(zbus::fdo::Error::Failed("stop denied".into()));
            }
            Ok("/org/freedesktop/systemd1/job/1".try_into().unwrap())
        }
    }

    struct FakeUnit(Arc<AtomicBool>);
    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl FakeUnit {
        #[zbus(property)]
        fn id(&self) -> &str {
            if self.0.load(Ordering::SeqCst) {
                "app-nested.scope"
            } else {
                UNIT_ID
            }
        }
        #[zbus(property)]
        fn active_state(&self) -> &str {
            "active"
        }
    }

    struct FakeService(Arc<AtomicU32>);
    #[zbus::interface(name = "org.freedesktop.systemd1.Service")]
    impl FakeService {
        #[zbus(property, name = "MainPID")]
        fn main_pid(&self) -> u32 {
            self.0.load(Ordering::SeqCst)
        }
    }

    async fn process(root: &Path, pid: u32, ppid: u32, exe: &str) -> Peer {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stat"),
            format!(
                "{pid} (arbitrary ) process title) S {ppid} {} 991 0",
                vec!["0"; 17].join(" ")
            ),
        )
        .unwrap();
        std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
        Peer::read(root, pid).await.unwrap()
    }

    async fn native_exit(commands: &UnixListener) {
        let (mut socket, _) = commands.accept().await.unwrap();
        let mut bytes = [0; 128];
        let len = socket.read(&mut bytes).await.unwrap();
        assert_eq!(&bytes[..len], b"/dispatch hl.dsp.exit()");
        socket.write_all(b"ok").await.unwrap();
    }

    #[tokio::test]
    async fn private_manager_stops_only_primary_instance_and_failure_never_exits_native() {
        let bus = crate::private_bus::private_bus!();
        let stops = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let main = Arc::new(AtomicU32::new(7));
        let ordinary = Arc::new(AtomicBool::new(false));
        let server = zbus::connection::Builder::address(bus.address())
            .unwrap()
            .name("org.freedesktop.systemd1")
            .unwrap()
            .serve_at(
                "/org/freedesktop/systemd1",
                FakeManager {
                    stops: stops.clone(),
                    fail: fail.clone(),
                },
            )
            .unwrap()
            .serve_at(UNIT_PATH, FakeUnit(ordinary.clone()))
            .unwrap()
            .serve_at(UNIT_PATH, FakeService(main.clone()))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = bus.connect().await;
        let root = directory();
        process(&root, 7, 1, "/nix/store/pkg/bin/start-hyprland").await;
        let primary = process(&root, 42, 7, "/nix/store/pkg/bin/Hyprland").await;
        let nested_direct = process(&root, 50, 42, "/nix/store/pkg/bin/Hyprland").await;
        process(&root, 54, 42, "/nix/store/pkg/bin/start-hyprland").await;
        let nested_watchdog = process(&root, 55, 54, "/nix/store/pkg/bin/Hyprland").await;
        let live = watch::channel(Live::default()).0;
        live.send_modify(|state| state.peer = Some(primary.clone()));
        let handle = HyprlandHandle::new(Some(root.clone()), live.clone());
        let commands = UnixListener::bind(root.join(".socket.sock")).unwrap();

        handle.quit_on(Some(&client), &root).await.unwrap();
        assert_eq!(*stops.lock().await, [(UNIT_ID.into(), "fail".into())]);
        fail.store(true, Ordering::SeqCst);
        assert!(handle.quit_on(Some(&client), &root).await.is_err());
        assert!(
            timeout(Duration::from_millis(60), commands.accept())
                .await
                .is_err(),
            "failed managed stop must not send native exit"
        );

        // Both nesting shapes can share the host unit's cgroup, but not MainPID.
        main.store(42, Ordering::SeqCst);
        for peer in [nested_direct, nested_watchdog] {
            live.send_modify(|state| state.peer = Some(peer));
            let quit = handle.quit_on(Some(&client), &root);
            let (result, ()) = timeout(Duration::from_secs(5), async {
                tokio::join!(quit, native_exit(&commands))
            })
            .await
            .unwrap();
            result.unwrap();
        }
        assert_eq!(
            stops.lock().await.len(),
            2,
            "nested instances never stop the host unit"
        );
        // An ordinary app unit and an absent manager are standalone/native cases.
        live.send_modify(|state| state.peer = Some(primary.clone()));
        ordinary.store(true, Ordering::SeqCst);
        let unmanaged = crate::private_bus::private_bus!();
        let unmanaged_client = unmanaged.connect().await;
        for bus in [&client, &unmanaged_client] {
            let (result, ()) = timeout(Duration::from_secs(5), async {
                tokio::join!(handle.quit_on(Some(bus), &root), native_exit(&commands))
            })
            .await
            .unwrap();
            result.unwrap();
        }
        ordinary.store(false, Ordering::SeqCst);
        live.send_modify(|state| state.peer = Some(primary));
        std::fs::write(
            root.join("42/stat"),
            format!("42 (reused PID) S 7 {} 992 0", vec!["0"; 17].join(" ")),
        )
        .unwrap();
        assert!(
            handle.quit_on(Some(&client), &root).await.is_err(),
            "changed process identity fails closed"
        );
        assert_eq!(stops.lock().await.len(), 2);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    pub(in crate::hyprland) fn directory() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "topbar-hypr-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn proc_titles_cannot_impersonate_the_watchdog_or_break_ppid_parsing() {
        let stat = format!(
            "42 (a tricky ) name)) S 7 {} 991 0",
            vec!["0"; 17].join(" ")
        );
        assert_eq!(parse_stat(&stat, 42).unwrap(), (7, 991));
        assert!(parse_stat(&stat, 43).is_err());
        let process = |pid, ppid, exe| Peer {
            pid,
            ppid,
            start_time: 1,
            exe: PathBuf::from(exe),
        };
        let watchdog = process(7, 1, "/nix/store/package/bin/start-hyprland");
        let primary = process(42, 7, "/nix/store/package/bin/Hyprland");
        assert!(owns_instance(&primary, &watchdog));
        assert!(owns_instance(&primary, &primary));
        assert!(!owns_instance(&process(50, 42, "/bin/Hyprland"), &primary));
        assert!(!owns_instance(&process(51, 50, "/bin/Hyprland"), &primary));
        assert!(!owns_instance(&process(51, 50, "/bin/Hyprland"), &watchdog));
        assert!(!owns_instance(&primary, &process(7, 1, "/usr/bin/python")));
    }

    #[test]
    fn identity_preference_urgency_mru_and_address_validation() {
        let clients: Vec<Client> = decode(r#"[{"address":"0x1","class":"Brave","initialClass":"browser","workspace":{"id":1},"focusHistoryID":-1},{"address":"0x2","class":"Brave","initialClass":"browser","workspace":{"id":1},"focusHistoryID":2},{"address":"0x3","class":"Other","initialClass":"other","workspace":{"id":1},"focusHistoryID":0}]"#).unwrap();
        assert_eq!(
            pick_client(&clients, &[" @BRAVE ", "Other"], &HashSet::new())
                .unwrap()
                .address,
            "0x2"
        );
        assert_eq!(
            pick_client(&clients, &["browser"], &HashSet::from(["0x1".into()]))
                .unwrap()
                .address,
            "0x1"
        );
        assert!(pick_client(&clients, &["absent"], &HashSet::new()).is_none());
        assert!(valid_address("0xA01"));
        assert!(!valid_address("0x1'); error('injected"));
        assert!(!valid_address("0x"));
    }

    #[tokio::test]
    async fn fresh_sockets_and_no_duplicate_after_timed_out_mutation() {
        let dir = directory();
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let handle = HyprlandHandle::new(Some(dir.clone()), watch::channel(Live::default()).0);
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let expected = b"/switchxkblayout current next";
            let mut bytes = [0; b"/switchxkblayout current next".len()];
            first.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, expected);
            first.write_all(b"ok").await.unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            let expected = b"/switchxkblayout current prev";
            let mut bytes = [0; b"/switchxkblayout current prev".len()];
            second.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, expected);
            assert!(
                timeout(DEADLINE + Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        handle.switch_layout_next().await.unwrap();
        assert!(matches!(
            handle.switch_layout_prev().await,
            Err(SvcError::Timeout(_))
        ));
        server.await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn oversized_or_rejected_replies_are_errors_not_success() {
        let dir = directory();
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let handle = HyprlandHandle::new(Some(dir.clone()), watch::channel(Live::default()).0);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let expected = b"/eval for _,w in ipairs(hl.get_workspaces()) do if w.id == -9 then if w.monitor == nil then error('workspace has no monitor') end if w.active then hl.dsp.focus({monitor=w.monitor}) else hl.dsp.focus({workspace=w.config_name}) end return end end error('workspace no longer exists')";
            let mut request = vec![0; expected.len()];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, expected);
            socket
                .write_all(b"error: workspace no longer exists")
                .await
                .unwrap();
            drop(socket);
            let (mut socket, _) = listener.accept().await.unwrap();
            let expected = b"j/clients";
            let mut request = [0; b"j/clients".len()];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, expected);
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..=REPLY_LIMIT / chunk.len() as u64 {
                if socket.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        assert!(matches!(
            handle.focus_workspace(-9).await,
            Err(SvcError::Rejected(_))
        ));
        assert!(matches!(
            handle.request("j/clients").await,
            Err(SvcError::Protocol(_))
        ));
        server.await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn workspace_focus_uses_the_pinned_lua_dispatcher_with_a_native_id() {
        let dir = directory();
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let handle = HyprlandHandle::new(Some(dir.clone()), watch::channel(Live::default()).0);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let expected = b"/eval for _,w in ipairs(hl.get_workspaces()) do if w.id == -9 then if w.monitor == nil then error('workspace has no monitor') end if w.active then hl.dsp.focus({monitor=w.monitor}) else hl.dsp.focus({workspace=w.config_name}) end return end end error('workspace no longer exists')";
            let mut bytes = vec![0; expected.len()];
            socket.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, expected);
            socket.write_all(b"ok").await.unwrap();
        });
        handle.focus_workspace(-9).await.unwrap();
        server.await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
