//! What the panel and its button show, worked out without any GTK.
//!
//! The bar button's icon order, which of those icons are drawn right now, the
//! order of the toggle grid and how it wraps into rows, the bounds a slider is
//! allowed to move between, and which device in a list is the one in use.
//! Every one of these is a decision the eye notices when it is wrong, and
//! every one of them is a pure function here so it can be checked without a
//! display.

use topbar_services::{AudioState, BatteryState, BtState, NetworkState};

use crate::style::icons;

/// One status icon on the Quick Settings button.
///
/// The order of this enum is not the order they are drawn in — see [`ORDER`],
/// which is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    /// Wi-Fi or wired.
    Network,
    /// A VPN badge.
    Vpn,
    /// The output device's volume level. Always drawn.
    Audio,
    /// Bluetooth, when something is connected over it.
    Bluetooth,
    /// The battery, when there is one.
    Battery,
    /// A dot saying something is listening.
    Microphone,
    /// A dot saying something is watching the screen.
    ScreenShare,
}

/// Left to right, the order the button draws its icons in.
///
/// Fixed rather than derived: the icons that come and go must not make the
/// others move about. Every slot was written out in full before there was
/// anything to put in half of them, which is why M9b and M9c each added an icon
/// rather than a layout.
pub const ORDER: &[Indicator] = &[
    Indicator::Network,
    Indicator::Vpn,
    Indicator::Audio,
    Indicator::Bluetooth,
    Indicator::Battery,
    Indicator::Microphone,
    Indicator::ScreenShare,
];

/// Which icon the network indicator is showing, if any.
///
/// Wired beats Wi-Fi, because a machine with a cable in it is using the cable
/// whatever the radio is doing, and that is the fact the icon is there to
/// state. `None` is a machine with no network hardware at all — a container, a
/// virtual machine with its interfaces unmanaged — where an icon would be
/// reporting an absence nobody asked about.
pub fn network_icon(network: &NetworkState) -> Option<&'static str> {
    if !network.has_device() {
        return None;
    }
    if network.wired.connected {
        return Some(icons::WIRED);
    }
    if let Some(active) = &network.wifi.active {
        return Some(icons::wifi_signal(active.bucket));
    }
    if network.wifi.present {
        return Some(if network.wifi.enabled {
            icons::WIFI_OFFLINE
        } else {
            icons::WIFI_DISABLED
        });
    }
    Some(icons::WIRED_DISCONNECTED)
}

/// What the button has to go on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Indicators {
    /// Whether a microphone is in use right now.
    pub microphone: bool,
    /// Whether there is a battery to draw.
    pub battery: bool,
    /// The network icon, when there is any network hardware.
    pub network: Option<&'static str>,
    /// Whether a VPN is up.
    pub vpn: bool,
    /// Whether anything is connected over Bluetooth.
    pub bluetooth: bool,
    /// Whether something is watching the screen.
    pub screen_share: bool,
}

impl Indicators {
    /// Read the services the button subscribes to.
    pub fn read(
        audio: &AudioState,
        battery: &BatteryState,
        network: &NetworkState,
        bluetooth: &BtState,
        screen_share: bool,
        show_battery: bool,
    ) -> Self {
        Self {
            microphone: audio.source_in_use,
            battery: show_battery && battery.available,
            network: network_icon(network),
            vpn: network.vpn_active(),
            // Whether something is *connected*, not whether the radio is on:
            // an adapter that is powered and idle is the state most laptops
            // sit in all day, and an icon for it would be permanent furniture.
            bluetooth: bluetooth.indicated(),
            screen_share,
        }
    }

    /// Whether `indicator` is drawn right now.
    ///
    /// The audio icon is always there, because a panel button with nothing on
    /// it is a panel button nobody finds. Everything else earns its place.
    pub fn shows(self, indicator: Indicator) -> bool {
        match indicator {
            Indicator::Audio => true,
            Indicator::Battery => self.battery,
            Indicator::Microphone => self.microphone,
            Indicator::Network => self.network.is_some(),
            Indicator::Vpn => self.vpn,
            Indicator::Bluetooth => self.bluetooth,
            Indicator::ScreenShare => self.screen_share,
        }
    }

    /// The icons on the button, in the order they are drawn.
    pub fn visible(self) -> Vec<Indicator> {
        ORDER
            .iter()
            .copied()
            .filter(|indicator| self.shows(*indicator))
            .collect()
    }
}

/// One pill in the toggle grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Wi-Fi.
    WiFi,
    /// Bluetooth.
    Bluetooth,
    /// VPN.
    Vpn,
    /// The idle inhibitor.
    Caffeine,
    /// The power-profiles daemon.
    PowerMode,
}

/// The order pills appear in, whichever of them exist.
///
/// Written out in full before three of them existed, so each one arrived in the
/// place the user had already learned rather than pushing the others along.
pub const GRID_ORDER: &[Toggle] = &[
    Toggle::WiFi,
    Toggle::Bluetooth,
    Toggle::Vpn,
    Toggle::Caffeine,
    Toggle::PowerMode,
];

/// How many pills fit across the panel.
pub const COLUMNS: usize = 2;

/// Sort `present` into grid order and cut it into rows.
///
/// The last row may be short; the grid pads it so a lone pill is half the
/// width of the panel rather than all of it, which is what a homogeneous row
/// does on its own.
pub fn grid_rows(present: &[Toggle]) -> Vec<Vec<Toggle>> {
    let ordered: Vec<Toggle> = GRID_ORDER
        .iter()
        .copied()
        .filter(|toggle| present.contains(toggle))
        .collect();
    ordered.chunks(COLUMNS).map(<[Toggle]>::to_vec).collect()
}

/// The highest volume the output slider may be dragged to.
///
/// `max_volume_pct` comes from the sound server and honours
/// `[audio] allow_overdrive`; a slider whose range disagreed with the service's
/// ceiling would either refuse the last few percent or silently clamp.
pub fn slider_ceiling(audio: &AudioState) -> f64 {
    f64::from(audio.max_volume_pct.max(1))
}

/// A value clamped to what a slider may be set to.
pub fn clamp_volume(percent: u32, audio: &AudioState) -> u32 {
    percent.min(audio.max_volume_pct.max(1))
}

/// A scroll step, from `[widgets.quick_settings] audio_scroll_percentage`.
///
/// Configuration validation already rejects anything outside 1–25, so this
/// only covers a `Config` built by hand — but a step of zero would make the
/// wheel do nothing at all, which reads as a broken widget rather than as a
/// misconfiguration.
pub fn scroll_step(configured: u32) -> u32 {
    configured.clamp(1, 25)
}

/// Which entry of a device list is the one in use.
///
/// PulseAudio's own flag rather than a name comparison: descriptions are not
/// unique — two identical headsets are two identical strings — and the default
/// sink is an identity, not a label.
pub fn selected_device<'a>(
    devices: impl IntoIterator<Item = &'a topbar_services::DeviceView>,
) -> Option<usize> {
    devices.into_iter().position(|device| device.is_default)
}

/// The devices worth offering, in the order they were given.
///
/// A device whose port says it is unplugged is left out: offering to send
/// audio to a headphone socket with nothing in it is offering silence.
pub fn choosable_devices(
    devices: &[topbar_services::DeviceView],
) -> impl Iterator<Item = &topbar_services::DeviceView> + Clone {
    devices
        .iter()
        .filter(|device| device.port_available != Some(false))
}

/// Whether the output chooser is worth showing at all.
pub fn wants_chooser(audio: &AudioState) -> bool {
    choosable_devices(&audio.sinks).nth(1).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use topbar_services::{BatteryStatus, DeviceView};

    /// Read every indicator with no Bluetooth and no screen share.
    fn read_all(audio: &AudioState, battery: &BatteryState, network: &NetworkState) -> Indicators {
        Indicators::read(
            audio,
            battery,
            network,
            &topbar_services::BtState::default(),
            false,
            true,
        )
    }

    fn device(id: &str, is_default: bool, port_available: Option<bool>) -> DeviceView {
        DeviceView {
            id: id.to_string(),
            description: id.to_string(),
            is_default,
            port_available,
        }
    }

    #[test]
    fn the_icon_order_is_the_one_the_panel_promises() {
        assert_eq!(
            ORDER,
            &[
                Indicator::Network,
                Indicator::Vpn,
                Indicator::Audio,
                Indicator::Bluetooth,
                Indicator::Battery,
                Indicator::Microphone,
                Indicator::ScreenShare,
            ]
        );
    }

    #[test]
    fn the_audio_icon_is_always_there_and_nothing_else_is_by_default() {
        let nothing = Indicators::default();
        assert_eq!(nothing.visible(), vec![Indicator::Audio]);
        for quiet in [
            Indicator::Bluetooth,
            Indicator::ScreenShare,
            Indicator::Battery,
            Indicator::Microphone,
            Indicator::Network,
            Indicator::Vpn,
        ] {
            assert!(!nothing.shows(quiet), "{quiet:?} has nothing to report");
        }
    }

    #[test]
    fn every_indicator_at_once_is_drawn_in_the_published_order() {
        let everything = Indicators {
            microphone: true,
            battery: true,
            network: Some(icons::WIRED),
            vpn: true,
            bluetooth: true,
            screen_share: true,
        };
        assert_eq!(everything.visible(), ORDER.to_vec());
    }

    #[test]
    fn bluetooth_reports_what_is_connected_rather_than_whether_the_radio_is_on() {
        let read = |bluetooth: &topbar_services::BtState| {
            Indicators::read(
                &AudioState::default(),
                &BatteryState::default(),
                &NetworkState::default(),
                bluetooth,
                false,
                true,
            )
        };

        // An adapter that is powered and idle is where most laptops sit all
        // day; an icon for it would be permanent furniture.
        let idle = topbar_services::BtState {
            available: true,
            powered: true,
            ..topbar_services::BtState::default()
        };
        assert!(!read(&idle).shows(Indicator::Bluetooth));

        let joined = topbar_services::BtState {
            devices: vec![topbar_services::BtDevice {
                path: "/org/bluez/hci0/dev_AA".into(),
                alias: "WH-1000XM4".into(),
                icon: topbar_services::IconKind::Headset,
                connected: true,
                paired: true,
                battery_pct: Some(85),
                pending: false,
            }],
            ..idle.clone()
        };
        assert!(read(&joined).shows(Indicator::Bluetooth));

        // The radio going off takes the icon with it, whatever stale flags the
        // device rows are still carrying.
        let off = topbar_services::BtState {
            powered: false,
            ..joined
        };
        assert!(!read(&off).shows(Indicator::Bluetooth));
    }

    #[test]
    fn a_shared_screen_raises_its_dot_beside_the_microphone_one() {
        let watched = Indicators::read(
            &AudioState {
                source_in_use: true,
                ..AudioState::default()
            },
            &BatteryState::default(),
            &NetworkState::default(),
            &topbar_services::BtState::default(),
            true,
            true,
        );
        assert_eq!(
            watched.visible(),
            vec![
                Indicator::Audio,
                Indicator::Microphone,
                Indicator::ScreenShare
            ],
            "the microphone dot comes first, and the screen-share dot last"
        );
    }

    /// A network state with one wireless card and nothing else.
    fn wifi_only(active: Option<u8>, enabled: bool) -> NetworkState {
        NetworkState {
            wifi: topbar_services::WifiState {
                present: true,
                enabled,
                active: active.map(|bucket| topbar_services::ApView {
                    ssid: "Home".into(),
                    strength: bucket * 25,
                    bucket,
                    secured: true,
                    known: true,
                    active: true,
                    connecting: false,
                }),
                ..topbar_services::WifiState::default()
            },
            ..NetworkState::default()
        }
    }

    #[test]
    fn the_network_icon_is_chosen_by_what_the_machine_is_actually_using() {
        // No hardware at all: nothing to say, so nothing is drawn.
        assert_eq!(network_icon(&NetworkState::default()), None);

        // A cable beats the radio, whatever the radio is doing.
        let both = NetworkState {
            wired: topbar_services::WiredState {
                present: true,
                carrier: true,
                connected: true,
                ..topbar_services::WiredState::default()
            },
            ..wifi_only(Some(4), true)
        };
        assert_eq!(network_icon(&both), Some("network-wired-symbolic"));

        // On a network: the strength bucket, which is where the icon comes from.
        assert_eq!(
            network_icon(&wifi_only(Some(4), true)),
            Some("network-wireless-signal-excellent-symbolic")
        );
        assert_eq!(
            network_icon(&wifi_only(Some(1), true)),
            Some("network-wireless-signal-weak-symbolic")
        );

        // Radio on, joined nothing.
        assert_eq!(
            network_icon(&wifi_only(None, true)),
            Some("network-wireless-offline-symbolic")
        );
        // Radio off — a different picture, because it is a different problem.
        assert_eq!(
            network_icon(&wifi_only(None, false)),
            Some("network-wireless-disabled-symbolic")
        );

        // A desktop with an unplugged socket and no card.
        let unplugged = NetworkState {
            wired: topbar_services::WiredState {
                present: true,
                ..topbar_services::WiredState::default()
            },
            ..NetworkState::default()
        };
        assert_eq!(
            network_icon(&unplugged),
            Some("network-wired-disconnected-symbolic")
        );
    }

    #[test]
    fn a_tunnel_puts_its_badge_beside_the_network_icon() {
        let audio = AudioState::default();
        let battery = BatteryState::default();
        let mut network = wifi_only(Some(4), true);
        network.vpn = vec![topbar_services::VpnView {
            id: "Work".into(),
            uuid: "w".into(),
            kind: topbar_services::VpnKind::WireGuard,
            active: true,
            pending: false,
        }];

        let indicators = read_all(&audio, &battery, &network);
        assert_eq!(
            indicators.visible(),
            vec![Indicator::Network, Indicator::Vpn, Indicator::Audio],
            "the network leads the pill and the badge sits beside it"
        );

        network.vpn[0].active = false;
        let down = read_all(&audio, &battery, &network);
        assert!(
            !down.shows(Indicator::Vpn),
            "a tunnel that is down is quiet"
        );
    }

    #[test]
    fn the_battery_comes_before_the_microphone_dot() {
        let both = Indicators {
            microphone: true,
            battery: true,
            ..Indicators::default()
        };
        assert_eq!(
            both.visible(),
            vec![Indicator::Audio, Indicator::Battery, Indicator::Microphone]
        );
    }

    #[test]
    fn a_desktop_draws_no_battery() {
        let audio = AudioState::default();
        let battery = BatteryState::default();
        let indicators = read_all(&audio, &battery, &NetworkState::default());
        assert!(!indicators.shows(Indicator::Battery));
    }

    #[test]
    fn a_battery_switched_off_in_the_config_is_not_drawn_either() {
        let audio = AudioState::default();
        let battery = BatteryState {
            available: true,
            percent: Some(50.0),
            status: BatteryStatus::Discharging,
            ..BatteryState::default()
        };
        let none = NetworkState::default();
        assert!(read_all(&audio, &battery, &none).shows(Indicator::Battery));
        let hidden = Indicators::read(
            &audio,
            &battery,
            &none,
            &topbar_services::BtState::default(),
            false,
            false,
        );
        assert!(!hidden.shows(Indicator::Battery));
    }

    #[test]
    fn a_microphone_in_use_raises_the_dot() {
        let audio = AudioState {
            source_in_use: true,
            ..AudioState::default()
        };
        let indicators = read_all(&audio, &BatteryState::default(), &NetworkState::default());
        assert!(indicators.shows(Indicator::Microphone));
    }

    #[test]
    fn the_grid_keeps_its_order_whatever_order_it_is_given() {
        let rows = grid_rows(&[Toggle::PowerMode, Toggle::Caffeine]);
        assert_eq!(rows, vec![vec![Toggle::Caffeine, Toggle::PowerMode]]);
    }

    #[test]
    fn the_grid_wraps_at_two_and_the_last_row_may_be_short() {
        let rows = grid_rows(&[
            Toggle::WiFi,
            Toggle::Bluetooth,
            Toggle::Vpn,
            Toggle::Caffeine,
            Toggle::PowerMode,
        ]);
        assert_eq!(
            rows,
            vec![
                vec![Toggle::WiFi, Toggle::Bluetooth],
                vec![Toggle::Vpn, Toggle::Caffeine],
                vec![Toggle::PowerMode],
            ]
        );
    }

    #[test]
    fn adding_the_later_toggles_does_not_move_the_ones_that_exist_now() {
        // Caffeine and Power Mode are adjacent today...
        let today = grid_rows(&[Toggle::Caffeine, Toggle::PowerMode]);
        assert_eq!(today[0], vec![Toggle::Caffeine, Toggle::PowerMode]);
        // ...and still adjacent, in the same order, with all five present.
        let later = grid_rows(GRID_ORDER);
        let flat: Vec<Toggle> = later.into_iter().flatten().collect();
        let caffeine = flat
            .iter()
            .position(|toggle| *toggle == Toggle::Caffeine)
            .expect("caffeine");
        assert_eq!(flat[caffeine + 1], Toggle::PowerMode);
    }

    #[test]
    fn an_empty_grid_has_no_rows() {
        assert!(grid_rows(&[]).is_empty());
    }

    #[test]
    fn the_slider_stops_where_the_sound_server_stops() {
        let plain = AudioState {
            max_volume_pct: 100,
            ..AudioState::default()
        };
        assert!((slider_ceiling(&plain) - 100.0).abs() < f64::EPSILON);
        assert_eq!(clamp_volume(150, &plain), 100);

        let overdriven = AudioState {
            max_volume_pct: 150,
            ..AudioState::default()
        };
        assert!((slider_ceiling(&overdriven) - 150.0).abs() < f64::EPSILON);
        assert_eq!(clamp_volume(150, &overdriven), 150);
        assert_eq!(clamp_volume(200, &overdriven), 150);
    }

    #[test]
    fn a_ceiling_of_zero_would_be_a_slider_with_no_range() {
        let broken = AudioState {
            max_volume_pct: 0,
            ..AudioState::default()
        };
        assert!(slider_ceiling(&broken) >= 1.0);
        assert_eq!(clamp_volume(50, &broken), 1);
    }

    #[test]
    fn the_scroll_step_stays_inside_the_range_the_config_documents() {
        assert_eq!(scroll_step(5), 5);
        assert_eq!(scroll_step(0), 1);
        assert_eq!(scroll_step(99), 25);
        assert_eq!(scroll_step(25), 25);
    }

    #[test]
    fn the_device_in_use_is_found_by_its_flag_not_its_name() {
        let devices = [
            device("hdmi", false, None),
            device("analog", true, None),
            device("usb", false, None),
        ];
        assert_eq!(selected_device(&devices), Some(1));
        assert_eq!(selected_device(&[]), None);
    }

    #[test]
    fn a_socket_with_nothing_plugged_into_it_is_not_offered() {
        let devices = [
            device("speakers", true, Some(true)),
            device("headphones", false, Some(false)),
            device("hdmi", false, None),
        ];
        let offered: Vec<&str> = choosable_devices(&devices)
            .map(|device| device.id.as_str())
            .collect();
        assert_eq!(
            offered,
            ["speakers", "hdmi"],
            "a device with no jack detection is not an unplugged one"
        );
    }

    #[test]
    fn the_chooser_appears_only_where_there_is_a_choice() {
        let one = AudioState {
            sinks: vec![device("speakers", true, None)],
            ..AudioState::default()
        };
        assert!(!wants_chooser(&one));

        let two = AudioState {
            sinks: vec![device("speakers", true, None), device("hdmi", false, None)],
            ..AudioState::default()
        };
        assert!(wants_chooser(&two));

        let one_unplugged = AudioState {
            sinks: vec![
                device("speakers", true, None),
                device("headphones", false, Some(false)),
            ],
            ..AudioState::default()
        };
        assert!(
            !wants_chooser(&one_unplugged),
            "an unplugged socket is not a second choice"
        );
    }
}
