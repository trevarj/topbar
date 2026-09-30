//! The Quick Settings panel: GNOME 45's aggregate menu, top to bottom.
//!
//! ```text
//! ┌─ 360px ───────────────────────────────┐
//! │ [🔋 62%]                    (🔒) (⏻) │  header
//! │ ┄┄┄┄┄ battery health / power ┄┄┄┄┄┄┄┄ │  one expandable open at a time
//! │ 🔊 ──────●────────────────────      ⌄ │  sliders
//! │ 🎤 ────●──────────────────────        │  only while something records
//! │ ☀  ──────────●────────────────        │
//! │ ┌───────────────┬───────────────────┐ │
//! │ │ 📶 Usadba  ⌄ │ 🔒 VPN         ⌄ │ │  toggle grid
//! │ │ ☕ Caffeine   │ ⚡ Balanced    ⌄ │ │
//! │ └───────────────┴───────────────────┘ │
//! │ 🖧  Wired · 1 Gb/s                     │  only while a cable is doing
//! │ ⭯  7 updates                          │     something / while there are
//! │ System                                │  cards
//! │ CPU    ▓▓▓░░░░░░░░░░░░░░░░░░░    34%  │
//! └───────────────────────────────────────┘
//! ```
//!
//! It is built on the shared popover host rather than on a layer window of its
//! own. That is what gives it, for free, the things every other menu in the
//! panel already has: exactly one open at a time, a click-catcher, Escape to
//! dismiss, keyboard focus taken only while it is up and handed back the
//! moment it starts closing, and content built once and retained rather than
//! rebuilt on every open.
//!
//! The height is bounded by a scroller, not by the surface: the panel grows
//! with its content until it would run past the work area, and after that the
//! content scrolls inside it. Expanding a card therefore changes the surface
//! height once — see [`crate::widgets::expander`] for why once matters.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Button, Orientation, PolicyType, ScrolledWindow};
use topbar_core::config::{AppearanceConfig, QuickSettingsConfig};
use topbar_services::{NetworkState, Services};

use crate::bridge::{self, ActionScope, BindingGuard};
use crate::style::{classes, icons};
use crate::surfaces::layer_popover;
use crate::surfaces::popovers;
use crate::surfaces::popovers::PopoverContent;
use crate::widgets::expander::{Accordion, Section};
use crate::widgets::quick_settings::cards::{
    battery::BatteryCard, header::Header, network::WiredRow, power::PowerSection,
    resources::ResourceOverview, sliders::Sliders, toggles::Toggles, updates::UpdatesCard,
};

/// The panel's width, from the UX spec. GNOME's own is 360 too.
pub const WIDTH: i32 = 360;
/// The gap left between the foot of the panel and the bottom of the monitor.
///
/// Room for the drop shadow and a little air, no more. The panel is meant to
/// use the screen it is given, and whatever does not fit scrolls.
const BOTTOM_MARGIN: i32 = 12;

/// The shortest the panel's content is ever squeezed to.
///
/// A monitor short enough to reach this is one the panel cannot fit on at all,
/// and a scroller is a better answer there than a surface taller than the
/// screen.
const MIN_CONTENT_HEIGHT: i32 = 240;

/// One of the panel's expandable blocks, for the smoke hook.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// The battery-health card, under the header's pill.
    BatteryHealth,
    /// The power section, under the header's power button.
    Power,
    /// The Power Mode toggle's radio rows.
    PowerMode,
    /// The Wi-Fi toggle's network list.
    WiFi,
    /// The Bluetooth toggle's device list.
    Bluetooth,
    /// The VPN toggle's profile list.
    Vpn,
}

/// The Quick Settings panel.
pub struct Panel {
    root: gtk4::Box,
    scroll: ScrolledWindow,
    accordion: Rc<Accordion>,
    header: Rc<Header>,
    battery: Rc<BatteryCard>,
    /// The two blocks the header opens. Kept only in a debug build: the
    /// accordion owns them, and nothing but the smoke hook opens one without
    /// a pointer.
    #[cfg(debug_assertions)]
    battery_section: Rc<Section>,
    power: Rc<PowerSection>,
    #[cfg(debug_assertions)]
    power_section: Rc<Section>,
    sliders: Rc<Sliders>,
    toggles: Rc<Toggles>,
    updates: Rc<UpdatesCard>,
    resources: Rc<ResourceOverview>,
    /// Held so the binding that draws it has something to upgrade to.
    _wired: Rc<WiredRow>,
    /// The monitor it is drawn on, for the height bound.
    monitor: gtk4::gdk::Monitor,
    bindings: std::cell::RefCell<Vec<BindingGuard>>,
}

impl Panel {
    /// Build the panel for one bar.
    pub fn new(
        services: &Services,
        config: &QuickSettingsConfig,
        appearance: &AppearanceConfig,
        monitor: &gtk4::gdk::Monitor,
    ) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.add_css_class(classes::QS_PANEL);
        root.set_size_request(WIDTH, -1);

        let content = gtk4::Box::new(Orientation::Vertical, 0);
        content.add_css_class(classes::QS_CONTENT);

        let accordion = Accordion::new();

        let battery = BatteryCard::new(services);
        let battery_section = Section::new(battery.root());
        accordion.add(&battery_section);

        let power = PowerSection::new(services);
        let power_section = Section::new(power.root());
        accordion.add(&power_section);

        let header = Header::new(
            services,
            &accordion,
            &battery_section,
            &power_section,
            config.on_click_right.clone(),
            config.battery,
        );
        content.append(header.root());
        if config.battery_health {
            content.append(battery_section.root());
        }
        if config.power {
            content.append(power_section.root());
        }

        let sliders = Sliders::new(services, config.audio, config.mic, config.brightness);
        content.append(sliders.root());

        let toggles = Toggles::new(
            services,
            &accordion,
            config.idle_inhibitor,
            config.network,
            config.bluetooth,
            config.vpn,
            config.vpn_close_on_connect,
        );
        content.append(toggles.root());

        let appearance_row = gtk4::Box::new(Orientation::Horizontal, 8);
        for (label, icon_name, command) in [
            ("Theme", icons::THEME, appearance.theme_command.as_ref()),
            (
                "Wallpaper",
                icons::WALLPAPER,
                appearance.wallpaper_command.as_ref(),
            ),
        ] {
            let Some(command) = command else { continue };
            let button = Button::new();
            let line = gtk4::Box::new(Orientation::Horizontal, 8);
            line.set_halign(gtk4::Align::Center);
            line.set_valign(gtk4::Align::Center);
            let icon = gtk4::Image::from_icon_name(icon_name);
            icon.add_css_class(classes::QS_ICON);
            line.append(&icon);
            line.append(&gtk4::Label::new(Some(label)));
            button.set_child(Some(&line));
            button.add_css_class(classes::DIALOG_BUTTON);
            button.set_hexpand(true);
            let argv = command.clone();
            button.connect_clicked(move |_| {
                popovers::dispatch(&topbar_core::ipc::PopoverAction::Hide(None), None);
                let argv = argv.clone();
                bridge::act(
                    ActionScope::Toast {
                        widget: "quick_settings",
                    },
                    async move { topbar_services::proc::run_argv(&argv).await },
                );
            });
            appearance_row.append(&button);
        }
        if appearance_row.first_child().is_some() {
            appearance_row.set_margin_top(8);
            content.append(&appearance_row);
        }

        // Under the grid rather than inside it: a cable is a statement, not a
        // control, and a non-interactive pill sitting among four that respond
        // to a click would read as one that had stopped working. v1 put the
        // same information in a section header inside its network card.
        let wired = WiredRow::new();
        if config.network {
            content.append(wired.root());
        }

        // The two cards at the foot of the panel, in the order the plan puts
        // them: updates first, because it is the one that comes and goes, and
        // an overview that moved down the panel whenever an update landed
        // would be a card the user had to look for.
        let updates = UpdatesCard::new(services);
        if config.updates {
            content.append(updates.root());
        }
        let resources = ResourceOverview::new(services, Some("System"));
        if config.resource_overview {
            content.append(resources.root());
        }

        // Vertical only: the panel is a fixed width and a horizontal scrollbar
        // would mean something in it had overflowed, which is a layout bug
        // rather than something to offer the user a way around.
        let scroll = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .vscrollbar_policy(PolicyType::Automatic)
            .propagate_natural_height(true)
            .child(&content)
            .build();
        scroll.add_css_class(classes::QS_SCROLL);
        root.append(&scroll);

        let panel = Rc::new(Self {
            root,
            scroll,
            accordion,
            header,
            battery,
            #[cfg(debug_assertions)]
            battery_section,
            power,
            #[cfg(debug_assertions)]
            power_section,
            sliders,
            toggles,
            updates,
            resources,
            _wired: Rc::clone(&wired),
            monitor: monitor.clone(),
            bindings: std::cell::RefCell::new(Vec::new()),
        });

        let binding = bridge::bind_state(&panel.root, services.network.state(), {
            let wired = Rc::downgrade(&wired);
            move |_: &gtk4::Box, state: &NetworkState| {
                if let Some(wired) = wired.upgrade() {
                    wired.render(state);
                }
            }
        });
        panel.bindings.borrow_mut().push(binding);

        panel
    }

    /// Open one of the panel's expandable blocks, without a pointer.
    ///
    /// The nested-niri smoke session has no synthetic input, so this is the
    /// only way an *expanded* card reaches a screenshot. Debug builds only.
    #[cfg(debug_assertions)]
    pub fn expand(&self, block: Block) {
        match block {
            Block::BatteryHealth => self.accordion.toggle(&self.battery_section),
            Block::Power => self.accordion.toggle(&self.power_section),
            Block::PowerMode => self.toggles.expand_power_mode(),
            Block::Bluetooth => self.toggles.expand_bluetooth(),
            Block::WiFi => self.toggles.expand_wifi(),
            Block::Vpn => self.toggles.expand_vpn(),
        }
    }

    /// The power section, for the smoke hook that paints a held row.
    #[cfg(debug_assertions)]
    pub fn power(&self) -> &Rc<PowerSection> {
        &self.power
    }

    /// Log where every control in the panel currently is, on the output.
    ///
    /// The pointer-driven smoke run has to click things, and a driver holding a
    /// table of coordinates measured off a screenshot is a driver that starts
    /// clicking empty space the first time a padding changes — which looks
    /// exactly like the dead-control bugs the run exists to catch. So the panel
    /// says where its controls are and the driver reads it back.
    ///
    /// Coordinates are **logical pixels on the monitor**: GTK measures against
    /// the surface, and the compositor put the surface where the layer-shell
    /// margins say. Debug builds only.
    #[cfg(debug_assertions)]
    pub fn dump(&self) {
        use gtk4_layer_shell::{Edge, LayerShell};

        let Some(root) = self.root.root() else {
            tracing::warn!("qs-dump: the panel is not on a surface");
            return;
        };
        let Ok(window) = root.clone().downcast::<gtk4::Window>() else {
            tracing::warn!("qs-dump: the panel's root is not a window");
            return;
        };
        // Anchored top-left, so the margins *are* the surface's origin — and
        // the bar's exclusive zone has already been taken off the top by the
        // compositor, which is why it is added here rather than measured.
        let origin_x = LayerShell::margin(&window, Edge::Left);
        let origin_y = LayerShell::margin(&window, Edge::Top) + layer_popover::bar_height();
        tracing::info!("qs-dump: origin {origin_x} {origin_y}");

        crate::surfaces::dump::tree(
            &self.root.clone().upcast(),
            &window,
            (origin_x, origin_y),
            "qs-dump",
        );
    }

    /// Bound the panel's height to the monitor it is on.
    ///
    /// Done on every open rather than once: a monitor can change resolution,
    /// and a panel that remembered a 4K height on a 1080p screen would hang off
    /// the bottom of it.
    fn clamp_height(&self) {
        let height = self.monitor.geometry().height();
        self.scroll
            .set_max_content_height(max_content_height(height, self.surface_top()));
    }

    /// How far down the monitor the panel's own surface starts.
    ///
    /// The compositor takes the bar's exclusive zone off the top by itself, and
    /// `bar.popover_offset` is the gap the host adds below it. Read off the
    /// window rather than passed in: the panel is built once, and the host that
    /// owns it is the only thing that knows where it put the surface.
    ///
    /// The margin is still zero the first time a panel is refreshed — the host
    /// places the surface after it asks its content to render — so the first
    /// open budgets `popover_offset` pixels too many. That is at most twelve,
    /// and it costs twelve pixels of a list that was already scrolling.
    fn surface_top(&self) -> i32 {
        use gtk4_layer_shell::{Edge, LayerShell};

        let offset = self
            .root
            .root()
            .and_then(|root| root.downcast::<gtk4::Window>().ok())
            .filter(LayerShell::is_layer_window)
            .map_or(0, |window| LayerShell::margin(&window, Edge::Top));
        layer_popover::bar_height() + offset
    }
}

/// The tallest the panel's content may be, on a monitor `monitor_height`
/// pixels tall whose panel starts `surface_top` pixels down it.
///
/// `surface_top` is not something that can be left out. A budget taken from the
/// full height of the monitor is too tall by exactly the distance the panel
/// starts down it, and the surface runs off the bottom of the screen — the foot
/// of the panel, which is where the updates and system cards live, simply is not
/// drawn. It went unnoticed because the margin it was traded against happened to
/// be about the height of a bar: the arithmetic was wrong and the result was
/// right, on one bar height, on one monitor.
fn max_content_height(monitor_height: i32, surface_top: i32) -> i32 {
    (monitor_height - surface_top - BOTTOM_MARGIN).max(MIN_CONTENT_HEIGHT)
}

impl PopoverContent for Panel {
    fn root(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    fn refresh(&self) {
        self.clamp_height();
        self.header.refresh();
        self.battery.refresh();
        self.sliders.refresh();
        self.toggles.refresh();
        self.updates.refresh();
        self.resources.refresh();
    }

    fn closed(&self) {
        // Everything the user opened goes back: a panel that reopened onto the
        // power section three days later would be a surprise, and a row still
        // counting down behind a closed popover would be worse than one.
        self.power.cancel_holds();
        self.sliders.collapse();
        self.accordion.collapse_all();
        self.toggles.sync_chevrons();
        // The device list has gone with the accordion, so the radio stops
        // looking for things to put in it. This is the path that covers a panel
        // somebody opened and walked away from.
        self.toggles.sync_discovery();
        // And so does everything that went wrong. A caption is cleared when the
        // action it explains is tried again, which for an action nobody tries
        // again is never: the panel is retained, so a failed lock command was
        // still sitting under the header the next time the panel was opened,
        // in red, about something that had happened days earlier.
        crate::surfaces::inline::clear_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_is_budgeted_from_where_it_starts_not_from_the_whole_screen() {
        // A 1080p screen, the shipped 36px bar and the 1px offset the example
        // config uses: the panel starts 37 pixels down and has the rest.
        assert_eq!(max_content_height(1080, 37), 1080 - 37 - BOTTOM_MARGIN);

        // The bug this replaces: budgeting from the full height of the monitor
        // put the foot of the panel below the bottom of the screen. Whatever
        // the bar costs has to come off, and the result plus the offset must
        // still fit on the monitor with the gap intact.
        for bar in [24, 36, 48, 64] {
            for offset in [0, 1, 12] {
                let top = bar + offset;
                assert!(max_content_height(1080, top) + top <= 1080 - BOTTOM_MARGIN);
            }
        }
    }

    #[test]
    fn a_screen_too_short_for_the_panel_gets_a_scroller_rather_than_a_negative() {
        assert_eq!(max_content_height(200, 37), MIN_CONTENT_HEIGHT);
        assert_eq!(max_content_height(0, 0), MIN_CONTENT_HEIGHT);
    }
}
