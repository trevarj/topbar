//! Banners: the notification surface under the bar.
//!
//! ```text
//! window .toast-window        layer Overlay, anchored top-centre under the bar
//! └── .toast-stack            a column of at most three banners
//!     └── slide-box           the arrive/leave motion (see anim::SlideBox)
//!         └── .toast          one banner
//! ```
//!
//! One surface exists per monitor and only the one on the focused output ever
//! shows anything, so a banner appears where the user is looking rather than on
//! all of them at once. The surface is unmapped whenever it is empty, which is
//! what keeps a transparent window from eating clicks on the desktop.
//!
//! A banner's slot is its final size the moment it exists; the arrival and
//! departure are drawn inside that slot (see [`SlideBox`]). The surface
//! therefore reconfigures once per banner rather than once per frame — which
//! on a layer surface is the difference between motion and a stall, because a
//! configure is a round trip to the compositor. The gap left by a banner
//! leaving closes in one step for the same reason.
//!
//! **The timer is not here.** Which banners exist, and how long each has left,
//! belongs to the service; hovering one is a `pause_toast` call rather than a
//! local `SourceId`. A banner replaced over D-Bus mid-hover therefore cannot
//! end up with two timers, which is the shape of bug the v1 toast manager had.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{Align, Button, Label, Orientation, Window, gdk, pango};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use topbar_core::Config;
use topbar_services::{NotifState, Services, ToastView};
use tracing::debug;

use crate::anim::{Animation, AnimationParams, Easing, SlideBox, ripple};
use crate::bridge::{self, ActionScope, BindingGuard};
use crate::style::{self, classes};
use crate::surfaces::layer_popover;
use crate::wayland::activation;
use crate::wayland::blur::{self, BlurAttachment};
use crate::widgets::notifications::{self as notifications, TOAST_ICON, icon, markup};

/// Width of a banner, in pixels.
const WIDTH: i32 = 380;
/// Gap between stacked banners.
const GAP: i32 = 8;
/// Room left around the stack for the banners' drop shadows.
const SHADOW_MARGIN: i32 = 8;
/// How long a banner takes to arrive, and to leave.
const SLIDE_MS: u64 = 150;
/// Most action buttons drawn on one banner.
const MAX_ACTIONS: usize = 3;
/// Widest an action's label is allowed to be before it ellipsizes.
///
/// Three of them have to fit across 380 pixels, and a sender is free to write
/// a sentence on one.
const MAX_ACTION_CHARS: i32 = 18;
/// Where a banner's own failures are reported.
const SCOPE: ActionScope = ActionScope::Toast { widget: "toast" };

/// How far below the top edge the banners sit, in pixels.
///
/// Stated outright rather than left to the compositor. A popover asks for an
/// exclusive zone of zero and lands below the bar because both are on the Top
/// layer; banners are on Overlay, and niri does not carry the bar's zone across
/// layers, so a banner asking the same question lands *behind* the panel. The
/// margin is therefore the bar's own height plus the gap popovers use, and the
/// surface ignores exclusive zones entirely.
pub fn top_margin(config: &Config) -> i32 {
    style::window_height(config) + layer_popover::window_top(config)
}

/// Which monitor should be showing banners.
///
/// The focused output, when the compositor names one the panel actually has a
/// bar on; otherwise the first, so a banner is never posted to nowhere at all —
/// during a hotplug, or before niri has reported anything.
pub fn hosting_output<'a>(focused: Option<&str>, connectors: &'a [String]) -> Option<&'a str> {
    if let Some(focused) = focused
        && let Some(found) = connectors.iter().find(|name| *name == focused)
    {
        return Some(found.as_str());
    }
    connectors.first().map(String::as_str)
}

/// One monitor's banner surface.
pub struct ToastSurface {
    window: Window,
    stack: gtk4::Box,
    /// The banners on screen, newest first.
    cards: Rc<RefCell<Vec<Card>>>,
    services: Services,
    connector: String,
    /// How far below the top edge the stack sits. See [`top_margin`].
    top_margin: i32,
    /// Subscriptions that keep this surface in step with the services.
    bindings: RefCell<Vec<BindingGuard>>,
    /// The blur behind the stack of banners.
    blur: BlurAttachment,
}

impl ToastSurface {
    /// Build the surface for `monitor` and subscribe it to the daemon.
    ///
    /// Nothing is mapped until a banner arrives.
    pub fn new(
        monitor: &gdk::Monitor,
        connector: &str,
        config: &Config,
        services: &Services,
    ) -> Rc<Self> {
        let window = build_window(monitor, top_margin(config) - SHADOW_MARGIN);

        let stack = gtk4::Box::new(Orientation::Vertical, GAP);
        stack.add_css_class(classes::TOAST_STACK);
        stack.set_size_request(WIDTH, -1);
        stack.set_valign(Align::Start);
        stack.set_margin_top(SHADOW_MARGIN);
        stack.set_margin_start(SHADOW_MARGIN);
        stack.set_margin_end(SHADOW_MARGIN);
        stack.set_margin_bottom(SHADOW_MARGIN);
        window.set_child(Some(&stack));

        let surface = Rc::new(Self {
            // One region for the whole stack rather than one per banner: the
            // banners are a single group, the gaps between them are eight
            // pixels wide, and a region per card would have to be rebuilt on
            // every arrival and departure.
            blur: blur::attach(&window, &stack, || style::POPOVER_RADIUS as i32),
            window,
            stack,
            cards: Rc::new(RefCell::new(Vec::new())),
            services: services.clone(),
            connector: connector.to_string(),
            top_margin: top_margin(config),
            bindings: RefCell::new(Vec::new()),
        });

        // Two subscriptions feeding one diff: the banner list comes from the
        // daemon, and the answer to "is this my monitor?" from the compositor.
        let notifications = bridge::bind_state(&surface.stack, services.notifications.state(), {
            let surface = Rc::downgrade(&surface);
            move |_, state| {
                if let Some(surface) = surface.upgrade() {
                    surface.render(state);
                }
            }
        });
        let workspaces = bridge::bind_state(&surface.stack, services.compositor.workspaces(), {
            let surface = Rc::downgrade(&surface);
            move |_, _| {
                if let Some(surface) = surface.upgrade() {
                    surface.rerender();
                }
            }
        });
        *surface.bindings.borrow_mut() = vec![notifications, workspaces];

        surface
    }

    /// Render again from whatever the daemon is saying right now.
    fn rerender(&self) {
        let receiver = self.services.notifications.state();
        let state = receiver.borrow().clone();
        self.render(&state);
    }

    /// Whether banners belong on this monitor right now.
    fn is_host(&self) -> bool {
        let workspaces = self.services.compositor.workspaces();
        let focused = workspaces.borrow().focused_output.clone();
        let connectors = connectors();
        hosting_output(focused.as_deref(), &connectors) == Some(self.connector.as_str())
    }

    /// Bring the surface in line with `state`.
    ///
    /// Banners already on screen are updated in place rather than rebuilt: a
    /// replacement over D-Bus must not make the stack flicker, and rebuilding
    /// would restart the arrival animation of something that never left.
    fn render(&self, state: &NotifState) {
        let wanted: Vec<&ToastView> = if self.is_host() {
            state.toasts.iter().collect()
        } else {
            Vec::new()
        };

        self.retire_missing(&wanted);

        for (position, view) in wanted.iter().enumerate() {
            let existing = self
                .cards
                .borrow()
                .iter()
                .find(|card| card.matches(view.notification.id))
                .cloned();

            match existing {
                Some(card) => {
                    card.update(view);
                    self.move_to(&card, position);
                }
                None => self.insert(view, position),
            }
        }

        // The surface stays mapped while the last banner slides away, then
        // gets out of the desktop's way entirely.
        if self.cards.borrow().is_empty() {
            self.window.set_visible(false);
        } else {
            self.show();
        }
    }

    /// Size the surface to its banners and map it, if it is not up already.
    ///
    /// The height is measured and passed explicitly rather than left as `-1`.
    /// The surface is anchored to one edge only — that is what centres it —
    /// so layer-shell stretches it on neither axis and takes both from the
    /// toplevel's default size. A toplevel that has never been given one maps
    /// at zero height, which puts a banner on screen clipped to nothing and
    /// leaves it there, because the window is not resizable and so never
    /// renegotiates.
    fn show(&self) {
        let width = WIDTH + 2 * SHADOW_MARGIN;
        let (_, height, _, _) = self.stack.measure(Orientation::Vertical, width);
        debug!("banner surface: {width}x{height} at +{}", self.top_margin);
        self.window.set_default_size(width, height.max(1));

        // Re-asserted around every map. A layer surface's geometry is part of
        // the state the compositor reads when it is created, and one set at
        // construction is not always the state it reads — a banner that lands
        // behind the bar and rights itself a configure later reads as a
        // glitch, so it is stated on both sides of the map.
        self.window
            .set_margin(Edge::Top, self.top_margin - SHADOW_MARGIN);
        if !self.window.is_visible() {
            self.window.present();
        }
        self.window
            .set_margin(Edge::Top, self.top_margin - SHADOW_MARGIN);
    }

    /// Start the leaving animation for every banner that is no longer wanted.
    fn retire_missing(&self, wanted: &[&ToastView]) {
        let going: Vec<Card> = self
            .cards
            .borrow()
            .iter()
            .filter(|card| {
                !card.leaving.get() && !wanted.iter().any(|view| card.matches(view.notification.id))
            })
            .cloned()
            .collect();

        // The stack fades out with its last banner while the surface stays
        // mapped, and compositor blur takes no notice of a widget's opacity —
        // so the region comes off as the last one starts leaving, and goes back
        // on when a banner arrives.
        if !going.is_empty() && going.len() == self.cards.borrow().len() {
            self.blur.suspend();
        }

        for card in going {
            card.leaving.set(true);
            let stack = self.stack.clone();
            let cards = Rc::clone(&self.cards);
            let leaving = card.clone();
            card.slide(0.0, move || {
                if leaving.slide.parent().as_ref() == Some(stack.upcast_ref::<gtk4::Widget>()) {
                    stack.remove(&leaving.slide);
                }
                cards.borrow_mut().retain(|other| !other.same(&leaving));
                // The previous render sized the window while this departing
                // banner still occupied a slot. Resize after removing it so
                // the remaining stack (and the blur's resize watcher) shrinks.
                if let Some(window) = stack.root().and_downcast::<Window>() {
                    if cards.borrow().is_empty() {
                        window.set_visible(false);
                    } else {
                        let width = WIDTH + 2 * SHADOW_MARGIN;
                        let (_, height, _, _) = stack.measure(Orientation::Vertical, width);
                        window.set_default_size(width, height.max(1));
                    }
                }
            });
        }
    }

    /// Build a banner and slide it in at `position`.
    fn insert(&self, view: &ToastView, position: usize) {
        let card = Card::new(view, &self.services);
        let after = self.child_at(position);
        self.stack.insert_child_after(&card.slide, after.as_ref());

        let mut cards = self.cards.borrow_mut();
        let position = position.min(cards.len());
        cards.insert(position, card.clone());
        drop(cards);

        // Present before animating: a banner sliding in on an unmapped surface
        // finishes its run before the compositor ever shows it.
        self.show();
        // A banner arriving while the last one is still sliding away catches a
        // surface that never unmapped, so the blur is asked back by hand.
        self.blur.resume();
        card.slide.set_reveal(0.0);
        card.card.set_opacity(0.0);
        card.slide(1.0, || {});
    }

    /// Put `card` at `position` in the stack, if it is not there already.
    fn move_to(&self, card: &Card, position: usize) {
        let mut cards = self.cards.borrow_mut();
        let Some(current) = cards.iter().position(|other| other.same(card)) else {
            return;
        };
        if current == position {
            return;
        }
        let card = cards.remove(current);
        let position = position.min(cards.len());
        cards.insert(position, card.clone());
        drop(cards);

        let after = self.child_at(position);
        self.stack.reorder_child_after(&card.slide, after.as_ref());
    }

    /// The child the banner at `position` should follow, or `None` for first.
    fn child_at(&self, position: usize) -> Option<gtk4::Widget> {
        if position == 0 {
            return None;
        }
        self.cards
            .borrow()
            .get(position - 1)
            .map(|card| card.slide.clone().upcast())
    }
}

impl Drop for ToastSurface {
    fn drop(&mut self) {
        self.window.close();
    }
}

/// The connector name of every monitor GDK currently reports.
///
/// Shared with the OSD, which picks its host the same way and for the same
/// reason: whatever the user is looking at.
pub fn connectors() -> Vec<String> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index).and_downcast::<gdk::Monitor>())
        .filter_map(|monitor| monitor.connector().map(|name| name.to_string()))
        .collect()
}

/// One banner on screen.
#[derive(Clone)]
struct Card {
    id: u32,
    slide: SlideBox,
    card: gtk4::Box,
    summary: Label,
    body: Label,
    icon: gtk4::Box,
    actions: gtk4::Box,
    close: Button,
    animation: Animation,
    /// Set once the banner is on its way out, so a render landing mid-fade
    /// does not revive it. Its identity also distinguishes two banners that
    /// happen to share a notification id.
    leaving: Rc<Cell<bool>>,
}

impl Card {
    /// Build a banner for `view`.
    fn new(view: &ToastView, services: &Services) -> Self {
        let notification = &view.notification;

        let card = gtk4::Box::new(Orientation::Vertical, 8);
        card.add_css_class(classes::TOAST);
        if notification.urgency.is_critical() {
            card.add_css_class(classes::TOAST_CRITICAL);
        }

        let top = gtk4::Box::new(Orientation::Horizontal, 10);

        let icon_slot = gtk4::Box::new(Orientation::Horizontal, 0);
        icon_slot.add_css_class(classes::TOAST_ICON);
        icon_slot.set_valign(Align::Start);
        top.append(&icon_slot);

        let text = gtk4::Box::new(Orientation::Vertical, 2);
        text.set_hexpand(true);

        let summary = Label::new(None);
        summary.add_css_class(classes::TOAST_SUMMARY);
        summary.set_xalign(0.0);
        summary.set_ellipsize(pango::EllipsizeMode::End);
        summary.set_single_line_mode(true);
        text.append(&summary);

        let body = Label::new(None);
        body.add_css_class(classes::TOAST_BODY);
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_wrap_mode(pango::WrapMode::WordChar);
        body.set_lines(2);
        body.set_ellipsize(pango::EllipsizeMode::End);
        text.append(&body);
        top.append(&text);

        // The close button holds its slot from the first frame and only fades
        // in, so revealing it cannot reflow the summary beside it.
        let close = Button::from_icon_name("window-close-symbolic");
        close.add_css_class(classes::TOAST_CLOSE);
        close.set_valign(Align::Start);
        close.set_focus_on_click(false);
        top.append(&close);

        card.append(&top);

        let actions = gtk4::Box::new(Orientation::Horizontal, 6);
        actions.add_css_class(classes::TOAST_ACTIONS);
        actions.set_halign(Align::End);
        card.append(&actions);

        let slide = SlideBox::new();
        slide.set_child(&card);

        let this = Self {
            id: notification.id,
            animation: Animation::new(&slide),
            slide,
            card,
            summary,
            body,
            icon: icon_slot,
            actions,
            close,
            leaving: Rc::new(Cell::new(false)),
        };
        this.reveal_close(false);
        this.update(view);
        this.install_gestures(services);
        this
    }

    /// Whether this banner is showing notification `id` and is still wanted.
    fn matches(&self, id: u32) -> bool {
        self.id == id && !self.leaving.get()
    }

    /// Whether two handles refer to the same banner.
    fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.leaving, &other.leaving)
    }

    /// Re-render from `view`, which a replacement may have changed wholesale.
    fn update(&self, view: &ToastView) {
        let notification = &view.notification;

        set_text(&self.summary, &notification.summary);
        markup::apply(&self.body, &notification.body);
        self.body.set_visible(!notification.body.is_empty());

        while let Some(child) = self.icon.first_child() {
            self.icon.remove(&child);
        }
        self.icon
            .append(&icon::image(&notification.icon, TOAST_ICON));

        while let Some(child) = self.actions.first_child() {
            self.actions.remove(&child);
        }
        let buttons: Vec<_> = notification.buttons().take(MAX_ACTIONS).collect();
        self.actions.set_visible(!buttons.is_empty());
        for action in buttons {
            // The label is built rather than taken from `with_label` so it can
            // be told to ellipsize: three actions with real names on them —
            // "Mark as read", "Remind me tomorrow" — are wider than a banner,
            // and a `GtkButton`'s own label has nowhere to go but off the end.
            let label = Label::new(Some(&action.label));
            label.set_ellipsize(pango::EllipsizeMode::End);
            label.set_max_width_chars(MAX_ACTION_CHARS);
            let button = Button::new();
            button.set_child(Some(&label));
            button.add_css_class(classes::TOAST_ACTION);
            ripple::install(&button);
            button.set_focus_on_click(false);
            let key = action.key.clone();
            let id = notification.id;
            button.connect_clicked(move |button| {
                invoke(id, &key, button.root().and_then(|root| root.surface()));
            });
            self.actions.append(&button);
        }
    }

    /// Hover, click, and close: everything the pointer can do to a banner.
    fn install_gestures(&self, services: &Services) {
        let motion = gtk4::EventControllerMotion::new();
        motion.connect_enter({
            let this = self.clone();
            let handle = services.notifications.handle().clone();
            move |_, _, _| {
                this.reveal_close(true);
                let handle = handle.clone();
                let id = this.id;
                bridge::act(SCOPE, async move { handle.pause_toast(id).await });
            }
        });
        motion.connect_leave({
            let this = self.clone();
            let handle = services.notifications.handle().clone();
            move |_| {
                this.reveal_close(false);
                let handle = handle.clone();
                let id = this.id;
                bridge::act(SCOPE, async move { handle.resume_toast(id).await });
            }
        });
        self.card.add_controller(motion);

        self.close.connect_clicked({
            let handle = services.notifications.handle().clone();
            let id = self.id;
            move |_| {
                let handle = handle.clone();
                bridge::act(SCOPE, async move { handle.dismiss_toast(id).await });
            }
        });

        // A click anywhere else on the banner takes the user to the application
        // that sent it: its default action when there is one, its window either
        // way. With no action to close the notification, the click also means
        // "I have read it" — the notification stays in the history, the banner
        // does not.
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_released({
            let services = services.clone();
            let this = self.clone();
            move |gesture, _, _, _| {
                let receiver = services.notifications.state();
                let notification = receiver
                    .borrow()
                    .toasts
                    .iter()
                    .find(|toast| toast.notification.id == this.id)
                    .map(|toast| Arc::clone(&toast.notification));
                let Some(notification) = notification else {
                    return;
                };

                let surface = gesture
                    .widget()
                    .and_then(|widget| widget.root().and_then(|root| root.surface()));
                if !notifications::activate(&services, &notification, surface, SCOPE) {
                    let handle = services.notifications.handle().clone();
                    let id = this.id;
                    bridge::act(SCOPE, async move { handle.dismiss_toast(id).await });
                }
            }
        });
        self.card.add_controller(click);
    }

    /// Show or hide the close button.
    fn reveal_close(&self, revealed: bool) {
        self.close.set_sensitive(revealed);
        self.close.set_opacity(f64::from(u8::from(revealed)));
    }

    /// Slide to `target` reveal, running `done` when it lands.
    ///
    /// The run starts from where the banner actually is and is paid for by the
    /// distance left, so a banner retired while it is still arriving turns
    /// around from there instead of snapping to fully arrived and sliding the
    /// whole way back out. With motion switched off the animator jumps straight
    /// to the end and still calls `done`, so a banner leaving is removed either
    /// way.
    fn slide(&self, target: f64, done: impl FnOnce() + 'static) {
        let start = self.slide.reveal();
        let duration = (SLIDE_MS as f64 * (target - start).abs()).round() as u64;
        let slide = self.slide.clone();
        let card = self.card.clone();
        let easing = if target > 0.0 {
            Easing::EaseOutCubic
        } else {
            Easing::EaseInCubic
        };

        if target < 1.0 {
            slide.set_overflow(gtk4::Overflow::Hidden);
        }
        self.animation.start(
            AnimationParams::new(duration).with_easing(easing),
            Box::new(move |progress| {
                let reveal = start + (target - start) * progress;
                slide.set_reveal(reveal);
                card.set_opacity(reveal);
                if reveal >= 1.0 {
                    slide.set_overflow(gtk4::Overflow::Visible);
                }
            }),
            Some(Box::new(done)),
        );
    }
}

/// Ask the compositor for an activation token, then run the action.
///
/// The token is what lets the receiving application raise its own window; the
/// action still fires without one, it just cannot take focus.
fn invoke(id: u32, key: &str, surface: Option<gdk::Surface>) {
    let token = activation::token(None, surface.as_ref());
    let key = key.to_string();
    let Some(handle) = bridge::notifications() else {
        return;
    };
    bridge::act(
        SCOPE,
        async move { handle.invoke_action(id, key, token).await },
    );
}

/// Set a label only when the text actually changed.
fn set_text(label: &Label, text: &str) {
    if label.text() != text {
        label.set_text(text);
    }
}

/// The banner surface's layer-shell window.
fn build_window(monitor: &gdk::Monitor, top_margin: i32) -> Window {
    let window = Window::builder().decorated(false).resizable(false).build();
    window.add_css_class(classes::TOAST_WINDOW);

    window.init_layer_shell();
    window.set_namespace(Some("topbar-toast"));
    // Overlay, not Top: a banner has to be readable over an open popover, and
    // popovers are the only other thing the panel puts on Top.
    window.set_layer(Layer::Overlay);
    window.set_monitor(Some(monitor));
    // -1: exclusive zones are ignored and the placement is the panel's own.
    // See `top_margin` for why the compositor cannot be left to do it.
    window.set_exclusive_zone(-1);
    // Top alone: anchoring one edge of an axis centres the surface on the
    // other, which is where a banner belongs.
    window.set_anchor(Edge::Top, true);
    window.set_margin(Edge::Top, top_margin);
    window.set_keyboard_mode(KeyboardMode::None);
    window
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn banners_hang_below_the_bar_rather_than_behind_it() {
        let mut config = Config::default();
        config.bar.size = 36;
        config.bar.padding = 0;
        config.bar.popover_offset = 1;
        assert_eq!(top_margin(&config), 37);

        // A padded, opaque bar is taller, and the banners follow it down.
        config.bar.padding = 4;
        assert_eq!(top_margin(&config), 45);

        config.bar.popover_offset = 12;
        assert_eq!(top_margin(&config), 56);
    }

    #[test]
    fn banners_follow_the_focused_output() {
        let connectors = outputs(&["DP-2", "eDP-1"]);
        assert_eq!(hosting_output(Some("eDP-1"), &connectors), Some("eDP-1"));
        assert_eq!(hosting_output(Some("DP-2"), &connectors), Some("DP-2"));
    }

    #[test]
    fn an_unknown_focused_output_falls_back_to_the_first() {
        // Mid-hotplug the compositor names an output the panel has no bar on.
        let connectors = outputs(&["DP-2", "eDP-1"]);
        assert_eq!(hosting_output(Some("HDMI-A-1"), &connectors), Some("DP-2"));
    }

    #[test]
    fn with_nothing_focused_the_first_monitor_hosts() {
        let connectors = outputs(&["DP-2", "eDP-1"]);
        assert_eq!(hosting_output(None, &connectors), Some("DP-2"));
    }

    #[test]
    fn with_no_monitors_nothing_hosts() {
        assert_eq!(hosting_output(Some("eDP-1"), &[]), None);
        assert_eq!(hosting_output(None, &[]), None);
    }

    #[test]
    fn exactly_one_monitor_ever_hosts() {
        let connectors = outputs(&["DP-2", "eDP-1", "HDMI-A-1"]);
        for focused in [None, Some("eDP-1"), Some("HDMI-A-1"), Some("nonsense")] {
            let host = hosting_output(focused, &connectors);
            assert_eq!(
                connectors
                    .iter()
                    .filter(|name| Some(name.as_str()) == host)
                    .count(),
                1,
                "focused {focused:?} picked {host:?}"
            );
        }
    }
}
