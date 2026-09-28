//! Every icon the panel names, in one place.
//!
//! Two rules, and this module exists to make both greppable. Icons are Adwaita
//! symbolic names — the packaged panel depends on `adwaita-icon-theme` and on
//! nothing else — and no widget writes an icon name as a literal, so a name
//! that Adwaita drops in some future release breaks in one file rather than in
//! nine.
//!
//! [`first_available`] is the exception that proves it: a couple of concepts
//! have a *better* icon that Adwaita has never carried, and a user who has the
//! GNOME extension that ships it should get it. The preferred name is tried
//! against the live icon theme and the Adwaita fallback is used when it is not
//! installed, which is a lookup rather than a guess.

/// Sound output, at four levels.
pub const VOLUME_MUTED: &str = "audio-volume-muted-symbolic";
/// Quiet.
pub const VOLUME_LOW: &str = "audio-volume-low-symbolic";
/// Middling.
pub const VOLUME_MEDIUM: &str = "audio-volume-medium-symbolic";
/// Loud.
pub const VOLUME_HIGH: &str = "audio-volume-high-symbolic";
/// Past what the hardware calls full volume.
pub const VOLUME_OVERDRIVEN: &str = "audio-volume-overamplified-symbolic";

/// The microphone, at four levels.
pub const MIC_MUTED: &str = "microphone-sensitivity-muted-symbolic";
/// Quiet.
pub const MIC_LOW: &str = "microphone-sensitivity-low-symbolic";
/// Middling.
pub const MIC_MEDIUM: &str = "microphone-sensitivity-medium-symbolic";
/// Loud.
pub const MIC_HIGH: &str = "microphone-sensitivity-high-symbolic";

/// The backlight.
pub const BRIGHTNESS: &str = "display-brightness-symbolic";

/// One row of the output chooser.
///
/// The same icon on every sink, on purpose. PulseAudio tells the panel a
/// device's description and nothing about what sort of thing it is, and
/// reading "headphones" out of a description is how a pair of desk speakers
/// comes to be drawn with a headband. What the icon is *for* is the column:
/// the network, Bluetooth and VPN lists all lead with one, and without it the
/// output rows were the only list in the panel whose names started somewhere
/// else.
pub const OUTPUT_DEVICE: &str = "audio-card-symbolic";

/// Lock the session.
pub const LOCK: &str = "system-lock-screen-symbolic";
/// The power section, and shutting down.
pub const SHUT_DOWN: &str = "system-shutdown-symbolic";
/// Restart.
pub const RESTART: &str = "system-reboot-symbolic";
/// Log out.
pub const LOG_OUT: &str = "system-log-out-symbolic";
/// Suspend, preferring the name GNOME Shell uses for it.
///
/// `system-suspend-symbolic` belongs to gnome-shell rather than to Adwaita, so
/// the fallback is the crescent moon — which is what "asleep" looks like in
/// every icon set that has ever drawn it.
pub const SUSPEND: &[&str] = &["system-suspend-symbolic", "weather-clear-night-symbolic"];

/// Caffeine, preferring the icon the GNOME extension ships.
///
/// Adwaita has no coffee cup. The fallback is the screensaver icon, which is
/// the thing being held off, and the toggle's accent fill is what says whether
/// it is on — which is how every other GNOME quick-settings toggle reads.
pub const CAFFEINE: &[&str] = &[
    "my-caffeine-on-symbolic",
    "preferences-desktop-screensaver-symbolic",
];

/// Bluetooth: a radio that is on, with nothing joined to it.
pub const BLUETOOTH: &str = "bluetooth-symbolic";
/// One with something joined to it.
pub const BLUETOOTH_ACTIVE: &str = "bluetooth-active-symbolic";
/// One that is switched off.
pub const BLUETOOTH_DISABLED: &str = "bluetooth-disabled-symbolic";

/// The icon for a Bluetooth adapter in a given state.
///
/// The *decision* is `BtState`'s, in the services crate where it is tested
/// without a display; the *names* are here, where every other icon name in the
/// panel lives, so a name Adwaita drops breaks in one file.
pub fn bluetooth(powered: bool, connected: bool) -> &'static str {
    match (powered, connected) {
        (false, _) => BLUETOOTH_DISABLED,
        (true, true) => BLUETOOTH_ACTIVE,
        (true, false) => BLUETOOTH,
    }
}

/// The picture beside one paired device.
pub fn bluetooth_device(kind: topbar_services::IconKind) -> &'static str {
    use topbar_services::IconKind;
    match kind {
        IconKind::Headphones => "audio-headphones-symbolic",
        IconKind::Headset => "audio-headset-symbolic",
        IconKind::Speaker => "audio-speakers-symbolic",
        IconKind::Keyboard => "input-keyboard-symbolic",
        IconKind::Mouse => "input-mouse-symbolic",
        IconKind::Gamepad => "input-gaming-symbolic",
        IconKind::Phone => "phone-symbolic",
        IconKind::Computer => "computer-symbolic",
        IconKind::Printer => "printer-symbolic",
        IconKind::Camera => "camera-photo-symbolic",
        IconKind::Display => "video-display-symbolic",
        // A Bluetooth logo beside a device BlueZ could not classify is
        // honest; a headset icon beside a heart-rate monitor is not.
        IconKind::Generic => BLUETOOTH,
    }
}
/// An Ethernet cable with something on the other end.
pub const WIRED: &str = "network-wired-symbolic";
/// A socket with nothing in it.
pub const WIRED_DISCONNECTED: &str = "network-wired-disconnected-symbolic";
/// A radio that is on but has joined nothing.
pub const WIFI_OFFLINE: &str = "network-wireless-offline-symbolic";
/// A radio that is switched off.
pub const WIFI_DISABLED: &str = "network-wireless-disabled-symbolic";
/// The padlock beside a network that wants a key.
pub const WIFI_LOCKED: &str = "network-wireless-encrypted-symbolic";
/// A tunnel that is up.
pub const VPN: &str = "network-vpn-symbolic";
/// One that is not.
pub const VPN_DISCONNECTED: &str = "network-vpn-disconnected-symbolic";

/// The five signal icons, weakest first.
///
/// Indexed by the bucket the network service computes, so the thresholds live
/// in one place — beside the rest of NetworkManager's constants — and the panel
/// only decides what to draw.
const WIFI_SIGNAL: [&str; 5] = [
    "network-wireless-signal-none-symbolic",
    "network-wireless-signal-weak-symbolic",
    "network-wireless-signal-ok-symbolic",
    "network-wireless-signal-good-symbolic",
    "network-wireless-signal-excellent-symbolic",
];

/// The signal icon for a strength bucket.
pub fn wifi_signal(bucket: u8) -> &'static str {
    WIFI_SIGNAL[(bucket as usize).min(WIFI_SIGNAL.len() - 1)]
}

/// There is something to install.
pub const UPDATES: &str = "software-update-available-symbolic";

/// Mail that has not been read.
///
/// The only mail state the panel ever draws: an inbox with nothing new in it
/// is an *absent* widget, not a read envelope, so `mail-read-symbolic` would
/// have nothing to say.
pub const MAIL_UNREAD: &str = "mail-unread-symbolic";

/// Something has arrived that has not been looked at yet.
///
/// A filled circle rather than a bell, which is what GNOME Shell's own date
/// menu draws for this and the only Adwaita name that is simply a dot. A second
/// bell beside the Do Not Disturb one would read as a second setting.
pub const UNSEEN_NOTIFICATIONS: &str = "media-record-symbolic";

/// A processor working hard, preferring the gauge GNOME extensions ship.
///
/// Adwaita has never carried a CPU icon. `speedometer-symbolic` comes with
/// several system-monitor extensions and is the one that reads as "load";
/// `utilities-system-monitor-symbolic` is what a desktop that ships GNOME's own
/// System Monitor has. The last is the Adwaita name that is always there.
pub const CPU: &[&str] = &[
    "speedometer-symbolic",
    "utilities-system-monitor-symbolic",
    "applications-system-symbolic",
];

/// Memory filling up.
///
/// Adwaita has no RAM icon either. The fallback is the flash-memory chip,
/// which is the closest thing in the set to "a module of memory".
pub const MEMORY: &[&str] = &["memory-symbolic", "ram-symbolic", "media-flash-symbolic"];

/// A filesystem filling up. Adwaita's own, so there is nothing to prefer.
pub const DISK: &[&str] = &["drive-harddisk-symbolic"];

/// The chevron that opens an expandable row.
pub const EXPAND: &str = "pan-down-symbolic";
/// The mark against the item in force — a checkmark, and also what a radio
/// row uses: a list where exactly one entry is ticked reads the same either
/// way, and one icon is one thing to keep.
pub const SELECTED: &str = "object-select-symbolic";
/// Select a wallpaper at random.
pub const WALLPAPER_RANDOM: &str = "media-playlist-shuffle-symbolic";

/// The volume icon for a level and a mute flag.
pub fn volume(percent: u32, muted: bool) -> &'static str {
    if muted || percent == 0 {
        return VOLUME_MUTED;
    }
    match percent {
        101.. => VOLUME_OVERDRIVEN,
        66..=100 => VOLUME_HIGH,
        33..=65 => VOLUME_MEDIUM,
        _ => VOLUME_LOW,
    }
}

/// The microphone icon for a level and a mute flag.
pub fn microphone(percent: u32, muted: bool) -> &'static str {
    if muted || percent == 0 {
        return MIC_MUTED;
    }
    match percent {
        66.. => MIC_HIGH,
        33..=65 => MIC_MEDIUM,
        _ => MIC_LOW,
    }
}

/// The first of `names` the icon theme actually has, or the last as a fallback.
///
/// A GTK display is needed to ask, so before one exists — in a unit test — the
/// last name wins, which is by construction the Adwaita one.
pub fn first_available(names: &[&'static str]) -> &'static str {
    let last = names.last().copied().unwrap_or(SHUT_DOWN);
    let Some(display) = gtk4::gdk::Display::default() else {
        return last;
    };
    let theme = gtk4::IconTheme::for_display(&display);
    names
        .iter()
        .find(|name| theme.has_icon(name))
        .copied()
        .unwrap_or(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_volume_table_covers_every_level() {
        assert_eq!(volume(0, false), VOLUME_MUTED);
        assert_eq!(volume(50, true), VOLUME_MUTED, "muted beats loud");
        assert_eq!(volume(1, false), VOLUME_LOW);
        assert_eq!(volume(32, false), VOLUME_LOW);
        assert_eq!(volume(33, false), VOLUME_MEDIUM);
        assert_eq!(volume(65, false), VOLUME_MEDIUM);
        assert_eq!(volume(66, false), VOLUME_HIGH);
        assert_eq!(volume(100, false), VOLUME_HIGH);
        assert_eq!(volume(140, false), VOLUME_OVERDRIVEN);
    }

    #[test]
    fn the_microphone_table_matches_it() {
        assert_eq!(microphone(0, false), MIC_MUTED);
        assert_eq!(microphone(80, true), MIC_MUTED);
        assert_eq!(microphone(10, false), MIC_LOW);
        assert_eq!(microphone(40, false), MIC_MEDIUM);
        assert_eq!(microphone(90, false), MIC_HIGH);
        // Overdrive has no microphone equivalent; loud is as loud as it gets.
        assert_eq!(microphone(150, false), MIC_HIGH);
    }

    #[test]
    fn every_name_is_a_symbolic_one() {
        let names = [
            MAIL_UNREAD,
            VOLUME_MUTED,
            VOLUME_LOW,
            VOLUME_MEDIUM,
            VOLUME_HIGH,
            VOLUME_OVERDRIVEN,
            MIC_MUTED,
            MIC_LOW,
            MIC_MEDIUM,
            MIC_HIGH,
            BRIGHTNESS,
            OUTPUT_DEVICE,
            LOCK,
            SHUT_DOWN,
            RESTART,
            LOG_OUT,
            BLUETOOTH,
            BLUETOOTH_ACTIVE,
            BLUETOOTH_DISABLED,
            WIRED,
            WIRED_DISCONNECTED,
            WIFI_OFFLINE,
            WIFI_DISABLED,
            WIFI_LOCKED,
            VPN,
            VPN_DISCONNECTED,
            EXPAND,
            SELECTED,
            WALLPAPER_RANDOM,
            UPDATES,
            UNSEEN_NOTIFICATIONS,
        ];
        for name in names
            .iter()
            .chain(SUSPEND)
            .chain(CAFFEINE)
            .chain(CPU)
            .chain(MEMORY)
            .chain(DISK)
            .chain(&WIFI_SIGNAL)
        {
            assert!(name.ends_with("-symbolic"), "{name} is not symbolic");
        }
    }

    #[test]
    fn every_preference_list_ends_in_a_name_adwaita_actually_ships() {
        // The last entry is what `first_available` falls back to, so it is the
        // one that must exist: asking for a name no theme has draws the
        // broken-image glyph, which is worse than the plainer icon.
        assert_eq!(CPU.last(), Some(&"applications-system-symbolic"));
        assert_eq!(MEMORY.last(), Some(&"media-flash-symbolic"));
        assert_eq!(DISK.last(), Some(&"drive-harddisk-symbolic"));
        assert_eq!(SUSPEND.last(), Some(&"weather-clear-night-symbolic"));
        assert_eq!(
            CAFFEINE.last(),
            Some(&"preferences-desktop-screensaver-symbolic")
        );
    }

    #[test]
    fn the_five_signal_icons_run_weakest_to_strongest() {
        assert_eq!(wifi_signal(0), "network-wireless-signal-none-symbolic");
        assert_eq!(wifi_signal(1), "network-wireless-signal-weak-symbolic");
        assert_eq!(wifi_signal(2), "network-wireless-signal-ok-symbolic");
        assert_eq!(wifi_signal(3), "network-wireless-signal-good-symbolic");
        assert_eq!(wifi_signal(4), "network-wireless-signal-excellent-symbolic");
        // A bucket from a future service is clamped rather than a panic.
        assert_eq!(wifi_signal(9), wifi_signal(4));
    }

    #[test]
    fn a_preference_falls_back_to_the_adwaita_name_without_a_display() {
        // No GTK display in a unit test, so the fallback — the last entry — is
        // what comes out, and it is the one Adwaita is known to carry.
        assert_eq!(first_available(SUSPEND), "weather-clear-night-symbolic");
        assert_eq!(
            first_available(CAFFEINE),
            "preferences-desktop-screensaver-symbolic"
        );
    }
}
