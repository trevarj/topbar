//! Async system services for topbar.
//!
//! Everything that talks to the outside world — D-Bus, niri, PulseAudio,
//! subprocesses, the network — lives here and runs on a tokio runtime. The
//! crate deliberately has **no** GTK dependency: that is what makes it
//! impossible for a service task to touch a widget. State leaves this crate as
//! `Send + Clone` handles and `Arc<Snapshot>` values; the GTK crate subscribes
//! to them from the main thread.

#![warn(missing_docs)]

pub mod audio;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod change;
pub mod compositor;
pub mod connectivity;
pub mod crypto;
pub mod custom;
pub mod error;
pub mod headset;
pub mod hyprland;
pub mod inhibitor;
pub mod ipc;
mod lazy;
pub mod lifecycle;
pub mod logind;
pub mod media;
pub mod network;
pub mod niri;
pub mod notifications;
pub mod notmuch;
pub mod power;
pub mod power_profiles;
pub mod privacy;
pub mod proc;
pub mod refresh;
pub mod resources;
pub mod runtime;
#[cfg(any(
    test,
    feature = "fake-player",
    feature = "fake-sni",
    feature = "fake-power",
    feature = "fake-bluez",
    feature = "fake-nm"
))]
pub mod sidecar;
pub mod state_store;
pub mod tray;
pub mod updates;
pub mod weather;

#[cfg(test)]
mod private_bus;

/// The channel type every service publishes its state through.
///
/// Re-exported so the GTK crate can name a subscription without depending on
/// tokio itself — the less of the async stack it can see, the harder it is to
/// accidentally do async work on the main thread.
pub use tokio::sync::watch;
/// The two channels that carry something *to* the main thread.
///
/// Re-exported for the same reason, and used for the same kind of thing: the
/// configuration watcher's file events arrive on a thread of `notify`'s and are
/// handled on GTK's. Neither of these needs a tokio reactor to be awaited, which
/// is what makes them safe to await on the GTK main context.
pub use tokio::sync::{mpsc, oneshot};

pub use audio::{Audio, AudioHandle, AudioState, DeviceView};
pub use battery::{Battery, BatteryHandle, BatteryState, BatteryStatus, Thresholds};
pub use bluetooth::{
    Bluetooth, BluetoothHandle, BtDevice, BtState, IconKind, PairingPrompt, PromptKind,
};
pub use brightness::{Brightness, BrightnessHandle, BrightnessState};
pub use change::{Change, ChangeSource};
pub use compositor::{
    Compositor, CompositorHandle, KeyboardLayoutSnapshot, WorkspaceId, WorkspaceView,
    WorkspacesSnapshot,
};
pub use connectivity::{Connectivity, ConnectivityState};
pub use crypto::{Asset, Crypto, CryptoHandle, CryptoState, Entry, EntryQuote, Quote};
pub use custom::{CustomClass, CustomDisplay, CustomExec, CustomState, CustomWidgets};
pub use error::SvcError;
pub use headset::{Headset, HeadsetReading, HeadsetState};
pub use inhibitor::{Inhibitor, InhibitorHandle, InhibitorState};
pub use media::{ArtRef, Media, MediaHandle, MediaState, PlaybackStatus, PlayerView};
pub use network::{
    Access, ApView, Network, NetworkHandle, NetworkState, Pending, PendingPrompt, Secret, VpnKind,
    VpnView, WifiState, WiredState,
};
pub use niri::{Niri, NiriHandle};
pub use notifications::{
    Action, CloseReason, GroupView, IconSource, ImageData, NotifState, NotificationView,
    Notifications, NotificationsHandle, ToastView, Urgency,
};
pub use notmuch::{MailThread, Notmuch, NotmuchHandle, NotmuchState};
pub use power::{Power, PowerAction};
pub use power_profiles::{PowerProfiles, PowerProfilesHandle, PowerProfilesState, ProfileView};
pub use privacy::{Privacy, PrivacyState};
pub use resources::{Disk, Memory, ResourceState, Resources, ResourcesHandle};
pub use runtime::{Runtime, Services};
pub use state_store::StateStore;
pub use tray::{
    IconView, ItemView, MenuEvent, MenuKind, MenuNode, Pixmap, ScrollAxis, Status as TrayStatus,
    ToggleKind, ToggleState, Tray, TrayHandle, TrayState,
};
pub use updates::{Count, Distro, Updates, UpdatesState};
pub use weather::{
    CurrentWeather, DailyWeather, GeocodeResult, LocationView, Phase, TemperatureUnit, Weather,
    WeatherData, WeatherHandle, WeatherState,
};
