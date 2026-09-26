//! What changed between two configurations, and therefore what has to happen.
//!
//! Hot reload is a routing problem: a new file arrives, and the panel has to
//! do the smallest correct thing. Regenerating the stylesheet is cheap and
//! invisible; rebuilding every bar restarts every widget's timers and closes
//! whatever popover was open. The difference between them is this type.
//!
//! It is derived rather than hand-maintained. v1 kept a list of key names and a
//! `match` saying which of them meant "restart the clock"; the list went stale
//! the moment a key was added, and a changed `clock.format` was ignored until
//! the next restart because of it. Here every section of [`Config`] derives
//! [`PartialEq`], and the delta is *comparisons* — a new key inside an existing
//! section is classified correctly the day it is added, without anyone
//! remembering to say so.
//!
//! The one rule to keep: a new **section** needs a line here and a case in the
//! tests below. [`ConfigDelta::is_empty`] is what proves nothing was missed —
//! `between(a, b)` reporting "nothing changed" for two configurations that are
//! not equal is a defect, and there is a test that says so for every section.

use std::collections::BTreeSet;
use std::fmt;

use crate::config::{Config, CustomWidgetConfig};
use crate::theme::Palette;

/// Everything that differs between two configurations.
///
/// Fields are independent: a file that changes the accent colour *and* adds a
/// widget sets both `style` and `layout`, and the caller does both things.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigDelta {
    /// The generated stylesheet would come out different.
    ///
    /// Covers `[theme]` colours, typography and icons, the `[widgets]`
    /// styling keys, and everything in `[bar]` — the sheet is generated from
    /// all three. Answered by regenerating the sheet and swapping the single
    /// provider, which needs no widget to be touched.
    pub style: bool,
    /// `theme.blur` was switched on or off.
    ///
    /// Separate from [`Self::style`] because blur is not CSS: it is a region
    /// handed to the compositor per surface, and changing it means dropping
    /// and re-making every attachment.
    pub blur: bool,
    /// `theme.animations` or `theme.ripple` changed.
    pub motion: bool,
    /// `[bar]` changed: the windows themselves are the wrong size or shape.
    pub bar: bool,
    /// `widgets.left`, `.center` or `.right` changed.
    pub layout: bool,
    /// Per-widget sections that changed, by widget name.
    ///
    /// `custom-*` names are in here too, both when their section changed and
    /// when the section appeared or disappeared entirely.
    pub widgets: BTreeSet<String>,
    /// `[osd]` changed.
    pub osd: bool,
    /// `[audio]` changed.
    pub audio: bool,
    /// `[updates]` changed.
    pub updates: bool,
    /// `[launcher]` changed.
    pub launcher: bool,
    /// `[appearance]` changed.
    pub appearance: bool,
    /// `[advanced]` changed.
    pub advanced: bool,
}

impl ConfigDelta {
    /// Classify the difference between two configurations.
    pub fn between(old: &Config, new: &Config) -> Self {
        let mut delta = Self::default();

        // Color-only bar changes do not destroy open clock/Quick Settings menus.
        if old.bar != new.bar {
            let a = &old.bar;
            let b = &new.bar;
            delta.bar = a.position != b.position
                || a.size != b.size
                || a.spacing != b.spacing
                || a.screen_margin != b.screen_margin
                || a.inset != b.inset
                || a.padding != b.padding
                || a.border_radius != b.border_radius
                || a.popover_offset != b.popover_offset
                || a.background_opacity != b.background_opacity;
            delta.style = true;
        }

        let (old_widgets, new_widgets) = (&old.widgets, &new.widgets);
        if old_widgets.left != new_widgets.left
            || old_widgets.center != new_widgets.center
            || old_widgets.right != new_widgets.right
        {
            delta.layout = true;
        }
        if old_widgets.border_radius != new_widgets.border_radius
            || old_widgets.background_color != new_widgets.background_color
            || old_widgets.background_opacity != new_widgets.background_opacity
            || old_widgets.popover_background_opacity != new_widgets.popover_background_opacity
        {
            delta.style = true;
        }

        delta.widgets = changed_widgets(old, new);

        let (old_theme, new_theme) = (&old.theme, &new.theme);
        if old_theme.blur != new_theme.blur {
            delta.blur = true;
        }
        if old_theme.animations != new_theme.animations || old_theme.ripple != new_theme.ripple {
            delta.motion = true;
        }
        if old_theme.mode != new_theme.mode
            || old_theme.accent != new_theme.accent
            || old_theme.icons != new_theme.icons
            || old_theme.states != new_theme.states
            || old_theme.typography != new_theme.typography
            || old_theme.palette != new_theme.palette
        {
            delta.style = true;
        }

        delta.osd = old.osd != new.osd;
        let before = Palette::from_config(old);
        let after = Palette::from_config(new);
        if before.bar != after.bar || before.urgent != after.urgent {
            delta.widgets.insert("workspaces".into());
        }
        if before.bar != after.bar || before.foreground != after.foreground {
            delta.widgets.insert("tray".into());
        }
        delta.osd |= before.accent != after.accent || before.urgent != after.urgent;
        delta.audio = old.audio != new.audio;
        delta.updates = old.updates != new.updates;
        delta.launcher = old.launcher != new.launcher;
        delta.appearance = old.appearance != new.appearance;
        if delta.appearance {
            delta.widgets.insert("quick_settings".into());
        }
        delta.advanced = old.advanced != new.advanced;

        delta
    }

    /// Whether the two configurations were the same in every way that matters.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the bar windows have to be built again from scratch.
    ///
    /// Geometry and placement cannot be edited in place: the window height is
    /// the exclusive zone, and the order of a section is the order its widgets
    /// were appended in. `advanced.pango_font_rendering` is here because it is
    /// applied to a window at build time.
    pub fn rebuilds_bars(&self) -> bool {
        self.bar || self.layout || self.advanced
    }

    /// Whether nothing beyond the stylesheet and the motion switches changed.
    ///
    /// The cheap path: no widget is touched, no bar is rebuilt, no service is
    /// reconfigured. Blur is deliberately *not* part of it — see [`Self::blur`].
    pub fn theme_only(&self) -> bool {
        !self.is_empty()
            && !self.blur
            && !self.rebuilds_bars()
            && self.widgets.is_empty()
            && !self.osd
            && !self.audio
            && !self.updates
            && !self.launcher
            && !self.appearance
    }
}

/// A one-line summary, for the log and for `topbar reload`'s answer.
impl fmt::Display for ConfigDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("nothing changed");
        }
        let mut parts: Vec<String> = Vec::new();
        for (flag, name) in [
            (self.style, "styling"),
            (self.blur, "blur"),
            (self.motion, "motion"),
            (self.bar, "bar geometry"),
            (self.layout, "widget placement"),
            (self.osd, "osd"),
            (self.audio, "audio"),
            (self.updates, "updates"),
            (self.launcher, "launcher"),
            (self.appearance, "appearance"),
            (self.advanced, "advanced"),
        ] {
            if flag {
                parts.push(name.to_string());
            }
        }
        if !self.widgets.is_empty() {
            parts.push(format!(
                "widgets ({})",
                self.widgets
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        f.write_str(&parts.join(", "))
    }
}

/// Every widget whose own section is different between the two files.
fn changed_widgets(old: &Config, new: &Config) -> BTreeSet<String> {
    let (a, b) = (&old.widgets, &new.widgets);
    let mut changed = BTreeSet::new();

    // One line per built-in section. A section missing from this list would be
    // a widget that never rebuilds, which is exactly the v1 defect; the test
    // `every_built_in_section_is_classified` walks `SUPPORTED_WIDGETS` and
    // fails if one of them is not reachable from here.
    let mut check = |name: &str, differs: bool| {
        if differs {
            changed.insert(name.to_string());
        }
    };
    check("workspaces", a.workspaces != b.workspaces);
    check("clock", a.clock != b.clock);
    check("weather", a.weather != b.weather);
    check("crypto", a.crypto != b.crypto);
    check("notmuch", a.notmuch != b.notmuch);
    check("tray", a.tray != b.tray);
    check("quick_settings", a.quick_settings != b.quick_settings);
    check("system_monitor", a.system_monitor != b.system_monitor);
    check("headset", a.headset != b.headset);
    check("keyboard_layout", a.keyboard_layout != b.keyboard_layout);
    check("os_logo", a.os_logo != b.os_logo);

    // `custom-*` sections come and go with the file, so the union of both key
    // sets is what has to be walked: a section that was deleted is a change to
    // the widget just as much as one that was edited.
    let names: BTreeSet<&String> = a.custom.keys().chain(b.custom.keys()).collect();
    for name in names {
        let before: Option<&CustomWidgetConfig> = a.custom.get(name);
        let after: Option<&CustomWidgetConfig> = b.custom.get(name);
        if before != after {
            changed.insert(name.clone());
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SUPPORTED_WIDGETS;

    /// The configuration this project is written for, as the baseline every
    /// case edits one section of.
    const LIVE_CONFIG: &str = include_str!("../tests/fixtures/live-config.toml");

    fn live() -> Config {
        Config::parse(LIVE_CONFIG)
            .expect("the live config parses")
            .0
    }

    /// Parse the live config with `extra` appended.
    ///
    /// Only good for sections the file does *not* already have — TOML rejects a
    /// repeated table outright — which is exactly the "a widget was added to
    /// the file" case. Editing an existing section is done on the parsed
    /// structure instead.
    fn live_with(extra: &str) -> Config {
        let source = format!("{LIVE_CONFIG}\n{extra}\n");
        Config::parse(&source)
            .unwrap_or_else(|error| panic!("edited config should parse: {error}"))
            .0
    }

    #[test]
    fn an_unchanged_file_changes_nothing() {
        let delta = ConfigDelta::between(&live(), &live());
        assert!(delta.is_empty(), "{delta:?}");
        assert_eq!(delta.to_string(), "nothing changed");
        assert!(!delta.theme_only(), "nothing changed is not a theme change");
    }

    #[test]
    fn an_accent_refreshes_osd_without_destroying_popovers() {
        let mut new = live();
        new.theme.accent = "#ff0000".to_string();
        let delta = ConfigDelta::between(&live(), &new);

        assert!(delta.style);
        assert!(delta.osd);
        assert!(!delta.rebuilds_bars());
        assert!(delta.widgets.is_empty());
    }

    #[test]
    fn palette_changes_refresh_only_components_with_cached_colors() {
        let old = Config::default();
        let mut new = old.clone();
        new.theme.mode = "light".into();
        let delta = ConfigDelta::between(&old, &new);
        assert!(delta.style && !delta.rebuilds_bars());
        assert_eq!(
            delta.widgets,
            BTreeSet::from(["tray".into(), "workspaces".into()])
        );
        assert!(!delta.osd, "a fixed accent and urgency have not changed");
        new = old.clone();
        new.theme.states.urgent = "#123".into();
        let delta = ConfigDelta::between(&old, &new);
        assert!(delta.style && delta.osd && !delta.rebuilds_bars());
        assert_eq!(delta.widgets, BTreeSet::from(["workspaces".into()]));
        new = old.clone();
        new.bar.background_color = Some("#abc".into());
        assert!(!ConfigDelta::between(&old, &new).rebuilds_bars());
        new = old.clone();
        new.theme.palette.surface = Some("#456".into());
        assert!(ConfigDelta::between(&old, &new).theme_only());
        let mut old = old;
        old.theme.accent = "none".into();
        new = old.clone();
        new.theme.palette.foreground = Some("#123".into());
        new.osd.show_value = !old.osd.show_value;
        let delta = ConfigDelta::between(&old, &new);
        assert!(delta.osd && delta.style && !delta.rebuilds_bars());
        assert_eq!(delta.widgets, BTreeSet::from(["tray".into()]));
    }

    #[test]
    fn blur_is_its_own_route_because_it_is_not_css() {
        let mut new = live();
        new.theme.blur = !new.theme.blur;
        let delta = ConfigDelta::between(&live(), &new);

        assert!(delta.blur);
        assert!(!delta.style, "blur does not appear in the sheet");
        assert!(!delta.theme_only(), "blur needs surfaces re-attached");
    }

    #[test]
    fn the_motion_switches_are_separate_from_the_palette() {
        let mut new = live();
        new.theme.animations = false;
        let delta = ConfigDelta::between(&live(), &new);
        assert!(delta.motion);
        assert!(!delta.style);
        assert!(delta.theme_only(), "motion is a setter, not a rebuild");

        let mut new = live();
        new.theme.ripple = false;
        assert!(ConfigDelta::between(&live(), &new).motion);
    }

    #[test]
    fn bar_geometry_rebuilds_the_windows_and_the_sheet() {
        let mut new = live();
        new.bar.size = 40;
        let delta = ConfigDelta::between(&live(), &new);

        assert!(delta.bar);
        assert!(delta.style, "the sheet is generated from the bar too");
        assert!(delta.rebuilds_bars());
        assert!(!delta.theme_only());
    }

    #[test]
    fn moving_a_widget_between_sections_is_a_placement_change() {
        let mut new = live();
        let widget = new.widgets.right.remove(0);
        new.widgets.left.push(widget);
        let delta = ConfigDelta::between(&live(), &new);

        assert!(delta.layout);
        assert!(delta.rebuilds_bars());
        assert!(
            delta.widgets.is_empty(),
            "moving a widget does not change its own section"
        );
    }

    #[test]
    fn the_widget_styling_keys_are_styling_and_not_placement() {
        let mut new = live();
        new.widgets.border_radius = 0;
        let delta = ConfigDelta::between(&live(), &new);
        assert!(delta.style);
        assert!(!delta.layout);
        assert!(delta.theme_only());
    }

    #[test]
    fn a_clock_format_edit_names_the_clock_and_only_the_clock() {
        let mut new = live();
        new.widgets.clock.format = "%H:%M".to_string();
        let delta = ConfigDelta::between(&live(), &new);

        assert_eq!(
            delta.widgets,
            BTreeSet::from(["clock".to_string()]),
            "{delta:?}"
        );
        assert!(!delta.rebuilds_bars(), "one widget, not every bar");
        assert!(!delta.style);
        assert_eq!(delta.to_string(), "widgets (clock)");
    }

    #[test]
    fn every_built_in_section_is_classified() {
        // The guard against the v1 defect: a widget whose section is edited and
        // which is never told about it. Every name the configuration accepts
        // has to be reachable from `changed_widgets`.
        let base = live();
        for name in SUPPORTED_WIDGETS {
            let mut new = base.clone();
            let widgets = &mut new.widgets;
            match *name {
                "workspaces" => widgets.workspaces.animate = Some(false),
                "clock" => widgets.clock.format = "%H".to_string(),
                "weather" => widgets.weather.forecast_days = 3,
                "crypto" => widgets.crypto.interval = 900,
                "tray" => widgets.tray.max_icons = 3,
                "quick_settings" => widgets.quick_settings.on_click_right = Some("x".into()),
                "system_monitor" => widgets.system_monitor.cpu_threshold = 50,
                "headset" => widgets.headset.interval = 30,
                "keyboard_layout" => widgets.keyboard_layout.format = "long".to_string(),
                "notmuch" => widgets.notmuch.interval = 900,
                "os_logo" => widgets.os_logo.tooltip = Some("distro".into()),
                other => panic!("`{other}` has no case here; add one"),
            }
            let delta = ConfigDelta::between(&base, &new);
            assert_eq!(
                delta.widgets,
                BTreeSet::from([(*name).to_string()]),
                "editing `[widgets.{name}]` should name exactly that widget"
            );
        }
    }

    #[test]
    fn a_custom_widget_is_named_when_it_changes_appears_or_goes_away() {
        let base = live();
        assert!(base.widgets.custom.contains_key("custom-crypto"));

        let mut edited = base.clone();
        edited
            .widgets
            .custom
            .get_mut("custom-crypto")
            .expect("the live config has one")
            .interval = 600;
        assert_eq!(
            ConfigDelta::between(&base, &edited).widgets,
            BTreeSet::from(["custom-crypto".to_string()])
        );

        let mut removed = base.clone();
        removed.widgets.custom.remove("custom-crypto");
        assert_eq!(
            ConfigDelta::between(&base, &removed).widgets,
            BTreeSet::from(["custom-crypto".to_string()]),
            "a deleted section is a change to that widget"
        );

        let added = live_with("[widgets.custom-moon]\nexec = \"phase\"");
        assert_eq!(
            ConfigDelta::between(&base, &added).widgets,
            BTreeSet::from(["custom-moon".to_string()])
        );
    }

    #[test]
    fn the_remaining_sections_each_have_a_flag_of_their_own() {
        let base = live();

        let mut osd = base.clone();
        osd.osd.timeout_ms = 3000;
        let delta = ConfigDelta::between(&base, &osd);
        assert!(delta.osd);
        assert!(!delta.rebuilds_bars() && delta.widgets.is_empty());

        let mut audio = base.clone();
        audio.audio.allow_overdrive = true;
        assert!(ConfigDelta::between(&base, &audio).audio);

        let mut updates = base.clone();
        updates.updates.check_interval = 7200;
        assert!(ConfigDelta::between(&base, &updates).updates);

        let mut launcher = base.clone();
        launcher.launcher.file_exclusions.push("private".into());
        assert!(ConfigDelta::between(&base, &launcher).launcher);

        let mut appearance = base.clone();
        appearance.appearance.theme_command = Some(vec!["theme-switch".into()]);
        assert!(ConfigDelta::between(&base, &appearance).appearance);

        let mut advanced = base.clone();
        advanced.advanced.pango_font_rendering = false;
        let delta = ConfigDelta::between(&base, &advanced);
        assert!(delta.advanced);
        assert!(
            delta.rebuilds_bars(),
            "it is applied when a window is built"
        );
    }

    #[test]
    fn two_different_configurations_never_compare_as_unchanged() {
        // The catch-all: whatever a section is, editing it has to show up.
        // Anything that fails here is a section with no line in `between`.
        let base = Config::default();
        let mut cases: Vec<(&str, Config)> = Vec::new();

        let mut each = |name: &'static str, edit: fn(&mut Config)| {
            let mut config = base.clone();
            edit(&mut config);
            cases.push((name, config));
        };
        each("bar", |c| c.bar.size = 48);
        each("widgets.placement", |c| c.widgets.right.clear());
        each("widgets.styling", |c| c.widgets.background_opacity = 0.5);
        each("widgets.section", |c| c.widgets.clock.control_panel = true);
        each("theme", |c| c.theme.accent = "none".to_string());
        each("theme.blur", |c| c.theme.blur = true);
        each("theme.motion", |c| c.theme.ripple = false);
        each("osd", |c| c.osd.enabled = false);
        each("audio", |c| c.audio.allow_overdrive = true);
        each("updates", |c| c.updates.check_interval = 120);
        each("launcher", |c| {
            c.launcher.file_exclusions.push("private".into())
        });
        each("appearance", |c| {
            c.appearance.theme_command = Some(vec!["theme-switch".into()])
        });
        each("advanced", |c| c.advanced.compositor = "niri".to_string());

        for (name, config) in cases {
            assert_ne!(config, base, "`{name}` did not actually change anything");
            let delta = ConfigDelta::between(&base, &config);
            assert!(
                !delta.is_empty(),
                "`{name}` changed but the delta says nothing did"
            );
        }
    }
}
