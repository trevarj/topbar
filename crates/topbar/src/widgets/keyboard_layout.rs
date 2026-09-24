//! The keyboard-layout indicator.
//!
//! Invisible with a single layout: a panel that tells you your only keyboard
//! layout is the one you are using is noise. It appears when a second layout
//! is configured and disappears again if one is removed.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    EventControllerScroll, EventControllerScrollFlags, GestureClick, Image, Label, gdk, glib,
};
use topbar_core::config::KeyboardLayoutConfig;
use topbar_core::xkb_names;
use topbar_services::{CompositorHandle, KeyboardLayoutSnapshot};
use tracing::debug;

use crate::anim::{Animation, AnimationParams, Easing};
use crate::bar::BarContext;
use crate::bridge::{self, ActionScope, BindingGuard};
use crate::style::classes;
use crate::surfaces::tooltip::TooltipHandle;
use crate::widgets::set_class;
use crate::widgets::shell::WidgetShell;

/// Name used in log lines about failed actions.
const WIDGET_NAME: &str = "keyboard_layout";
/// The Adwaita symbolic icon for a keyboard.
const ICON: &str = "input-keyboard-symbolic";
/// How long the label takes to fade back in after a switch.
const SWITCH_FADE_MS: u64 = 150;
/// Shown when niri reports a layout we cannot name.
const UNKNOWN: &str = "?";

/// The keyboard-layout widget.
pub struct KeyboardLayoutWidget {
    shell: WidgetShell,
    _binding: BindingGuard,
    _state: Rc<State>,
}

/// What the render closure and the gestures share.
struct State {
    label: Label,
    tooltip: TooltipHandle,
    fade: Animation,
    /// The text on screen, so an unchanged snapshot does not re-fade it.
    shown: RefCell<String>,
    compositor: CompositorHandle,
    long_format: bool,
}

impl KeyboardLayoutWidget {
    /// Build the widget from `[widgets.keyboard_layout]`.
    pub fn new(settings: &KeyboardLayoutConfig, context: &BarContext) -> Self {
        let shell = WidgetShell::new(classes::KEYBOARD_LAYOUT);
        shell.make_interactive();

        let icon = Image::from_icon_name(ICON);
        icon.add_css_class(classes::KEYBOARD_LAYOUT_ICON);
        icon.set_visible(settings.show_icon);
        shell.content().append(&icon);

        let label = Label::new(None);
        label.set_visible(settings.show_label);
        shell.content().append(&label);

        let state = Rc::new(State {
            fade: Animation::new(&label),
            label,
            tooltip: shell.set_tooltip(""),
            shown: RefCell::new(String::new()),
            compositor: context.services.compositor.handle(),
            long_format: settings.format == "long",
        });

        let wrapper = shell.root().clone();
        let binding = bridge::bind_state(
            shell.root(),
            context.services.compositor.keyboard_layout(),
            {
                let state = Rc::clone(&state);
                move |root: &gtk4::Box, snapshot: &KeyboardLayoutSnapshot| {
                    // One layout is not a choice, so there is nothing to show.
                    root.set_visible(snapshot.is_switchable());
                    if !snapshot.is_switchable() {
                        return;
                    }
                    state.render(snapshot);
                    set_class(&wrapper, classes::DISCONNECTED, !snapshot.connected);
                }
            },
        );

        install_gestures(shell.root(), &state);

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

impl State {
    /// Show the active layout, fading the label when it actually changed.
    fn render(&self, snapshot: &KeyboardLayoutSnapshot) {
        let full = snapshot.current().unwrap_or(UNKNOWN);
        let text = if self.long_format {
            full.to_string()
        } else {
            short_name(full)
        };

        self.tooltip.set_text(&format!("Keyboard layout: {full}"));
        if *self.shown.borrow() == text {
            return;
        }
        *self.shown.borrow_mut() = text.clone();
        self.label.set_text(&text);

        // A dip rather than a true crossfade: the code is two characters wide,
        // and overlaying two of them reads as a smear.
        let label = self.label.clone();
        label.set_opacity(0.0);
        self.fade.start(
            AnimationParams::new(SWITCH_FADE_MS).with_easing(Easing::EaseOutCubic),
            Box::new(move |progress| label.set_opacity(progress)),
            None,
        );
    }

    /// Move to the next layout, reporting failure through the one action path.
    fn switch_next(&self) {
        debug!("switching to the next keyboard layout");
        let compositor = self.compositor.clone();
        bridge::act(
            ActionScope::Toast {
                widget: WIDGET_NAME,
            },
            async move { compositor.switch_layout_next().await },
        );
    }

    /// Move to the previous layout.
    fn switch_previous(&self) {
        debug!("switching to the previous keyboard layout");
        let compositor = self.compositor.clone();
        bridge::act(
            ActionScope::Toast {
                widget: WIDGET_NAME,
            },
            async move { compositor.switch_layout_prev().await },
        );
    }
}

/// Click cycles forward; scrolling goes either way.
fn install_gestures(wrapper: &gtk4::Box, state: &Rc<State>) {
    let click = GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.connect_released({
        let state = Rc::clone(state);
        move |_, _, _, _| state.switch_next()
    });
    wrapper.add_controller(click);

    let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll({
        let state = Rc::clone(state);
        move |_, _, delta_y| {
            // niri takes a direction, so scrolling can do what it looks like it
            // should: down for the next layout, up for the previous one.
            if delta_y > 0.0 {
                state.switch_next();
            } else if delta_y < 0.0 {
                state.switch_previous();
            }
            glib::Propagation::Stop
        }
    });
    wrapper.add_controller(scroll);
}

/// Reduce a layout description to something two characters wide.
///
/// niri reports xkb *descriptions*, not codes: `"English (US)"`, `"Russian"`,
/// `"German (Dvorak)"`. Ported from v1, where the order of these strategies
/// was settled against real layout names.
fn short_name(full: &str) -> String {
    if full.is_empty() {
        return UNKNOWN.to_string();
    }

    // A trailing parenthesis usually holds the code: "English (US)" → "US".
    if let Some(open) = full.rfind('(')
        && let Some(close) = full[open + 1..].find(')')
    {
        let inner = full[open + 1..open + 1 + close].trim();
        if !inner.is_empty() {
            // The code comes first when there is a list: "English (US, intl.
            // with dead keys)" is still US. A space in the leading item means
            // there is no code at all ("no dead keys"), only a variant
            // description, so fall back to the language.
            let code = inner.split(',').next().unwrap_or(inner).trim();
            if !code.is_empty() && !code.contains(' ') {
                return code.to_string();
            }
            let base = full[..open].trim();
            if !base.is_empty() {
                return language_code(base);
            }
        }
    }

    language_code(full)
}

/// Turn a bare language or layout name into a display code.
fn language_code(name: &str) -> String {
    if let Some(code) = xkb_names::short_code_from_language(name) {
        return code.to_string();
    }
    if let Some(code) = xkb_names::short_code_from_xkb(name) {
        return code.to_string();
    }
    if name.len() <= 3 && name.chars().all(|c| c.is_ascii_alphabetic()) {
        return name.to_uppercase();
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parenthesised_code_wins() {
        assert_eq!(short_name("English (US)"), "US");
        assert_eq!(short_name("German (Dvorak)"), "Dvorak");
        assert_eq!(short_name("Russian (phonetic)"), "phonetic");
    }

    #[test]
    fn a_variant_description_falls_back_to_the_language() {
        assert_eq!(short_name("German (no dead keys)"), "DE");
    }

    #[test]
    fn a_code_followed_by_a_variant_list_keeps_the_code() {
        assert_eq!(short_name("English (US, intl. with dead keys)"), "US");
        assert_eq!(short_name("Russian (RU, phonetic)"), "RU");
    }

    #[test]
    fn bare_language_names_map_through_the_xkb_table() {
        assert_eq!(short_name("Russian"), "RU");
        assert_eq!(short_name("Swedish"), "SE");
        assert_eq!(short_name("German"), "DE");
    }

    #[test]
    fn raw_xkb_codes_are_uppercased() {
        assert_eq!(short_name("us"), "US");
        assert_eq!(short_name("de"), "DE");
        assert_eq!(short_name("fr"), "FR");
    }

    #[test]
    fn an_unknown_layout_is_shown_verbatim() {
        assert_eq!(short_name("Klingon"), "Klingon");
        assert_eq!(short_name(""), UNKNOWN);
    }

    /// The panel's own live configuration: two layouts, short format.
    #[test]
    fn the_live_layout_pair_renders_as_codes() {
        assert_eq!(short_name("English (US)"), "US");
        assert_eq!(short_name("Russian"), "RU");
    }

    #[test]
    fn a_single_layout_is_not_switchable() {
        let one = KeyboardLayoutSnapshot {
            connected: true,
            current_name: Some("English (US)".into()),
            layout_count: 1,
        };
        assert!(!one.is_switchable());

        let two = KeyboardLayoutSnapshot {
            layout_count: 2,
            ..one
        };
        assert!(two.is_switchable());
        assert_eq!(two.current(), Some("English (US)"));
    }

    #[test]
    fn an_unknown_layout_stays_displayable() {
        let snapshot = KeyboardLayoutSnapshot {
            connected: true,
            current_name: None,
            layout_count: 2,
        };
        assert_eq!(snapshot.current(), None);
        assert_eq!(short_name(snapshot.current().unwrap_or(UNKNOWN)), UNKNOWN);
    }
}
