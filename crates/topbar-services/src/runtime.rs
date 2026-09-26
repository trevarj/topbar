//! The tokio runtime every service task shares, and the bundle of handles the
//! panel holds onto.
//!
//! [`Services::start`] runs before GTK is initialised and returns only handles
//! — watch receivers and `Clone` command handles. Nothing in the bundle can
//! reach a widget, and nothing a widget holds can block a service.

use std::path::PathBuf;
use std::sync::OnceLock;

use tokio::runtime;
use topbar_core::Config;
use tracing::info;

use crate::applications::Applications;
use crate::audio::Audio;
use crate::battery::Battery;
use crate::bluetooth::Bluetooth;
use crate::brightness::Brightness;
use crate::compositor::Compositor;
use crate::connectivity::Connectivity;
use crate::crypto::Crypto;
use crate::custom::CustomWidgets;
use crate::file_search::{FileSearch, FileSearchConfig};
use crate::headset::Headset;
use crate::inhibitor::Inhibitor;
use crate::ipc::Ipc;
use crate::lifecycle::Lifecycle;
use crate::media::Media;
use crate::network::Network;
use crate::notifications::Notifications;
use crate::notmuch::Notmuch;
use crate::power::Power;
use crate::power_profiles::PowerProfiles;
use crate::privacy::Privacy;
use crate::resources::Resources;
use crate::state_store::{LauncherUsage, StateStore};
use crate::tray::{DEFAULT_ICON_SIZE, Tray};
use crate::updates::Updates;
use crate::weather::Weather;

static RUNTIME: OnceLock<runtime::Runtime> = OnceLock::new();

/// Accessor for the process-wide service runtime.
#[derive(Debug, Clone, Copy)]
pub struct Runtime;

impl Runtime {
    /// Start the runtime if it has not been started yet, returning its handle.
    ///
    /// Two worker threads are enough for the panel's workload: services are
    /// almost entirely I/O bound.
    pub fn handle() -> runtime::Handle {
        RUNTIME
            .get_or_init(|| {
                runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("topbar-svc")
                    .enable_all()
                    .build()
                    .expect("service runtime should start")
            })
            .handle()
            .clone()
    }
}

/// Every running service, as handles the GTK side can hold.
///
/// New services become fields here; `start` is the one place that knows the
/// start-up order, so `main` does not change again as milestones land.
#[derive(Clone)]
pub struct Services {
    /// Installed desktop entries, discovered through GIO for launcher search.
    pub applications: Applications,
    /// The selected compositor service.
    pub compositor: Compositor,
    /// In-memory filename catalog for launcher search.
    pub files: FileSearch,
    /// Frequent application scores backed by the existing state writer.
    pub launcher_usage: LauncherUsage,
    /// The notification daemon.
    pub notifications: Notifications,
    /// The MPRIS media players.
    pub media: Media,
    /// The network: Wi-Fi, Ethernet, VPN and the secret agent.
    pub network: Network,
    /// Bluetooth: the adapter, its paired devices, and the pairing agent.
    pub bluetooth: Bluetooth,
    /// Whether the machine is online. A projection of [`Services::network`].
    pub connectivity: Connectivity,
    /// The weather, as one cache for the whole panel.
    pub weather: Weather,
    /// How many updates are pending, on whatever distribution this is.
    pub updates: Updates,
    /// Crypto prices, as one cache for the whole panel.
    pub crypto: Crypto,
    /// Unread mail, as notmuch sees whatever fills the maildir.
    pub notmuch: Notmuch,
    /// Every configured `custom-*` widget's script, one runner each.
    pub custom: CustomWidgets,
    /// The headset battery, when there is a headset to read.
    pub headset: Headset,
    /// The system tray.
    pub tray: Tray,
    /// The sound server.
    pub audio: Audio,
    /// The screen backlight.
    pub brightness: Brightness,
    /// The idle inhibitor.
    pub inhibitor: Inhibitor,
    /// The battery, and its charge limit.
    pub battery: Battery,
    /// CPU, memory and disks. Shared with M10's system_monitor widget.
    pub resources: Resources,
    /// Whether something is watching the screen.
    pub privacy: Privacy,
    /// The power-profiles daemon, when there is one.
    pub power_profiles: PowerProfiles,
    /// Shutting down, restarting and suspending.
    pub power: Power,
    /// The socket `topbar` commands arrive on.
    pub ipc: Ipc,
    /// Suspend and resume, as one subscriber with one inhibitor.
    pub lifecycle: Lifecycle,
}

/// Points the battery and power-profiles clients at a bus of a test's own.
///
/// Debug builds only, and read once at start-up. The nested-niri smoke run
/// puts a stand-in UPower and power-profiles daemon on its private session
/// bus, because there is no way to fake the *system* bus inside the sandbox
/// and pointing the panel at the real one would mean a screenshot taken by
/// changing the developer's machine. logind is deliberately not covered: the
/// idle inhibitor keeps talking to the real one, as it has since M8.
const SMOKE_BUS: &str = "TOPBAR_SMOKE_POWER_BUS";
/// The same, for `/sys/class/power_supply`.
const SMOKE_SYSFS: &str = "TOPBAR_SMOKE_POWER_SYSFS";
/// The same again, for NetworkManager.
///
/// The real one is the machine's live network, and this is the switch that
/// makes the network service willing to change anything at all — see
/// [`crate::network::Access`]. A debug build with this unset reads and does
/// nothing else.
const SMOKE_NM_BUS: &str = "TOPBAR_SMOKE_NM_BUS";
/// And again, for BlueZ.
///
/// The real one is the machine's live adapter, and this is the switch that
/// makes the Bluetooth service willing to change anything at all — including
/// whether it registers a pairing agent, which on a real session would take
/// the prompts the user's own desktop is waiting for. A debug build with this
/// unset reads and does nothing else.
const SMOKE_BLUEZ_BUS: &str = "TOPBAR_SMOKE_BLUEZ_BUS";
/// Where the updates service looks for `/etc/os-release`.
///
/// Debug builds only. The smoke run copies a distribution's own file into its
/// sandbox so a scenario can be on Arch or Debian without the machine being
/// either; `/etc` itself is only ever read.
const SMOKE_ROOT: &str = "TOPBAR_SMOKE_ROOT";
/// Which `notmuch` the mail service runs.
///
/// Debug builds only, and the one seam here that names a *program* rather than
/// a bus or a directory. `PATH` is not a strong enough promise: the smoke run
/// prepends its own directory, which leaves the developer's real notmuch — and
/// their real mail — one entry behind a fake of the same name. This says which
/// binary outright, and the scenario for "there is no notmuch on this machine"
/// points it at a path that does not exist.
const SMOKE_NOTMUCH: &str = "TOPBAR_SMOKE_NOTMUCH";

/// A smoke override, in debug builds only.
fn smoke(variable: &str) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
}

/// Which optional services a configuration actually asks for.
///
/// Derived from widget placement rather than from a switch of its own: a
/// service exists to feed a surface, so the question "does anything draw this"
/// already has an answer in the file. See [`crate::lazy`] for what "not wanted"
/// costs — the handles are real either way, the task is not started.
///
/// Everything absent from here is unconditional, and each for a reason: audio,
/// brightness and the inhibitor answer `topbar volume`/`brightness`/`inhibit`
/// with no bar in sight; the compositor drives OSD and per-output decisions;
/// notifications is a *role* on the session bus rather than a widget; the
/// network is what connectivity is projected from, and weather, crypto and
/// `requires_network` scripts all gate on that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Demand {
    /// A `crypto` widget is placed.
    crypto: bool,
    /// A `notmuch` widget is placed.
    notmuch: bool,
    /// A `weather` widget is placed, or the clock's control panel draws one.
    weather: bool,
    /// A `headset` widget is placed.
    headset: bool,
    /// A `tray` widget is placed.
    tray: bool,
    /// The clock's control panel is on the bar; it is what draws media.
    media: bool,
    /// The Quick Settings menu is placed.
    quick_settings: bool,
    /// Quick Settings, or a `system_monitor` widget.
    resources: bool,
}

impl Demand {
    /// Read the demand out of a configuration.
    fn of(config: &Config) -> Self {
        let placed = |name: &str| config.widgets.placed().any(|widget| widget == name);
        let control_panel = placed("clock") && config.widgets.clock.control_panel;
        let quick_settings = placed("quick_settings");
        Self {
            crypto: placed("crypto"),
            notmuch: placed("notmuch"),
            weather: placed("weather") || control_panel,
            headset: placed("headset"),
            tray: placed("tray"),
            media: control_panel,
            quick_settings,
            resources: quick_settings || placed("system_monitor"),
        }
    }
}

impl Services {
    /// Start every service. Call once, from `main`, before GTK.
    ///
    /// Blocking here is deliberate and momentary: services are spawned onto
    /// the runtime, not awaited, so this returns as soon as their tasks exist.
    ///
    /// Optional services are started only if something in `config` draws them
    /// — see [`Demand`]. A reload that adds such a widget calls
    /// [`Self::start_if_needed`], which is why the two live next to each other.
    pub fn start(config: &Config) -> Self {
        let compositor = config.advanced.compositor.clone();
        let file_search = FileSearchConfig::new(
            config.launcher.file_roots.clone(),
            config.launcher.file_exclusions.clone(),
        );
        let weather = config.widgets.weather.clone();
        let crypto = config.widgets.crypto.clone();
        let custom = config.widgets.custom.clone();
        let headset = config.widgets.headset.clone();
        // The tray picks its pixmaps for the size the widget will draw them
        // at, so the icon that arrives is the icon that is shown.
        let icon_size = config
            .widgets
            .tray
            .pixmap_icon_size
            .map_or(DEFAULT_ICON_SIZE, |size| size as i32);
        let allow_overdrive = config.audio.allow_overdrive;
        let updates = config.updates.clone();
        let notmuch = config.widgets.notmuch.clone();
        let power_bus = smoke(SMOKE_BUS);
        let power_sysfs = smoke(SMOKE_SYSFS).map(PathBuf::from);
        let nm_bus = smoke(SMOKE_NM_BUS);
        let bluez_bus = smoke(SMOKE_BLUEZ_BUS);
        let root = smoke(SMOKE_ROOT).map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let notmuch_program = smoke(SMOKE_NOTMUCH).map(PathBuf::from);
        let demand = Demand::of(config);
        let placed: std::collections::BTreeSet<String> =
            config.widgets.placed().map(str::to_string).collect();
        Runtime::handle().block_on(async move {
            // The state file is read once, here, so every service that
            // restores something starts from one consistent document.
            let (state, store) = StateStore::open();
            // The network first, and connectivity out of it: the weather and
            // crypto services subscribe to connectivity, and a subscriber built
            // before the service exists would have nothing to read on its first
            // frame.
            let network = Network::start(nm_bus, state.network, Some(store.clone()));
            let connectivity = Connectivity::from_network(&network);
            Self {
                applications: Applications::start(),
                compositor: Compositor::start(&compositor),
                files: FileSearch::start(file_search),
                launcher_usage: LauncherUsage::new(state.launcher, store.clone()),
                notifications: Notifications::start(state.notifications, store.clone(), None),
                media: Media::start(None, demand.media),
                weather: Weather::start(
                    &weather,
                    state.weather,
                    store.clone(),
                    &connectivity,
                    demand.weather,
                ),
                updates: Updates::with_root(&updates, &connectivity, root, demand.quick_settings),
                crypto: Crypto::start(&crypto, state.crypto, store, &connectivity, demand.crypto),
                notmuch: Notmuch::start(&notmuch, notmuch_program, demand.notmuch),
                custom: CustomWidgets::start(&custom, &connectivity, &|name| placed.contains(name)),
                headset: Headset::start(&headset, demand.headset),
                tray: Tray::start(icon_size, None, demand.tray),
                audio: Audio::start(allow_overdrive),
                brightness: Brightness::start(None),
                inhibitor: Inhibitor::start(None),
                battery: Battery::start(power_bus.clone(), power_sysfs, demand.quick_settings),
                power_profiles: PowerProfiles::start(power_bus, demand.quick_settings),
                bluetooth: Bluetooth::start(bluez_bus, demand.quick_settings),
                resources: Resources::start(demand.resources),
                privacy: Privacy::start(demand.quick_settings),
                power: Power::new(None),
                ipc: Ipc::start(),
                lifecycle: Lifecycle::start(None),
                network,
                connectivity,
            }
        })
    }

    /// Refresh everything a sleep made stale, and keep doing it.
    ///
    /// Call once, after `start`. Everything on a panel goes stale in the same
    /// instant and for the same reason — the machine was not running — so
    /// exactly one thing notices and everything else is told.
    ///
    /// Deliberately not here: the clock, whose one-shot tick is re-armed from
    /// inside its own callback and therefore fires the moment a deadline that
    /// passed during the sleep is noticed; the audio and brightness services,
    /// which are told by their own servers; and the notification daemon, which
    /// has nothing to re-read.
    pub fn wake_on_resume(&self) {
        let services = self.clone();
        Runtime::handle().spawn(async move {
            let mut state = services.lifecycle.state();
            let mut seen = state.borrow_and_update().resumes;
            while state.changed().await.is_ok() {
                let resumes = state.borrow_and_update().resumes;
                if resumes == seen {
                    continue;
                }
                seen = resumes;
                info!("the machine is back; refreshing what slept through it");
                services.wake().await;
            }
        });
    }

    /// Ask every service that has something stale to go and look again.
    async fn wake(&self) {
        // The compositor stream first: everything else is a number on the panel,
        // this one is whether the panel is showing this session at all.
        self.compositor.health_check();
        self.files.handle().resumed().await.ok();
        // The CPU delta spans the sleep and is meaningless; the next reading
        // has to start a fresh pair rather than report a spike.
        self.resources.handle().discard_stale_sample().await;
        self.headset.poll_now().await;
        self.battery.handle().refresh().await.ok();
        self.updates.recheck().await;
        // A machine that has been asleep for a day has a day's mail in it, and
        // this one costs a local index query rather than a round trip.
        self.notmuch.recheck().await;
        // The network before the two things that go out over it. The radio was
        // down for the length of the sleep and everything NetworkManager said
        // about it was said to a socket nobody was reading, so the panel reads
        // the lot again — and connectivity, which is projected from it, is
        // right by the time the weather asks.
        self.network.handle().refresh_now().await.ok();
        // The two that reach the network. Failures are the services' own
        // business — both keep what they had and back off.
        self.weather.handle().refresh_now().await.ok();
        self.crypto.handle().refresh_now().await.ok();
    }

    /// Start whatever a freshly loaded configuration now asks for.
    ///
    /// The other half of [`Demand`]: a reload that places a `crypto` widget on
    /// a panel that never had one has to start the service before the widget is
    /// built, or the widget subscribes to a snapshot nothing will ever fill.
    /// Starting is idempotent, so this is safe to call on every reload, and it
    /// is one-way — a widget taken off the bar leaves its service running,
    /// because stopping it would mean deciding what "stopped" means for a
    /// pairing agent, a tray host and a state file.
    ///
    /// Returns the names of the services this call started, for the log.
    pub fn start_if_needed(&self, config: &Config) -> Vec<&'static str> {
        let demand = Demand::of(config);
        let mut started = Vec::new();
        let checks: [(bool, &'static str, &dyn Fn() -> bool); 12] = [
            (demand.crypto, "crypto", &|| self.crypto.ensure_started()),
            (demand.notmuch, "notmuch", &|| self.notmuch.ensure_started()),
            (demand.weather, "weather", &|| self.weather.ensure_started()),
            (demand.headset, "headset", &|| self.headset.ensure_started()),
            (demand.tray, "tray", &|| self.tray.ensure_started()),
            (demand.media, "media", &|| self.media.ensure_started()),
            (demand.resources, "resources", &|| {
                self.resources.ensure_started()
            }),
            (demand.quick_settings, "updates", &|| {
                self.updates.ensure_started()
            }),
            (demand.quick_settings, "battery", &|| {
                self.battery.ensure_started()
            }),
            (demand.quick_settings, "bluetooth", &|| {
                self.bluetooth.ensure_started()
            }),
            (demand.quick_settings, "power-profiles", &|| {
                self.power_profiles.ensure_started()
            }),
            (demand.quick_settings, "privacy", &|| {
                self.privacy.ensure_started()
            }),
        ];
        for (wanted, name, start) in checks {
            if wanted && start() {
                started.push(name);
            }
        }
        started
    }

    /// Bring the `custom-*` runners in line with a freshly loaded config.
    ///
    /// Separate from [`Self::start_if_needed`] because it needs the *previous*
    /// configuration too: a section that did not change keeps the runner it
    /// has, timer and all.
    pub fn sync_custom(&self, previous: &Config, config: &Config) {
        let placed: std::collections::BTreeSet<&str> = config.widgets.placed().collect();
        self.custom.sync(
            &config.widgets.custom,
            &self.connectivity,
            &|name| placed.contains(name),
            &previous.widgets.custom,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration this project is written for.
    const LIVE_CONFIG: &str = include_str!("../../topbar-core/tests/fixtures/live-config.toml");

    #[test]
    fn the_live_configuration_asks_for_everything_it_draws() {
        let (config, _) = Config::parse(LIVE_CONFIG).expect("the live config parses");
        let demand = Demand::of(&config);

        assert!(demand.weather, "a weather widget is placed");
        assert!(demand.headset, "a headset widget is placed");
        assert!(demand.tray, "a tray widget is placed");
        assert!(demand.quick_settings);
        assert!(demand.resources, "quick_settings and a system_monitor");
        assert!(demand.media, "the clock opens a control panel");
        assert!(
            !demand.crypto,
            "the live config uses the custom-* script, not the built-in widget"
        );
    }

    #[test]
    fn a_bar_that_draws_nothing_optional_asks_for_nothing() {
        let (config, _) = Config::parse(
            "[widgets]\nleft = []\ncenter = [\"clock\"]\nright = []\n\
             \n[widgets.clock]\ncontrol_panel = false\n",
        )
        .expect("a minimal config parses");
        let demand = Demand::of(&config);

        assert_eq!(
            demand,
            Demand {
                crypto: false,
                notmuch: false,
                weather: false,
                headset: false,
                tray: false,
                media: false,
                quick_settings: false,
                resources: false,
            },
            "a clock with no control panel needs no optional service at all"
        );
    }

    #[test]
    fn the_control_panel_is_what_asks_for_the_weather_and_the_players() {
        let (config, _) = Config::parse(
            "[widgets]\nleft = []\ncenter = [\"clock\"]\nright = []\n\
             \n[widgets.clock]\ncontrol_panel = true\n",
        )
        .expect("a config with a control panel parses");
        let demand = Demand::of(&config);

        assert!(demand.weather, "the panel draws a forecast card");
        assert!(demand.media, "the panel draws the media controls");
        assert!(!demand.quick_settings);
    }

    #[test]
    fn a_system_monitor_on_its_own_still_needs_the_sampler() {
        let (config, _) =
            Config::parse("[widgets]\nleft = []\ncenter = []\nright = [\"system_monitor\"]\n")
                .expect("a config with a system monitor parses");
        let demand = Demand::of(&config);

        assert!(demand.resources);
        assert!(!demand.quick_settings, "nothing else came with it");
    }

    #[test]
    fn handle_is_stable_across_calls() {
        let first = Runtime::handle();
        let second = Runtime::handle();
        assert_eq!(first.id(), second.id());
    }

    #[test]
    fn runtime_can_run_a_task() {
        let value = Runtime::handle().block_on(async { 1 + 1 });
        assert_eq!(value, 2);
    }

    /// Compile-time proof that zbus links and its address parser works without
    /// a live bus, so M2 does not discover the native toolchain is broken.
    #[test]
    fn zbus_address_parsing_links() {
        let address: zbus::Address = "unix:path=/run/user/1000/bus"
            .try_into()
            .expect("well-formed bus address should parse");
        assert!(address.to_string().contains("/run/user/1000/bus"));
    }
}
