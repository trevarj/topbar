//! The shared tooltip surface.
//!
//! GTK's native tooltips do not position correctly against layer-shell
//! surfaces, so the panel owns one tooltip window of its own: a single
//! `Overlay`-layer, non-focusable, input-transparent window that every
//! tooltipped widget shares.
//!
//! Behaviour follows GNOME Shell:
//!
//! - 600 ms hover delay before the first tooltip appears;
//! - no delay at all when the pointer moves to another tooltipped widget
//!   within 500 ms of the last one hiding ("browse mode");
//! - centered under the anchor and clamped at least 8 px from the monitor
//!   edges;
//! - dismissed by leaving, clicking, scrolling, or the anchor going away.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{Label, Window, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::style::classes;

/// Delay before a tooltip appears on first hover.
const SHOW_DELAY: Duration = Duration::from_millis(600);
/// Window after a tooltip hides during which the next one shows instantly.
const CHAIN_WINDOW: Duration = Duration::from_millis(500);
/// Minimum gap between a tooltip and the monitor edges.
const EDGE_MARGIN: i32 = 8;
/// Width assumed when the tooltip cannot be measured yet.
const FALLBACK_WIDTH: i32 = 240;

thread_local! {
    static MANAGER: Rc<TooltipManager> = Rc::new(TooltipManager::default());
}

/// The process-wide tooltip manager.
fn manager() -> Rc<TooltipManager> {
    MANAGER.with(Rc::clone)
}

/// A widget's tooltip content, with updatable text and an optional passive child.
///
/// Dropping the handle does not detach the tooltip; keep it for as long as the
/// widget lives if the text changes (the clock updates its date at midnight).
#[derive(Clone)]
pub struct TooltipHandle {
    content: Rc<TooltipContent>,
}

impl TooltipHandle {
    /// Replace the tooltip text, updating the surface if it is on screen.
    pub fn set_text(&self, text: &str) {
        if self.content.text.borrow().as_str() == text {
            return;
        }
        self.content.text.borrow_mut().clear();
        self.content.text.borrow_mut().push_str(text);
        manager().refresh(&self.content);
    }
}

struct TooltipContent {
    text: RefCell<String>,
    child: Option<gtk4::Widget>,
}

/// Give `widget` a tooltip.
///
/// The pointer controllers are installed once per call, so call it once per
/// widget and use the returned handle to change the text later.
pub fn attach(widget: &impl IsA<gtk4::Widget>, text: &str) -> TooltipHandle {
    attach_content(widget, text, None)
}

/// Give `widget` a tooltip with passive content below its text.
pub fn attach_with_child(
    widget: &impl IsA<gtk4::Widget>,
    text: &str,
    child: &impl IsA<gtk4::Widget>,
) -> TooltipHandle {
    attach_content(widget, text, Some(child.as_ref().clone()))
}

fn attach_content(
    widget: &impl IsA<gtk4::Widget>,
    text: &str,
    child: Option<gtk4::Widget>,
) -> TooltipHandle {
    let widget = widget.as_ref();
    let handle = TooltipHandle {
        content: Rc::new(TooltipContent {
            text: RefCell::new(text.to_string()),
            child,
        }),
    };

    let motion = gtk4::EventControllerMotion::new();
    motion.connect_enter({
        let content = Rc::clone(&handle.content);
        move |controller, _x, _y| {
            if let Some(widget) = controller.widget() {
                manager().schedule(&widget, &content);
            }
        }
    });
    motion.connect_leave(|_| manager().hide());
    widget.add_controller(motion);

    // Any deliberate interaction dismisses the tooltip, as does the anchor
    // disappearing under it (a bar rebuild, a widget hiding itself).
    let click = gtk4::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    click.connect_pressed(|_, _, _, _| manager().hide());
    widget.add_controller(click);

    let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
    scroll.connect_scroll(|_, _, _| {
        manager().hide();
        glib::Propagation::Proceed
    });
    widget.add_controller(scroll);

    widget.connect_unmap(|_| manager().hide());

    handle
}

/// The tooltip window itself: a layer-shell surface with text and passive content.
struct TooltipWindow {
    window: Window,
    surface: gtk4::Box,
    label: Label,
}

impl TooltipWindow {
    fn new() -> Self {
        let window = Window::builder().decorated(false).resizable(false).build();
        window.add_css_class(classes::TOOLTIP_WINDOW);

        window.init_layer_shell();
        window.set_namespace(Some("topbar-tooltip"));
        window.set_layer(Layer::Overlay);
        // Zero (not -1) so the compositor keeps the tooltip clear of the bar's
        // own exclusive zone instead of drawing it over the panel.
        window.set_exclusive_zone(0);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);

        let surface = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        surface.add_css_class(classes::TOOLTIP_SURFACE);

        let label = Label::new(None);
        label.add_css_class(classes::TOOLTIP_LABEL);
        surface.append(&label);
        window.set_child(Some(&surface));

        // A tooltip must never take pointer input: it floats over application
        // content and would otherwise swallow clicks meant for the window
        // underneath.
        window.connect_map(|window| {
            if let Some(gdk_surface) = window.surface() {
                gdk_surface.set_input_region(Some(&gtk4::cairo::Region::create()));
            }
        });

        Self {
            window,
            surface,
            label,
        }
    }

    /// Set the content and return the width the surface wants for it.
    fn prepare(&self, content: &TooltipContent) -> i32 {
        let text = content.text.borrow();
        if self.label.text() != *text {
            self.label.set_text(&text);
        }
        let current = self.label.next_sibling();
        if current.as_ref() != content.child.as_ref() {
            if let Some(current) = current {
                self.surface.remove(&current);
            }
            if let Some(child) = &content.child {
                self.surface.append(child);
            }
        }
        let (_, natural, _, _) = self.surface.measure(gtk4::Orientation::Horizontal, -1);
        if natural > 0 { natural } else { FALLBACK_WIDTH }
    }

    fn show_at(&self, x: i32, monitor: Option<&gtk4::gdk::Monitor>) {
        if let Some(monitor) = monitor {
            self.window.set_monitor(Some(monitor));
        }
        self.window.set_margin(Edge::Left, x);
        self.window.set_margin(Edge::Top, 0);
        self.window.present();
    }

    fn hide(&self) {
        if self.window.is_visible() {
            self.window.set_visible(false);
        }
    }
}

#[derive(Default)]
struct TooltipManager {
    window: RefCell<Option<TooltipWindow>>,
    pending: RefCell<Option<glib::SourceId>>,
    anchor: RefCell<Option<glib::WeakRef<gtk4::Widget>>>,
    content: RefCell<Option<Rc<TooltipContent>>>,
    visible: Cell<bool>,
    hidden_at: Cell<Option<Instant>>,
}

impl TooltipManager {
    /// Arm the show timer for `widget`.
    fn schedule(self: &Rc<Self>, widget: &gtk4::Widget, content: &Rc<TooltipContent>) {
        self.cancel_pending();

        let anchor = glib::WeakRef::new();
        anchor.set(Some(widget));
        *self.anchor.borrow_mut() = Some(anchor);
        *self.content.borrow_mut() = Some(Rc::clone(content));

        let delay = show_delay(self.hidden_at.get().map(|at| at.elapsed()));
        let manager = Rc::clone(self);
        let source = glib::timeout_add_local_once(delay, move || {
            *manager.pending.borrow_mut() = None;
            manager.show();
        });
        *self.pending.borrow_mut() = Some(source);
    }

    fn show(&self) {
        let Some(widget) = self
            .anchor
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
        else {
            return;
        };
        if !widget.is_mapped() {
            return;
        }
        let content = self.content.borrow();
        let Some(content) = content.as_ref() else {
            return;
        };
        if content.text.borrow().is_empty() {
            return;
        }

        let (monitor, monitor_width) = monitor_for(&widget);
        self.ensure_window();
        let window = self.window.borrow();
        let Some(window) = window.as_ref() else {
            return;
        };

        let width = window.prepare(content);
        let center = anchor_center_x(&widget).unwrap_or(monitor_width / 2);
        window.show_at(clamp_x(center, width, monitor_width), monitor.as_ref());
        self.visible.set(true);
    }

    /// Refresh and reclamp the surface if `content` belongs to the visible tooltip.
    fn refresh(&self, content: &Rc<TooltipContent>) {
        if !self.visible.get() {
            return;
        }
        let matches = self
            .content
            .borrow()
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, content));
        if !matches {
            return;
        }
        if content.text.borrow().is_empty() {
            self.hide();
            return;
        }
        self.show();
    }

    /// Cancel any pending show and take the tooltip off screen.
    fn hide(&self) {
        self.cancel_pending();
        if let Some(window) = self.window.borrow().as_ref() {
            window.hide();
        }
        if self.visible.replace(false) {
            self.hidden_at.set(Some(Instant::now()));
        }
        *self.anchor.borrow_mut() = None;
        *self.content.borrow_mut() = None;
    }

    fn cancel_pending(&self) {
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
    }

    fn ensure_window(&self) {
        if self.window.borrow().is_some() {
            return;
        }
        *self.window.borrow_mut() = Some(TooltipWindow::new());
    }
}

/// How long to wait before showing, given how long ago the last tooltip hid.
fn show_delay(since_hidden: Option<Duration>) -> Duration {
    match since_hidden {
        Some(elapsed) if elapsed <= CHAIN_WINDOW => Duration::ZERO,
        _ => SHOW_DELAY,
    }
}

/// Center a tooltip of `width` under `center_x`, clamped to the monitor.
fn clamp_x(center_x: i32, width: i32, monitor_width: i32) -> i32 {
    let max_left = (monitor_width - width - EDGE_MARGIN).max(EDGE_MARGIN);
    (center_x - width / 2).clamp(EDGE_MARGIN, max_left)
}

/// The horizontal center of `widget` in its window's coordinates.
///
/// Bar windows span the full monitor width and are anchored left, so window
/// coordinates are monitor coordinates.
fn anchor_center_x(widget: &gtk4::Widget) -> Option<i32> {
    let root = widget.root()?;
    let center = gtk4::graphene::Point::new(widget.width() as f32 / 2.0, 0.0);
    let point = widget.compute_point(root.upcast_ref::<gtk4::Widget>(), &center)?;
    Some(point.x() as i32)
}

/// The monitor showing `widget`, plus its width.
fn monitor_for(widget: &gtk4::Widget) -> (Option<gtk4::gdk::Monitor>, i32) {
    let monitor = widget
        .root()
        .and_then(|root| root.downcast::<Window>().ok())
        .and_then(|window| window.surface())
        .and_then(|surface| {
            gtk4::gdk::Display::default().and_then(|display| display.monitor_at_surface(&surface))
        });
    let width = monitor
        .as_ref()
        .map_or(1920, |monitor| monitor.geometry().width());
    (monitor, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_hover_waits_before_showing() {
        assert_eq!(show_delay(None), SHOW_DELAY);
        assert_eq!(show_delay(Some(Duration::from_secs(5))), SHOW_DELAY);
    }

    #[test]
    fn moving_between_widgets_shows_instantly() {
        assert_eq!(show_delay(Some(Duration::from_millis(0))), Duration::ZERO);
        assert_eq!(show_delay(Some(Duration::from_millis(499))), Duration::ZERO);
        assert_eq!(show_delay(Some(Duration::from_millis(501))), SHOW_DELAY);
    }

    #[test]
    fn tooltip_is_centered_under_its_anchor() {
        assert_eq!(clamp_x(500, 120, 1000), 440);
    }

    #[test]
    fn tooltip_keeps_clear_of_monitor_edges() {
        assert_eq!(clamp_x(20, 120, 1000), EDGE_MARGIN);
        assert_eq!(clamp_x(990, 120, 1000), 1000 - 120 - EDGE_MARGIN);
    }

    #[test]
    fn tooltip_wider_than_the_monitor_still_starts_on_screen() {
        assert_eq!(clamp_x(500, 2000, 1000), EDGE_MARGIN);
    }
}

#[cfg(test)]
mod gtk_tests {
    use super::*;

    #[test]
    #[ignore = "requires a GTK display; run under Xvfb with --ignored --exact"]
    fn text_tooltip_replaces_calendar_content() {
        gtk4::init().expect("GTK display");
        // Exercise the actual popup contents on X11, without layer-shell.
        let surface = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        let label = Label::new(None);
        surface.append(&label);
        let window = Window::new();
        window.set_child(Some(&surface));
        let tooltip = TooltipWindow {
            window,
            surface,
            label,
        };
        let calendar = gtk4::Calendar::new();
        let date = TooltipContent {
            text: RefCell::new("Wednesday, September 30, 2026".to_string()),
            child: Some(calendar.clone().upcast()),
        };
        let ordinary = TooltipContent {
            text: RefCell::new("Volume: 50%".to_string()),
            child: None,
        };
        tooltip.prepare(&date);
        tooltip.window.present();
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }
        assert!(calendar.is_mapped());
        tooltip.prepare(&ordinary);
        assert!(calendar.parent().is_none());
        assert_eq!(tooltip.label.text(), "Volume: 50%");
        assert!(tooltip.label.next_sibling().is_none());
        tooltip.prepare(&date);
        assert_eq!(tooltip.label.text(), "Wednesday, September 30, 2026");
        assert_eq!(calendar.parent(), Some(tooltip.surface.clone().upcast()));
        tooltip.prepare(&ordinary);
        let content = Rc::new(ordinary);
        let handle = TooltipHandle {
            content: Rc::clone(&content),
        };
        let manager = manager();
        *manager.anchor.borrow_mut() =
            Some(tooltip.label.clone().upcast::<gtk4::Widget>().downgrade());
        *manager.content.borrow_mut() = Some(content);
        manager.visible.set(true);
        *manager.window.borrow_mut() = Some(tooltip);
        assert!(
            manager
                .window
                .borrow()
                .as_ref()
                .unwrap()
                .window
                .is_visible()
        );
        // A custom script may clear its tooltip while the anchor stays mapped.
        handle.set_text("");
        let tooltip = manager.window.borrow_mut().take().unwrap();
        assert!(
            !tooltip.window.is_visible(),
            "cleared tooltip must disappear"
        );
        tooltip.window.close();
    }
}
