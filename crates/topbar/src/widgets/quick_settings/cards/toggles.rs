//! The toggle grid: two columns of pills, some of which expand.
//!
//! ```text
//! ┌──────────────────┬──────────────────┐
//! │ 📶 Usadba     ⌄ │ ᛒ WH-1000XM4 ⌄ │
//! ├──────────────────┼──────────────────┤
//! │ 🔒 VPN        ⌄ │ ☕ Caffeine      │
//! ├──────────────────┼──────────────────┤
//! │ ⚡ Balanced   ⌄ │                  │
//! └──────────────────┴──────────────────┘
//!   ○ Power Saver                          ← the expanded section, full width
//!   ● Balanced
//!   ○ Performance
//! ```
//!
//! The grid is built from a list, not from a hand-written layout: one pill is
//! one entry in [`model::GRID_ORDER`] plus a [`Pill`]. The wrapping, the
//! ordering and the short last row are [`model::grid_rows`]'s problem and are
//! tested there.
//!
//! Which pills *exist* is decided by the configuration, at build time; which
//! of them are *visible* is decided by the machine, from state. Rebuilding the
//! grid when NetworkManager first answered would move every pill under the
//! pointer, which is the one thing a row of controls must never do.
//!
//! An expandable pill's section is appended after the *row* it is in rather
//! than inside the grid, so it spans the panel: a list of radio rows squeezed
//! into half the width would be unreadable.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Button, Image, Label, Orientation};
use topbar_services::{BtState, InhibitorState, NetworkState, PowerProfilesState, Services};

use crate::anim::{Animation, AnimationParams, Easing, RotateBox, ripple};
use crate::bridge::{self, BindingGuard};
use crate::style::{classes, icons};
use crate::surfaces::inline::{self, names};
use crate::surfaces::search::ChoiceFilter;
use crate::widgets::expander::{Accordion, REVEAL_MS, Section};
use crate::widgets::quick_settings::cards::bluetooth::BluetoothList;
use crate::widgets::quick_settings::cards::network::{self, VpnList, WifiList};
use crate::widgets::quick_settings::model::{self, Toggle};
use crate::widgets::quick_settings::{attempt, set_icon, set_text};

/// Gap between the two columns, and between rows.
const GAP: i32 = 8;
/// What the Power Mode pill shows before the daemon has answered.
const POWER_MODE_ICON: &str = "power-profile-balanced-symbolic";
/// Half a turn, which is what takes the chevron from pointing down to up.
const CHEVRON_TURN: f32 = 180.0;

/// One pill: an icon, a label, a subtitle, and optionally a chevron.
///
/// The two halves are **siblings** inside `root`, and they have to be. GTK's
/// own `GtkButton` runs its click gesture in the *capture* phase and claims the
/// sequence when the button comes back up; claiming cancels every gesture in
/// every widget below the claimant, so a `GtkButton` nested inside another
/// `GtkButton` never sees the release and never emits `clicked` — the outer one
/// swallows the whole click. The chevron was inside the body button until the
/// first time anyone clicked one on a real desktop: the arrow did nothing and
/// the *radio* toggled instead, which is exactly the outer button firing.
///
/// So `root` is the pill — it wears the shape and the accent fill — and the
/// body and the chevron sit in it side by side, each with its own hover.
struct Pill {
    root: gtk4::Box,
    button: Button,
    icon: Image,
    label: Label,
    subtitle: Label,
    expand: Option<Button>,
    /// The chevron, and the run that turns it.
    chevron: Option<RotateBox>,
    rotation: Option<Animation>,
}

impl Pill {
    /// Build a pill. `expandable` gives it a chevron.
    fn new(icon_name: &str, title: &str, expandable: bool) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Horizontal, 0);
        root.add_css_class(classes::QS_TOGGLE_PILL);
        root.set_hexpand(true);

        let button = Button::new();
        button.add_css_class(classes::QS_TOGGLE);
        button.set_hexpand(true);

        let content = gtk4::Box::new(Orientation::Horizontal, 0);
        content.set_valign(Align::Center);

        let icon = Image::from_icon_name(icon_name);
        icon.add_css_class(classes::QS_TOGGLE_ICON);
        content.append(&icon);

        let text = gtk4::Box::new(Orientation::Vertical, 0);
        text.set_valign(Align::Center);
        text.set_hexpand(true);

        let label = Label::new(Some(title));
        label.add_css_class(classes::QS_TOGGLE_LABEL);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        text.append(&label);

        let subtitle = Label::new(None);
        subtitle.add_css_class(classes::QS_TOGGLE_SUBTITLE);
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        subtitle.set_visible(false);
        text.append(&subtitle);

        content.append(&text);
        button.set_child(Some(&content));
        ripple::install(&button);
        root.append(&button);

        // One arrow that turns, rather than two that swap: the chevron is the
        // handle on the section, so it moves with it.
        let chevron = expandable.then(|| {
            let chevron = RotateBox::new();
            chevron.set_child(&Image::from_icon_name(icons::EXPAND));
            chevron
        });
        let expand = chevron.as_ref().map(|chevron| {
            let expand = Button::new();
            expand.add_css_class(classes::QS_TOGGLE_EXPAND);
            expand.set_child(Some(chevron));
            ripple::install(&expand);
            expand.set_valign(Align::Center);
            // Beside the body, never inside it: see the note on `Pill`.
            button.add_css_class(classes::QS_TOGGLE_SPLIT);
            root.append(&expand);
            expand
        });
        let rotation = chevron.as_ref().map(Animation::new);

        Rc::new(Self {
            root,
            button,
            icon,
            label,
            subtitle,
            expand,
            chevron,
            rotation,
        })
    }

    /// Wear the accent fill, or take it off.
    ///
    /// On the shell rather than on the body, so the fill runs under the chevron
    /// too and the pill still reads as one control.
    fn set_checked(&self, checked: bool) {
        if checked {
            self.root.add_css_class(classes::CHECKED);
        } else {
            self.root.remove_css_class(classes::CHECKED);
        }
    }

    /// Change the pill's title.
    fn set_title(&self, title: &str) {
        set_text(&self.label, title);
    }

    /// Put a second line under the title, or take it away.
    fn set_subtitle(&self, text: Option<&str>) {
        match text {
            Some(text) if !text.is_empty() => {
                set_text(&self.subtitle, text);
                self.subtitle.set_visible(true);
            }
            _ => self.subtitle.set_visible(false),
        }
    }

    /// Show or hide the chevron, taking the body's right padding with it.
    ///
    /// A pill whose chevron has gone has to look like a pill that never had
    /// one: the body owns the padding again, or its label runs into the edge.
    fn set_chevron_visible(&self, visible: bool) {
        let Some(expand) = &self.expand else {
            return;
        };
        expand.set_visible(visible);
        if visible {
            self.button.add_css_class(classes::QS_TOGGLE_SPLIT);
        } else {
            self.button.remove_css_class(classes::QS_TOGGLE_SPLIT);
        }
    }

    /// Turn the chevron the way the section is going.
    ///
    /// Half a turn over exactly as long as the section takes, from wherever the
    /// arrow currently is: a section closed halfway through opening turns its
    /// arrow back from there rather than snapping to upright first.
    fn set_expanded(&self, expanded: bool) {
        let (Some(chevron), Some(rotation)) = (&self.chevron, &self.rotation) else {
            return;
        };
        let start = chevron.angle();
        let target = if expanded { CHEVRON_TURN } else { 0.0 };
        let distance = f64::from((target - start).abs()) / f64::from(CHEVRON_TURN);
        let duration = (REVEAL_MS as f64 * distance).round() as u64;

        let chevron = chevron.clone();
        rotation.start(
            AnimationParams::new(duration).with_easing(if expanded {
                Easing::EaseOutCubic
            } else {
                Easing::EaseInCubic
            }),
            Box::new(move |progress| chevron.set_angle(start + (target - start) * progress as f32)),
            None,
        );
    }
}

/// The grid, and everything keeping it alive.
pub struct Toggles {
    root: gtk4::Box,
    wifi: Option<Rc<Pill>>,
    wifi_section: Option<Rc<Section>>,
    wifi_list: Rc<WifiList>,
    bluetooth: Option<Rc<Pill>>,
    bluetooth_section: Option<Rc<Section>>,
    bluetooth_list: Rc<BluetoothList>,
    vpn: Option<Rc<Pill>>,
    vpn_section: Option<Rc<Section>>,
    vpn_list: Rc<VpnList>,
    caffeine: Option<Rc<Pill>>,
    power_mode: Option<Rc<Pill>>,
    power_mode_section: Option<Rc<Section>>,
    power_mode_list: gtk4::Box,
    profile_filter: Rc<ChoiceFilter>,
    /// The profiles the list was last built from, so it is not rebuilt on
    /// every percentage change the daemon happens to publish alongside.
    built_profiles: RefCell<Vec<String>>,
    /// Each radio row's checkmark, beside the profile it belongs to.
    profile_marks: RefCell<Vec<(String, Image)>>,
    /// Whether the Bluetooth service has been asked to look for devices.
    ///
    /// Held here rather than read back out of the snapshot so the session is
    /// opened and closed exactly once per transition: every call goes through
    /// [`attempt`], which clears the inline caption first, and a repeat would
    /// wipe the error explaining why the last one failed.
    discovering: std::cell::Cell<bool>,
    services: Services,
    _slots: Vec<inline::InlineSlot>,
    bindings: RefCell<Vec<BindingGuard>>,
}

impl Toggles {
    /// Build the grid.
    ///
    /// `show_caffeine` is `[widgets.quick_settings] idle_inhibitor`; Power
    /// Mode has no switch of its own because it is hidden exactly when the
    /// daemon is absent, which is a fact rather than a preference.
    pub fn new(
        services: &Services,
        accordion: &Rc<Accordion>,
        show_caffeine: bool,
        show_network: bool,
        show_bluetooth: bool,
        show_vpn: bool,
        vpn_close_on_connect: bool,
    ) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, GAP);
        root.add_css_class(classes::QS_GRID);

        // Wi-Fi and VPN are built whenever the configuration asks for them and
        // *hidden* when the machine turns out to have no card or no profiles.
        // Rebuilding the grid from state instead would move every pill under
        // the pointer the first time NetworkManager answered.
        let wifi = show_network.then(|| Pill::new(icons::WIFI_OFFLINE, "Wi-Fi", true));
        let wifi_list = WifiList::new(services);
        let wifi_section = Section::new(wifi_list.root());
        accordion.add(&wifi_section);

        let bluetooth = show_bluetooth.then(|| Pill::new(icons::BLUETOOTH, "Bluetooth", true));
        let bluetooth_list = BluetoothList::new(services);
        let bluetooth_section = Section::new(bluetooth_list.root());
        accordion.add(&bluetooth_section);

        let vpn = show_vpn.then(|| Pill::new(icons::VPN_DISCONNECTED, "VPN", true));
        let vpn_list = VpnList::new(services, vpn_close_on_connect);
        let vpn_section = Section::new(vpn_list.root());
        accordion.add(&vpn_section);

        let caffeine = show_caffeine
            .then(|| Pill::new(icons::first_available(icons::CAFFEINE), "Caffeine", false));
        // The icon is replaced from state on the first render; the balanced
        // one stands in until the daemon has said which profile is in force.
        let power_mode = Some(Pill::new(POWER_MODE_ICON, "Power Mode", true));

        let power_mode_scope = gtk4::Box::new(Orientation::Vertical, 2);
        power_mode_scope.add_css_class(classes::QS_DEVICE_LIST);
        let profile_filter = ChoiceFilter::new(&power_mode_scope, "Filter power profiles");
        let power_mode_list = gtk4::Box::new(Orientation::Vertical, 2);
        power_mode_scope.append(&power_mode_list);
        let power_mode_section = Section::new(&power_mode_scope);
        accordion.add(&power_mode_section);

        let (wifi_error, wifi_slot) = inline::slot(names::WIFI);
        let (bluetooth_error, bluetooth_slot) = inline::slot(names::BLUETOOTH);
        let (vpn_error, vpn_slot) = inline::slot(names::VPN);
        let (caffeine_error, caffeine_slot) = inline::slot(names::CAFFEINE);
        let (power_error, power_slot) = inline::slot(names::POWER_MODE);

        let present: Vec<Toggle> = [
            wifi.as_ref().map(|_| Toggle::WiFi),
            bluetooth.as_ref().map(|_| Toggle::Bluetooth),
            vpn.as_ref().map(|_| Toggle::Vpn),
            caffeine.as_ref().map(|_| Toggle::Caffeine),
            Some(Toggle::PowerMode),
        ]
        .into_iter()
        .flatten()
        .collect();

        for row in model::grid_rows(&present) {
            let line = gtk4::Box::new(Orientation::Horizontal, GAP);
            line.add_css_class(classes::QS_GRID_ROW);
            line.set_homogeneous(true);

            // A section belongs under the *row* its pill is in rather than
            // inside the grid, so a list of networks spans the panel instead of
            // being squeezed into half its width.
            let mut below: Vec<(&gtk4::Label, Option<&Rc<Section>>)> = Vec::new();
            for toggle in &row {
                match toggle {
                    Toggle::WiFi => {
                        if let Some(pill) = &wifi {
                            line.append(&pill.root);
                            below.push((&wifi_error, Some(&wifi_section)));
                        }
                    }
                    Toggle::Vpn => {
                        if let Some(pill) = &vpn {
                            line.append(&pill.root);
                            below.push((&vpn_error, Some(&vpn_section)));
                        }
                    }
                    // Caffeine is a plain switch: it has a caption for failures
                    // and nothing to expand.
                    Toggle::Caffeine => {
                        if let Some(pill) = &caffeine {
                            line.append(&pill.root);
                            below.push((&caffeine_error, None));
                        }
                    }
                    Toggle::PowerMode => {
                        if let Some(pill) = &power_mode {
                            line.append(&pill.root);
                            below.push((&power_error, Some(&power_mode_section)));
                        }
                    }
                    Toggle::Bluetooth => {
                        if let Some(pill) = &bluetooth {
                            line.append(&pill.root);
                            below.push((&bluetooth_error, Some(&bluetooth_section)));
                        }
                    }
                }
            }
            // A lone pill on the last row keeps its column width rather than
            // stretching across the panel and looking like a different control.
            for _ in row.len()..model::COLUMNS {
                let filler = gtk4::Box::new(Orientation::Horizontal, 0);
                filler.set_hexpand(true);
                line.append(&filler);
            }

            root.append(&line);
            for (error, section) in below {
                root.append(error);
                if let Some(section) = section {
                    root.append(section.root());
                }
            }
        }

        let toggles = Rc::new(Self {
            root,
            wifi,
            wifi_section: Some(wifi_section),
            wifi_list,
            bluetooth,
            bluetooth_section: Some(bluetooth_section),
            bluetooth_list,
            vpn,
            vpn_section: Some(vpn_section),
            vpn_list,
            caffeine,
            power_mode,
            power_mode_section: Some(power_mode_section),
            power_mode_list,
            profile_filter,
            built_profiles: RefCell::new(Vec::new()),
            profile_marks: RefCell::new(Vec::new()),
            discovering: std::cell::Cell::new(false),
            services: services.clone(),
            _slots: vec![
                wifi_slot,
                bluetooth_slot,
                vpn_slot,
                caffeine_slot,
                power_slot,
            ],
            bindings: RefCell::new(Vec::new()),
        });

        Self::wire(&toggles, accordion);
        toggles
    }

    /// The widget to put in the panel.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Re-render from current state.
    ///
    /// Opening the panel also asks the card to look around. The service
    /// rate-limits that to one scan every ten seconds, so opening Quick
    /// Settings four times in a row does not make the radio transmit four
    /// times — and a build with no bus of its own does not scan at all.
    pub fn refresh(self: &Rc<Self>) {
        self.render_inhibitor(&self.services.inhibitor.current());
        self.render_profiles(&self.services.power_profiles.current());
        self.render_network(&self.services.network.current());
        self.render_bluetooth(&self.services.bluetooth.current());

        if self.wifi.is_some() {
            let network = self.services.network.handle().clone();
            attempt(names::WIFI, async move { network.scan().await });
        }
    }

    /// Open the Wi-Fi list, without a pointer. Debug builds only.
    #[cfg(debug_assertions)]
    pub fn expand_wifi(self: &Rc<Self>) {
        if let Some(section) = &self.wifi_section {
            section.set_expanded(true);
            self.sync_chevrons();
        }
    }

    /// Open the Bluetooth device list, without a pointer. Debug builds only.
    #[cfg(debug_assertions)]
    pub fn expand_bluetooth(self: &Rc<Self>) {
        if let Some(section) = &self.bluetooth_section {
            section.set_expanded(true);
            self.sync_chevrons();
            self.sync_discovery();
        }
    }

    /// Open the VPN list, without a pointer. Debug builds only.
    #[cfg(debug_assertions)]
    pub fn expand_vpn(self: &Rc<Self>) {
        if let Some(section) = &self.vpn_section {
            section.set_expanded(true);
            self.sync_chevrons();
        }
    }

    /// Open the Power Mode radio list, without a pointer.
    ///
    /// The smoke hook's way in: there is no synthetic input in the nested
    /// session, so an expanded toggle could not otherwise be photographed.
    #[cfg(debug_assertions)]
    pub fn expand_power_mode(&self) {
        if let Some(section) = &self.power_mode_section {
            section.set_expanded(true);
            self.sync_chevrons();
        }
    }

    /// Match the Bluetooth discovery session to whether its list is open.
    ///
    /// Called after **every** accordion change and on every Bluetooth snapshot,
    /// never from the chevron alone. Two things would otherwise leave a radio
    /// transmitting at nobody:
    ///
    /// - The accordion closes one section when another opens, so the device
    ///   list can leave the screen without its own handler ever running.
    /// - A radio switched off underneath an open list ends the session at
    ///   BlueZ's end; when it comes back on, this is what asks again.
    ///
    /// The panel closing collapses the accordion and then calls this, which is
    /// the path that covers "the user walked away with it open".
    pub fn sync_discovery(self: &Rc<Self>) {
        let Some(section) = &self.bluetooth_section else {
            return;
        };
        let wanted = self.bluetooth.is_some()
            && section.is_expanded()
            && self.services.bluetooth.current().powered;
        if self.discovering.get() == wanted {
            return;
        }
        self.discovering.set(wanted);
        let bluetooth = self.services.bluetooth.handle().clone();
        attempt(names::BLUETOOTH, async move {
            if wanted {
                bluetooth.start_discovery().await
            } else {
                bluetooth.stop_discovery().await
            }
        });
    }

    /// Put the chevrons back when a section is closed from outside.
    pub fn sync_chevrons(&self) {
        for (pill, section) in [
            (&self.wifi, &self.wifi_section),
            (&self.bluetooth, &self.bluetooth_section),
            (&self.vpn, &self.vpn_section),
            (&self.power_mode, &self.power_mode_section),
        ] {
            if let (Some(pill), Some(section)) = (pill, section) {
                pill.set_expanded(section.is_expanded());
            }
        }
    }

    /// Connect every handler and subscription.
    fn wire(toggles: &Rc<Self>, accordion: &Rc<Accordion>) {
        if let Some(pill) = &toggles.caffeine {
            pill.button.connect_clicked({
                let inhibitor = toggles.services.inhibitor.handle().clone();
                move |_| {
                    let inhibitor = inhibitor.clone();
                    attempt(names::CAFFEINE, async move { inhibitor.toggle().await });
                }
            });
        }

        // Wi-Fi is the one pill whose two halves mean different things, and
        // GNOME's does the same: the body switches the radio, the chevron opens
        // the list. Anything else would make "turn Wi-Fi off" a two-step.
        if let Some(pill) = &toggles.wifi {
            pill.button.connect_clicked({
                let toggles = Rc::downgrade(toggles);
                move |_| {
                    let Some(toggles) = toggles.upgrade() else {
                        return;
                    };
                    let state = toggles.services.network.current();
                    if network::radio_busy(&state) {
                        return;
                    }
                    let network = toggles.services.network.handle().clone();
                    let wanted = !state.wifi.enabled;
                    attempt(names::WIFI, async move {
                        network.set_wifi_enabled(wanted).await
                    });
                }
            });
        }

        // Bluetooth's body is the radio switch, exactly as Wi-Fi's is: "turn
        // Bluetooth off" must not be a two-step. The chevron opens the devices.
        if let Some(pill) = &toggles.bluetooth {
            pill.button.connect_clicked({
                let toggles = Rc::downgrade(toggles);
                move |_| {
                    let Some(toggles) = toggles.upgrade() else {
                        return;
                    };
                    let state = toggles.services.bluetooth.current();
                    if state.powering {
                        return;
                    }
                    let bluetooth = toggles.services.bluetooth.handle().clone();
                    let wanted = !state.powered;
                    attempt(names::BLUETOOTH, async move {
                        bluetooth.set_powered(wanted).await
                    });
                }
            });
        }

        // Power Mode expands from either half: there is nothing else a click on
        // it could sensibly mean, and a pill whose left half did nothing would
        // read as broken. Wi-Fi's, Bluetooth's and VPN's bodies are already
        // spoken for, so only their chevrons open their lists.
        for (pill, section, body_expands, scans) in [
            (&toggles.wifi, &toggles.wifi_section, false, true),
            (&toggles.bluetooth, &toggles.bluetooth_section, false, false),
            (&toggles.vpn, &toggles.vpn_section, true, false),
            (
                &toggles.power_mode,
                &toggles.power_mode_section,
                true,
                false,
            ),
        ] {
            let (Some(pill), Some(section)) = (pill, section) else {
                continue;
            };
            let expanders: Vec<&Button> =
                [body_expands.then_some(&pill.button), pill.expand.as_ref()]
                    .into_iter()
                    .flatten()
                    .collect();
            for button in expanders {
                button.connect_clicked({
                    let toggles = Rc::downgrade(toggles);
                    let accordion = Rc::clone(accordion);
                    let section = Rc::clone(section);
                    move |_| {
                        accordion.toggle(&section);
                        let Some(toggles) = toggles.upgrade() else {
                            return;
                        };
                        toggles.sync_chevrons();
                        // Opening the list is the moment the user wants it to
                        // be current. The service rate-limits the scan itself.
                        if scans && section.is_expanded() {
                            let network = toggles.services.network.handle().clone();
                            attempt(names::WIFI, async move { network.scan().await });
                        }
                        // Unconditional, and not only on the Bluetooth
                        // chevron: this click may have closed the device list
                        // by opening something else.
                        toggles.sync_discovery();
                    }
                });
            }
        }

        let inhibitor_binding =
            bridge::bind_state(&toggles.root, toggles.services.inhibitor.state(), {
                let toggles = Rc::downgrade(toggles);
                move |_: &gtk4::Box, state: &InhibitorState| {
                    if let Some(toggles) = toggles.upgrade() {
                        toggles.render_inhibitor(state);
                    }
                }
            });
        let profiles_binding =
            bridge::bind_state(&toggles.root, toggles.services.power_profiles.state(), {
                let toggles = Rc::downgrade(toggles);
                move |_: &gtk4::Box, state: &PowerProfilesState| {
                    if let Some(toggles) = toggles.upgrade() {
                        toggles.render_profiles(state);
                    }
                }
            });

        let network_binding =
            bridge::bind_state(&toggles.root, toggles.services.network.state(), {
                let toggles = Rc::downgrade(toggles);
                move |_: &gtk4::Box, state: &NetworkState| {
                    if let Some(toggles) = toggles.upgrade() {
                        toggles.render_network(state);
                    }
                }
            });

        let bluetooth_binding =
            bridge::bind_state(&toggles.root, toggles.services.bluetooth.state(), {
                let toggles = Rc::downgrade(toggles);
                move |_: &gtk4::Box, state: &BtState| {
                    if let Some(toggles) = toggles.upgrade() {
                        toggles.render_bluetooth(state);
                    }
                }
            });

        toggles.bindings.borrow_mut().extend([
            inhibitor_binding,
            profiles_binding,
            network_binding,
            bluetooth_binding,
        ]);
    }

    /// Draw the Bluetooth pill, and the device list under it.
    fn render_bluetooth(self: &Rc<Self>, state: &BtState) {
        if let Some(pill) = &self.bluetooth {
            // No adapter means no pill at all. A greyed-out one on a desktop
            // with no dongle would be dead space explaining an absence nobody
            // asked about — the same rule the Wi-Fi pill follows.
            pill.root.set_visible(state.available);
            pill.set_checked(state.powered);
            set_icon(
                &pill.icon,
                icons::bluetooth(state.powered, state.connected_count() > 0),
            );
            pill.set_title(&state.title());
            pill.set_subtitle(Some(&state.subtitle()));
            pill.button.set_sensitive(!state.powering);
        }
        self.bluetooth_list.render(state);
        // A radio that came back on under an open list starts looking again.
        self.sync_discovery();
    }

    /// Draw the Wi-Fi and VPN pills, and the lists under them.
    fn render_network(self: &Rc<Self>, state: &NetworkState) {
        if let Some(pill) = &self.wifi {
            // No wireless card means no Wi-Fi pill at all. A greyed-out one on
            // a desktop would be a row of dead space explaining an absence
            // nobody asked about.
            pill.root.set_visible(state.wifi.present);
            pill.set_checked(network::wifi_checked(state));
            set_icon(&pill.icon, network::wifi_icon(state));
            pill.set_title(&network::wifi_title(state));
            pill.set_subtitle(Some(network::wifi_subtitle(state)));
            pill.button.set_sensitive(!network::radio_busy(state));
            // A list of networks nobody can join is a list nobody wants.
            if !state.wifi.enabled
                && let Some(section) = &self.wifi_section
            {
                section.collapse_now();
                pill.set_expanded(false);
            }
        }
        self.wifi_list.render(state);

        if let Some(pill) = &self.vpn {
            pill.root.set_visible(!state.vpn.is_empty());
            let active = state.vpn_active();
            pill.set_checked(active);
            set_icon(
                &pill.icon,
                if active {
                    icons::VPN
                } else {
                    icons::VPN_DISCONNECTED
                },
            );
            pill.set_title(&network::vpn_title(state));
            pill.set_subtitle(Some(network::vpn_subtitle(state)));
            // One tunnel needs no list, so it gets no chevron either: the pill
            // is the switch.
            let lone = network::lone_vpn(state);
            pill.set_chevron_visible(lone.is_none());
            if lone.is_some()
                && let Some(section) = &self.vpn_section
            {
                section.collapse_now();
                pill.set_expanded(false);
            }
        }
        self.vpn_list.render(state);
    }

    /// Draw the idle inhibitor.
    fn render_inhibitor(&self, state: &InhibitorState) {
        let Some(pill) = &self.caffeine else {
            return;
        };
        // No logind means no lock to hold; the pill goes rather than sitting
        // there insensitive explaining a machine the user does not have.
        pill.root.set_visible(state.available);
        pill.set_checked(state.active);
        pill.set_subtitle(Some(if state.active { "On" } else { "Off" }));
    }

    /// Draw the power profiles.
    fn render_profiles(&self, state: &PowerProfilesState) {
        let (Some(pill), Some(section)) = (&self.power_mode, &self.power_mode_section) else {
            return;
        };

        pill.root.set_visible(state.available);
        if !state.available {
            section.collapse_now();
            pill.set_expanded(false);
            return;
        }

        // GNOME labels the collapsed pill with the *profile*, not with the
        // control: "Balanced" is the answer to the question the user is
        // asking, and "Power Mode" underneath says what kind of answer it is.
        if let Some(active) = &state.active {
            set_icon(&pill.icon, active.icon);
            pill.set_title(&active.label);
            pill.set_subtitle(Some("Power Mode"));
        } else {
            pill.set_title("Power Mode");
            pill.set_subtitle(None);
        }

        let ids: Vec<String> = state
            .profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect();
        if *self.built_profiles.borrow() != ids {
            self.rebuild_profiles(state);
            *self.built_profiles.borrow_mut() = ids;
        }
        self.mark_active(state);
    }

    /// Rebuild the radio rows: exactly the profiles the daemon reports.
    fn rebuild_profiles(&self, state: &PowerProfilesState) {
        self.profile_filter.clear_rows();
        while let Some(child) = self.power_mode_list.first_child() {
            self.power_mode_list.remove(&child);
        }
        let mut marks = Vec::new();

        for profile in &state.profiles {
            let row = Button::new();
            row.add_css_class(classes::QS_RADIO_ROW);

            let line = gtk4::Box::new(Orientation::Horizontal, 8);

            let icon = Image::from_icon_name(profile.icon);
            icon.add_css_class(classes::QS_ICON);
            line.append(&icon);

            let label = Label::new(Some(&profile.label));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            line.append(&label);

            let mark = Image::from_icon_name(icons::SELECTED);
            mark.add_css_class(classes::QS_RADIO_MARK);
            line.append(&mark);

            row.set_child(Some(&line));
            ripple::install(&row);
            // The mark is remembered beside the identifier it belongs to, so
            // moving the checkmark is a lookup rather than a walk over the
            // widget tree guessing which child is which.
            marks.push((profile.id.clone(), mark));

            row.connect_clicked({
                let profiles = self.services.power_profiles.handle().clone();
                let id = profile.id.clone();
                move |_| {
                    let profiles = profiles.clone();
                    let id = id.clone();
                    attempt(
                        names::POWER_MODE,
                        async move { profiles.set_profile(id).await },
                    );
                }
            });
            self.power_mode_list.append(&row);
            self.profile_filter
                .add(&row, format!("{} {}", profile.label, profile.id), None);
        }

        *self.profile_marks.borrow_mut() = marks;
        self.profile_filter.apply();
    }

    /// Move the checkmark to the profile in force.
    ///
    /// The mark is always present and only its opacity moves, so the labels
    /// beside it do not shift sideways when the selection changes.
    fn mark_active(&self, state: &PowerProfilesState) {
        let active = state.active_id();
        for (id, mark) in self.profile_marks.borrow().iter() {
            mark.set_opacity(if Some(id.as_str()) == active {
                1.0
            } else {
                0.0
            });
        }
    }
}
