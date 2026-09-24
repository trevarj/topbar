//! Configuration schema, loading, validation, and the v1 compatibility surface.
//!
//! # Shape
//!
//! Every section is `#[serde(default)]` and every struct implements [`Default`],
//! so `Config::default()` *is* the merge: a user file only has to name the keys
//! it wants to change. There is no TOML deep-merge machinery.
//!
//! # Compatibility
//!
//! v1 config files must keep working byte-for-byte. Keys whose features were
//! dropped in the rewrite are accepted and produce a *specific* warning
//! explaining what happened (see [`DROPPED_KEYS`]); keys that were never
//! recognized produce a generic "unknown option" warning. Warnings never fail a
//! load — `--strict` is the caller's opt-in for that. Values that cannot be
//! honored at all (`bar.position = "bottom"`) are hard errors with actionable
//! text.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Table;

use crate::error::{Error, Result};
use crate::theme::is_valid_hex_color;

/// The compiled-in example configuration, printed by `--print-example-config`.
pub const EXAMPLE_CONFIG_TOML: &str = include_str!("../../../config.toml");

/// Widget names v2 knows how to build (plus the `custom-*` prefix).
pub const SUPPORTED_WIDGETS: &[&str] = &[
    "clock",
    "crypto",
    "headset",
    "keyboard_layout",
    "notmuch",
    "os_logo",
    "quick_settings",
    "system_monitor",
    "tray",
    "weather",
    "workspaces",
];

/// Crypto assets the built-in widget can price.
pub const SUPPORTED_CRYPTO_ASSETS: &[&str] = &["btc", "eth", "xmr"];

const VALID_OSD_POSITIONS: &[&str] = &["bottom", "left", "right", "top"];
const VALID_COMPOSITORS: &[&str] = &["auto", "niri", "hyprland"];
const VALID_WEATHER_UNITS: &[&str] = &["c", "celsius", "f", "fahrenheit"];
const VALID_LABEL_TYPES: &[&str] = &["none", "index", "name"];
const VALID_LAYOUT_FORMATS: &[&str] = &["short", "long"];

/// Minimum polling interval, in seconds, for services that hit the network or
/// a package manager. Anything lower is a hard error rather than a silent clamp.
const MIN_NETWORK_INTERVAL_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Dropped-feature message tables
// ---------------------------------------------------------------------------

/// Keys whose feature was removed in v2, with the explanation users get.
///
/// Paths are fully qualified. Per-widget keys live in [`DROPPED_WIDGET_KEYS`]
/// because their path contains a user-chosen widget name.
pub const DROPPED_KEYS: &[(&str, &str)] = &[
    (
        "bar.outputs",
        "the output allow-list was dropped; v2 draws a bar on every monitor",
    ),
    (
        "bar.outline",
        "the outline system was dropped; v2 uses a solid GNOME Shell panel with no decorative border",
    ),
    (
        "widgets.outline",
        "the outline system was dropped; widgets are transparent until hovered",
    ),
    (
        "widgets.media",
        "the media section was dropped; control-panel media has no configuration in v2",
    ),
    (
        "theme.scheme",
        "Material You theming was dropped; select theme.mode and theme.palette explicitly",
    ),
    (
        "theme.wallpaper",
        "wallpaper palette extraction was dropped; configure theme.palette explicitly",
    ),
    (
        "theme.popover",
        "per-surface light/dark polarity was dropped; popovers follow theme.mode",
    ),
    (
        "theme.shadows",
        "popover shadows are always on in v2 and are no longer configurable",
    ),
    (
        "theme.outline",
        "the outline system was dropped; surfaces use a fixed 1px hairline border",
    ),
    (
        "theme.outline_width",
        "the outline system was dropped; surfaces use a fixed 1px hairline border",
    ),
    (
        "theme.outline_color",
        "the outline system was dropped; surfaces use a fixed 1px hairline border",
    ),
    (
        "theme.outline_opacity",
        "the outline system was dropped; surfaces use a fixed 1px hairline border",
    ),
    (
        "updates.terminal",
        "the terminal-detection table was dropped; v2 opens upgrades with the XDG default terminal",
    ),
    (
        "widgets.clock.control_panel_weather_widget",
        "the control panel always uses the shared weather service in v2",
    ),
    (
        "widgets.workspaces.separator",
        "workspace separators were dropped; v2 draws GNOME-style dots and an active pill",
    ),
];

/// Per-widget keys whose feature was removed. Matched against the last path
/// segment of any `[widgets.<name>]` table.
pub const DROPPED_WIDGET_KEYS: &[(&str, &str)] = &[
    (
        "disabled",
        "per-widget disabling was dropped; remove the widget from the left/center/right arrays instead",
    ),
    (
        "show_if",
        "conditional visibility was dropped; widgets hide themselves when they have nothing to show",
    ),
    (
        "show_if_interval",
        "conditional visibility was dropped; widgets hide themselves when they have nothing to show",
    ),
    (
        "background_color",
        "per-widget background colors were dropped; panel buttons are transparent until hovered",
    ),
    (
        "outline_color",
        "the outline system was dropped; surfaces use a fixed 1px hairline border",
    ),
];

/// Keys removed from `custom-*` widgets specifically.
pub const DROPPED_CUSTOM_KEYS: &[(&str, &str)] = &[
    (
        "image",
        "custom widget images were dropped; use `icon` with a symbolic icon name",
    ),
    (
        "position",
        "custom widget icon positioning was dropped; the icon always precedes the label",
    ),
];

/// Dropped boolean toggles whose value can still agree with v2's fixed
/// behavior. Setting `theme.outline = false` asks for what v2 already does, so
/// there is nothing to tell the user; `theme.outline = true` asks for something
/// v2 cannot deliver and does warn.
const DROPPED_NOOP_TOGGLES: &[(&str, bool)] = &[
    ("bar.outline", false),
    ("widgets.outline", false),
    ("theme.outline", false),
    ("theme.shadows", true),
];

fn lookup(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, message)| *message)
}

/// Whether a dropped key's configured value already matches v2's behavior, in
/// which case it is silently ignored rather than warned about.
fn is_silent_noop(path: &str, value: &toml::Value, scope: WidgetScope) -> bool {
    if let Some((_, expected)) = DROPPED_NOOP_TOGGLES.iter().find(|(key, _)| *key == path) {
        return value.as_bool() == Some(*expected);
    }
    // An empty output allow-list already means "every monitor".
    if path == "bar.outputs" {
        return value.as_array().is_some_and(|outputs| outputs.is_empty());
    }
    // `disabled = false` is the widget being enabled, which placing it already says.
    if scope != WidgetScope::None && path.ends_with(".disabled") {
        return value.as_bool() == Some(false);
    }
    false
}

// ---------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------

/// A non-fatal configuration diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Fully qualified config key the warning is about.
    pub key: String,
    /// Explanation shown to the user.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

/// Diagnostics accumulated while parsing.
#[derive(Debug, Default)]
struct Lint {
    warnings: Vec<Warning>,
    errors: Vec<String>,
}

impl Lint {
    fn warn(&mut self, key: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Warning {
            key: key.into(),
            message: message.into(),
        });
    }

    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    /// Warn about a key that is not part of the v2 schema, preferring a
    /// specific dropped-feature explanation over the generic message.
    fn unknown_key(&mut self, path: &str, widget_scope: WidgetScope) {
        let leaf = path.rsplit('.').next().unwrap_or(path);
        let message = lookup(DROPPED_KEYS, path)
            .or_else(|| match widget_scope {
                WidgetScope::Custom => {
                    lookup(DROPPED_CUSTOM_KEYS, leaf).or_else(|| lookup(DROPPED_WIDGET_KEYS, leaf))
                }
                WidgetScope::BuiltIn => lookup(DROPPED_WIDGET_KEYS, leaf),
                WidgetScope::None => None,
            })
            .unwrap_or("unknown option, ignored");
        self.warn(path, message);
    }
}

/// Which dropped-key table applies to the enclosing section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidgetScope {
    None,
    BuiltIn,
    Custom,
}

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

/// Root configuration.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Panel geometry and background.
    pub bar: BarConfig,
    /// Widget placement and per-widget options.
    pub widgets: WidgetsConfig,
    /// Colors, motion, icons, typography.
    pub theme: ThemeConfig,
    /// On-screen display.
    pub osd: OsdConfig,
    /// Audio policy.
    pub audio: AudioConfig,
    /// Update-count service.
    pub updates: UpdatesConfig,
    /// Advanced/escape-hatch options.
    pub advanced: AdvancedConfig,
}

/// Outcome of resolving the config search chain.
#[derive(Debug, Clone)]
pub struct ConfigLoad {
    /// The effective configuration.
    pub config: Config,
    /// File the configuration came from, if any.
    pub source: Option<PathBuf>,
    /// Whether built-in defaults were used because no file was found.
    pub used_defaults: bool,
    /// Non-fatal diagnostics collected while parsing.
    pub warnings: Vec<Warning>,
    /// Set when the file was found under the project's former name.
    ///
    /// Deliberately not a [`Warning`]: those are about the file's *contents*
    /// and `--strict` turns them into errors. Where the file sits is a
    /// deprecation, not a mistake, and it must not fail a strict start.
    pub legacy_location: Option<LegacyLocation>,
}

/// A config file found through the pre-rename `gnome-topbar` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyLocation {
    /// Where the file was found.
    pub found: PathBuf,
    /// Where it belongs now.
    pub expected: PathBuf,
}

impl fmt::Display for LegacyLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "config found at legacy path {}; move it to {}",
            abbreviate_home(&self.found).display(),
            abbreviate_home(&self.expected).display()
        )
    }
}

/// Render `path` with `$HOME` written back as `~`, for messages people read.
fn abbreviate_home(path: &Path) -> PathBuf {
    let Ok(home) = env::var("HOME") else {
        return path.to_path_buf();
    };
    if home.is_empty() {
        return path.to_path_buf();
    }
    match path.strip_prefix(&home) {
        Ok(rest) => Path::new("~").join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Directory the config lives in, under an XDG config base.
const CONFIG_DIR: &str = "topbar";
/// The same directory under the project's former name, still honoured.
const LEGACY_CONFIG_DIR: &str = "gnome-topbar";
/// File name inside either directory.
const CONFIG_FILE: &str = "config.toml";

/// One entry in the config search chain.
struct Candidate {
    /// Where to look.
    path: PathBuf,
    /// The current-name path this entry stands in for, if it is a legacy one.
    supersedes: Option<PathBuf>,
}

impl Candidate {
    /// The deprecation this candidate carries, if any.
    fn legacy_location(&self) -> Option<LegacyLocation> {
        self.supersedes.clone().map(|expected| LegacyLocation {
            found: self.path.clone(),
            expected,
        })
    }
}

/// The search chain for the current environment.
fn search_candidates() -> Vec<Candidate> {
    chain_from(
        env::var("XDG_CONFIG_HOME").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    )
}

/// The search chain for a given environment, so tests need not mutate one.
fn chain_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Vec<Candidate> {
    let mut bases = Vec::new();
    if let Some(xdg) = xdg_config_home.filter(|value| !value.is_empty()) {
        bases.push(PathBuf::from(xdg));
    }
    if let Some(home) = home.filter(|value| !value.is_empty()) {
        bases.push(PathBuf::from(home).join(".config"));
    }

    let current = |base: &PathBuf| base.join(CONFIG_DIR).join(CONFIG_FILE);

    let mut candidates: Vec<Candidate> = bases
        .iter()
        .map(|base| Candidate {
            path: current(base),
            supersedes: None,
        })
        .collect();
    // Every legacy directory sorts after every current one, so a user who has
    // already moved the file never sees the deprecation notice.
    candidates.extend(bases.iter().map(|base| Candidate {
        path: base.join(LEGACY_CONFIG_DIR).join(CONFIG_FILE),
        supersedes: Some(current(base)),
    }));
    // The working directory stays last and stays un-namespaced: it is the
    // dev-shell convenience, not a place users keep configuration.
    candidates.push(Candidate {
        path: PathBuf::from(CONFIG_FILE),
        supersedes: None,
    });
    candidates
}

impl Config {
    /// Parse a TOML document into a validated [`Config`] plus its warnings.
    ///
    /// Syntax errors surface as [`Error::Parse`]; unusable values (bad types,
    /// out-of-range numbers, dropped-with-no-fallback values) are collected and
    /// returned together as [`Error::Validation`] so one run reports everything.
    pub fn parse(source: &str) -> Result<(Self, Vec<Warning>)> {
        let mut root: Table = source.parse()?;
        let mut lint = Lint::default();

        let bar = parse_bar(take_table(&mut root, "bar", &mut lint), &mut lint);
        let widgets = parse_widgets(take_table(&mut root, "widgets", &mut lint), &mut lint);
        let theme = parse_theme(take_table(&mut root, "theme", &mut lint), &mut lint);
        let osd = parse_plain(
            "osd",
            take_table(&mut root, "osd", &mut lint),
            OSD_KEYS,
            &mut lint,
        );
        let audio = parse_plain(
            "audio",
            take_table(&mut root, "audio", &mut lint),
            AUDIO_KEYS,
            &mut lint,
        );
        let updates = parse_plain(
            "updates",
            take_table(&mut root, "updates", &mut lint),
            UPDATES_KEYS,
            &mut lint,
        );
        let advanced = parse_plain(
            "advanced",
            take_table(&mut root, "advanced", &mut lint),
            ADVANCED_KEYS,
            &mut lint,
        );

        for key in root.keys() {
            lint.unknown_key(key, WidgetScope::None);
        }

        let config = Config {
            bar,
            widgets,
            theme,
            osd,
            audio,
            updates,
            advanced,
        };

        config.collect_validation_errors(&mut lint);

        if lint.errors.is_empty() {
            Ok((config, lint.warnings))
        } else {
            Err(Error::Validation(lint.errors))
        }
    }

    /// Read and parse a configuration file.
    pub fn load_file(path: &Path) -> Result<(Self, Vec<Warning>)> {
        if !path.exists() {
            return Err(Error::NotFound(path.to_path_buf()));
        }
        let source = std::fs::read_to_string(path)?;
        Self::parse(&source)
    }

    /// Resolve the config search chain and load the first file that exists.
    ///
    /// An explicit path is used strictly: it must exist and parse, with no
    /// fallback — and never reports a legacy location, because the caller
    /// named the file it wanted. Otherwise the XDG chain is searched
    /// (`$XDG_CONFIG_HOME/topbar/config.toml`, `~/.config/topbar/config.toml`,
    /// then the same two under the project's former `gnome-topbar` name, then
    /// `./config.toml`) and built-in defaults are used only when no file
    /// exists anywhere.
    pub fn find_and_load(explicit_path: Option<&Path>) -> Result<ConfigLoad> {
        if let Some(path) = explicit_path {
            let (config, warnings) = Self::load_file(path)?;
            return Ok(ConfigLoad {
                config,
                source: Some(path.to_path_buf()),
                used_defaults: false,
                warnings,
                legacy_location: None,
            });
        }

        for candidate in search_candidates() {
            if candidate.path.exists() {
                let (config, warnings) = Self::load_file(&candidate.path)?;
                return Ok(ConfigLoad {
                    config,
                    warnings,
                    used_defaults: false,
                    legacy_location: candidate.legacy_location(),
                    source: Some(candidate.path),
                });
            }
        }

        Ok(ConfigLoad {
            config: Config::default(),
            source: None,
            used_defaults: true,
            warnings: Vec::new(),
            legacy_location: None,
        })
    }

    /// The configuration as TOML, defaults and all.
    ///
    /// This is what `topbar dump config` prints: the *effective* settings, not
    /// the file the user wrote. Anything the file left out appears here with
    /// the value the panel is actually using, which is the question the
    /// command exists to answer.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|error| Error::Serialize(error.to_string()))
    }

    /// The configuration as JSON, for `topbar dump --json`.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).map_err(|error| Error::Serialize(error.to_string()))
    }

    /// Paths searched, in order, when no `--config` is given.
    pub fn search_paths() -> Vec<PathBuf> {
        search_candidates()
            .into_iter()
            .map(|candidate| candidate.path)
            .collect()
    }

    /// Best-effort read of `[audio] allow_overdrive` alone.
    ///
    /// Volume keybinds must keep working when the rest of the config is broken,
    /// so every failure mode falls back to the safe `false`.
    pub fn read_audio_allow_overdrive(explicit_path: Option<&Path>) -> bool {
        fn read(path: &Path) -> Option<bool> {
            let contents = std::fs::read_to_string(path).ok()?;
            let table: Table = contents.parse().ok()?;
            table
                .get("audio")?
                .as_table()?
                .get("allow_overdrive")?
                .as_bool()
        }

        if let Some(path) = explicit_path {
            return read(path).unwrap_or(false);
        }
        for path in Self::search_paths() {
            if path.exists() {
                return read(&path).unwrap_or(false);
            }
        }
        false
    }

    /// Collect hard validation errors into `lint`.
    fn collect_validation_errors(&self, lint: &mut Lint) {
        self.bar.validate(lint);
        self.widgets.validate(lint);
        self.theme.validate(lint);
        self.osd.validate(lint);
        self.updates.validate(lint);
        self.advanced.validate(lint);
    }

    /// Validate an already-constructed config (used by tests and hot reload).
    pub fn validate(&self) -> Result<()> {
        let mut lint = Lint::default();
        self.collect_validation_errors(&mut lint);
        if lint.errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Validation(lint.errors))
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn take_table(root: &mut Table, name: &str, lint: &mut Lint) -> Table {
    match root.remove(name) {
        None => Table::new(),
        Some(toml::Value::Table(table)) => table,
        Some(other) => {
            lint.error(format!(
                "{name}: expected a table, found {}",
                other.type_str()
            ));
            Table::new()
        }
    }
}

/// Drop keys that are not part of the section's schema, warning about each.
fn retain_known(
    prefix: &str,
    table: &mut Table,
    known: &[&str],
    scope: WidgetScope,
    lint: &mut Lint,
) {
    let unknown: Vec<String> = table
        .keys()
        .filter(|key| !known.contains(&key.as_str()))
        .cloned()
        .collect();
    for key in unknown {
        let path = format!("{prefix}.{key}");
        let value = table.remove(&key).unwrap_or(toml::Value::Boolean(false));
        if !is_silent_noop(&path, &value, scope) {
            lint.unknown_key(&path, scope);
        }
    }
}

fn deserialize<T: Default + for<'de> Deserialize<'de>>(
    prefix: &str,
    table: Table,
    lint: &mut Lint,
) -> T {
    match table.try_into() {
        Ok(value) => value,
        Err(err) => {
            lint.error(format!("{prefix}: {err}"));
            T::default()
        }
    }
}

/// Lint + deserialize a section that has no nested tables of its own.
fn parse_plain<T: Default + for<'de> Deserialize<'de>>(
    prefix: &str,
    mut table: Table,
    known: &[&str],
    lint: &mut Lint,
) -> T {
    retain_known(prefix, &mut table, known, WidgetScope::None, lint);
    deserialize(prefix, table, lint)
}

fn parse_bar(mut table: Table, lint: &mut Lint) -> BarConfig {
    if let Some(position) = table.get("position").and_then(toml::Value::as_str)
        && position == "bottom"
    {
        lint.error("bar.position: \"bottom\" is not supported — v2 supports top only".to_string());
        table.remove("position");
    }
    retain_known("bar", &mut table, BAR_KEYS, WidgetScope::None, lint);
    deserialize("bar", table, lint)
}

fn parse_theme(mut table: Table, lint: &mut Lint) -> ThemeConfig {
    match table.get("mode").and_then(toml::Value::as_str) {
        Some("dark" | "light") | None => {}
        Some(mode @ ("auto" | "gtk")) => {
            lint.warn(
                "theme.mode",
                format!(
                    "\"{mode}\" was dropped; choose \"dark\" or \"light\"; rendering as \"dark\""
                ),
            );
            table.remove("mode");
        }
        Some(other) => {
            lint.error(format!(
                "theme.mode: invalid value '{other}', expected \"dark\" or \"light\""
            ));
            table.remove("mode");
        }
    }

    if let Some("gtk") = table.get("accent").and_then(toml::Value::as_str) {
        lint.warn(
            "theme.accent",
            "\"gtk\" was dropped; v2 does not follow the GTK theme — set a hex color or \"none\"",
        );
        table.remove("accent");
    }

    for (name, keys) in [
        ("icons", THEME_ICONS_KEYS),
        ("states", THEME_STATES_KEYS),
        ("typography", THEME_TYPOGRAPHY_KEYS),
        ("palette", THEME_PALETTE_KEYS),
    ] {
        if let Some(toml::Value::Table(nested)) = table.get_mut(name) {
            retain_known(
                &format!("theme.{name}"),
                nested,
                keys,
                WidgetScope::None,
                lint,
            );
        }
    }

    retain_known("theme", &mut table, THEME_KEYS, WidgetScope::None, lint);
    deserialize("theme", table, lint)
}

fn parse_widgets(mut table: Table, lint: &mut Lint) -> WidgetsConfig {
    let defaults = WidgetsConfig::default();

    let left = parse_placements("left", table.remove("left"), &defaults.left, lint);
    let center = parse_placements("center", table.remove("center"), &defaults.center, lint);
    let right = parse_placements("right", table.remove("right"), &defaults.right, lint);

    let referenced: Vec<String> = left
        .iter()
        .chain(center.iter())
        .chain(right.iter())
        .cloned()
        .collect();

    // Split the per-widget tables out before linting the section's own keys.
    let mut widget_tables: BTreeMap<String, Table> = BTreeMap::new();
    let nested: Vec<String> = table
        .iter()
        .filter(|(_, value)| value.is_table())
        .map(|(key, _)| key.clone())
        .collect();
    for key in nested {
        if let Some(toml::Value::Table(nested)) = table.remove(&key) {
            widget_tables.insert(key, nested);
        }
    }

    retain_known("widgets", &mut table, WIDGETS_KEYS, WidgetScope::None, lint);
    let mut widgets: WidgetsConfig = deserialize("widgets", table, lint);
    widgets.left = left;
    widgets.center = center;
    widgets.right = right;

    for (name, mut nested) in widget_tables {
        let prefix = format!("widgets.{name}");

        if name == "media" {
            lint.unknown_key(&prefix, WidgetScope::None);
            continue;
        }

        if !referenced.contains(&name) {
            lint.warn(
                &prefix,
                "options defined but the widget is not placed in left, center, or right",
            );
        }

        if let Some(suffix) = name.strip_prefix("custom-") {
            if suffix.is_empty() {
                lint.error(format!(
                    "{prefix}: custom widgets need a name after `custom-`"
                ));
                continue;
            }
            retain_known(&prefix, &mut nested, CUSTOM_KEYS, WidgetScope::Custom, lint);
            let config: CustomWidgetConfig = deserialize(&prefix, nested, lint);
            widgets.custom.insert(name, config);
            continue;
        }

        macro_rules! widget_section {
            ($field:ident, $keys:expr) => {{
                retain_known(&prefix, &mut nested, $keys, WidgetScope::BuiltIn, lint);
                widgets.$field = deserialize(&prefix, nested, lint);
            }};
        }

        match name.as_str() {
            "workspaces" => widget_section!(workspaces, WORKSPACES_KEYS),
            "clock" => widget_section!(clock, CLOCK_KEYS),
            "weather" => widget_section!(weather, WEATHER_KEYS),
            "crypto" => widget_section!(crypto, CRYPTO_KEYS),
            "notmuch" => widget_section!(notmuch, NOTMUCH_KEYS),
            "tray" => widget_section!(tray, TRAY_KEYS),
            "quick_settings" => widget_section!(quick_settings, QUICK_SETTINGS_KEYS),
            "system_monitor" => widget_section!(system_monitor, SYSTEM_MONITOR_KEYS),
            "headset" => widget_section!(headset, HEADSET_KEYS),
            "keyboard_layout" => widget_section!(keyboard_layout, KEYBOARD_LAYOUT_KEYS),
            "os_logo" => widget_section!(os_logo, OS_LOGO_KEYS),
            _ => lint.unknown_key(&prefix, WidgetScope::None),
        }
    }

    widgets
}

/// Normalize a placement array to plain widget names.
///
/// Accepts v1's `{ group = [...] }` entries (flattened with a warning) and
/// v1's inline `name:arg` syntax (base name kept, argument warned about).
fn parse_placements(
    section: &str,
    value: Option<toml::Value>,
    default: &[String],
    lint: &mut Lint,
) -> Vec<String> {
    let prefix = format!("widgets.{section}");
    let Some(value) = value else {
        return default.to_vec();
    };
    let toml::Value::Array(items) = value else {
        lint.error(format!("{prefix}: expected an array of widget names"));
        return default.to_vec();
    };

    let mut raw = Vec::new();
    for item in items {
        match item {
            toml::Value::String(name) => raw.push(name),
            toml::Value::Table(table) => match table.get("group").and_then(toml::Value::as_array) {
                Some(group) => {
                    lint.warn(
                        &prefix,
                        "widget groups were dropped; the grouped widgets are placed individually",
                    );
                    for entry in group {
                        match entry.as_str() {
                            Some(name) => raw.push(name.to_string()),
                            None => {
                                lint.error(format!("{prefix}: group entries must be widget names"))
                            }
                        }
                    }
                }
                None => lint.error(format!("{prefix}: expected a widget name or a group table")),
            },
            other => lint.error(format!(
                "{prefix}: expected a widget name, found {}",
                other.type_str()
            )),
        }
    }

    let mut names = Vec::with_capacity(raw.len());
    for name in raw {
        let base = match name.split_once(':') {
            Some((base, _)) => {
                lint.warn(
                    &prefix,
                    format!(
                        "inline widget arguments were dropped; \"{name}\" is placed as \"{base}\""
                    ),
                );
                base.to_string()
            }
            None => name,
        };

        if base.starts_with("custom-") || SUPPORTED_WIDGETS.contains(&base.as_str()) {
            names.push(base);
        } else {
            lint.warn(
                &prefix,
                format!("unknown widget \"{base}\" will be skipped"),
            );
        }
    }

    names
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

const BAR_KEYS: &[&str] = &[
    "position",
    "size",
    "spacing",
    "screen_margin",
    "inset",
    "padding",
    "border_radius",
    "popover_offset",
    "background_color",
    "background_opacity",
];

/// Panel geometry and background.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarConfig {
    /// Screen edge the bar occupies. v2 supports `"top"` only.
    pub position: String,
    /// Bar height in pixels.
    pub size: u32,
    /// Gap between widgets in pixels.
    pub spacing: u32,
    /// Gap between the screen edge and the bar window in pixels.
    pub screen_margin: u32,
    /// Gap between the bar edge and the first/last section in pixels.
    pub inset: u32,
    /// Extra vertical padding inside the bar.
    pub padding: u32,
    /// Bar corner radius in pixels.
    pub border_radius: u32,
    /// Gap between the bar and popovers anchored to it, in pixels.
    pub popover_offset: u32,
    /// Bar background override (hex). Unset inherits the resolved palette.
    pub background_color: Option<String>,
    /// Bar background opacity, 0.0..=1.0.
    pub background_opacity: f64,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            position: "top".to_string(),
            size: 36,
            spacing: 2,
            screen_margin: 0,
            inset: 4,
            padding: 0,
            border_radius: 0,
            popover_offset: 1,
            background_color: None,
            background_opacity: 1.0,
        }
    }
}

impl BarConfig {
    fn validate(&self, lint: &mut Lint) {
        if self.position != "top" {
            lint.error(format!(
                "bar.position: invalid value '{}', expected \"top\"",
                self.position
            ));
        }
        if self.size == 0 {
            lint.error("bar.size: must be greater than 0");
        }
        if let Some(color) = &self.background_color {
            check_hex(lint, "bar.background_color", color);
        }
        check_opacity(lint, "bar.background_opacity", self.background_opacity);
    }
}

const WIDGETS_KEYS: &[&str] = &[
    "left",
    "center",
    "right",
    "border_radius",
    "background_color",
    "background_opacity",
    "popover_background_opacity",
];

/// Widget placement plus every per-widget section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetsConfig {
    /// Widgets in the left section, in order.
    pub left: Vec<String>,
    /// Widgets in the center section, in order.
    pub center: Vec<String>,
    /// Widgets in the right section, in order.
    pub right: Vec<String>,
    /// Widget corner radius as a percentage of widget height.
    pub border_radius: u32,
    /// Widget background color (hex). Unset uses the theme surface color.
    pub background_color: Option<String>,
    /// Widget background opacity, 0.0..=1.0.
    pub background_opacity: f64,
    /// Popover background opacity, 0.0..=1.0. Unset follows the bar.
    pub popover_background_opacity: Option<f64>,

    /// `[widgets.workspaces]`
    #[serde(skip)]
    pub workspaces: WorkspacesConfig,
    /// `[widgets.clock]`
    #[serde(skip)]
    pub clock: ClockConfig,
    /// `[widgets.weather]`
    #[serde(skip)]
    pub weather: WeatherConfig,
    /// `[widgets.crypto]`
    #[serde(skip)]
    pub crypto: CryptoConfig,
    /// `[widgets.notmuch]`
    #[serde(skip)]
    pub notmuch: NotmuchConfig,
    /// `[widgets.tray]`
    #[serde(skip)]
    pub tray: TrayConfig,
    /// `[widgets.quick_settings]`
    #[serde(skip)]
    pub quick_settings: QuickSettingsConfig,
    /// `[widgets.system_monitor]`
    #[serde(skip)]
    pub system_monitor: SystemMonitorConfig,
    /// `[widgets.headset]`
    #[serde(skip)]
    pub headset: HeadsetConfig,
    /// `[widgets.keyboard_layout]`
    #[serde(skip)]
    pub keyboard_layout: KeyboardLayoutConfig,
    /// `[widgets.os_logo]`
    #[serde(skip)]
    pub os_logo: OsLogoConfig,
    /// `[widgets.custom-*]`, keyed by the full widget name.
    #[serde(skip)]
    pub custom: BTreeMap<String, CustomWidgetConfig>,
}

impl Default for WidgetsConfig {
    fn default() -> Self {
        Self {
            left: vec!["workspaces".to_string()],
            center: vec!["clock".to_string()],
            right: vec!["tray".to_string(), "quick_settings".to_string()],
            border_radius: 50,
            background_color: None,
            background_opacity: 0.0,
            popover_background_opacity: None,
            workspaces: WorkspacesConfig::default(),
            clock: ClockConfig::default(),
            weather: WeatherConfig::default(),
            crypto: CryptoConfig::default(),
            notmuch: NotmuchConfig::default(),
            tray: TrayConfig::default(),
            quick_settings: QuickSettingsConfig::default(),
            system_monitor: SystemMonitorConfig::default(),
            headset: HeadsetConfig::default(),
            keyboard_layout: KeyboardLayoutConfig::default(),
            os_logo: OsLogoConfig::default(),
            custom: BTreeMap::new(),
        }
    }
}

impl WidgetsConfig {
    /// Every widget name placed in any section, in left→center→right order.
    pub fn placed(&self) -> impl Iterator<Item = &str> {
        self.left
            .iter()
            .chain(self.center.iter())
            .chain(self.right.iter())
            .map(String::as_str)
    }

    fn validate(&self, lint: &mut Lint) {
        check_opacity(lint, "widgets.background_opacity", self.background_opacity);
        if let Some(color) = &self.background_color {
            check_hex(lint, "widgets.background_color", color);
        }
        if let Some(opacity) = self.popover_background_opacity {
            check_opacity(lint, "widgets.popover_background_opacity", opacity);
        }

        self.workspaces.validate(lint);
        self.clock.validate(lint);
        self.weather.validate(lint);
        self.crypto.validate(lint);
        self.notmuch.validate(lint);
        self.quick_settings.validate(lint);
        self.system_monitor.validate(lint);
        self.headset.validate(lint);
        self.keyboard_layout.validate(lint);
        for (name, custom) in &self.custom {
            custom.validate(name, lint);
        }
    }
}

const WORKSPACES_KEYS: &[&str] = &[
    "label_type",
    "animate",
    "filter_by_output",
    "show_unoccupied",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.workspaces]` — GNOME Activities-style dots with an active pill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspacesConfig {
    /// Indicator label: `"none"`, `"index"`, or `"name"`.
    pub label_type: String,
    /// Animate dot↔pill transitions. Unset inherits `theme.animations`.
    pub animate: Option<bool>,
    /// Show only workspaces belonging to this bar's output.
    pub filter_by_output: bool,
    /// Show compositor workspaces that hold no windows.
    pub show_unoccupied: bool,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            label_type: "none".to_string(),
            animate: None,
            filter_by_output: true,
            show_unoccupied: false,
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl WorkspacesConfig {
    fn validate(&self, lint: &mut Lint) {
        check_enum(
            lint,
            "widgets.workspaces.label_type",
            &self.label_type,
            VALID_LABEL_TYPES,
        );
    }
}

const CLOCK_KEYS: &[&str] = &[
    "format",
    "control_panel",
    "show_week_numbers",
    "world_clocks",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// A secondary time zone shown in the control panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldClock {
    /// Display name for the row.
    pub label: String,
    /// IANA time zone, e.g. `"America/New_York"`.
    pub timezone: String,
}

/// `[widgets.clock]` — panel clock and the GNOME date-menu control panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    /// `strftime` format for the panel label.
    pub format: String,
    /// Open the notifications/calendar control panel on click.
    pub control_panel: bool,
    /// Show ISO week numbers in the calendar.
    pub show_week_numbers: bool,
    /// Extra time zones listed in the control panel.
    pub world_clocks: Vec<WorldClock>,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            format: "%a %b %-d  %H:%M".to_string(),
            control_panel: false,
            show_week_numbers: true,
            world_clocks: Vec::new(),
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl ClockConfig {
    fn validate(&self, lint: &mut Lint) {
        if self.format.trim().is_empty() {
            lint.error("widgets.clock.format: must not be empty");
        }
        for clock in &self.world_clocks {
            if clock.label.trim().is_empty() || clock.timezone.trim().is_empty() {
                lint.error(
                    "widgets.clock.world_clocks: each entry needs a non-empty label and timezone",
                );
            }
        }
    }
}

const WEATHER_KEYS: &[&str] = &[
    "latitude",
    "longitude",
    "unit",
    "interval",
    "tooltip",
    "max_chars",
    "show_description",
    "forecast_days",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.weather]` — Open-Meteo current conditions and forecast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    /// Latitude. Unset falls back to the coordinates saved by the widget.
    pub latitude: Option<f64>,
    /// Longitude. Unset falls back to the coordinates saved by the widget.
    pub longitude: Option<f64>,
    /// `"celsius"` or `"fahrenheit"`.
    pub unit: String,
    /// Seconds between refreshes.
    pub interval: u64,
    /// Static tooltip prefix.
    pub tooltip: String,
    /// Ellipsize the panel label past this many characters.
    pub max_chars: Option<u32>,
    /// Whether the panel label says the condition as well as the temperature.
    ///
    /// False leaves `21°` alone, which is what a narrow centre section has room
    /// for. The popover and the control panel's forecast card still name the
    /// condition: this is the bar's line, not the reading.
    pub show_description: bool,
    /// Forecast rows in the control panel, 3..=5.
    pub forecast_days: u32,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            latitude: None,
            longitude: None,
            unit: "celsius".to_string(),
            interval: 1800,
            tooltip: "Weather".to_string(),
            max_chars: None,
            show_description: true,
            forecast_days: 5,
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl WeatherConfig {
    fn validate(&self, lint: &mut Lint) {
        check_enum(
            lint,
            "widgets.weather.unit",
            &self.unit,
            VALID_WEATHER_UNITS,
        );
        check_interval(lint, "widgets.weather.interval", self.interval);
        if !(3..=5).contains(&self.forecast_days) {
            lint.error(format!(
                "widgets.weather.forecast_days: invalid value '{}', must be between 3 and 5",
                self.forecast_days
            ));
        }
        if let Some(latitude) = self.latitude
            && !(-90.0..=90.0).contains(&latitude)
        {
            lint.error(format!(
                "widgets.weather.latitude: invalid value '{latitude}', must be between -90 and 90"
            ));
        }
        if let Some(longitude) = self.longitude
            && !(-180.0..=180.0).contains(&longitude)
        {
            lint.error(format!(
                "widgets.weather.longitude: invalid value '{longitude}', must be between -180 and 180"
            ));
        }
    }
}

const CRYPTO_KEYS: &[&str] = &[
    "entries",
    "interval",
    "tooltip",
    "max_chars",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.crypto]` — CoinGecko prices for a closed set of assets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CryptoConfig {
    /// Entries: a single asset (`"btc"`) or a pair ratio (`"eth/btc"`).
    pub entries: Vec<String>,
    /// Seconds between refreshes.
    pub interval: u64,
    /// Static tooltip prefix.
    pub tooltip: String,
    /// Ellipsize the panel label past this many characters.
    pub max_chars: Option<u32>,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for CryptoConfig {
    fn default() -> Self {
        Self {
            entries: vec!["btc".to_string(), "eth".to_string(), "eth/btc".to_string()],
            interval: 1800,
            tooltip: "Crypto prices".to_string(),
            max_chars: None,
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl CryptoConfig {
    fn validate(&self, lint: &mut Lint) {
        check_interval(lint, "widgets.crypto.interval", self.interval);
        for entry in &self.entries {
            if !is_valid_crypto_entry(entry) {
                lint.error(format!(
                    "widgets.crypto.entries: invalid entry '{entry}', expected one of {} or a pair like \"eth/btc\"",
                    SUPPORTED_CRYPTO_ASSETS.join(", ")
                ));
            }
        }
    }
}

/// Whether a crypto entry names a supported asset or a supported pair.
pub fn is_valid_crypto_entry(entry: &str) -> bool {
    let asset = |name: &str| SUPPORTED_CRYPTO_ASSETS.contains(&name);
    match entry.split_once('/') {
        Some((base, quote)) => asset(base) && asset(quote) && base != quote,
        None => asset(entry),
    }
}

const NOTMUCH_KEYS: &[&str] = &[
    "query",
    "interval",
    "max_items",
    "tooltip",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.notmuch]` — unread mail, counted with notmuch.
///
/// The maildir itself is whatever fills it — lieer, offlineimap, mbsync — and
/// notmuch's business after that. The panel reads the index notmuch already
/// knows where to find and never opens a message file, which is why there is
/// no `maildir` key here: `NOTMUCH_CONFIG` is where a person says where their
/// mail lives, and a second answer to that question is a second one to get
/// wrong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotmuchConfig {
    /// The notmuch query the count and the list both come from.
    ///
    /// Not validated beyond being non-empty. Notmuch's parser is lenient —
    /// `tag:unread and ((` returns a number rather than an error — so there is
    /// nothing here that could tell a typo from an intention.
    pub query: String,
    /// Seconds between counts.
    pub interval: u64,
    /// How many conversations the popover lists.
    pub max_items: u32,
    /// Static tooltip, shown until there is a count to show instead.
    pub tooltip: String,
    /// Shell command run on left-click. Left-click opens the popover.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for NotmuchConfig {
    fn default() -> Self {
        Self {
            query: "tag:unread and tag:inbox".to_string(),
            interval: 300,
            max_items: 10,
            tooltip: "Mail".to_string(),
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl NotmuchConfig {
    fn validate(&self, lint: &mut Lint) {
        check_interval(lint, "widgets.notmuch.interval", self.interval);
        if self.query.trim().is_empty() {
            lint.error("widgets.notmuch.query: must not be empty".to_string());
        }
        if self.max_items == 0 {
            lint.error(
                "widgets.notmuch.max_items: invalid value '0', a list of nothing is not a list"
                    .to_string(),
            );
        }
    }
}

const TRAY_KEYS: &[&str] = &[
    "max_icons",
    "pixmap_icon_size",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.tray]` — StatusNotifierItem host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrayConfig {
    /// Icons shown inline before the overflow chevron appears.
    pub max_icons: u32,
    /// Render size for pixmap (non-themed) icons. Unset uses the theme size.
    pub pixmap_icon_size: Option<u32>,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            max_icons: 12,
            pixmap_icon_size: None,
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

const QUICK_SETTINGS_KEYS: &[&str] = &[
    "network",
    "bluetooth",
    "vpn",
    "idle_inhibitor",
    "updates",
    "audio",
    "mic",
    "brightness",
    "power",
    "battery",
    "battery_health",
    "resource_overview",
    "vpn_close_on_connect",
    "audio_scroll_percentage",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.quick_settings]` — the GNOME 45-style aggregate menu.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuickSettingsConfig {
    /// Show the Wi-Fi/wired network row.
    pub network: bool,
    /// Show the Bluetooth row.
    pub bluetooth: bool,
    /// Show the VPN row.
    pub vpn: bool,
    /// Show the idle-inhibitor (Caffeine) toggle.
    pub idle_inhibitor: bool,
    /// Show the pending-updates card.
    pub updates: bool,
    /// Show the output volume slider.
    pub audio: bool,
    /// When the microphone slider is shown.
    pub mic: MicSlider,
    /// Show the backlight brightness slider.
    pub brightness: bool,
    /// Show the power/suspend/restart section.
    pub power: bool,
    /// Show the battery pill in the panel indicator.
    pub battery: bool,
    /// Show the battery health and charge-threshold card.
    pub battery_health: bool,
    /// Show the CPU/memory/disk overview card.
    pub resource_overview: bool,
    /// Close the panel once a VPN connection succeeds.
    pub vpn_close_on_connect: bool,
    /// Volume change per scroll tick, in percentage points (1..=25).
    pub audio_scroll_percentage: u32,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for QuickSettingsConfig {
    fn default() -> Self {
        Self {
            network: true,
            bluetooth: true,
            vpn: true,
            idle_inhibitor: true,
            updates: true,
            audio: true,
            mic: MicSlider::Auto,
            brightness: true,
            power: true,
            battery: true,
            battery_health: true,
            resource_overview: true,
            vpn_close_on_connect: true,
            audio_scroll_percentage: 5,
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

/// When the Quick Settings microphone slider is shown.
///
/// Accepts `"auto"`, `"always"` and `"never"`, and — because the key was a
/// plain toggle before the tri-state existed — the booleans too: `true` is
/// `"auto"`, `false` is `"never"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MicSlider {
    /// While a source is in use — less clutter, and a privacy signal.
    Auto,
    /// Whenever the menu is open.
    Always,
    /// Not at all.
    Never,
}

impl<'de> Deserialize<'de> for MicSlider {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = MicSlider;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#""auto", "always", "never", or a boolean"#)
            }

            fn visit_bool<E: serde::de::Error>(
                self,
                on: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(if on {
                    MicSlider::Auto
                } else {
                    MicSlider::Never
                })
            }

            fn visit_str<E: serde::de::Error>(
                self,
                word: &str,
            ) -> std::result::Result<Self::Value, E> {
                match word {
                    "auto" => Ok(MicSlider::Auto),
                    "always" => Ok(MicSlider::Always),
                    "never" => Ok(MicSlider::Never),
                    other => Err(E::invalid_value(serde::de::Unexpected::Str(other), &self)),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl QuickSettingsConfig {
    fn validate(&self, lint: &mut Lint) {
        if !(1..=25).contains(&self.audio_scroll_percentage) {
            lint.error(format!(
                "widgets.quick_settings.audio_scroll_percentage: invalid value '{}', must be between 1 and 25",
                self.audio_scroll_percentage
            ));
        }
    }
}

const SYSTEM_MONITOR_KEYS: &[&str] = &[
    "cpu_threshold",
    "memory_threshold",
    "disk_threshold",
    "interval",
    "tooltip",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.system_monitor]` — alert-only resource indicator.
///
/// The widget stays invisible while every metric is healthy and fades in with
/// warning-tinted icons once a threshold is crossed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemMonitorConfig {
    /// CPU usage percentage that makes the widget appear.
    pub cpu_threshold: u32,
    /// Memory usage percentage that makes the widget appear.
    pub memory_threshold: u32,
    /// Disk usage percentage that makes the widget appear.
    pub disk_threshold: u32,
    /// Seconds between samples.
    pub interval: u64,
    /// Static tooltip prefix.
    pub tooltip: String,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for SystemMonitorConfig {
    fn default() -> Self {
        Self {
            cpu_threshold: 90,
            memory_threshold: 85,
            disk_threshold: 90,
            interval: 5,
            tooltip: "System load".to_string(),
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl SystemMonitorConfig {
    fn validate(&self, lint: &mut Lint) {
        for (key, value) in [
            ("cpu_threshold", self.cpu_threshold),
            ("memory_threshold", self.memory_threshold),
            ("disk_threshold", self.disk_threshold),
        ] {
            if !(1..=100).contains(&value) {
                lint.error(format!(
                    "widgets.system_monitor.{key}: invalid value '{value}', must be between 1 and 100"
                ));
            }
        }
        if self.interval == 0 {
            lint.error("widgets.system_monitor.interval: must be greater than 0");
        }
    }
}

const HEADSET_KEYS: &[&str] = &[
    "interval",
    "tooltip",
    "max_chars",
    "command",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.headset]` — `headsetcontrol` battery indicator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeadsetConfig {
    /// Seconds between polls.
    pub interval: u64,
    /// Static tooltip prefix.
    pub tooltip: String,
    /// Ellipsize the panel label past this many characters.
    pub max_chars: Option<u32>,
    /// Executable queried for battery state.
    pub command: String,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for HeadsetConfig {
    fn default() -> Self {
        Self {
            interval: 5,
            tooltip: "Headset battery".to_string(),
            max_chars: None,
            command: "headsetcontrol".to_string(),
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl HeadsetConfig {
    fn validate(&self, lint: &mut Lint) {
        if self.interval == 0 {
            lint.error("widgets.headset.interval: must be greater than 0");
        }
        if self.command.trim().is_empty() {
            lint.error("widgets.headset.command: must not be empty");
        }
    }
}

const KEYBOARD_LAYOUT_KEYS: &[&str] = &[
    "show_icon",
    "show_label",
    "format",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.keyboard_layout]` — active xkb layout indicator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardLayoutConfig {
    /// Show the keyboard icon.
    pub show_icon: bool,
    /// Show the layout label.
    pub show_label: bool,
    /// `"short"` (US) or `"long"` (English (US)).
    pub format: String,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for KeyboardLayoutConfig {
    fn default() -> Self {
        Self {
            show_icon: true,
            show_label: true,
            format: "short".to_string(),
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

impl KeyboardLayoutConfig {
    fn validate(&self, lint: &mut Lint) {
        check_enum(
            lint,
            "widgets.keyboard_layout.format",
            &self.format,
            VALID_LAYOUT_FORMATS,
        );
    }
}

const OS_LOGO_KEYS: &[&str] = &[
    "tooltip",
    "label",
    "max_chars",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.os_logo]` — distro glyph read from `/etc/os-release`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OsLogoConfig {
    /// Tooltip override. Unset uses the detected distro name.
    pub tooltip: Option<String>,
    /// Label override. Unset uses the detected distro glyph.
    pub label: Option<String>,
    /// Ellipsize the panel label past this many characters.
    pub max_chars: Option<u32>,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl Default for OsLogoConfig {
    fn default() -> Self {
        Self {
            tooltip: None,
            label: None,
            max_chars: Some(4),
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
        }
    }
}

const CUSTOM_KEYS: &[&str] = &[
    "exec",
    "interval",
    "tooltip",
    "max_chars",
    "requires_network",
    "icon",
    "label",
    "template",
    "on_click",
    "on_click_right",
    "on_click_middle",
];

/// `[widgets.custom-*]` — script-backed indicator.
///
/// `exec` output is read as a plain first line or as Waybar-style JSON
/// (`{"text": …, "tooltip": …, "class": …}`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomWidgetConfig {
    /// Shell command whose output becomes the label.
    pub exec: Option<String>,
    /// Seconds between runs. `0` runs once at startup.
    pub interval: u64,
    /// Static tooltip. Overridden by JSON output.
    pub tooltip: Option<String>,
    /// Ellipsize the panel label past this many characters.
    pub max_chars: Option<u32>,
    /// Wait for an active network connection before running `exec`.
    pub requires_network: bool,
    /// Symbolic icon name shown before the label.
    pub icon: Option<String>,
    /// Static/fallback label text.
    pub label: String,
    /// Format string for `exec` output; `{output}` is substituted.
    pub template: Option<String>,
    /// Shell command run on left-click.
    pub on_click: Option<String>,
    /// Shell command run on right-click.
    pub on_click_right: Option<String>,
    /// Shell command run on middle-click.
    pub on_click_middle: Option<String>,
}

impl CustomWidgetConfig {
    fn validate(&self, name: &str, lint: &mut Lint) {
        if self.exec.is_none() && self.label.is_empty() && self.icon.is_none() {
            lint.error(format!(
                "widgets.{name}: needs at least one of `exec`, `label`, or `icon`"
            ));
        }
        if let Some(template) = &self.template
            && !template.contains("{output}")
        {
            lint.error(format!(
                "widgets.{name}.template: must contain the `{{output}}` placeholder"
            ));
        }
    }
}

const THEME_KEYS: &[&str] = &[
    "mode",
    "accent",
    "animations",
    "ripple",
    "blur",
    "icons",
    "states",
    "typography",
    "palette",
];

/// `[theme]` — palette, motion, icons, typography.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Native `"dark"` or `"light"` polarity.
    pub mode: String,
    /// Accent color: a hex color, or `"none"` for monochrome.
    pub accent: String,
    /// Master switch for transitions and animations.
    pub animations: bool,
    /// Material-style ripple on press.
    pub ripple: bool,
    /// Request compositor blur behind panel surfaces.
    pub blur: bool,
    /// Icon theme selection.
    pub icons: ThemeIcons,
    /// Semantic state colors.
    pub states: ThemeStates,
    /// Font settings.
    pub typography: ThemeTypography,
    /// Optional neutral palette overrides.
    pub palette: ThemePalette,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            mode: "dark".to_string(),
            accent: "#3584e4".to_string(),
            animations: true,
            ripple: true,
            blur: false,
            icons: ThemeIcons::default(),
            states: ThemeStates::default(),
            typography: ThemeTypography::default(),
            palette: ThemePalette::default(),
        }
    }
}

impl ThemeConfig {
    fn validate(&self, lint: &mut Lint) {
        check_enum(lint, "theme.mode", &self.mode, &["dark", "light"]);
        for (key, value) in [
            ("background", &self.palette.background),
            ("surface", &self.palette.surface),
            ("foreground", &self.palette.foreground),
        ] {
            if let Some(color) = value {
                check_hex(lint, &format!("theme.palette.{key}"), color);
            }
        }
        if self.accent != "none" {
            check_hex(lint, "theme.accent", &self.accent);
        }
        check_hex(lint, "theme.states.success", &self.states.success);
        check_hex(lint, "theme.states.warning", &self.states.warning);
        check_hex(lint, "theme.states.urgent", &self.states.urgent);
        if self.typography.font_family.trim().is_empty() {
            lint.error("theme.typography.font_family: must not be empty");
        }
        if self.icons.theme.trim().is_empty() {
            lint.error("theme.icons.theme: must not be empty");
        }
    }
}

const THEME_PALETTE_KEYS: &[&str] = &["background", "surface", "foreground"];

/// `[theme.palette]` — omitted roles inherit the selected mode's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemePalette {
    /// Panel/popover background before explicit bar overrides.
    pub background: Option<String>,
    /// Elevated and widget surface color.
    pub surface: Option<String>,
    /// Normal text, neutral fills and symbolic icons.
    pub foreground: Option<String>,
}

const THEME_ICONS_KEYS: &[&str] = &["theme", "weight"];

/// `[theme.icons]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeIcons {
    /// GTK icon theme name.
    pub theme: String,
    /// Symbolic icon stroke weight hint.
    pub weight: u16,
}

impl Default for ThemeIcons {
    fn default() -> Self {
        Self {
            theme: "Adwaita".to_string(),
            weight: 400,
        }
    }
}

const THEME_STATES_KEYS: &[&str] = &["success", "warning", "urgent"];

/// `[theme.states]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeStates {
    /// Success/connected tint.
    pub success: String,
    /// Warning tint.
    pub warning: String,
    /// Urgent/error tint.
    pub urgent: String,
}

impl Default for ThemeStates {
    fn default() -> Self {
        Self {
            success: "#4a7a4a".to_string(),
            warning: "#e5c07b".to_string(),
            urgent: "#ff6b6b".to_string(),
        }
    }
}

const THEME_TYPOGRAPHY_KEYS: &[&str] = &["font_family"];

/// `[theme.typography]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeTypography {
    /// CSS font stack for panel and popover text.
    pub font_family: String,
}

impl Default for ThemeTypography {
    fn default() -> Self {
        Self {
            font_family: "Adwaita Sans, Cantarell, Noto Sans, sans-serif".to_string(),
        }
    }
}

const OSD_KEYS: &[&str] = &["enabled", "position", "show_value", "timeout_ms"];

/// `[osd]` — the volume/brightness capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OsdConfig {
    /// Show the OSD at all.
    pub enabled: bool,
    /// Screen edge: `"bottom"`, `"top"`, `"left"`, or `"right"`.
    pub position: String,
    /// Render the numeric value next to the bar.
    pub show_value: bool,
    /// Milliseconds the OSD stays visible after the last event.
    pub timeout_ms: u32,
}

impl Default for OsdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: "bottom".to_string(),
            show_value: false,
            timeout_ms: 1500,
        }
    }
}

impl OsdConfig {
    fn validate(&self, lint: &mut Lint) {
        check_enum(lint, "osd.position", &self.position, VALID_OSD_POSITIONS);
        if self.timeout_ms == 0 {
            lint.error("osd.timeout_ms: must be greater than 0");
        }
    }
}

const AUDIO_KEYS: &[&str] = &["allow_overdrive"];

/// `[audio]`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Allow volume above 100%, capped at PulseAudio's recommended UI maximum.
    pub allow_overdrive: bool,
}

const UPDATES_KEYS: &[&str] = &["check_interval", "update_count_command", "flake"];

/// `[updates]` — the Quick Settings pending-updates card.
///
/// With no `update_count_command` the service auto-detects the distro's package
/// manager (guix, nix/NixOS, debian, arch, fedora, fedora silverblue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatesConfig {
    /// Seconds between update checks.
    pub check_interval: u64,
    /// Command printing either a count or one update per line.
    pub update_count_command: Option<String>,
    /// NixOS only: where the system flake lives, when not at `/etc/nixos`.
    ///
    /// The updates card counts pending updates there by re-locking a scratch
    /// copy of the flake; the real lock file is never written.
    pub flake: Option<String>,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            check_interval: 3600,
            update_count_command: None,
            flake: None,
        }
    }
}

impl UpdatesConfig {
    fn validate(&self, lint: &mut Lint) {
        check_interval(lint, "updates.check_interval", self.check_interval);
    }
}

const ADVANCED_KEYS: &[&str] = &["compositor", "pango_font_rendering"];

/// `[advanced]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    /// Startup-only backend: `"auto"`, `"niri"` or `"hyprland"`.
    pub compositor: String,
    /// Apply Pango font attributes directly instead of relying on GTK CSS.
    pub pango_font_rendering: bool,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            compositor: "auto".to_string(),
            pango_font_rendering: false,
        }
    }
}

impl AdvancedConfig {
    fn validate(&self, lint: &mut Lint) {
        check_enum(
            lint,
            "advanced.compositor",
            &self.compositor,
            VALID_COMPOSITORS,
        );
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn check_hex(lint: &mut Lint, key: &str, value: &str) {
    if !is_valid_hex_color(value) {
        lint.error(format!(
            "{key}: invalid value '{value}', expected a hex color like '#3584e4'"
        ));
    }
}

fn check_opacity(lint: &mut Lint, key: &str, value: f64) {
    if !(0.0..=1.0).contains(&value) {
        lint.error(format!(
            "{key}: invalid value '{value}', must be between 0.0 and 1.0"
        ));
    }
}

fn check_enum(lint: &mut Lint, key: &str, value: &str, allowed: &[&str]) {
    if !allowed.contains(&value) {
        lint.error(format!(
            "{key}: invalid value '{value}', expected one of: {}",
            allowed.join(", ")
        ));
    }
}

fn check_interval(lint: &mut Lint, key: &str, value: u64) {
    if value < MIN_NETWORK_INTERVAL_SECS {
        lint.error(format!(
            "{key}: invalid value '{value}', must be at least {MIN_NETWORK_INTERVAL_SECS} seconds"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(source: &str) -> (Config, Vec<Warning>) {
        Config::parse(source).expect("config should parse")
    }

    fn warning_keys(warnings: &[Warning]) -> Vec<&str> {
        warnings.iter().map(|w| w.key.as_str()).collect()
    }

    fn errors_of(source: &str) -> Vec<String> {
        match Config::parse(source) {
            Err(Error::Validation(errors)) => errors,
            other => panic!("expected validation errors, got {other:?}"),
        }
    }

    #[test]
    fn example_config_equals_defaults_and_warns_about_nothing() {
        let (config, warnings) = parse_ok(EXAMPLE_CONFIG_TOML);
        assert_eq!(config, Config::default());
        assert_eq!(warnings, Vec::new(), "example config must be warning-free");
    }

    #[test]
    fn defaults_validate() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn empty_config_is_the_default() {
        let (config, warnings) = parse_ok("");
        assert_eq!(config, Config::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn partial_section_keeps_sibling_defaults() {
        let (config, _) = parse_ok("[bar]\nsize = 40\n");
        assert_eq!(config.bar.size, 40);
        assert_eq!(config.bar.inset, BarConfig::default().inset);
        assert_eq!(config.widgets.left, WidgetsConfig::default().left);
    }

    #[test]
    fn bottom_bar_is_a_hard_error() {
        let errors = errors_of("[bar]\nposition = \"bottom\"\n");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("v2 supports top only"), "{errors:?}");
    }

    #[test]
    fn dropped_theme_modes_are_accepted_as_dark() {
        for mode in ["auto", "gtk"] {
            let (config, warnings) = parse_ok(&format!("[theme]\nmode = \"{mode}\"\n"));
            assert_eq!(config.theme.mode, "dark");
            assert_eq!(warning_keys(&warnings), vec!["theme.mode"]);
        }
    }

    #[test]
    fn palette_and_mode_are_validated_at_the_configuration_boundary() {
        let (config, warnings) = parse_ok(
            "[theme]\nmode = \"light\"\naccent = \"none\"\n[theme.palette]\nbackground = \"#abc\"\nsurface = \"#123456\"\nforeground = \"#000\"",
        );
        assert_eq!(config.theme.mode, "light");
        assert_eq!(config.theme.palette.background.as_deref(), Some("#abc"));
        assert!(warnings.is_empty());
        for value in ["none", "#12345678", "rgb(1,2,3)", "white"] {
            assert!(Config::parse(&format!("[theme.palette]\nforeground = {value:?}")).is_err());
        }
        assert!(Config::parse("[theme]\npalette = false").is_err());
        let (_, warnings) = parse_ok("[theme.palette]\nunknown = \"#fff\"");
        assert_eq!(warning_keys(&warnings), ["theme.palette.unknown"]);
        for backend in ["auto", "niri", "hyprland"] {
            assert!(Config::parse(&format!("[advanced]\ncompositor = {backend:?}")).is_ok());
        }
        assert!(Config::parse("[advanced]\ncompositor = \"sway\"").is_err());
    }

    #[test]
    fn unknown_theme_mode_is_an_error() {
        let errors = errors_of("[theme]\nmode = \"neon\"\n");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("theme.mode:"), "{errors:?}");
    }

    #[test]
    fn dropped_keys_get_specific_messages() {
        let source = r#"
[bar]
outputs = ["eDP-1"]
outline = true

[widgets]
outline = true

[theme]
scheme = "dark"
wallpaper = "/tmp/a.png"
popover = "light"
shadows = false
outline = true
outline_width = 2
outline_color = "accent"
outline_opacity = 0.5
"#;
        let (_, warnings) = parse_ok(source);
        assert_eq!(
            warning_keys(&warnings),
            vec![
                "bar.outline",
                "bar.outputs",
                "widgets.outline",
                "theme.outline",
                "theme.outline_color",
                "theme.outline_opacity",
                "theme.outline_width",
                "theme.popover",
                "theme.scheme",
                "theme.shadows",
                "theme.wallpaper",
            ]
        );
        assert!(
            warnings
                .iter()
                .all(|w| w.message != "unknown option, ignored")
        );
    }

    #[test]
    fn dropped_per_widget_keys_get_specific_messages() {
        let source = r##"
[widgets]
left = ["clock"]

[widgets.clock]
disabled = true
show_if = "true"
show_if_interval = 30
background_color = "#123456"
outline_color = "accent"
control_panel_weather_widget = "weather"
"##;
        let (_, warnings) = parse_ok(source);
        assert_eq!(
            warning_keys(&warnings),
            vec![
                "widgets.clock.background_color",
                "widgets.clock.control_panel_weather_widget",
                "widgets.clock.disabled",
                "widgets.clock.outline_color",
                "widgets.clock.show_if",
                "widgets.clock.show_if_interval",
            ]
        );
        assert!(
            warnings
                .iter()
                .all(|w| w.message != "unknown option, ignored")
        );
    }

    #[test]
    fn dropped_custom_widget_keys_get_specific_messages() {
        let source = r#"
[widgets]
left = ["custom-thing"]

[widgets.custom-thing]
exec = "echo hi"
image = "/tmp/a.png"
position = "right"
"#;
        let (config, warnings) = parse_ok(source);
        assert_eq!(
            warning_keys(&warnings),
            vec![
                "widgets.custom-thing.image",
                "widgets.custom-thing.position"
            ]
        );
        assert!(
            warnings
                .iter()
                .all(|w| w.message != "unknown option, ignored")
        );
        assert_eq!(
            config.widgets.custom["custom-thing"].exec.as_deref(),
            Some("echo hi")
        );
    }

    #[test]
    fn dropped_toggles_are_silent_when_they_already_match_v2() {
        // Each of these asks for exactly what v2 does, so there is nothing to
        // tell the user. The inverted value in the test above does warn.
        let source = r#"
[bar]
outputs = []
outline = false

[widgets]
outline = false
left = ["clock"]

[widgets.clock]
disabled = false

[theme]
mode = "dark"
outline = false
shadows = true
"#;
        let (_, warnings) = parse_ok(source);
        assert_eq!(warnings, Vec::new(), "{warnings:#?}");
    }

    #[test]
    fn media_section_is_dropped_with_a_specific_message() {
        let (_, warnings) = parse_ok("[widgets.media]\nart_radius = 12\nvisualizer = true\n");
        assert_eq!(warning_keys(&warnings), vec!["widgets.media"]);
        assert!(warnings[0].message.contains("control-panel media"));
    }

    #[test]
    fn unknown_keys_get_the_generic_message() {
        let source = "[bar]\nwobble = 3\n\n[nonsense]\nkey = 1\n";
        let (_, warnings) = parse_ok(source);
        assert_eq!(warning_keys(&warnings), vec!["bar.wobble", "nonsense"]);
        assert!(
            warnings
                .iter()
                .all(|w| w.message == "unknown option, ignored")
        );
    }

    #[test]
    fn widget_groups_are_flattened_with_a_warning() {
        let source = r#"
[widgets]
right = ["tray", { group = ["headset", "clock"] }]
"#;
        let (config, warnings) = parse_ok(source);
        assert_eq!(config.widgets.right, ["tray", "headset", "clock"]);
        assert_eq!(warning_keys(&warnings), vec!["widgets.right"]);
        assert!(warnings[0].message.contains("groups were dropped"));
    }

    #[test]
    fn inline_widget_arguments_are_stripped_with_a_warning() {
        let (config, warnings) = parse_ok("[widgets]\nleft = [\"clock:short\"]\n");
        assert_eq!(config.widgets.left, ["clock"]);
        assert_eq!(warning_keys(&warnings), vec!["widgets.left"]);
        assert!(warnings[0].message.contains("inline widget arguments"));
    }

    #[test]
    fn unknown_widget_names_are_skipped_with_a_warning() {
        let (config, warnings) = parse_ok("[widgets]\nleft = [\"cava\", \"clock\"]\n");
        assert_eq!(config.widgets.left, ["clock"]);
        assert_eq!(warning_keys(&warnings), vec!["widgets.left"]);
        assert!(warnings[0].message.contains("unknown widget \"cava\""));
    }

    #[test]
    fn unplaced_widget_section_warns() {
        let source = "[widgets]\nleft = [\"clock\"]\n\n[widgets.weather]\nunit = \"celsius\"\n";
        let (_, warnings) = parse_ok(source);
        assert_eq!(warning_keys(&warnings), vec!["widgets.weather"]);
        assert!(warnings[0].message.contains("not placed"));
    }

    #[test]
    fn per_widget_click_commands_parse() {
        let source = r#"
[widgets]
right = ["quick_settings"]

[widgets.quick_settings]
on_click_right = "loginctl lock-session"
"#;
        let (config, warnings) = parse_ok(source);
        assert!(warnings.is_empty());
        assert_eq!(
            config.widgets.quick_settings.on_click_right.as_deref(),
            Some("loginctl lock-session")
        );
    }

    #[test]
    fn invalid_hex_colors_are_errors() {
        let source = r##"
[bar]
background_color = "black"

[theme]
accent = "blurple"

[theme.states]
urgent = "#ff00"
"##;
        let errors = errors_of(source);
        assert_eq!(errors.len(), 3);
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("bar.background_color:"))
        );
        assert!(errors.iter().any(|e| e.starts_with("theme.accent:")));
        assert!(errors.iter().any(|e| e.starts_with("theme.states.urgent:")));
    }

    #[test]
    fn accent_none_is_allowed() {
        let (config, warnings) = parse_ok("[theme]\naccent = \"none\"\n");
        assert_eq!(config.theme.accent, "none");
        assert!(warnings.is_empty());
    }

    #[test]
    fn accent_gtk_is_dropped_with_a_warning() {
        let (config, warnings) = parse_ok("[theme]\naccent = \"gtk\"\n");
        assert_eq!(config.theme.accent, ThemeConfig::default().accent);
        assert_eq!(warning_keys(&warnings), vec!["theme.accent"]);
    }

    #[test]
    fn opacity_must_be_within_zero_and_one() {
        let source = r#"
[bar]
background_opacity = 1.5

[widgets]
background_opacity = -0.2
popover_background_opacity = 2.0
"#;
        let errors = errors_of(source);
        assert_eq!(errors.len(), 3);
        assert!(errors.iter().all(|e| e.contains("between 0.0 and 1.0")));
    }

    #[test]
    fn zero_sizes_and_timeouts_are_errors() {
        let errors = errors_of("[bar]\nsize = 0\n\n[osd]\ntimeout_ms = 0\n");
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|e| e.starts_with("bar.size:")));
        assert!(errors.iter().any(|e| e.starts_with("osd.timeout_ms:")));
    }

    #[test]
    fn osd_position_is_an_enum() {
        let errors = errors_of("[osd]\nposition = \"middle\"\n");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("bottom, left, right, top"));
    }

    #[test]
    fn the_notmuch_query_defaults_to_the_unread_inbox() {
        let (config, _) = parse_ok("[widgets]\nright = [\"clock\"]\n");
        assert_eq!(config.widgets.notmuch.query, "tag:unread and tag:inbox");
        assert_eq!(config.widgets.notmuch.interval, 300);
        assert_eq!(config.widgets.notmuch.max_items, 10);
    }

    #[test]
    fn a_blank_notmuch_query_is_refused() {
        // Notmuch answers an empty query with every message in the database,
        // which is the one wrong number it is easiest to ship by accident.
        let errors = errors_of("[widgets.notmuch]\nquery = \"   \"\n");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].starts_with("widgets.notmuch.query:"),
            "{errors:?}"
        );
    }

    #[test]
    fn a_notmuch_list_of_nothing_is_refused() {
        let errors = errors_of("[widgets.notmuch]\nmax_items = 0\n");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("max_items"), "{errors:?}");
    }

    #[test]
    fn the_notmuch_poll_never_runs_more_often_than_once_a_minute() {
        let errors = errors_of("[widgets.notmuch]\ninterval = 5\n");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("at least 60 seconds"), "{errors:?}");
    }

    #[test]
    fn an_unknown_notmuch_key_warns_rather_than_being_ignored() {
        let (_, warnings) = parse_ok("[widgets.notmuch]\nmaildir = \"~/Mail\"\n");
        assert!(
            warning_keys(&warnings)
                .iter()
                .any(|key| key.contains("notmuch")),
            "{warnings:?}"
        );
    }

    #[test]
    fn crypto_entries_must_name_supported_assets() {
        let source = r#"
[widgets]
left = ["crypto"]

[widgets.crypto]
entries = ["btc", "eth/btc", "doge", "btc/btc", "sol/eth"]
"#;
        let errors = errors_of(source);
        assert_eq!(errors.len(), 3);
        assert!(errors.iter().any(|e| e.contains("'doge'")));
        assert!(errors.iter().any(|e| e.contains("'btc/btc'")));
        assert!(errors.iter().any(|e| e.contains("'sol/eth'")));
    }

    #[test]
    fn valid_crypto_entries_pass() {
        for entry in ["btc", "eth", "xmr", "eth/btc", "xmr/eth"] {
            assert!(is_valid_crypto_entry(entry), "{entry} should be valid");
        }
        for entry in ["", "sol", "btc/", "/btc", "btc/doge"] {
            assert!(!is_valid_crypto_entry(entry), "{entry} should be invalid");
        }
    }

    #[test]
    fn thresholds_must_be_percentages() {
        let source = r#"
[widgets]
right = ["system_monitor"]

[widgets.system_monitor]
cpu_threshold = 0
memory_threshold = 101
disk_threshold = 100
"#;
        let errors = errors_of(source);
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|e| e.contains("cpu_threshold")));
        assert!(errors.iter().any(|e| e.contains("memory_threshold")));
    }

    #[test]
    fn the_weather_description_can_be_turned_off() {
        let source =
            "[widgets]\ncenter = [\"weather\"]\n\n[widgets.weather]\nshow_description = false\n";
        let (config, warnings) = parse_ok(source);
        assert!(warnings.is_empty(), "{warnings:#?}");
        assert!(!config.widgets.weather.show_description);
        // A file that says nothing keeps the condition, which is what v1 drew.
        assert!(WeatherConfig::default().show_description);
    }

    #[test]
    fn forecast_days_are_bounded() {
        let source = "[widgets]\ncenter = [\"weather\"]\n\n[widgets.weather]\nforecast_days = 7\n";
        let errors = errors_of(source);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("between 3 and 5"));
    }

    #[test]
    fn network_intervals_have_a_floor() {
        let source = r#"
[updates]
check_interval = 30
"#;
        let errors = errors_of(source);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("at least 60 seconds"));
    }

    #[test]
    fn wrong_types_are_reported_per_section() {
        let errors = errors_of("[bar]\nsize = \"tall\"\n");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("bar: "), "{errors:?}");
    }

    #[test]
    fn all_errors_are_reported_at_once() {
        let errors = errors_of("[bar]\nsize = 0\nbackground_color = \"nope\"\n");
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn missing_explicit_config_is_not_found() {
        let err = Config::load_file(Path::new("/nonexistent/topbar.toml")).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn dropped_tables_have_no_duplicate_keys() {
        for table in [DROPPED_KEYS, DROPPED_WIDGET_KEYS, DROPPED_CUSTOM_KEYS] {
            let mut keys: Vec<&str> = table.iter().map(|(key, _)| *key).collect();
            keys.sort_unstable();
            let count = keys.len();
            keys.dedup();
            assert_eq!(keys.len(), count, "duplicate dropped key");
        }
    }

    #[test]
    fn warning_display_is_key_then_message() {
        let warning = Warning {
            key: "theme.scheme".to_string(),
            message: "gone".to_string(),
        };
        assert_eq!(warning.to_string(), "theme.scheme: gone");
    }

    /// The search chain as strings, for readable assertions.
    fn chain(xdg: Option<&str>, home: Option<&str>) -> Vec<String> {
        chain_from(xdg, home)
            .into_iter()
            .map(|candidate| candidate.path.display().to_string())
            .collect()
    }

    #[test]
    fn the_current_name_is_searched_before_the_old_one() {
        assert_eq!(
            chain(None, Some("/home/u")),
            [
                "/home/u/.config/topbar/config.toml",
                "/home/u/.config/gnome-topbar/config.toml",
                "config.toml",
            ]
        );
    }

    #[test]
    fn xdg_config_home_wins_over_home_at_every_name() {
        assert_eq!(
            chain(Some("/xdg"), Some("/home/u")),
            [
                "/xdg/topbar/config.toml",
                "/home/u/.config/topbar/config.toml",
                "/xdg/gnome-topbar/config.toml",
                "/home/u/.config/gnome-topbar/config.toml",
                "config.toml",
            ]
        );
    }

    #[test]
    fn an_empty_environment_leaves_only_the_working_directory() {
        assert_eq!(chain(Some(""), Some("")), ["config.toml"]);
    }

    #[test]
    fn only_the_old_directories_carry_a_deprecation() {
        for candidate in chain_from(Some("/xdg"), Some("/home/u")) {
            let is_legacy = candidate.path.to_string_lossy().contains("gnome-topbar");
            assert_eq!(
                candidate.legacy_location().is_some(),
                is_legacy,
                "{}",
                candidate.path.display()
            );
        }
    }

    #[test]
    fn the_deprecation_names_both_paths_with_home_abbreviated() {
        let home = env::var("HOME").expect("tests run with HOME set");
        let location = LegacyLocation {
            found: PathBuf::from(&home).join(".config/gnome-topbar/config.toml"),
            expected: PathBuf::from(&home).join(".config/topbar/config.toml"),
        };
        assert_eq!(
            location.to_string(),
            "config found at legacy path ~/.config/gnome-topbar/config.toml; \
             move it to ~/.config/topbar/config.toml"
        );
    }

    #[test]
    fn the_dumped_config_parses_back_to_the_same_config() {
        // `topbar dump config` is only useful if what it prints is a config
        // file, so the round trip is the contract rather than the format.
        let (config, _) = Config::parse(EXAMPLE_CONFIG_TOML).expect("the example parses");
        let dumped = config.to_toml().expect("a config renders");
        let (again, warnings) = Config::parse(&dumped).expect("the dump parses");
        assert_eq!(again, config);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn the_dump_states_the_defaults_the_file_left_out() {
        let (config, _) = Config::parse("[bar]\nsize = 40\n").expect("a minimal config");
        let dumped = config.to_toml().expect("a config renders");
        assert!(dumped.contains("size = 40"));
        // Nothing in that file mentioned the OSD, and the dump has to.
        assert!(dumped.contains("timeout_ms = 1500"), "{dumped}");
    }

    #[test]
    fn the_json_dump_carries_the_same_values() {
        let config = Config::default();
        let json = config.to_json().expect("a config renders as JSON");
        assert_eq!(json["bar"]["size"], serde_json::json!(36));
        assert_eq!(json["osd"]["position"], serde_json::json!("bottom"));
    }

    #[test]
    fn the_mic_slider_takes_three_words_and_two_leftovers() {
        // The key was a boolean before it grew "always", so both spellings of
        // the old file still mean what they meant.
        for (written, wanted) in [
            ("\"auto\"", MicSlider::Auto),
            ("\"always\"", MicSlider::Always),
            ("\"never\"", MicSlider::Never),
            ("true", MicSlider::Auto),
            ("false", MicSlider::Never),
        ] {
            let file = format!("[widgets.quick_settings]\nmic = {written}\n");
            let (config, warnings) = Config::parse(&file).expect(written);
            assert_eq!(config.widgets.quick_settings.mic, wanted, "{written}");
            assert!(warnings.is_empty(), "{written} must not warn");
        }
    }

    #[test]
    fn a_misspelt_mic_value_names_the_accepted_ones() {
        let error = Config::parse("[widgets.quick_settings]\nmic = \"on\"\n")
            .expect_err("not a value")
            .to_string();
        assert!(error.contains("always"), "{error}");
    }

    #[test]
    fn the_mic_slider_serialises_as_a_word_that_parses_back() {
        // Per-widget sections are hand-parsed and skipped by the config
        // serialiser, so the round trip that matters is the enum's own: what
        // it writes must be a value the file syntax accepts.
        for mode in [MicSlider::Auto, MicSlider::Always, MicSlider::Never] {
            let word = toml::Value::try_from(mode).expect("a mode serialises");
            let file = format!("[widgets.quick_settings]\nmic = {word}\n");
            let (config, _) = Config::parse(&file).expect("and parses back");
            assert_eq!(config.widgets.quick_settings.mic, mode);
        }
    }
}
