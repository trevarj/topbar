//! Panel widgets and the shell they all share.

mod app_icon;
mod clock;
mod control_panel;
mod crypto;
mod custom;
mod expander;
mod headset;
mod keyboard_layout;
pub mod notifications;
mod notmuch;
mod os_logo;
mod quick_settings;
mod rounded_picture;
mod shell;
mod system_monitor;
mod tray;
pub(crate) mod weather;
mod workspaces;

use std::any::Any;

use gtk4::prelude::*;
use topbar_core::Config;
use tracing::warn;

use crate::bar::BarContext;
use crate::bridge::{self, ActionScope};

/// A widget that has been built and put in a bar section.
pub struct MountedWidget {
    /// The widget to append to a section.
    pub root: gtk4::Widget,
    /// Whatever must outlive this call — timers, subscriptions, the widget
    /// struct itself. Dropped when the bar is torn down.
    _keepalive: Box<dyn Any>,
}

impl MountedWidget {
    /// Pair a widget with the state that keeps it running.
    pub fn new(root: impl IsA<gtk4::Widget>, keepalive: impl Any) -> Self {
        Self {
            root: root.upcast(),
            _keepalive: Box::new(keepalive),
        }
    }
}

/// Build the widget named `name`, or `None` if there is nothing to build.
///
/// `context` carries the identity of the bar being built — chiefly its
/// connector name, which is how a per-monitor widget knows which monitor it is
/// on — and the service handles.
///
/// Every name configuration validation accepts is built here, which is what
/// [`handles`] and the test under it assert. Until M10 that was not true and
/// the fallback was a routine `debug!`; now a name reaching it means the two
/// lists have drifted apart, which is a defect rather than a milestone.
pub fn mount(name: &str, config: &Config, context: &BarContext) -> Option<MountedWidget> {
    if !handles(name) {
        // Unreachable through the configuration, which rejects an unknown name
        // long before this: reachable only if the two lists have drifted apart.
        warn!("`{name}` is not a widget this panel knows how to build");
        return None;
    }

    if let Some(settings) = config.widgets.custom.get(name) {
        let custom = custom::CustomWidget::new(name, settings, context);
        return Some(MountedWidget::new(custom.root(), custom));
    }

    match name {
        "clock" => {
            let clock =
                clock::ClockWidget::new(&config.widgets.clock, &config.widgets.weather, context);
            Some(MountedWidget::new(clock.root(), clock))
        }
        "workspaces" => {
            let workspaces = workspaces::WorkspacesWidget::new(config, context);
            Some(MountedWidget::new(workspaces.root(), workspaces))
        }
        "weather" => {
            let weather = weather::WeatherWidget::new(&config.widgets.weather, context);
            Some(MountedWidget::new(weather.root(), weather))
        }
        "crypto" => {
            let crypto = crypto::CryptoWidget::new(&config.widgets.crypto, context);
            Some(MountedWidget::new(crypto.root(), crypto))
        }
        "tray" => {
            let tray = tray::TrayWidget::new(config, context);
            Some(MountedWidget::new(tray.root(), tray))
        }
        "quick_settings" => {
            let quick_settings = quick_settings::QuickSettingsWidget::new(config, context);
            Some(MountedWidget::new(quick_settings.root(), quick_settings))
        }
        "notmuch" => {
            let mail = notmuch::NotmuchWidget::new(&config.widgets.notmuch, context);
            Some(MountedWidget::new(mail.root(), mail))
        }
        "keyboard_layout" => {
            let layout = keyboard_layout::KeyboardLayoutWidget::new(
                &config.widgets.keyboard_layout,
                context,
            );
            Some(MountedWidget::new(layout.root(), layout))
        }
        "system_monitor" => {
            let monitor =
                system_monitor::SystemMonitorWidget::new(&config.widgets.system_monitor, context);
            Some(MountedWidget::new(monitor.root(), monitor))
        }
        "headset" => {
            let headset = headset::HeadsetWidget::new(&config.widgets.headset, context);
            Some(MountedWidget::new(headset.root(), headset))
        }
        "os_logo" => {
            let logo = os_logo::OsLogoWidget::new(&config.widgets.os_logo);
            Some(MountedWidget::new(logo.root(), logo))
        }
        // `handles` already refused everything else, and a `custom-*` name with
        // no section of its own has nothing to configure it with.
        other => {
            warn!("`{other}` has no section in the configuration");
            None
        }
    }
}

/// Whether [`mount`] knows how to build `name`.
///
/// The match above as data, so a test can hold it against the set of names
/// configuration validation accepts. A name in one list and not the other is a
/// widget the user is allowed to configure and the panel quietly drops.
pub fn handles(name: &str) -> bool {
    name.starts_with("custom-")
        || matches!(
            name,
            "clock"
                | "crypto"
                | "headset"
                | "keyboard_layout"
                | "notmuch"
                | "os_logo"
                | "quick_settings"
                | "system_monitor"
                | "tray"
                | "weather"
                | "workspaces"
        )
}

/// Cut `text` to `max_chars`, ending it in an ellipsis.
///
/// Done in Rust rather than left to Pango because `max_chars` is a promise
/// about the *label*, not about the space the bar happened to give it: the
/// widget must be the same width whether the condition is "Fog" or
/// "Thunderstorm with hail". Characters, not bytes — the panel is full of
/// degree signs and Nerd Font glyphs, and cutting one in half writes a
/// replacement character onto the bar.
pub fn ellipsize(text: &str, max_chars: Option<usize>) -> String {
    let Some(max_chars) = max_chars.filter(|max| *max > 0) else {
        return text.to_string();
    };
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

/// Wire a widget's configured right- and middle-click commands.
///
/// Every widget takes these three keys, so the gesture, the runner and the
/// reporting live here once. Left click is deliberately not included: on most
/// widgets it opens something, and the ones where it runs a command — the
/// `custom-*` widgets — have to refresh themselves afterwards.
pub fn install_click_commands(
    anchor: &impl IsA<gtk4::Widget>,
    widget: &'static str,
    right: Option<&str>,
    middle: Option<&str>,
) {
    for (button, command) in [
        (gtk4::gdk::BUTTON_SECONDARY, right),
        (gtk4::gdk::BUTTON_MIDDLE, middle),
    ] {
        let Some(command) = command.map(str::to_string) else {
            continue;
        };
        let click = gtk4::GestureClick::new();
        click.set_button(button);
        click.connect_released(move |_, _, _, _| {
            let command = command.clone();
            // A toast rather than an inline slot: there is no row under the
            // pointer for a caption to belong to.
            bridge::act(ActionScope::Toast { widget }, async move {
                topbar_services::proc::run(&command).await
            });
        });
        anchor.as_ref().add_controller(click);
    }
}

/// Add or remove a CSS class without churning the style context.
pub fn set_class(widget: &impl IsA<gtk4::Widget>, class: &str, wanted: bool) {
    let widget = widget.as_ref();
    if wanted == widget.has_css_class(class) {
        return;
    }
    if wanted {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

#[cfg(test)]
mod tests {
    use topbar_core::config::SUPPORTED_WIDGETS;

    use super::*;

    /// The live configuration, shared with the drop-in compatibility contract.
    const LIVE_CONFIG: &str = include_str!("../../../topbar-core/tests/fixtures/live-config.toml");

    #[test]
    fn every_name_the_configuration_accepts_has_a_widget_behind_it() {
        for name in SUPPORTED_WIDGETS {
            assert!(
                handles(name),
                "`{name}` passes validation but `mount` builds nothing"
            );
        }
    }

    #[test]
    fn every_widget_in_the_live_configuration_is_built() {
        // The M10 definition of done, as a unit test: the file the user
        // actually runs names ten widgets, and not one of them may be skipped.
        let (config, warnings) = Config::parse(LIVE_CONFIG).expect("the live config parses");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:#?}");

        let placed: Vec<&String> = config
            .widgets
            .left
            .iter()
            .chain(&config.widgets.center)
            .chain(&config.widgets.right)
            .collect();
        assert_eq!(placed.len(), 10, "the live config places ten widgets");

        let unbuilt: Vec<&&String> = placed
            .iter()
            .filter(|name| !handles(name.as_str()))
            .collect();
        assert!(unbuilt.is_empty(), "nothing builds these: {unbuilt:?}");
    }

    #[test]
    fn a_custom_widget_is_matched_by_its_prefix_whatever_it_is_called() {
        assert!(handles("custom-crypto"));
        assert!(handles("custom-anything-at-all"));
        assert!(!handles("custom"), "the prefix alone is not a widget");
        assert!(!handles("media"), "v1's media widget was dropped");
    }

    #[test]
    fn a_label_that_fits_is_left_alone() {
        assert_eq!(
            ellipsize("21° Partly cloudy", Some(24)),
            "21° Partly cloudy"
        );
        assert_eq!(ellipsize("21° Partly cloudy", None), "21° Partly cloudy");
    }

    #[test]
    fn a_label_that_does_not_fit_is_cut_with_an_ellipsis() {
        // 24 characters is the live config's weather max_chars, and the longest
        // condition the WMO table produces is "Thunderstorm with hail".
        let cut = ellipsize("-11° Thunderstorm with hail", Some(24));
        assert_eq!(cut.chars().count(), 24);
        assert!(cut.ends_with('…'));
        assert_eq!(cut, "-11° Thunderstorm with …");
    }

    #[test]
    fn the_cut_counts_characters_rather_than_bytes() {
        // The degree sign is two bytes; cutting on bytes would split it.
        let cut = ellipsize("21° Nebliger Hochnebel", Some(8));
        assert_eq!(cut.chars().count(), 8);
        assert_eq!(cut, "21° Neb…");
    }

    #[test]
    fn a_max_chars_of_zero_is_treated_as_unset() {
        assert_eq!(ellipsize("21° Clear", Some(0)), "21° Clear");
    }
}
