//! Drop-in compatibility contract.
//!
//! `tests/fixtures/live-config.toml` is a byte-for-byte copy of the v1 config
//! this project is written for. It must keep loading unchanged: no validation
//! errors, and no warnings beyond the ones this test names explicitly. Any new
//! warning is a compatibility regression and must either be fixed or added
//! here deliberately.

use topbar_core::config::{Config, Warning};

const LIVE_CONFIG: &str = include_str!("fixtures/live-config.toml");

fn load() -> (Config, Vec<Warning>) {
    Config::parse(LIVE_CONFIG).expect("the live config must parse and validate")
}

#[test]
fn live_config_parses_without_errors() {
    let (config, _) = load();
    assert!(config.validate().is_ok());
}

#[test]
fn live_config_produces_no_warnings_at_all() {
    let (_, warnings) = load();
    let rendered: Vec<String> = warnings.iter().map(Warning::to_string).collect();

    // Every key in this file maps onto a supported v2 key, including
    // `system_monitor`, which v1 never implemented. The one dropped feature it
    // names, `theme.outline`, is set to `false` — which is exactly what v2
    // does — so there is nothing to report.
    assert_eq!(
        warnings,
        Vec::new(),
        "unexpected compatibility warnings: {rendered:#?}"
    );
}

#[test]
fn live_config_widget_placement_survives() {
    let (config, _) = load();
    assert_eq!(config.widgets.left, ["workspaces", "custom-crypto"]);
    assert_eq!(config.widgets.center, ["weather", "clock"]);
    assert_eq!(
        config.widgets.right,
        [
            "tray",
            "system_monitor",
            "headset",
            "keyboard_layout",
            "quick_settings",
            "os_logo",
        ]
    );
}

#[test]
fn live_config_bar_and_theme_values_survive() {
    let (config, _) = load();

    assert_eq!(config.bar.position, "top");
    assert_eq!(config.bar.size, 36);
    assert_eq!(config.bar.spacing, 2);
    assert_eq!(config.bar.inset, 4);
    assert_eq!(config.bar.background_color.as_deref(), Some("#000000"));
    assert_eq!(config.bar.background_opacity, 1.0);

    assert_eq!(config.widgets.border_radius, 50);
    assert_eq!(config.widgets.background_opacity, 0.0);
    assert_eq!(config.widgets.popover_background_opacity, Some(0.76));

    assert_eq!(config.theme.mode, "dark");
    assert_eq!(config.theme.accent, "#70B49B");
    assert!(config.theme.animations);
    assert!(config.theme.ripple);
    assert!(config.theme.blur);
    assert_eq!(config.theme.icons.theme, "Adwaita");
    assert_eq!(config.theme.icons.weight, 400);
    assert_eq!(config.theme.states.success, "#22c55e");
    assert_eq!(config.theme.states.warning, "#f59e0b");
    assert_eq!(config.theme.states.urgent, "#ef4444");
    assert!(config.theme.typography.font_family.starts_with("NotoSans"));
}

#[test]
fn live_config_widget_options_survive() {
    let (config, _) = load();

    let workspaces = &config.widgets.workspaces;
    assert_eq!(workspaces.label_type, "none");
    assert_eq!(workspaces.animate, Some(true));
    assert!(workspaces.filter_by_output);
    assert!(!workspaces.show_unoccupied);

    let clock = &config.widgets.clock;
    assert_eq!(clock.format, "%A, %b %d  %H:%M");
    assert!(clock.control_panel);
    assert_eq!(clock.world_clocks.len(), 2);
    assert_eq!(clock.world_clocks[0].label, "New York");
    assert_eq!(clock.world_clocks[0].timezone, "America/New_York");
    assert_eq!(clock.world_clocks[1].timezone, "Etc/UTC");

    let weather = &config.widgets.weather;
    assert_eq!(weather.unit, "celsius");
    assert_eq!(weather.interval, 1800);
    assert_eq!(weather.max_chars, Some(24));
    assert!(weather.show_description, "new key keeps its default");
    assert_eq!(weather.forecast_days, 5);

    let headset = &config.widgets.headset;
    assert_eq!(headset.interval, 5);
    assert_eq!(headset.tooltip, "Headset battery");
    assert_eq!(headset.max_chars, Some(12));

    let monitor = &config.widgets.system_monitor;
    assert_eq!(monitor.cpu_threshold, 90);
    assert_eq!(monitor.memory_threshold, 85);
    assert_eq!(monitor.disk_threshold, 90, "new key keeps its default");
    assert_eq!(monitor.interval, 5);
    assert_eq!(monitor.tooltip, "System load");

    assert_eq!(
        config.widgets.quick_settings.on_click_right.as_deref(),
        Some("/run/current-system/profile/bin/loginctl lock-session")
    );

    let crypto = config
        .widgets
        .custom
        .get("custom-crypto")
        .expect("custom-crypto section must be kept");
    assert!(
        crypto
            .exec
            .as_deref()
            .is_some_and(|exec| exec.ends_with("crypto.sh -r"))
    );
    assert_eq!(crypto.interval, 1800);
    assert_eq!(crypto.tooltip.as_deref(), Some("Crypto prices"));
    assert_eq!(crypto.max_chars, Some(40));
    assert!(crypto.requires_network);
}

#[test]
fn live_config_top_level_sections_survive() {
    let (config, _) = load();

    assert!(config.osd.enabled);
    assert_eq!(config.osd.position, "bottom");
    assert_eq!(config.osd.timeout_ms, 1500);
    assert!(!config.osd.show_value);

    assert!(!config.audio.allow_overdrive);

    assert_eq!(config.advanced.compositor, "niri");
    assert!(config.advanced.pango_font_rendering);

    assert_eq!(config.updates.check_interval, 3600);
    assert_eq!(
        config.updates.update_count_command.as_deref(),
        Some("guixboy updates --quiet")
    );
}
