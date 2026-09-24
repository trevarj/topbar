//! The shared popover surface: one host window per monitor.
//!
//! GTK's own `GtkPopover` positions itself against a toplevel, which a
//! layer-shell bar is not, so the panel puts its menus on a layer surface of
//! their own. Exactly one host exists per monitor and it is reused for every
//! widget's popover, which is what keeps opening a menu free of window
//! allocation:
//!
//! ```text
//! window .popover-window          layer Top, anchored under the bar
//! └── .popover-wrapper            reserves room for the drop shadow
//!     └── scale-box               the open/close animation (clip, not transform)
//!         └── .popover-surface    the widget's retained content
//!
//! window .click-catcher-window    the rest of the monitor; a click dismisses
//! ```
//!
//! The catcher is a second, transparent layer surface below the popover. It
//! leaves the bar itself uncovered — the compositor's own exclusive-zone
//! arithmetic does that, since the catcher asks for a zone of zero — so
//! clicking the button that opened a popover reaches the button and toggles it
//! shut, and the bar keeps its hover states while a menu is open.
//!
//! Only the host knows which widget is on screen, which is how "exactly one
//! popover open at a time" is structural rather than a rule every widget has
//! to remember.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Window, gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use topbar_core::Config;
use tracing::debug;

use crate::anim::{Animation, AnimationParams, Easing, ScaleBox, motion_enabled};
use crate::fonts::{self, FontRendering};
use crate::style::{self, classes};
use crate::wayland::blur::{self, BlurAttachment};

/// Minimum gap between the popover surface and the monitor edges.
const EDGE_MARGIN: i32 = 8;
/// Room left around the surface for its drop shadow.
///
/// Equal to [`EDGE_MARGIN`] on purpose: a surface clamped to the edge then
/// puts the *window* flush against it, so the shadow is never cut off and the
/// window never overhangs the monitor.
const SHADOW_MARGIN: i32 = 8;
/// Width assumed when the content cannot be measured yet.
const FALLBACK_WIDTH: i32 = 360;
/// How long a popover takes to open.
const OPEN_MS: u64 = 200;
/// How long it takes to close. Shorter than opening, GNOME style.
const CLOSE_MS: u64 = 150;
/// The scale a popover grows from, and shrinks back to.
const SCALE_FROM: f64 = 0.95;
/// Width of the outline drawn on the animating clip boundary.
const OUTLINE_WIDTH: f32 = 1.0;

// ---------------------------------------------------------------------------
// Motion — the open/close state machine, with no GTK in it
// ---------------------------------------------------------------------------

/// Where a popover is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Off screen.
    Closed,
    /// Growing in.
    Opening,
    /// Fully shown.
    Open,
    /// Fading out; still on screen.
    Closing,
}

/// One segment of animation: where to go, and how long it may take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Run {
    /// Visible progress the segment starts from.
    pub from: f64,
    /// Visible progress it ends at: 1.0 opening, 0.0 closing.
    pub to: f64,
    /// Duration in milliseconds, scaled by the distance left to travel.
    pub duration_ms: u64,
}

/// The open/close state machine.
///
/// `progress` is how visible the popover is, `0.0..=1.0`, and it is the whole
/// trick behind mid-flight reversal: a close that interrupts an open starts
/// from wherever the open had got to and takes proportionally less time, so
/// rapid clicking never snaps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    phase: Phase,
    progress: f64,
}

impl Default for Motion {
    fn default() -> Self {
        Self::new()
    }
}

impl Motion {
    /// A closed popover.
    pub const fn new() -> Self {
        Self {
            phase: Phase::Closed,
            progress: 0.0,
        }
    }

    /// The current phase. Read by the state-machine tests; the host itself
    /// only ever asks [`Motion::is_open`].
    #[cfg(test)]
    pub fn phase(self) -> Phase {
        self.phase
    }

    /// How visible the popover is right now.
    #[cfg(test)]
    pub fn progress(self) -> f64 {
        self.progress
    }

    /// Whether the popover counts as open for toggling purposes.
    ///
    /// True from the moment [`Motion::open`] is called until [`Motion::close`]
    /// is, so a click during the close animation reopens rather than closing
    /// something that is already on its way out.
    pub fn is_open(self) -> bool {
        matches!(self.phase, Phase::Opening | Phase::Open)
    }

    /// Head for open. `None` when it is already open or on its way there.
    pub fn open(&mut self) -> Option<Run> {
        if self.is_open() {
            return None;
        }
        self.phase = Phase::Opening;
        Some(Run {
            from: self.progress,
            to: 1.0,
            duration_ms: scaled(OPEN_MS, 1.0 - self.progress),
        })
    }

    /// Head for closed. `None` when it is already closed or on its way there.
    pub fn close(&mut self) -> Option<Run> {
        if !self.is_open() {
            return None;
        }
        self.phase = Phase::Closing;
        Some(Run {
            from: self.progress,
            to: 0.0,
            duration_ms: scaled(CLOSE_MS, self.progress),
        })
    }

    /// Record the visible progress of the frame just drawn.
    pub fn advance(&mut self, progress: f64) {
        self.progress = progress.clamp(0.0, 1.0);
    }

    /// A run reached its end.
    pub fn settle(&mut self) {
        match self.phase {
            Phase::Opening | Phase::Open => {
                self.phase = Phase::Open;
                self.progress = 1.0;
            }
            Phase::Closing | Phase::Closed => {
                self.phase = Phase::Closed;
                self.progress = 0.0;
            }
        }
    }
}

/// A full-length duration scaled by the fraction of the distance left.
fn scaled(full_ms: u64, distance: f64) -> u64 {
    (full_ms as f64 * distance.clamp(0.0, 1.0)).round() as u64
}

// ---------------------------------------------------------------------------
// Placement — where the surface lands, with no GTK in it either
// ---------------------------------------------------------------------------

/// Left margin for the popover window, in monitor-local pixels.
///
/// The surface is centred under `anchor_center_x` and clamped so it keeps
/// [`EDGE_MARGIN`] clear of both monitor edges; a surface wider than the
/// monitor starts at the left margin and overflows to the right, which at
/// least keeps its beginning readable.
///
/// The returned value is for the *window*, which is [`SHADOW_MARGIN`] wider on
/// each side than the surface it contains.
pub fn window_left(anchor_center_x: i32, surface_width: i32, monitor_width: i32) -> i32 {
    let max_left = (monitor_width - surface_width - EDGE_MARGIN).max(EDGE_MARGIN);
    let surface_left = (anchor_center_x - surface_width / 2).clamp(EDGE_MARGIN, max_left);
    (surface_left - SHADOW_MARGIN).max(0)
}

/// Top margin for the popover window, in pixels.
///
/// The window asks for an exclusive zone of zero, so the compositor already
/// anchors it below the bar's own zone; `bar.popover_offset` is the only gap
/// left to add. The wrapper reserves no shadow room above the surface — that
/// space would sit behind the opaque bar — so the margin needs no correction.
pub fn window_top(config: &Config) -> i32 {
    config.bar.popover_offset as i32
}

/// How far down the monitor a surface that honours exclusive zones starts.
///
/// The bar's own reserved height. The compositor takes it off the top before it
/// places anything else, so a coordinate on the monitor is this plus whatever
/// margin the surface asked for. GDK4 has no work-area call, so the number is
/// read off the layer surface that reserved it — which is the bar.
pub fn bar_height() -> i32 {
    use gtk4_layer_shell::LayerShell;

    // By index rather than through the typed iterator: GTK reports the
    // toplevel list's item type as `GtkWidget`, and asking gio for `GtkWindow`s
    // out of it asserts.
    let toplevels = gtk4::Window::toplevels();
    (0..toplevels.n_items())
        .filter_map(|index| toplevels.item(index))
        .filter_map(|object| object.downcast::<gtk4::Window>().ok())
        .filter(LayerShell::is_layer_window)
        .map(|window| LayerShell::exclusive_zone(&window))
        .find(|zone| *zone > 0)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// The popover that is on screen: what to draw, and who opened it.
#[derive(Clone)]
pub struct Anchored {
    /// Widget name, e.g. `clock`. Identity for the one-open-at-a-time rule.
    pub name: String,
    /// The retained content parented into the host.
    pub content: gtk4::Widget,
    /// The panel button that owns it. Wears `.checked` while open, and its
    /// centre is what the surface is aligned to.
    pub anchor: gtk4::Widget,
    /// Re-render from current state. Runs on every open.
    pub refresh: Rc<dyn Fn()>,
    /// The content has left the screen. Runs when it is unparented, which is
    /// either the end of the close animation or another popover taking its
    /// place.
    pub closed: Rc<dyn Fn()>,
}

/// One monitor's popover host.
///
/// Created with its bar and dropped with it, so a monitor going away takes its
/// surfaces with it. Both windows are built up front but stay unmapped until
/// something is actually opened.
pub struct LayerPopover {
    window: Window,
    catcher: Window,
    shell: ScaleBox,
    anim: Animation,
    /// The monitor both surfaces are pinned to, for the clamping arithmetic.
    monitor: gdk::Monitor,
    /// Shared with the per-frame closure, which must not keep the host alive.
    motion: Rc<Cell<Motion>>,
    /// Gap between the bar and the surface, from `bar.popover_offset`.
    top_margin: i32,
    /// The font settings for the glyph-clipping workaround, re-applied on
    /// every open. `None` when `advanced.pango_font_rendering` is off.
    font_rendering: Option<FontRendering>,
    /// Whoever is open. `None` once the close animation has finished.
    open: RefCell<Option<Anchored>>,
    /// The blur behind the surface. Suspended for the length of every close.
    blur: BlurAttachment,
}

impl LayerPopover {
    /// Build the host for `monitor`, `top_margin` pixels below the bar.
    ///
    /// `font_rendering` carries the Pango glyph-clipping workaround's settings;
    /// see [`Self::open`] for why it is applied per-open rather than once.
    ///
    /// Nothing is shown until [`Self::open`]; both surfaces stay unmapped.
    pub fn new(
        monitor: &gdk::Monitor,
        top_margin: i32,
        font_rendering: Option<FontRendering>,
    ) -> Rc<Self> {
        // The catcher is created first so the compositor stacks it below the
        // popover: layer surfaces within one layer keep their creation order.
        let catcher = build_catcher(monitor);
        let window = build_window(monitor);

        let shell = ScaleBox::new();
        shell.set_radius(style::POPOVER_RADIUS as f32);
        shell.set_opacity(0.0);
        shell.set_scale(SCALE_FROM);
        // The shadow needs room to render, and GTK clips it to the surface.
        // Nothing is reserved above: that space is behind the bar.
        shell.set_margin_start(SHADOW_MARGIN);
        shell.set_margin_end(SHADOW_MARGIN);
        shell.set_margin_bottom(SHADOW_MARGIN);

        let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        wrapper.add_css_class(classes::POPOVER_WRAPPER);
        wrapper.append(&shell);
        window.set_child(Some(&wrapper));

        let host = Rc::new(Self {
            anim: Animation::new(&shell),
            // The blurred area is the surface, not the window: the wrapper
            // around it is transparent room for the drop shadow.
            blur: blur::attach(&window, &shell, || style::POPOVER_RADIUS as i32),
            window,
            catcher,
            shell,
            monitor: monitor.clone(),
            motion: Rc::new(Cell::new(Motion::new())),
            top_margin,
            font_rendering,
            open: RefCell::new(None),
        });

        host.install_dismissal();
        host.install_focus_visibility();
        host
    }

    /// The widget whose popover is on screen, if any.
    ///
    /// Reports the widget from the moment it is opened until the close
    /// animation finishes, so a reopen mid-close is recognised as a reopen.
    pub fn open_widget(&self) -> Option<String> {
        self.open.borrow().as_ref().map(|open| open.name.clone())
    }

    /// Whether `name`'s popover is open (or on its way out but reopenable).
    pub fn is_open(&self, name: &str) -> bool {
        self.motion.get().is_open() && self.open_widget().as_deref() == Some(name)
    }

    /// Open `target`, closing whatever else was open.
    ///
    /// Opening a second widget's popover swaps the content in place rather
    /// than closing and reopening the surface: there is no frame where the
    /// monitor has no menu on it.
    pub fn open(self: &Rc<Self>, target: Anchored) {
        let replacing = self
            .open
            .borrow()
            .as_ref()
            .is_some_and(|open| open.name != target.name);
        if replacing {
            self.detach();
        }

        if self.open.borrow().is_none() {
            self.shell.set_child(&target.content);
            target.anchor.add_css_class(classes::CHECKED);
            *self.open.borrow_mut() = Some(target);
        }

        // Refresh-on-open: everything a popover shows is re-rendered from
        // current state each time it appears, so nothing can go stale while
        // the content sits retained and unparented.
        let refresh = self
            .open
            .borrow()
            .as_ref()
            .map(|open| Rc::clone(&open.refresh));
        if let Some(refresh) = refresh {
            refresh();
        }

        // The glyph-clipping workaround the bar applies once at build, applied
        // here instead: a popover is a layer surface too, so its labels clip the
        // same tall glyphs the bar's did, and it is applied *after* refresh so
        // the rows a panel rebuilds on every open — the notification history
        // rebuilds its groups each time — are covered. See fonts.rs.
        if let (Some(rendering), Some(open)) = (&self.font_rendering, self.open.borrow().as_ref()) {
            fonts::render_tree(rendering, &open.content);
        }

        self.place();
        // ...and again once GTK has actually laid the content out. The first
        // measurement of a freshly parented tree is taken before it has been
        // through a size negotiation, and it can come back far wider than the
        // surface ends up being — which puts the window's left margin at zero
        // and slides a 360px panel to the far edge of the monitor. It happened
        // in five of eight smoke scenarios and in none of the other three,
        // which is what a race looks like.
        self.replace_soon();

        // Order matters: the catcher maps first so it stays below the popover.
        self.catcher.set_visible(true);
        // Keyboard focus is taken only while a popover is up, and handed back
        // to the compositor the moment it starts closing. It has to be set
        // before the surface maps — a layer surface's interactivity is part of
        // the state the compositor reads at map time.
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        self.window.present();
        // No focus ring until the user actually reaches for the keyboard: no
        // widget holds the focus, and the class that would draw a ring around
        // one is off. Both are reset on every open, so a popover dismissed
        // mid-keyboard does not reopen wearing the last ring it drew.
        gtk4::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk4::Widget>);
        self.window.remove_css_class(classes::KEYBOARD);

        // A reopen that catches a close mid-fade never unmapped, so the map
        // that normally revives the blur is not coming.
        self.blur.resume();

        let mut motion = self.motion.get();
        let run = motion.open();
        self.motion.set(motion);
        if let Some(run) = run {
            self.animate(run);
        }
    }

    /// Close whatever is open.
    pub fn close(self: &Rc<Self>) {
        let mut motion = self.motion.get();
        let Some(run) = motion.close() else {
            return;
        };
        self.motion.set(motion);

        // The compositor blurs what is behind a surface whatever the surface's
        // own opacity is, so the region comes off as the fade *starts*.
        // Leaving it would put a rectangle of blurred desktop on screen for the
        // length of the close, with a surface fading to nothing over it.
        self.blur.suspend();

        // The bar becomes live again immediately, so the button that opened
        // this popover can reopen it mid-fade.
        self.catcher.set_visible(false);
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.animate(run);
    }

    /// Toggle `target`: close it if it is the one on screen, else open it.
    pub fn toggle(self: &Rc<Self>, target: Anchored) {
        if self.is_open(&target.name) {
            self.close();
        } else {
            self.open(target);
        }
    }

    /// Place the surface again, once and then again after the next frame.
    ///
    /// Twice because the two cheap moments to ask are both unreliable on their
    /// own: an idle runs before the frame that allocates the content, and a
    /// frame callback can be seconds away on a compositor that is throttling
    /// this surface. Placing an already-placed popover costs one margin write.
    fn replace_soon(self: &Rc<Self>) {
        let host = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(host) = host.upgrade() {
                host.replace_if_open();
            }
        });

        let host = Rc::downgrade(self);
        self.shell.add_tick_callback(move |_, _| {
            if let Some(host) = host.upgrade() {
                host.replace_if_open();
            }
            glib::ControlFlow::Break
        });
    }

    /// Place the surface again, if there is still something on it.
    fn replace_if_open(&self) {
        if self.open.borrow().is_some() {
            self.place();
        }
    }

    /// Position the window under its anchor for the width the content wants.
    fn place(&self) {
        let width = self.surface_width();
        let monitor_width = self.monitor.geometry().width();
        let center = self
            .open
            .borrow()
            .as_ref()
            .and_then(|open| anchor_center_x(&open.anchor))
            .unwrap_or(monitor_width / 2);

        self.window.set_margin(Edge::Top, self.top_margin);
        self.window
            .set_margin(Edge::Left, window_left(center, width, monitor_width));
        self.window.set_default_size(width + 2 * SHADOW_MARGIN, -1);
    }

    /// The width the content asks for, before the shadow margins.
    fn surface_width(&self) -> i32 {
        let (_, natural, _, _) = self.shell.measure(gtk4::Orientation::Horizontal, -1);
        if natural > 0 {
            natural.saturating_sub(2 * SHADOW_MARGIN)
        } else {
            FALLBACK_WIDTH
        }
    }

    /// Run one segment of the open/close animation.
    fn animate(self: &Rc<Self>, run: Run) {
        let opening = run.to > run.from;
        let easing = if opening {
            Easing::EaseOutCubic
        } else {
            Easing::EaseInCubic
        };

        // The CSS border sits on the full-size content and would be clipped
        // away for the whole run, so the boundary is drawn by the ScaleBox
        // instead while motion is in flight.
        if motion_enabled() {
            self.shell.set_outline(OUTLINE_WIDTH);
            if let Some(open) = self.open.borrow().as_ref() {
                open.content.add_css_class(classes::BORDERLESS);
            }
        }

        let on_frame = {
            let shell = self.shell.clone();
            let motion = Rc::clone(&self.motion);
            let host = Rc::downgrade(self);
            move |eased: f64| {
                let progress = run.from + (run.to - run.from) * eased;
                shell.set_opacity(progress);
                shell.set_scale(SCALE_FROM + (1.0 - SCALE_FROM) * progress);
                // The blurred area follows how *visible* the surface is rather
                // than how big it is drawn: the compositor cannot blur at an
                // opacity, so the region grows in with the popover instead of
                // arriving whole behind something still fading up.
                if let Some(host) = host.upgrade() {
                    host.blur.set_scale(progress);
                }
                let mut current = motion.get();
                current.advance(progress);
                motion.set(current);
            }
        };

        let on_done = {
            let host = Rc::downgrade(self);
            move || {
                if let Some(host) = host.upgrade() {
                    host.settle(opening);
                }
            }
        };

        self.anim.start(
            AnimationParams::new(run.duration_ms).with_easing(easing),
            Box::new(on_frame),
            Some(Box::new(on_done)),
        );
    }

    /// Finish a run. Superseded runs never reach here — [`Animation`] drops
    /// their done callback — so this only ever sees the state that won.
    fn settle(&self, opening: bool) {
        let mut motion = self.motion.get();
        motion.settle();
        self.motion.set(motion);

        self.shell.set_outline(0.0);
        if let Some(open) = self.open.borrow().as_ref() {
            open.content.remove_css_class(classes::BORDERLESS);
        }

        if opening {
            self.shell.set_opacity(1.0);
            self.shell.set_scale(1.0);
            self.blur.set_scale(1.0);
            return;
        }

        self.shell.set_opacity(0.0);
        self.shell.set_scale(SCALE_FROM);
        self.window.set_visible(false);
        self.detach();
    }

    /// Unparent the content and hand the button back its resting state.
    ///
    /// The content itself is *not* dropped: the widget that owns it keeps it
    /// for its own lifetime and hands the same tree back on the next open.
    fn detach(&self) {
        self.shell.remove_child();
        let open = self.open.borrow_mut().take();
        if let Some(open) = open {
            open.anchor.remove_css_class(classes::CHECKED);
            // Told after it is unparented, and outside the borrow: content is
            // free to do whatever it likes here, including opening something.
            (open.closed)();
            debug!("popover for `{}` released", open.name);
        }
    }

    /// Click-away and Escape both dismiss.
    fn install_dismissal(self: &Rc<Self>) {
        let click = gtk4::GestureClick::new();
        click.set_button(0);
        // Released, not pressed: letting GTK finish the gesture before the
        // surface goes away avoids its "broken accounting" warnings.
        click.connect_released({
            let host = Rc::downgrade(self);
            move |_, _, _, _| {
                if let Some(host) = host.upgrade() {
                    host.close();
                }
            }
        });
        self.catcher.add_controller(click);

        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed({
            let host = Rc::downgrade(self);
            move |_, key, _, _| {
                if key != gdk::Key::Escape {
                    return glib::Propagation::Proceed;
                }
                if let Some(host) = host.upgrade() {
                    host.close();
                }
                glib::Propagation::Stop
            }
        });
        self.window.add_controller(keys);
    }

    /// Mark the surface as keyboard-driven from the first key pressed in it
    /// until the pointer is used again. It is what draws the focus ring.
    ///
    /// GTK's own answer to this is `:focus-visible`, and on a layer surface it
    /// does not work. The flag behind that selector is stamped onto a widget
    /// by the code that gives a window's focus away, and it is stamped from
    /// state GTK maintains for a window the compositor has *activated* — which
    /// a layer surface never is, because there is no xdg-toplevel to activate.
    /// Setting `GtkWindow:focus-visible` by hand does not rescue it either: the
    /// property reads back true, on the right widget, and the ring still is not
    /// drawn, because nothing restamps a widget that already has the focus.
    ///
    /// So the panel keeps the state itself. `.keyboard` on the window means the
    /// user has reached for the keyboard, and the stylesheet hangs its rings on
    /// `.keyboard <control>:focus` — the same rule `:focus-visible` was written
    /// to express, made out of parts that a layer surface has. Both halves
    /// matter: without the second, one Tab would leave every control clicked
    /// afterwards wearing a ring.
    ///
    /// A legacy controller rather than a gesture because it observes without
    /// ever claiming an event sequence, so nothing below it loses a press it
    /// would otherwise have received; and in the capture phase so the class is
    /// on before GTK moves the focus the key asked for, and the control the
    /// focus lands on is drawn ringed on its first frame rather than the next.
    fn install_focus_visibility(self: &Rc<Self>) {
        let events = gtk4::EventControllerLegacy::new();
        events.set_propagation_phase(gtk4::PropagationPhase::Capture);
        events.connect_event({
            // Weak: the controller belongs to the window it is watching, and a
            // strong handle here would be a window keeping itself alive.
            let window = self.window.downgrade();
            move |_, event| {
                let Some(window) = window.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                match event.event_type() {
                    gdk::EventType::KeyPress => window.add_css_class(classes::KEYBOARD),
                    gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                        window.remove_css_class(classes::KEYBOARD);
                    }
                    _ => {}
                }
                glib::Propagation::Proceed
            }
        });
        self.window.add_controller(events);
    }
}

impl Drop for LayerPopover {
    fn drop(&mut self) {
        // The catcher must never outlive the popover it belongs to: a
        // fullscreen surface eating clicks with nothing on screen to dismiss
        // would look exactly like a frozen session.
        self.catcher.close();
        self.window.close();
    }
}

/// The popover's own layer surface.
fn build_window(monitor: &gdk::Monitor) -> Window {
    let window = Window::builder().decorated(false).resizable(false).build();
    window.add_css_class(classes::POPOVER_WINDOW);

    window.init_layer_shell();
    window.set_namespace(Some("topbar-popover"));
    // Top, not Overlay: a menu has no business covering a fullscreen video.
    window.set_layer(Layer::Top);
    window.set_monitor(Some(monitor));
    // Zero, not -1: the compositor then anchors the surface below the bar's
    // exclusive zone, which is exactly where a popover belongs.
    window.set_exclusive_zone(0);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_keyboard_mode(KeyboardMode::None);
    window
}

/// The transparent surface that turns a click anywhere else into a dismissal.
fn build_catcher(monitor: &gdk::Monitor) -> Window {
    let window = Window::builder().decorated(false).build();
    window.add_css_class(classes::CLICK_CATCHER_WINDOW);

    window.init_layer_shell();
    window.set_namespace(Some("topbar-click-catcher"));
    window.set_layer(Layer::Top);
    window.set_monitor(Some(monitor));
    // A zone of zero fills what is left after the bar's, so the bar keeps its
    // hover states and its buttons keep toggling while a popover is open.
    window.set_exclusive_zone(0);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    // The catcher exists for clicks alone; the popover owns the keyboard.
    window.set_keyboard_mode(KeyboardMode::None);

    let surface = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    surface.add_css_class(classes::CLICK_CATCHER);
    surface.set_hexpand(true);
    surface.set_vexpand(true);
    window.set_child(Some(&surface));
    window
}

/// The horizontal centre of `widget` in monitor coordinates.
///
/// Bar windows span the monitor and are anchored left, so a point in the bar
/// window's coordinates is already a point on the monitor.
fn anchor_center_x(widget: &gtk4::Widget) -> Option<i32> {
    let root = widget.root()?;
    let centre = gtk4::graphene::Point::new(widget.width() as f32 / 2.0, 0.0);
    let point = widget.compute_point(root.upcast_ref::<gtk4::Widget>(), &centre)?;
    Some(point.x() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_from_closed_takes_the_full_duration() {
        let mut motion = Motion::new();
        let run = motion.open().expect("a closed popover can open");
        assert_eq!(
            run,
            Run {
                from: 0.0,
                to: 1.0,
                duration_ms: OPEN_MS
            }
        );
        assert_eq!(motion.phase(), Phase::Opening);
        assert!(motion.is_open());
    }

    #[test]
    fn closing_from_open_takes_the_full_duration() {
        let mut motion = Motion::new();
        motion.open();
        motion.settle();
        assert_eq!(motion.phase(), Phase::Open);

        let run = motion.close().expect("an open popover can close");
        assert_eq!(
            run,
            Run {
                from: 1.0,
                to: 0.0,
                duration_ms: CLOSE_MS
            }
        );
        assert_eq!(motion.phase(), Phase::Closing);
    }

    #[test]
    fn a_reversal_only_pays_for_the_distance_left() {
        let mut motion = Motion::new();
        motion.open();
        motion.advance(0.5);

        let run = motion.close().expect("an opening popover can reverse");
        assert_eq!(run.from, 0.5);
        assert_eq!(run.to, 0.0);
        assert_eq!(run.duration_ms, CLOSE_MS / 2);
    }

    #[test]
    fn reopening_mid_close_resumes_from_where_it_got_to() {
        let mut motion = Motion::new();
        motion.open();
        motion.settle();
        motion.close();
        motion.advance(0.25);
        assert!(!motion.is_open(), "a closing popover reads as closed");

        let run = motion.open().expect("a closing popover can reopen");
        assert_eq!(run.from, 0.25);
        assert_eq!(run.to, 1.0);
        assert_eq!(run.duration_ms, scaled(OPEN_MS, 0.75));
        assert!(motion.is_open());
    }

    #[test]
    fn repeated_requests_in_the_same_direction_do_nothing() {
        let mut motion = Motion::new();
        assert!(motion.open().is_some());
        assert!(motion.open().is_none(), "already opening");
        motion.settle();
        assert!(motion.open().is_none(), "already open");

        assert!(motion.close().is_some());
        assert!(motion.close().is_none(), "already closing");
        motion.settle();
        assert!(motion.close().is_none(), "already closed");
    }

    #[test]
    fn settling_pins_progress_to_an_endpoint() {
        let mut motion = Motion::new();
        motion.open();
        motion.advance(0.87);
        motion.settle();
        assert_eq!(motion.progress(), 1.0);

        motion.close();
        motion.advance(0.13);
        motion.settle();
        assert_eq!(motion.progress(), 0.0);
        assert_eq!(motion.phase(), Phase::Closed);
    }

    #[test]
    fn progress_never_leaves_its_range() {
        let mut motion = Motion::new();
        motion.open();
        motion.advance(1.4);
        assert_eq!(motion.progress(), 1.0);
        motion.advance(-0.3);
        assert_eq!(motion.progress(), 0.0);
    }

    #[test]
    fn a_popover_is_centred_under_its_anchor() {
        // 300px surface under x=500 puts the surface at 350 and the window,
        // which is 8px wider on each side, at 342.
        assert_eq!(window_left(500, 300, 1000), 342);
    }

    #[test]
    fn a_popover_keeps_clear_of_the_monitor_edges() {
        // Hard against the left: the surface stops at EDGE_MARGIN and the
        // window, shadow and all, sits flush with the monitor.
        assert_eq!(window_left(10, 300, 1000), 0);
        // Hard against the right: 1000 - 300 - 8 = 692 for the surface.
        assert_eq!(window_left(990, 300, 1000), 692 - SHADOW_MARGIN);
    }

    #[test]
    fn the_window_never_overhangs_the_monitor() {
        let monitor = 1920;
        for center in [0, 1, 200, 960, 1700, 1919, 5000] {
            // Widths that fit the monitor with both margins; a surface too
            // wide for the screen is covered by its own test below.
            for surface in [120, 360, 765, 1200] {
                let left = window_left(center, surface, monitor);
                assert!(left >= 0, "left {left} for centre {center}");
                assert!(
                    left + surface + 2 * SHADOW_MARGIN <= monitor,
                    "surface {surface} at centre {center} overhangs"
                );
            }
        }
    }

    #[test]
    fn a_popover_wider_than_the_monitor_starts_on_screen() {
        assert_eq!(window_left(500, 2000, 1000), 0);
    }

    #[test]
    fn the_offset_is_the_only_gap_the_panel_adds() {
        // The compositor anchors an exclusive-zone-zero surface below the
        // bar, so the panel's own margin is just `bar.popover_offset`.
        let mut config = Config::default();
        config.bar.popover_offset = 1;
        assert_eq!(window_top(&config), 1);

        config.bar.popover_offset = 12;
        assert_eq!(window_top(&config), 12);
    }
}
