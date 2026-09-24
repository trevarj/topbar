//! The volume/brightness capsule.
//!
//! ```text
//! window .osd-window          layer Overlay, anchored per [osd] position
//! └── scale-box               the enter/leave fade and grow (see anim::ScaleBox)
//!     └── .osd-capsule        GNOME 42's pill: icon, bar, optional number
//! ```
//!
//! **It cannot be clicked.** The surface takes no keyboard focus and its input
//! region is empty, so a pointer press goes through it to whatever is
//! underneath. A capsule that appeared under the cursor and swallowed a click
//! would be worse than no capsule.
//!
//! **It does not raise itself for the panel's own controls.** A change carries
//! a [`ChangeSource`], and a Quick Settings slider (M9) sets
//! [`ChangeSource::Ui`], which is filtered out here — the slider under the
//! user's finger is already the feedback, and restating it in the middle of
//! the screen is what GNOME conspicuously does not do. Media keys and anything
//! else on the machine raise it; see [`topbar_services::change`] for how the
//! two are told apart.
//!
//! **The timer is a reset, not a queue.** A second event while the capsule is
//! up retargets the fill and starts the countdown again; the capsule itself is
//! not re-animated, because a pill that pulses once per volume step is a pill
//! nobody can read.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Label, Orientation, Window, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use topbar_core::Config;
use topbar_core::theme::{Palette, Rgb};
use topbar_services::{AudioState, BrightnessState, InhibitorState, Services};
use tracing::debug;

use crate::anim::{Animation, AnimationParams, Easing, ScaleBox};
use crate::bridge::{self, BindingGuard};
use crate::style::classes;
use crate::surfaces::osd_bar::{BarColors, OsdBar};
use crate::surfaces::toast;
use crate::wayland::blur::{self, BlurAttachment};

/// How long the capsule takes to appear.
const ENTER_MS: u64 = 150;
/// How long it takes to go.
const LEAVE_MS: u64 = 200;
/// The scale it grows from, and shrinks back to.
const SCALE_FROM: f64 = 0.95;
/// Length of the bar along its own axis, in pixels.
const BAR_LENGTH: i32 = 148;
/// Distance from the screen edge for a top or bottom capsule.
const EDGE_MARGIN: i32 = 48;
/// Distance from the screen edge for a left or right one, which is narrower.
const SIDE_MARGIN: i32 = 24;
/// Room left around the capsule for its drop shadow.
const SHADOW_MARGIN: i32 = 12;
/// The volume above which the fill turns urgent, when overdrive allows it.
const OVERDRIVE_FROM: u32 = 100;

/// What the capsule says when there is nothing to play out of.
const NO_OUTPUT: &str = "No output device";

/// The brightness the smoke hook shows, having no backlight to read one from.
const SMOKE_BRIGHTNESS: u32 = 60;

/// Icons, all Adwaita symbolic.
const VOLUME_MUTED: &str = "audio-volume-muted-symbolic";
const VOLUME_LOW: &str = "audio-volume-low-symbolic";
const VOLUME_MEDIUM: &str = "audio-volume-medium-symbolic";
const VOLUME_HIGH: &str = "audio-volume-high-symbolic";
const BRIGHTNESS_OFF: &str = "display-brightness-off-symbolic";
const BRIGHTNESS_LOW: &str = "display-brightness-low-symbolic";
const BRIGHTNESS_MEDIUM: &str = "display-brightness-medium-symbolic";
const BRIGHTNESS_HIGH: &str = "display-brightness-high-symbolic";
/// The ungraded name, for a theme without the graded set.
const BRIGHTNESS_FALLBACK: &str = "display-brightness-symbolic";
const INHIBIT_ON: &str = "my-caffeine-on-symbolic";
const INHIBIT_OFF: &str = "my-caffeine-off-symbolic";
/// What the caffeine icons fall back to on a theme that has no `my-caffeine-*`.
///
/// A sun and a moon: awake, and allowed to sleep. Adwaita has no coffee cup,
/// and `preferences-desktop-screensaver-symbolic` — the obvious-looking choice
/// — is a picture of the very thing being *suppressed*, which reads backwards.
const INHIBIT_ON_FALLBACK: &str = "weather-clear-symbolic";
const INHIBIT_OFF_FALLBACK: &str = "weather-clear-night-symbolic";

/// Where a capsule sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// Centred above the bottom edge. The default, and GNOME's.
    Bottom,
    /// Centred below the top edge, clear of the panel.
    Top,
    /// Centred on the left edge.
    Left,
    /// Centred on the right edge.
    Right,
}

impl Position {
    /// Read `[osd] position`, falling back to the default.
    ///
    /// Validation has already rejected anything else with a message naming the
    /// four it accepts, so this is only reached with a valid value — but a
    /// fallback beats a panic on a path the user can reach by editing a file.
    pub fn parse(value: &str) -> Self {
        match value {
            "top" => Self::Top,
            "left" => Self::Left,
            "right" => Self::Right,
            _ => Self::Bottom,
        }
    }

    /// The axis the capsule's contents run along.
    fn orientation(self) -> Orientation {
        match self {
            Self::Left | Self::Right => Orientation::Vertical,
            Self::Bottom | Self::Top => Orientation::Horizontal,
        }
    }

    /// The edge to anchor to, and how far from it to sit.
    ///
    /// Exactly one edge: anchoring one edge of an axis is what centres the
    /// surface on the other, which is where a capsule belongs.
    fn anchor(self) -> (Edge, i32) {
        match self {
            Self::Bottom => (Edge::Bottom, EDGE_MARGIN),
            Self::Top => (Edge::Top, EDGE_MARGIN),
            Self::Left => (Edge::Left, SIDE_MARGIN),
            Self::Right => (Edge::Right, SIDE_MARGIN),
        }
    }
}

/// What the capsule is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdEvent {
    /// A volume, out of `max` — which is above 100 only with overdrive on.
    Volume {
        /// The volume, as a percentage.
        percent: u32,
        /// Whether the sink is muted, which draws a crossed icon and no fill.
        muted: bool,
        /// The top of the bar.
        max: u32,
    },
    /// There is nothing to play out of.
    NoOutput,
    /// A backlight percentage.
    Brightness {
        /// How bright, 0–100.
        percent: u32,
    },
    /// The idle inhibitor was turned on or off. Icon only, no bar.
    Inhibitor {
        /// Whether the machine is now being kept awake.
        active: bool,
    },
}

impl OsdEvent {
    /// The icon to draw.
    fn icon(self) -> &'static str {
        match self {
            Self::NoOutput => VOLUME_MUTED,
            Self::Volume { muted: true, .. } | Self::Volume { percent: 0, .. } => VOLUME_MUTED,
            Self::Volume { percent, .. } if percent < 34 => VOLUME_LOW,
            Self::Volume { percent, .. } if percent < 67 => VOLUME_MEDIUM,
            Self::Volume { .. } => VOLUME_HIGH,
            Self::Brightness { percent: 0 } => BRIGHTNESS_OFF,
            Self::Brightness { percent } if percent < 34 => BRIGHTNESS_LOW,
            Self::Brightness { percent } if percent < 67 => BRIGHTNESS_MEDIUM,
            Self::Brightness { .. } => BRIGHTNESS_HIGH,
            Self::Inhibitor { active: true } => INHIBIT_ON,
            Self::Inhibitor { active: false } => INHIBIT_OFF,
        }
    }

    /// The icon to draw when the theme has never heard of [`Self::icon`].
    ///
    /// Two names need one. The caffeine pair is GNOME-extension territory
    /// rather than stock Adwaita, and the graded `display-brightness-*` set is
    /// recent enough that an older icon theme has only the ungraded name —
    /// which is exactly what the smoke run caught: a capsule with a
    /// missing-image square where the brightness icon should have been.
    fn fallback_icon(self) -> Option<&'static str> {
        match self {
            Self::Inhibitor { active: true } => Some(INHIBIT_ON_FALLBACK),
            Self::Inhibitor { active: false } => Some(INHIBIT_OFF_FALLBACK),
            Self::Brightness { .. } => Some(BRIGHTNESS_FALLBACK),
            Self::Volume { .. } | Self::NoOutput => None,
        }
    }

    /// The fill: how much of what, or `None` for an icon-only capsule.
    fn fill(self) -> Option<(u32, u32)> {
        match self {
            Self::Volume {
                muted: true, max, ..
            } => Some((0, max)),
            Self::Volume { percent, max, .. } => Some((percent, max)),
            Self::Brightness { percent } => Some((percent, 100)),
            Self::NoOutput | Self::Inhibitor { .. } => None,
        }
    }

    /// The caption under an icon-only capsule, if it has one.
    fn caption(self) -> Option<&'static str> {
        match self {
            Self::NoOutput => Some(NO_OUTPUT),
            _ => None,
        }
    }

    /// The number drawn beside the bar when `[osd] show_value` is on.
    fn value(self) -> Option<u32> {
        match self {
            Self::Volume { muted: true, .. } => Some(0),
            Self::Volume { percent, .. } | Self::Brightness { percent } => Some(percent),
            Self::NoOutput | Self::Inhibitor { .. } => None,
        }
    }
}

/// Which snapshots have already been shown.
///
/// Pure: it holds the serial of the last change it raised the capsule for, and
/// nothing else. A snapshot republished for an unrelated reason — a sink
/// appearing, a recording client starting — carries the same serial and is not
/// an event.
#[derive(Debug, Default)]
pub struct Watcher {
    audio: Option<u64>,
    brightness: Option<u64>,
    inhibitor: Option<bool>,
}

impl Watcher {
    /// A watcher that has seen nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide what an audio snapshot should show, if anything.
    pub fn audio(&mut self, state: &AudioState) -> Option<OsdEvent> {
        let change = state.sink_change?;
        if self.audio == Some(change.serial) {
            return None;
        }
        self.audio = Some(change.serial);
        if !change.source.shows_osd() {
            return None;
        }
        if !state.available || state.default_sink.is_none() {
            return Some(OsdEvent::NoOutput);
        }
        Some(OsdEvent::Volume {
            percent: state.sink_volume_pct,
            muted: state.sink_muted,
            max: state.max_volume_pct.max(1),
        })
    }

    /// The same, for the backlight.
    pub fn brightness(&mut self, state: &BrightnessState) -> Option<OsdEvent> {
        let change = state.change?;
        if self.brightness == Some(change.serial) {
            return None;
        }
        self.brightness = Some(change.serial);
        if !change.source.shows_osd() || !state.available {
            return None;
        }
        Some(OsdEvent::Brightness {
            percent: state.percent,
        })
    }

    /// The same, for the inhibitor — which has no source, because only the
    /// panel can toggle it.
    pub fn inhibitor(&mut self, state: &InhibitorState) -> Option<OsdEvent> {
        if !state.available {
            self.inhibitor = None;
            return None;
        }
        let previous = self.inhibitor.replace(state.active);
        // The first reading establishes the baseline: a panel starting with
        // the inhibitor already off has not just turned it off.
        match previous {
            Some(previous) if previous != state.active => Some(OsdEvent::Inhibitor {
                active: state.active,
            }),
            _ => None,
        }
    }
}

/// One monitor's capsule.
pub struct OsdSurface {
    window: Window,
    shell: ScaleBox,
    capsule: gtk4::Box,
    icon: gtk4::Image,
    bar: OsdBar,
    caption: Label,
    value: Label,
    animation: Animation,
    position: Position,
    timeout_ms: u32,
    show_value: bool,
    services: Services,
    connector: String,
    watcher: RefCell<Watcher>,
    /// The pending auto-hide, replaced rather than stacked on every event.
    hide: RefCell<Option<gtk4::glib::SourceId>>,
    /// Whether the capsule is up, so a second event retargets rather than
    /// replaying the entrance.
    shown: Cell<bool>,
    /// The blur behind the capsule, suspended for the length of every fade-out.
    blur: BlurAttachment,
    bindings: RefCell<Vec<BindingGuard>>,
}

impl OsdSurface {
    /// Build the capsule for `monitor` and subscribe it to the services.
    ///
    /// Returns `None` when `[osd] enabled` is off, in which case no surface is
    /// created at all — the feature costs nothing when it is switched off.
    pub fn new(
        monitor: &gdk::Monitor,
        connector: &str,
        config: &Config,
        services: &Services,
    ) -> Option<Rc<Self>> {
        if !config.osd.enabled {
            return None;
        }

        let position = Position::parse(&config.osd.position);
        let orientation = position.orientation();

        let capsule = gtk4::Box::new(orientation, 12);
        capsule.add_css_class(classes::OSD_CAPSULE);
        capsule.set_halign(Align::Center);
        capsule.set_valign(Align::Center);

        let icon = gtk4::Image::from_icon_name(VOLUME_MEDIUM);
        icon.add_css_class(classes::OSD_ICON);
        icon.set_halign(Align::Center);
        icon.set_valign(Align::Center);

        let bar = OsdBar::new();
        bar.set_axis(orientation, BAR_LENGTH);
        bar.set_colors(colors(config));
        bar.set_halign(Align::Center);
        bar.set_valign(Align::Center);

        let caption = Label::new(None);
        caption.add_css_class(classes::OSD_CAPTION);
        caption.set_visible(false);

        let value = Label::new(None);
        value.add_css_class(classes::OSD_VALUE);
        value.set_visible(false);
        // The widest number the label ever holds, so revealing it cannot make
        // the capsule change width mid-slide.
        value.set_width_chars(4);

        // A vertical capsule reads bottom-up: the number on top, then the
        // fill, then the icon nearest the hand.
        if orientation == Orientation::Vertical {
            capsule.append(&value);
            capsule.append(&bar);
            capsule.append(&caption);
            capsule.append(&icon);
        } else {
            capsule.append(&icon);
            capsule.append(&bar);
            capsule.append(&caption);
            capsule.append(&value);
        }

        let shell = ScaleBox::new();
        shell.set_child(&capsule);
        shell.set_opacity(0.0);
        shell.set_scale(SCALE_FROM);

        let window = build_window(monitor, position);
        window.set_child(Some(&shell));

        let surface = Rc::new(Self {
            animation: Animation::new(&shell),
            // The capsule is a pill; the region rasteriser clamps an enormous
            // radius to half the smaller side, which is exactly that shape.
            blur: blur::attach(&window, &capsule, || i32::MAX),
            window,
            shell,
            capsule,
            icon,
            bar,
            caption,
            value,
            position,
            timeout_ms: config.osd.timeout_ms,
            show_value: config.osd.show_value,
            services: services.clone(),
            connector: connector.to_string(),
            watcher: RefCell::new(Watcher::new()),
            hide: RefCell::new(None),
            shown: Cell::new(false),
            bindings: RefCell::new(Vec::new()),
        });

        let audio = bridge::bind_state(&surface.capsule, services.audio.state(), {
            let surface = Rc::downgrade(&surface);
            move |_, state| {
                if let Some(surface) = surface.upgrade() {
                    let event = surface.watcher.borrow_mut().audio(state);
                    surface.raise_if(event);
                }
            }
        });
        let brightness = bridge::bind_state(&surface.capsule, services.brightness.state(), {
            let surface = Rc::downgrade(&surface);
            move |_, state| {
                if let Some(surface) = surface.upgrade() {
                    let event = surface.watcher.borrow_mut().brightness(state);
                    surface.raise_if(event);
                }
            }
        });
        let inhibitor = bridge::bind_state(&surface.capsule, services.inhibitor.state(), {
            let surface = Rc::downgrade(&surface);
            move |_, state| {
                if let Some(surface) = surface.upgrade() {
                    let event = surface.watcher.borrow_mut().inhibitor(state);
                    surface.raise_if(event);
                }
            }
        });
        *surface.bindings.borrow_mut() = vec![audio, brightness, inhibitor];

        register(&surface);
        // There is no backlight in a nested compositor, so the brightness
        // capsule has no way to be photographed from a real event. This feeds
        // it a synthetic one; debug builds only, like every other smoke hook.
        crate::surfaces::popovers::register_smoke_action("osd-brightness", {
            let surface = Rc::downgrade(&surface);
            move || {
                if let Some(surface) = surface.upgrade() {
                    surface.raise(OsdEvent::Brightness {
                        percent: SMOKE_BRIGHTNESS,
                    });
                }
            }
        });
        Some(surface)
    }

    /// Show `event`, if there is one and this is the monitor to show it on.
    fn raise_if(self: &Rc<Self>, event: Option<OsdEvent>) {
        if let Some(event) = event {
            self.raise(event);
        }
    }

    /// Show `event` and restart the countdown.
    ///
    /// Public because the IPC server raises the capsule directly: a `topbar
    /// volume` that acted on PulseAudio without the panel's help still sends a
    /// frame so the user sees what they pressed.
    pub fn raise(self: &Rc<Self>, event: OsdEvent) {
        if !self.is_host() {
            return;
        }

        set_icon(&self.icon, event);

        match event.fill() {
            Some((value, max)) => {
                self.bar.set_visible(true);
                if self.shown.get() {
                    self.bar.set_value(value, max, OVERDRIVE_FROM);
                } else {
                    self.bar.jump_to(value, max, OVERDRIVE_FROM);
                }
            }
            None => self.bar.set_visible(false),
        }

        match event.caption() {
            Some(text) => {
                self.caption.set_text(text);
                self.caption.set_visible(true);
            }
            None => self.caption.set_visible(false),
        }

        match event
            .value()
            .filter(|_| self.show_value && event.fill().is_some())
        {
            Some(number) => {
                self.value.set_text(&format!("{number}%"));
                self.value.set_visible(true);
            }
            None => self.value.set_visible(false),
        }

        self.show();
        self.restart_timer();
    }

    /// Whether the capsule belongs on this monitor right now.
    ///
    /// The focused output, exactly as the banners do it, so a media key pressed
    /// on one screen does not light up all of them.
    fn is_host(&self) -> bool {
        let workspaces = self.services.compositor.workspaces();
        let focused = workspaces.borrow().focused_output.clone();
        let connectors = toast::connectors();
        toast::hosting_output(focused.as_deref(), &connectors) == Some(self.connector.as_str())
    }

    /// Map the surface and fade it in, if it is not up already.
    fn show(self: &Rc<Self>) {
        // Sized explicitly for the same reason the banners are: a surface
        // anchored to one edge is stretched on neither axis, so a toplevel
        // that was never given a default size maps at nothing.
        let (_, width, _, _) = self.capsule.measure(Orientation::Horizontal, -1);
        let (_, height, _, _) = self.capsule.measure(Orientation::Vertical, width);
        debug!("OSD capsule: {width}x{height} on {}", self.connector);
        self.window
            .set_default_size(width + 2 * SHADOW_MARGIN, height + 2 * SHADOW_MARGIN);

        let (edge, margin) = self.position.anchor();
        self.window.set_margin(edge, margin);
        if !self.window.is_visible() {
            self.window.present();
        }
        // Re-asserted around the map: a layer surface's geometry is part of
        // what the compositor reads when it is created, and one set before the
        // map is not always the state it reads.
        self.window.set_margin(edge, margin);

        if self.shown.get() {
            return;
        }
        self.shown.set(true);
        // An event arriving mid-dismissal reverses a fade on a surface that
        // never unmapped, so the blur has to be asked back by hand.
        self.blur.resume();

        // One main-loop turn after the map, not during it. The fade is driven
        // by the shell's frame clock, and a widget that has not been realised
        // yet does not have one — a run started here would register a tick
        // callback that never ticks and leave the capsule mapped at zero
        // opacity, which is a surface on screen that draws nothing at all.
        let surface = Rc::downgrade(self);
        gtk4::glib::idle_add_local_once(move || {
            if let Some(surface) = surface.upgrade()
                && surface.shown.get()
            {
                surface.fade(1.0, ENTER_MS, Easing::EaseOutCubic);
            }
        });
    }

    /// Fade the capsule out and unmap it when it has gone.
    fn dismiss(self: &Rc<Self>) {
        if !self.shown.get() {
            return;
        }
        self.shown.set(false);
        // Removed as the fade starts, not when it ends: compositor-side blur
        // does not fade with the surface it sits behind.
        self.blur.suspend();
        let window = self.window.clone();
        self.fade_with(0.0, LEAVE_MS, Easing::EaseInCubic, move || {
            window.set_visible(false);
        });
    }

    /// Drive the opacity and scale toward `target`.
    fn fade(self: &Rc<Self>, target: f64, duration_ms: u64, easing: Easing) {
        self.fade_with(target, duration_ms, easing, || {});
    }

    /// The same, running `done` when it lands.
    ///
    /// The run starts from the shell's live opacity, so reversing mid-flight —
    /// an event arriving while the capsule is fading out — picks up where the
    /// fade had got to instead of jumping.
    fn fade_with(
        self: &Rc<Self>,
        target: f64,
        duration_ms: u64,
        easing: Easing,
        done: impl FnOnce() + 'static,
    ) {
        let shell = self.shell.clone();
        let start = shell.opacity();
        let distance = (target - start).abs();
        let duration = (duration_ms as f64 * distance).round() as u64;

        debug!("OSD fade {start} -> {target} over {duration}ms");
        let surface = Rc::downgrade(self);
        self.animation.start(
            AnimationParams::new(duration).with_easing(easing),
            Box::new(move |progress| {
                let value = start + (target - start) * progress;
                shell.set_opacity(value);
                shell.set_scale(SCALE_FROM + (1.0 - SCALE_FROM) * value);
                // The blurred area tracks how visible the capsule is; on the
                // way out it is already suspended and this does nothing.
                if let Some(surface) = surface.upgrade() {
                    surface.blur.set_scale(value);
                }
            }),
            Some(Box::new(done)),
        );
    }

    /// Start the countdown again, replacing whatever was pending.
    fn restart_timer(self: &Rc<Self>) {
        if let Some(pending) = self.hide.borrow_mut().take() {
            pending.remove();
        }
        // Validation rejects zero, but a config reload could in principle hand
        // one over; a capsule with no timer would never leave.
        let timeout = u64::from(self.timeout_ms.max(1));

        let surface = Rc::downgrade(self);
        let source = gtk4::glib::timeout_add_local_once(
            std::time::Duration::from_millis(timeout),
            move || {
                if let Some(surface) = surface.upgrade() {
                    *surface.hide.borrow_mut() = None;
                    surface.dismiss();
                }
            },
        );
        *self.hide.borrow_mut() = Some(source);
    }
}

impl Drop for OsdSurface {
    fn drop(&mut self) {
        if let Some(pending) = self.hide.borrow_mut().take() {
            pending.remove();
        }
        self.window.close();
    }
}

thread_local! {
    /// Every capsule the panel has, held weakly.
    ///
    /// A `Vec` for the same reason the popover registry is one: there is at
    /// most one entry per monitor, and it is only ever walked when a `topbar`
    /// command arrives.
    static SURFACES: RefCell<Vec<std::rc::Weak<OsdSurface>>> = const { RefCell::new(Vec::new()) };
}

/// Remember a capsule so [`show`] can reach it.
fn register(surface: &Rc<OsdSurface>) {
    SURFACES.with_borrow_mut(|surfaces| {
        surfaces.retain(|held| held.strong_count() > 0);
        surfaces.push(Rc::downgrade(surface));
    });
}

/// Raise the capsule on whichever monitor should be showing it.
///
/// Every surface is asked; each one decides for itself whether it is the host,
/// which is the same rule the banners use. Returns whether any of them showed
/// it, so an IPC caller can tell "no capsule" from "no panel".
pub fn show(event: OsdEvent) -> bool {
    let surfaces: Vec<Rc<OsdSurface>> = SURFACES
        .with_borrow(|surfaces| surfaces.iter().filter_map(std::rc::Weak::upgrade).collect());
    let mut shown = false;
    for surface in surfaces {
        if surface.is_host() {
            surface.raise(event);
            shown = true;
        }
    }
    shown
}

/// Set the icon, falling back when the theme has never heard of it.
fn set_icon(image: &gtk4::Image, event: OsdEvent) {
    let wanted = event.icon();
    let theme = gtk4::IconTheme::for_display(&image.display());
    let name = match event.fallback_icon() {
        Some(fallback) if !theme.has_icon(wanted) => fallback,
        _ => wanted,
    };
    if image.icon_name().as_deref() != Some(name) {
        image.set_icon_name(Some(name));
    }
}

/// Colours the bar paints itself with, from the configured palette.
fn colors(config: &Config) -> BarColors {
    let palette = Palette::from_config(config);
    BarColors {
        accent: rgba(palette.accent),
        urgent: rgba(palette.urgent),
    }
}

/// Convert a resolved color into a GDK one.
fn rgba(color: Rgb) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    )
}

/// The capsule's layer-shell window.
fn build_window(monitor: &gdk::Monitor, position: Position) -> Window {
    let window = Window::builder().decorated(false).resizable(false).build();
    window.add_css_class(classes::OSD_WINDOW);

    window.init_layer_shell();
    window.set_namespace(Some("topbar-osd"));
    // Overlay: the capsule has to be readable over a fullscreen video, which
    // is exactly when somebody reaches for the volume key.
    window.set_layer(Layer::Overlay);
    window.set_monitor(Some(monitor));
    // Reserves nothing: a capsule that pushed the desktop around for a second
    // and a half would be a disaster.
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);

    let (edge, margin) = position.anchor();
    for other in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(other, other == edge);
    }
    window.set_margin(edge, margin);

    // An empty input region is what makes the capsule click-through. It has to
    // be set on the GDK surface, which does not exist until the window is
    // realised, and re-set on every map — a surface recreated by a hotplug
    // starts with the default region again.
    window.connect_map(|window| {
        let Some(surface) = window.surface() else {
            return;
        };
        surface.set_input_region(Some(&gtk4::cairo::Region::create()));
        debug!("OSD surface is click-through");
    });

    window
}

/// How wide the capsule is asked to be, for the record.
///
/// GNOME 42's is a fixed 220-odd pixels; this is the same figure arrived at
/// from its parts, and the test below is what keeps a padding change from
/// quietly turning it into something else.
#[cfg(test)]
const NOMINAL_WIDTH: i32 = BAR_LENGTH + 2 * crate::style::stylesheet::OSD_PADDING as i32 + 24 + 12;

#[cfg(test)]
mod tests {
    use super::*;
    use topbar_services::{Change, ChangeSource, InhibitorState};

    fn change(source: ChangeSource, serial: u64) -> Change {
        Change { source, serial }
    }

    fn audio(percent: u32, muted: bool, change: Option<Change>) -> AudioState {
        AudioState {
            available: true,
            default_sink: Some("analog".into()),
            sink_volume_pct: percent,
            sink_muted: muted,
            sink_controllable: true,
            max_volume_pct: 100,
            sink_change: change,
            ..AudioState::default()
        }
    }

    #[test]
    fn every_position_anchors_to_exactly_one_edge() {
        for (value, expected) in [
            ("bottom", Position::Bottom),
            ("top", Position::Top),
            ("left", Position::Left),
            ("right", Position::Right),
        ] {
            assert_eq!(Position::parse(value), expected);
        }
        assert_eq!(Position::parse("nonsense"), Position::Bottom);

        assert_eq!(Position::Bottom.anchor(), (Edge::Bottom, EDGE_MARGIN));
        assert_eq!(Position::Top.anchor(), (Edge::Top, EDGE_MARGIN));
        assert_eq!(Position::Left.anchor(), (Edge::Left, SIDE_MARGIN));
        assert_eq!(Position::Right.anchor(), (Edge::Right, SIDE_MARGIN));
    }

    #[test]
    fn a_side_capsule_stands_on_end() {
        assert_eq!(Position::Bottom.orientation(), Orientation::Horizontal);
        assert_eq!(Position::Top.orientation(), Orientation::Horizontal);
        assert_eq!(Position::Left.orientation(), Orientation::Vertical);
        assert_eq!(Position::Right.orientation(), Orientation::Vertical);
    }

    #[test]
    fn the_capsule_is_about_the_width_gnome_uses() {
        assert!(
            (200..=240).contains(&NOMINAL_WIDTH),
            "{NOMINAL_WIDTH}px is not a GNOME 42 capsule"
        );
    }

    #[test]
    fn the_icon_follows_the_value() {
        let volume = |percent, muted| {
            OsdEvent::Volume {
                percent,
                muted,
                max: 100,
            }
            .icon()
        };
        assert_eq!(volume(0, false), VOLUME_MUTED);
        assert_eq!(volume(50, true), VOLUME_MUTED, "muted outranks the value");
        assert_eq!(volume(20, false), VOLUME_LOW);
        assert_eq!(volume(50, false), VOLUME_MEDIUM);
        assert_eq!(volume(90, false), VOLUME_HIGH);
        assert_eq!(volume(140, false), VOLUME_HIGH);

        assert_eq!(OsdEvent::Brightness { percent: 0 }.icon(), BRIGHTNESS_OFF);
        assert_eq!(OsdEvent::Brightness { percent: 20 }.icon(), BRIGHTNESS_LOW);
        assert_eq!(
            OsdEvent::Brightness { percent: 50 }.icon(),
            BRIGHTNESS_MEDIUM
        );
        assert_eq!(
            OsdEvent::Brightness { percent: 100 }.icon(),
            BRIGHTNESS_HIGH
        );

        assert_eq!(OsdEvent::NoOutput.icon(), VOLUME_MUTED);
        assert_eq!(OsdEvent::Inhibitor { active: true }.icon(), INHIBIT_ON);
        assert_eq!(OsdEvent::Inhibitor { active: false }.icon(), INHIBIT_OFF);
    }

    #[test]
    fn every_icon_a_theme_may_not_have_has_a_stock_one_behind_it() {
        // The volume names are stock Adwaita and always resolve; the other two
        // are not, and a capsule with a missing-image square in it is worse
        // than one drawn with a plainer icon.
        for event in [
            OsdEvent::Brightness { percent: 50 },
            OsdEvent::Inhibitor { active: true },
            OsdEvent::Inhibitor { active: false },
        ] {
            let fallback = event.fallback_icon().expect("a fallback");
            assert!(fallback.ends_with("-symbolic"), "{fallback}");
            assert_ne!(fallback, event.icon());
        }
        assert_eq!(
            OsdEvent::Volume {
                percent: 50,
                muted: false,
                max: 100
            }
            .fallback_icon(),
            None
        );
    }

    #[test]
    fn a_muted_sink_draws_no_fill_at_all() {
        let muted = OsdEvent::Volume {
            percent: 60,
            muted: true,
            max: 100,
        };
        assert_eq!(muted.fill(), Some((0, 100)));
        assert_eq!(muted.value(), Some(0), "the number matches the bar");
    }

    #[test]
    fn the_inhibitor_and_the_missing_sink_are_icon_only() {
        assert_eq!(OsdEvent::Inhibitor { active: true }.fill(), None);
        assert_eq!(OsdEvent::Inhibitor { active: true }.caption(), None);
        assert_eq!(OsdEvent::NoOutput.fill(), None);
        assert_eq!(OsdEvent::NoOutput.caption(), Some(NO_OUTPUT));
        assert_eq!(OsdEvent::NoOutput.value(), None);
    }

    #[test]
    fn a_snapshot_with_no_change_on_it_shows_nothing() {
        let mut watcher = Watcher::new();
        assert_eq!(watcher.audio(&audio(40, false, None)), None);
        assert_eq!(watcher.brightness(&BrightnessState::default()), None);
    }

    #[test]
    fn a_media_key_raises_the_capsule() {
        let mut watcher = Watcher::new();
        assert_eq!(
            watcher.audio(&audio(40, false, Some(change(ChangeSource::Cli, 1)))),
            Some(OsdEvent::Volume {
                percent: 40,
                muted: false,
                max: 100
            })
        );
    }

    #[test]
    fn something_else_on_the_machine_raises_it_too() {
        let mut watcher = Watcher::new();
        assert!(
            watcher
                .audio(&audio(70, false, Some(change(ChangeSource::External, 1))))
                .is_some()
        );
    }

    #[test]
    fn a_quick_settings_slider_does_not() {
        let mut watcher = Watcher::new();
        assert_eq!(
            watcher.audio(&audio(70, false, Some(change(ChangeSource::Ui, 1)))),
            None,
            "the slider is its own feedback"
        );
    }

    #[test]
    fn the_same_change_is_only_shown_once() {
        let mut watcher = Watcher::new();
        let state = audio(40, false, Some(change(ChangeSource::Cli, 7)));
        assert!(watcher.audio(&state).is_some());
        assert_eq!(
            watcher.audio(&state),
            None,
            "a republished snapshot is not a new event"
        );
    }

    #[test]
    fn a_slider_change_is_consumed_so_it_cannot_replay() {
        let mut watcher = Watcher::new();
        // Suppressed — but remembered, so the next snapshot carrying the same
        // serial does not sneak past as though it were new.
        assert_eq!(
            watcher.audio(&audio(70, false, Some(change(ChangeSource::Ui, 4)))),
            None
        );
        assert_eq!(
            watcher.audio(&audio(70, false, Some(change(ChangeSource::Ui, 4)))),
            None
        );
        assert!(
            watcher
                .audio(&audio(75, false, Some(change(ChangeSource::Cli, 5))))
                .is_some()
        );
    }

    #[test]
    fn a_sink_that_went_away_says_so() {
        let mut watcher = Watcher::new();
        let state = AudioState {
            available: true,
            default_sink: None,
            max_volume_pct: 100,
            sink_change: Some(change(ChangeSource::Cli, 1)),
            ..AudioState::default()
        };
        assert_eq!(watcher.audio(&state), Some(OsdEvent::NoOutput));
    }

    #[test]
    fn overdrive_reaches_past_the_bar_but_only_when_it_is_allowed() {
        let mut watcher = Watcher::new();
        let mut state = audio(140, false, Some(change(ChangeSource::Cli, 1)));
        state.max_volume_pct = 153;
        assert_eq!(
            watcher.audio(&state),
            Some(OsdEvent::Volume {
                percent: 140,
                muted: false,
                max: 153
            })
        );
        // With the live config's policy the ceiling is 100 and the urgent
        // segment can never be reached.
        assert_eq!(audio(100, false, None).max_volume_pct, 100);
    }

    #[test]
    fn the_backlight_follows_the_same_rules() {
        let mut watcher = Watcher::new();
        let state = BrightnessState {
            available: true,
            percent: 30,
            device: Some("intel_backlight".into()),
            change: Some(change(ChangeSource::External, 3)),
        };
        assert_eq!(
            watcher.brightness(&state),
            Some(OsdEvent::Brightness { percent: 30 })
        );
        assert_eq!(watcher.brightness(&state), None);

        let ui = BrightnessState {
            change: Some(change(ChangeSource::Ui, 4)),
            ..state
        };
        assert_eq!(watcher.brightness(&ui), None);
    }

    #[test]
    fn the_inhibitor_shows_the_flip_and_not_the_starting_state() {
        let mut watcher = Watcher::new();
        let off = InhibitorState {
            available: true,
            active: false,
        };
        assert_eq!(
            watcher.inhibitor(&off),
            None,
            "starting off is not an event"
        );

        let on = InhibitorState {
            available: true,
            active: true,
        };
        assert_eq!(
            watcher.inhibitor(&on),
            Some(OsdEvent::Inhibitor { active: true })
        );
        assert_eq!(watcher.inhibitor(&on), None, "and only once");
        assert_eq!(
            watcher.inhibitor(&off),
            Some(OsdEvent::Inhibitor { active: false })
        );
    }

    #[test]
    fn an_inhibitor_that_is_not_there_shows_nothing_and_forgets() {
        let mut watcher = Watcher::new();
        let on = InhibitorState {
            available: true,
            active: true,
        };
        assert_eq!(watcher.inhibitor(&on), None, "the baseline");

        let gone = InhibitorState {
            available: false,
            active: false,
        };
        assert_eq!(watcher.inhibitor(&gone), None);
        // logind came back with the lock lost; that is a fresh baseline, not a
        // flip the user asked for.
        assert_eq!(
            watcher.inhibitor(&InhibitorState {
                available: true,
                active: false
            }),
            None
        );
    }

    #[test]
    fn the_colours_come_from_the_configured_palette() {
        let mut config = Config::default();
        config.theme.accent = "#00ff00".to_string();
        config.theme.states.urgent = "#ff0000".to_string();
        let palette = colors(&config);
        assert_eq!(palette.accent, gdk::RGBA::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(palette.urgent, gdk::RGBA::new(1.0, 0.0, 0.0, 1.0));

        config.theme.accent = "not a colour".to_string();
        assert_eq!(colors(&config).accent.alpha(), 1.0);
    }
}
