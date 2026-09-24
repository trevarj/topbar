//! The workspaces widget: GNOME Activities dots with an active pill.
//!
//! The widget is three parts, deliberately separated:
//!
//! - [`model`] — pure arithmetic: which workspaces are visible, where their
//!   indicators sit, what a scroll gesture means. All of it unit-tested.
//! - [`strip`] — one custom widget that draws the whole row in `snapshot()`.
//! - this file — the plumbing: subscribe to the service, translate clicks and
//!   scrolls into actions, dim when the compositor is gone.

mod model;
mod strip;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gtk4::prelude::*;
use gtk4::{EventControllerScroll, EventControllerScrollFlags, GestureClick, gdk, glib};
use topbar_core::Config;
use topbar_core::config::WorkspacesConfig;
use topbar_core::theme::{Palette, Rgb};
use topbar_services::{CompositorHandle, WorkspaceId, WorkspacesSnapshot};
use tracing::debug;

use crate::bar::BarContext;
use crate::bridge::{self, ActionScope, BindingGuard};
use crate::style::classes;
use crate::widgets::set_class;
use crate::widgets::shell::WidgetShell;
use model::{LabelType, ScrollAccumulator, Slot, SlotOptions};
use strip::{StripColors, WorkspaceStrip};

/// Name used in log lines about failed actions.
const WIDGET_NAME: &str = "workspaces";

/// The workspaces widget.
pub struct WorkspacesWidget {
    shell: WidgetShell,
    _binding: BindingGuard,
    _state: Rc<State>,
}

/// Everything the gestures need after the widget is built.
struct State {
    strip: WorkspaceStrip,
    /// The slots currently on screen, for hit testing and scroll stepping.
    slots: RefCell<Vec<Slot>>,
    scroll: RefCell<ScrollAccumulator>,
    compositor: CompositorHandle,
}

impl WorkspacesWidget {
    /// Build the widget for the bar described by `context`.
    pub fn new(config: &Config, context: &BarContext) -> Self {
        let settings = &config.widgets.workspaces;
        let shell = WidgetShell::new(classes::WORKSPACES);
        shell.make_interactive();

        let strip = WorkspaceStrip::new(colors(config), animate(config, settings));
        strip.set_valign(gtk4::Align::Center);
        shell.content().append(&strip);

        let state = Rc::new(State {
            strip: strip.clone(),
            slots: RefCell::new(Vec::new()),
            scroll: RefCell::new(ScrollAccumulator::default()),
            compositor: context.services.compositor.handle(),
        });

        // `SlotOptions` borrows the connector name, so the closure keeps its
        // own copy of the string rather than the options struct.
        let connector = context.connector.clone();
        let filter_by_output = settings.filter_by_output;
        let show_unoccupied = settings.show_unoccupied;
        let label_type = LabelType::parse(&settings.label_type);

        let wrapper = shell.root().clone();
        let binding = bridge::bind_state(&strip, context.services.compositor.workspaces(), {
            let state = Rc::clone(&state);
            move |strip: &WorkspaceStrip, snapshot: &WorkspacesSnapshot| {
                let slots = model::visible_slots(
                    snapshot,
                    SlotOptions {
                        connector: &connector,
                        filter_by_output,
                        show_unoccupied,
                        label_type,
                    },
                );
                // A reconnect arrives as a full snapshot, so rebuilding
                // from it needs no special case.
                strip.set_slots(&slots);
                *state.slots.borrow_mut() = slots;

                // Dim rather than empty: the last known workspaces are
                // still the truth, they are just no longer live.
                set_class(&wrapper, classes::DISCONNECTED, !snapshot.connected);
            }
        });

        install_click(&strip, &state);
        install_scroll(shell.root(), &state);

        Self {
            shell,
            _binding: binding,
            _state: state,
        }
    }

    /// The widget to put in a bar section.
    pub fn root(&self) -> gtk4::Widget {
        self.shell.root().clone().upcast()
    }
}

/// Clicking an indicator focuses its workspace.
fn install_click(strip: &WorkspaceStrip, state: &Rc<State>) {
    let click = GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.connect_released({
        let state = Rc::clone(state);
        move |_, _, x, _| {
            let rects = state.strip.rects();
            let Some(index) = model::hit_test(&rects, x as f32) else {
                return;
            };
            let Some(slot) = state.slots.borrow().get(index).cloned() else {
                return;
            };
            if slot.is_focused {
                return;
            }
            focus(&state.compositor, slot.id);
        }
    });
    strip.add_controller(click);
}

/// Scrolling anywhere on the widget walks the workspaces, clamped at the ends.
fn install_scroll(wrapper: &gtk4::Box, state: &Rc<State>) {
    let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll({
        let state = Rc::clone(state);
        move |_, _, delta_y| {
            let Some(steps) = state.scroll.borrow_mut().feed(delta_y, Instant::now()) else {
                return glib::Propagation::Stop;
            };

            let slots = state.slots.borrow();
            let Some(current) = slots.iter().position(|slot| slot.is_active) else {
                return glib::Propagation::Stop;
            };
            let Some(target) = model::step_target(current, steps, slots.len()) else {
                return glib::Propagation::Stop;
            };

            focus(&state.compositor, slots[target].id);
            glib::Propagation::Stop
        }
    });
    wrapper.add_controller(scroll);
}

/// Focus the captured native workspace through the selected backend.
fn focus(compositor: &CompositorHandle, id: WorkspaceId) {
    debug!("focusing workspace {id:?}");
    let compositor = compositor.clone();
    bridge::act(
        ActionScope::Toast {
            widget: WIDGET_NAME,
        },
        async move { compositor.focus_workspace(id).await },
    );
}

/// Colors the strip paints itself with.
///
/// The foreground comes from CSS (the strip reads its own `color`); these two
/// cannot, because GTK4 has no supported way to look a custom property up from
/// Rust, and inventing one would put the palette in two places.
fn colors(config: &Config) -> StripColors {
    let palette = Palette::from_config(config);
    StripColors {
        urgent: rgba(palette.urgent),
        on_active: rgba(palette.bar),
        on_urgent: rgba(palette.urgent.contrasting_foreground()),
    }
}

/// Whether this widget animates: its own setting, or the theme's if unset.
fn animate(config: &Config, settings: &WorkspacesConfig) -> bool {
    settings.animate.unwrap_or(config.theme.animations)
}

/// Convert a resolved color into a GDK color.
fn rgba(color: Rgb) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_animate_option_overrides_the_theme() {
        let mut config = Config::default();
        config.theme.animations = true;

        let mut settings = WorkspacesConfig::default();
        assert!(animate(&config, &settings), "unset inherits the theme");

        settings.animate = Some(false);
        assert!(!animate(&config, &settings));

        config.theme.animations = false;
        settings.animate = None;
        assert!(!animate(&config, &settings));
    }
}
