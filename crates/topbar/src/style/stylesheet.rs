//! The panel's one and only stylesheet.
//!
//! Everything the panel paints is styled by a single CSS string generated from
//! the configuration and installed as a single [`gtk4::CssProvider`] at
//! `APPLICATION` priority. There are no per-surface providers, no theme
//! variants, and no CSS `transition` rules: motion is Rust-driven (see
//! [`crate::anim`]) so it can be switched off wholesale and cannot leak.
//!
//! The generated sheet has two halves:
//!
//! - [`root_block`] — every value the configuration influences, emitted as CSS
//!   custom properties on `:root`, resolved from the shared palette.
//! - [`RULES`] — the selectors, which only ever reference those properties.

use std::cell::RefCell;

use gtk4::gdk;
use topbar_core::Config;
use topbar_core::theme::{Palette, Rgb};

/// Tooltips stay near-opaque: they must be readable before blur exists.
const SURFACE_OPACITY: f64 = 0.92;
/// Alpha of the hairline border every elevated surface carries.
const SURFACE_BORDER_ALPHA: f64 = 0.08;

/// Corner radius of a popover surface, in pixels.
///
/// The popover's grow-in clips to a rounded rectangle drawn from Rust (see
/// [`crate::anim::ScaleBox`]), so this value has to exist outside the
/// stylesheet as well — the two would drift apart as separate constants.
pub const POPOVER_RADIUS: u32 = 16;

/// Horizontal padding inside a widget's content box, in pixels.
const WIDGET_PADDING_X: u32 = 10;

/// Horizontal padding inside the OSD capsule, in pixels.
///
/// Stated in Rust as well as in the sheet because the capsule's overall width
/// is a design constraint — GNOME 42's is about 220px — and the test that
/// checks it has to be able to add the parts up.
pub(crate) const OSD_PADDING: u32 = 20;

/// Alpha of a state colour used as a fill rather than as text.
///
/// Enough to read as tinted against a dark surface, far short of enough to
/// compete with the number it sits beside.
const STATE_FILL_ALPHA: f64 = 0.16;

/// Fraction of the bar height taken by the gap above and below a widget.
const WIDGET_INSET_SCALE: f64 = 0.14;
/// Font size as a fraction of the widget height.
const FONT_SCALE: f64 = 0.6;
/// Symbolic icon size as a fraction of the bar height.
const ICON_SCALE: f64 = 0.5;
/// Spacing between a widget's own children as a fraction of the bar height.
const CONTENT_GAP_SCALE: f64 = 0.25;

/// Round up to the nearest even number so heights center on whole pixels.
fn round_to_even(value: u32) -> u32 {
    if value.is_multiple_of(2) {
        value
    } else {
        value + 1
    }
}

/// Height of a panel button: the bar height minus a symmetric inset.
pub fn widget_height(bar_size: u32) -> u32 {
    let inset = round_to_even((f64::from(bar_size) * WIDGET_INSET_SCALE) as u32);
    round_to_even(bar_size.saturating_sub(2 * inset))
}

/// Body font size for the bar, derived from the widget height.
pub fn font_size(bar_size: u32) -> u32 {
    round_to_even((f64::from(widget_height(bar_size)) * FONT_SCALE) as u32)
}

/// Launcher text stays readable when the panel itself uses a compact height.
pub fn launcher_font_size(bar_size: u32) -> u32 {
    font_size(bar_size).max(20)
}

/// Standalone dialogs need a readable body size even with a compact panel.
pub fn dialog_font_size(bar_size: u32) -> u32 {
    font_size(bar_size).max(16)
}

/// Total height the bar window occupies, including `bar.padding`.
///
/// An opaque bar pads both sides of its content; a transparent one only pads
/// the screen-edge side, keeping the exclusive zone tight.
pub fn window_height(config: &Config) -> i32 {
    let padding = config.bar.padding;
    let padding_total = if config.bar.background_opacity > 0.0 {
        2 * padding
    } else {
        padding
    };
    (config.bar.size + padding_total) as i32
}

/// Widget corner radius as a CSS length.
///
/// `widgets.border_radius` is a percentage of the bar height; at 50% or more a
/// panel button is a full pill.
fn widget_radius(config: &Config) -> String {
    let percent = config.widgets.border_radius;
    if percent >= 50 {
        return "9999px".to_string();
    }
    let radius = (config.bar.size * percent / 100).min(config.bar.size / 2);
    format!("{radius}px")
}

/// Render a color plus opacity as a CSS color value.
fn tinted(color: Rgb, opacity: f64) -> String {
    if opacity <= 0.0 {
        "transparent".to_string()
    } else if opacity >= 1.0 {
        color.to_hex()
    } else {
        color.to_rgba(opacity)
    }
}

/// Generate the complete stylesheet for a configuration.
pub fn generate(config: &Config) -> String {
    format!("{}{RULES}", root_block(config))
}

/// The configuration-derived half of the stylesheet.
fn root_block(config: &Config) -> String {
    let bar = &config.bar;
    let widgets = &config.widgets;
    let theme = &config.theme;

    let bar_padding_center = if bar.background_opacity > 0.0 {
        bar.padding
    } else {
        0
    };
    let palette = Palette::from_config(config);

    format!(
        r#"/* Generated by topbar. Do not edit: regenerated on every start. */
:root {{
    /* Geometry */
    --bar-height: {bar_height}px;
    --bar-padding-top: {bar_padding_top}px;
    --bar-padding-bottom: {bar_padding_bottom}px;
    --widget-height: {widget_height}px;
    --widget-padding-x: {widget_padding_x}px;
    --widget-gap: {widget_gap}px;
    --spacing-widget: {spacing}px;
    --radius-bar: {radius_bar}px;
    --radius-widget: {radius_widget};
    --radius-surface: {radius_surface}px;
    --radius-popover: {radius_popover}px;
    --radius-card: 12px;
    --osd-padding: {osd_padding}px;

    /* Typography */
    --font-family: {font_family};
    --font-size: {font_size}px;
    --dialog-font-size: {dialog_font_size}px;
    --launcher-font-size: {launcher_font_size}px;
    --icon-size: {icon_size}px;

    /* Surfaces */
    --color-bar-background: {bar_background};
    --color-widget-background: {widget_background};
    --color-surface: {surface};
    --color-dialog-surface: {dialog_surface};
    --color-surface-border: {surface_border};
    --color-popover: {popover};
    --color-popover-shadow: {popover_shadow};
    --shadow-popover: 0 1px 4px var(--color-popover-shadow);
    --shadow-modal: 0 4px 14px var(--color-popover-shadow);
    --color-card: {card};
    --color-launcher-backdrop: {launcher_backdrop};

    /* Panel-button states */
    --color-widget-hover: {hover};
    --color-widget-pressed: {pressed};
    --color-widget-checked: {checked};

    /* Foreground */
    --color-foreground: {foreground};
    --color-foreground-muted: {foreground_muted};
    --color-foreground-disabled: {foreground_disabled};
    --color-ripple: {ripple};
    --color-accent: {accent};
    --color-on-accent: {on_accent};
    --color-state-success: {success};
    --color-state-warning: {warning};
    --color-state-urgent: {urgent};
    --color-state-success-fill: {success_fill};
    --color-state-warning-fill: {warning_fill};
    --color-state-urgent-fill: {urgent_fill};
}}
"#,
        bar_height = bar.size,
        bar_padding_top = bar.padding,
        bar_padding_bottom = bar_padding_center,
        widget_height = widget_height(bar.size),
        widget_padding_x = WIDGET_PADDING_X,
        widget_gap = ((f64::from(bar.size) * CONTENT_GAP_SCALE) as u32 / 2).max(4) + 5,
        spacing = bar.spacing,
        radius_bar = bar.border_radius,
        radius_widget = widget_radius(config),
        radius_surface = (bar.size / 3).max(6),
        radius_popover = POPOVER_RADIUS,
        osd_padding = OSD_PADDING,
        font_family = theme.typography.font_family,
        font_size = font_size(bar.size),
        dialog_font_size = dialog_font_size(bar.size),
        launcher_font_size = launcher_font_size(bar.size),
        icon_size = round_to_even((f64::from(bar.size) * ICON_SCALE) as u32),
        bar_background = tinted(palette.bar, bar.background_opacity),
        widget_background = tinted(palette.widget, widgets.background_opacity),
        surface = palette.surface.to_rgba(SURFACE_OPACITY),
        dialog_surface = palette.surface.to_hex(),
        surface_border = palette.foreground.to_rgba(SURFACE_BORDER_ALPHA),
        // A popover with no opacity of its own follows the bar: an opaque
        // panel means opaque menus, which is what a user who turned
        // translucency off is asking for.
        popover = tinted(
            palette.background,
            widgets
                .popover_background_opacity
                .unwrap_or(bar.background_opacity)
        ),
        popover_shadow = Rgb::new(0, 0, 0).to_rgba(0.28),
        card = palette.foreground.to_rgba(0.06),
        launcher_backdrop = palette.background.to_hex(),
        hover = palette.foreground.to_rgba(0.1),
        pressed = palette.foreground.to_rgba(0.15),
        checked = palette.foreground.to_rgba(0.18),
        foreground = palette.foreground.to_hex(),
        foreground_muted = palette.foreground.to_rgba(0.6),
        foreground_disabled = palette.foreground.to_rgba(0.4),
        ripple = palette.foreground.to_rgba(0.12),
        accent = palette.accent.to_hex(),
        on_accent = palette.accent.contrasting_foreground().to_hex(),
        success = palette.success.to_hex(),
        warning = palette.warning.to_hex(),
        urgent = palette.urgent.to_hex(),
        success_fill = palette.success.to_rgba(STATE_FILL_ALPHA),
        warning_fill = palette.warning.to_rgba(STATE_FILL_ALPHA),
        urgent_fill = palette.urgent.to_rgba(STATE_FILL_ALPHA),
    )
}

/// The selectors. Every value here comes from a `:root` custom property.
const RULES: &str = r#"
/* ===== Bar ===== */

/* The layer-shell window and its shell box are invisible; only the bar paints. */
window.bar-window,
.bar-shell {
    background: transparent;
}

sectioned-bar.bar {
    min-height: var(--bar-height);
    padding-top: var(--bar-padding-top);
    padding-bottom: var(--bar-padding-bottom);
    background-color: var(--color-bar-background);
    border-radius: var(--radius-bar);
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--font-size);
    font-weight: 700;
}

.bar-section--left > *:not(:last-child),
.bar-section--center > *:not(:last-child),
.bar-section--right > *:not(:last-child) {
    margin-right: var(--spacing-widget);
}

/* ===== Panel buttons ===== */

/* The wrapper reserves height; the surface underneath does the painting. */
.widget-wrapper {
    min-height: var(--widget-height);
    background: transparent;
}

.widget {
    background-color: var(--color-widget-background);
    border-radius: var(--radius-widget);
}

/* Hover and press fill. It sits behind the content and its opacity is
   animated from Rust (see anim::Animation) — deliberately no CSS transition. */
.widget-fill {
    background-color: var(--color-widget-hover);
    border-radius: var(--radius-widget);
}

.widget-fill.pressed {
    background-color: var(--color-widget-pressed);
}

/* Ripples retain the palette foreground rather than inheriting semantic tints. */
.ripple {
    background: transparent;
    color: var(--color-ripple);
}

/* Wrapping a button's child to hold its ripple must change nothing about the
   button, so the wrapper takes the shape of whatever it was put inside and the
   circle is clipped to that instead of to a rectangle. */
.ripple-clip {
    border-radius: inherit;
}

/* Exactly one widget is checked at a time: the one whose popover is open.
   It paints on the surface rather than on the fill, because the fill's opacity
   belongs to the hover animation and sits at zero while the pointer is away —
   a checked button has to stay lit whether or not it is hovered. Hover then
   layers on top, which is what GNOME does with an open menu under the mouse. */
.widget-wrapper.checked .widget {
    background-color: var(--color-widget-checked);
}

/* Passive widgets never light up, whatever the pointer does. */
.widget-wrapper:not(.clickable) .widget-fill {
    background-color: transparent;
}

.content {
    padding: 0 var(--widget-padding-x);
}

.content > *:not(:last-child) {
    margin-right: var(--widget-gap);
}

/* A widget whose service has dropped keeps its last state on screen, dimmed:
   an empty panel would read as "no workspaces", which is a different claim. */
.widget-wrapper.disconnected {
    opacity: 0.5;
}

/* ===== Clock ===== */

/* Tabular figures keep the width steady as the digits change. */
.clock .content {
    font-feature-settings: "tnum";
}

/* The Do Not Disturb indicator: small, dimmed, and to the right of the time —
   present enough to explain a silent desktop, quiet enough to ignore. */
.clock-dnd {
    -gtk-icon-size: 12px;
    color: var(--color-foreground-muted);
}

/* The unread dot: the one thing on the bar drawn in the accent colour on its
   own account, because it is the one thing on the bar asking to be clicked.
   Smaller than the Do Not Disturb bell beside it — a mark, not an icon — and
   it goes the moment the panel is opened. */
.clock-unseen {
    -gtk-icon-size: 10px;
    color: var(--color-accent);
}

/* ===== Workspaces ===== */

/* The strip paints its own indicators in snapshot(); the only thing it takes
   from CSS is the foreground color they are derived from. Their sizes are
   Rust constants (see widgets/workspaces/model.rs) so the drawn geometry and
   the measured width can never disagree. */
workspace-strip {
    color: var(--color-foreground);
}

/* Dots are their own visual rhythm; the widget's own padding is enough. */
.workspaces .content > * {
    margin: 0;
}

/* ===== Keyboard layout ===== */

/* A two-letter code has to read as a unit rather than as running text. */
.keyboard-layout label {
    font-weight: 700;
}

.keyboard-layout-icon {
    -gtk-icon-size: var(--icon-size);
}

/* ===== Semantic states ===== */

/* One class per state, put on whatever should wear it: a custom-* widget whose
   script asked for `class: "warning"`, a headset down to its last tenth, a
   metric over its threshold. `color` inherits, so a wrapper carrying one of
   these tints every icon and label inside it. */
.state-success {
    color: var(--color-state-success);
}

.state-warning {
    color: var(--color-state-warning);
}

.state-urgent {
    color: var(--color-state-urgent);
}

/* ===== Custom widgets ===== */

/* A script's own glyphs are usually the point, so the label is left alone
   beyond the tabular figures every changing number on the bar gets. */
.custom-label {
    font-feature-settings: "tnum";
}

.custom-icon {
    -gtk-icon-size: var(--icon-size);
}

/* ===== Headset ===== */

.headset-icon {
    -gtk-icon-size: var(--icon-size);
}

/* ===== System monitor ===== */

/* Alert-only: everything here is drawn on a widget that is invisible until a
   threshold is crossed. The reading beside each icon is small — the icon is
   what catches the eye, the number is for whoever looks twice. */
.system-monitor-icon {
    -gtk-icon-size: var(--icon-size);
}

.system-monitor-value {
    font-feature-settings: "tnum";
    font-size: 0.85em;
}

/* The popover is the Quick Settings resource card with no panel around it, so
   it brings its own padding rather than inheriting one. */
.system-monitor-popover {
    min-width: 300px;
    padding: 12px;
}

/* ===== os-release logo ===== */

/* The one glyph in the panel. It is bigger than the body text because a Nerd
   Font logo is drawn to sit inside a monospace cell and reads small next to
   symbolic icons at the same nominal size. */
.os-logo-glyph {
    font-size: 1.15em;
}

.os-logo-icon {
    -gtk-icon-size: var(--icon-size);
}

/* ===== Popovers ===== */

/* Both layer surfaces look invisible, but only the window frames are actually
   transparent. */
window.popover-window,
window.click-catcher-window,
.popover-wrapper {
    background: transparent;
}

/* GTK's window shadow follows the rectangular layer surface, not the
   rounded popup inside it. Leave only each popup's own CSS shadow. */
window.popover-window,
window.location-window,
window.launcher-window,
window.chooser-window,
window.pinentry-window,
window.toast-window,
window.osd-window {
    box-shadow: none;
}

/* The catcher has to PAINT, and this is the whole reason it works.

   It draws nothing anyone can see, but a widget that draws literally nothing
   gives GTK an empty scene, and GDK then commits the layer surface with no
   buffer attached to it. A wl_surface with no buffer is an unmapped surface,
   and an unmapped surface is not in the compositor's input routing at all — so
   every click went straight through the catcher to the window underneath while
   `niri msg layers` still listed the surface, because a layer surface is
   listed once it is configured, buffer or no buffer. The dismissal gesture was
   correct the whole time and simply never received an event.

   One 255th of an alpha is the smallest fill that survives to a buffer, and it
   is imperceptible over any background. v1 landed on the same number for the
   same reason ("nearly invisible but clickable"); v2 dropped it to
   `transparent` and lost click-away dismissal with it. */
.click-catcher {
    background-color: rgba(128, 128, 128, 0.004);
}

.popover-surface {
    background-color: var(--color-popover);
    border: 1px solid var(--color-surface-border);
    border-radius: var(--radius-popover);
    box-shadow: var(--shadow-popover);
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--font-size);
    font-weight: 400;
    padding: 16px;
}

/* While a popover grows in, its outline is drawn on the animating clip (see
   anim::ScaleBox) because this border sits outside it until the last frame. */
.popover-surface.borderless {
    border-color: transparent;
}

/* An expandable section's slot, wherever one is opened: Quick Settings' cards
   and the notification groups both use it. The clip is what lets its content be
   drawn part-way out of the slot while the reveal runs — see
   anim/slide_box.rs. */
.expander-section {
    background: transparent;
}

/* ===== Control panel ===== */

/* The columns are fixed width so nothing reflows as content arrives; the
   widths live in Rust beside the layout that depends on them. */
.control-panel-column {
    background: transparent;
}

.control-panel-divider {
    background-color: var(--color-surface-border);
}

.control-panel-card {
    background-color: var(--color-card);
    border-radius: var(--radius-card);
    padding: 12px 14px;
}

.control-panel-title {
    font-size: 1.05em;
    font-weight: 700;
}

.control-panel-time {
    font-size: 2.4em;
    font-weight: 700;
    font-feature-settings: "tnum";
}

.control-panel-date {
    font-size: 1.05em;
    font-weight: 700;
    color: var(--color-foreground-muted);
}

.world-clock-row {
    min-height: 22px;
}

.world-clock-name {
    font-weight: 500;
}

.world-clock-zone {
    color: var(--color-foreground-muted);
}

.world-clock-time {
    font-weight: 600;
    font-feature-settings: "tnum";
}

/* An empty column is a designed state, not a hole: a large dimmed icon and a
   line of text, centred, exactly as GNOME's own notification list does it. */
.empty-state-icon {
    -gtk-icon-size: 48px;
    opacity: 0.4;
}

.empty-state-label {
    font-size: 1.05em;
    font-weight: 500;
    color: var(--color-foreground-muted);
}

.dnd-row {
    min-height: 32px;
    margin-top: 6px;
}

.dnd-label {
    font-weight: 500;
}

/* ===== Calendar ===== */

/* The chevrons sit tight against the popover's padding, GNOME style. */
.calendar-header {
    margin: -4px -6px 0 -6px;
}

.calendar-grid {
    margin-top: 2px;
}

button.calendar-month,
button.calendar-nav {
    min-width: 28px;
    min-height: 28px;
    padding: 0 6px;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-card);
    color: var(--color-foreground);
}

button.calendar-month:hover,
button.calendar-nav:hover {
    background-color: var(--color-widget-hover);
}

.calendar-title {
    font-size: 1.05em;
    font-weight: 700;
}

.calendar-weekday {
    font-size: 0.85em;
    font-weight: 600;
    color: var(--color-foreground-muted);
}

.calendar-week {
    font-size: 0.8em;
    color: var(--color-foreground-disabled);
    margin-right: 6px;
}

/* Every cell carries the selection ring's border, transparent until it is the
   selected one, so picking a day cannot change the size of the grid. */
button.calendar-day {
    min-width: 30px;
    min-height: 30px;
    padding: 0;
    background: none;
    box-shadow: none;
    border: 1px solid transparent;
    border-radius: 9999px;
    color: var(--color-foreground);
    font-weight: 400;
    font-feature-settings: "tnum";
}

button.calendar-day:hover {
    background-color: var(--color-widget-hover);
}

/* Days from the months either side stay clickable — clicking one navigates
   there — but they are clearly not part of the month on screen. */
button.calendar-day.calendar-outside {
    opacity: 0.35;
}

button.calendar-day.calendar-selected {
    border-color: var(--color-accent);
}

button.calendar-day.calendar-today {
    background-color: var(--color-accent);
    color: var(--color-on-accent);
    font-weight: 700;
}

/* ===== Weather ===== */

/* Tabular figures so the label does not shuffle sideways every time the
   temperature crosses ten degrees. */
.weather label {
    font-feature-settings: "tnum";
}

.weather-icon {
    -gtk-icon-size: var(--icon-size);
}

/* ===== Forecast ===== */

/* The same component is the control panel's last card and the whole of the
   weather widget's popover, so it carries no padding of its own: the card
   class supplies it in one mount and the popover surface in the other. */
.forecast {
    min-width: 300px;
}

/* The gear sits tight against the card's padding, as the calendar's chevrons
   do, so the title line is not pushed in by a button's own box. */
.forecast-header {
    margin-right: -6px;
}

.forecast-days {
    margin-top: 2px;
}

.forecast-location {
    color: var(--color-foreground-muted);
}

button.forecast-configure {
    min-width: 28px;
    min-height: 28px;
    padding: 0;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: 9999px;
    color: var(--color-foreground-muted);
    -gtk-icon-size: 16px;
}

button.forecast-configure:hover {
    background-color: var(--color-widget-hover);
    color: var(--color-foreground);
}

button.forecast-configure:active {
    background-color: var(--color-widget-pressed);
}

.forecast-current {
    margin: 4px 0 2px 0;
}

.forecast-current-icon {
    -gtk-icon-size: 32px;
}

.forecast-current-temp {
    font-size: 1.8em;
    font-weight: 700;
    font-feature-settings: "tnum";
}

.forecast-current-condition {
    color: var(--color-foreground-muted);
}

.forecast-row {
    min-height: 22px;
}

.forecast-day {
    font-weight: 600;
}

.forecast-icon {
    -gtk-icon-size: 16px;
    color: var(--color-foreground-muted);
}

.forecast-condition {
    color: var(--color-foreground-muted);
}

.forecast-temps {
    font-feature-settings: "tnum";
}

/* Dimmer than the temperatures: a chance of rain is a footnote to the row,
   and the droplet in front of it already carries the meaning. */
.forecast-precipitation {
    color: var(--color-foreground-disabled);
    font-size: 0.9em;
    -gtk-icon-size: 12px;
}

.forecast-stale {
    margin-top: 4px;
    color: var(--color-foreground-disabled);
    font-size: 0.9em;
}

button.forecast-retry {
    min-height: 20px;
    padding: 0 8px;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: 9999px;
    color: var(--color-accent);
    font-size: 0.9em;
}

button.forecast-retry:hover {
    background-color: var(--color-widget-hover);
}

/* ===== Crypto ===== */

/* Tabular figures, so a price crossing a digit boundary does not shuffle
   everything to the right of it sideways. */
.crypto label {
    font-feature-settings: "tnum";
}

/* Each entry is one unit — logo then number — so the gap inside it is tighter
   than the gap between entries, which is what stops three prices reading as
   six things. */
.crypto-entry > *:not(:last-child) {
    margin-right: 4px;
}

/* The number is what the eye goes to and the logo beside it is the label, so
   the number keeps the full foreground whatever the logo is doing. */
.crypto-value {
    color: var(--color-foreground);
}

/* The logos are textures and take no colour from CSS. This rule is for the
   symbolic glyph that stands in when one fails to decode: it has to read as a
   placeholder rather than as a fourth asset. Their pixel sizes come from Rust,
   which is where the size a logo is drawn at is decided. */
.crypto-icon {
    color: var(--color-foreground-muted);
}

/* A pair's two coins overlap on a diagonal, numerator in front. The ring
   behind the front coin is what keeps its edge legible over the coin below;
   it is the panel's own background, which is also near enough the popover's
   for the same ring to work on both. */
.crypto-badge {
    padding: 1px;
    border-radius: 9999px;
    background-color: var(--color-bar-background);
}

.crypto-popover {
    min-width: 260px;
}

/* The gear sits tight against the surface's padding, as the forecast's does,
   so the title line is not pushed in by a button's own box. */
.crypto-header {
    margin-right: -6px;
}

button.crypto-configure,
button.crypto-back {
    min-width: 28px;
    min-height: 28px;
    padding: 0;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: 9999px;
    color: var(--color-foreground-muted);
    -gtk-icon-size: 16px;
}

button.crypto-configure:hover,
button.crypto-back:hover {
    background-color: var(--color-widget-hover);
    color: var(--color-foreground);
}

button.crypto-configure:active,
button.crypto-back:active {
    background-color: var(--color-widget-pressed);
}

/* The back button leads the settings header, so its negative margin is on the
   other side from the gear's. */
button.crypto-back {
    margin-left: -6px;
}

.crypto-list {
    margin-top: 2px;
}

.crypto-row {
    min-height: 28px;
}

/* The name is the row's label and the price is its answer, so the name is the
   quieter of the two. */
.crypto-name {
    color: var(--color-foreground-muted);
}

.crypto-row-value {
    font-feature-settings: "tnum";
    font-weight: 700;
}

/* The change is a chip rather than coloured text: a tinted pill reads as a
   badge attached to the price, where coloured text would read as a second,
   competing number. */
.crypto-change {
    min-width: 54px;
    padding: 1px 8px;
    border-radius: 9999px;
    color: var(--color-foreground-disabled);
    font-size: 0.85em;
    font-feature-settings: "tnum";
}

.crypto-change.crypto-change-up {
    background-color: var(--color-state-success-fill);
    color: var(--color-state-success);
}

.crypto-change.crypto-change-down {
    background-color: var(--color-state-urgent-fill);
    color: var(--color-state-urgent);
}

.crypto-updated {
    margin-top: 4px;
    color: var(--color-foreground-disabled);
    font-size: 0.9em;
}

/* ===== Crypto settings ===== */

.crypto-settings {
    min-width: 260px;
}

/* A heading is a label rather than a frame: the popover is small enough that
   two words in the accent-free muted colour are all the separation two lists
   need. */
.crypto-section {
    margin-top: 6px;
    color: var(--color-foreground-muted);
    font-size: 0.85em;
    font-weight: 700;
}

.crypto-setting-row {
    min-height: 32px;
}

/* The stock switch is the one control in the panel that ships its own colour,
   and the colour it ships is the toolkit's blue. The accent belongs to the
   user's config. */
.crypto-setting-row switch:checked {
    background-color: var(--color-accent);
}

.crypto-setting-row switch:checked > slider {
    background-color: var(--color-on-accent);
}

button.crypto-reorder,
button.crypto-remove {
    min-width: 24px;
    min-height: 24px;
    padding: 0;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: 9999px;
    color: var(--color-foreground-muted);
    -gtk-icon-size: 14px;
}

button.crypto-reorder:hover,
button.crypto-remove:hover {
    background-color: var(--color-widget-hover);
    color: var(--color-foreground);
}

/* An arrow with nowhere to go is dimmed rather than hidden, so the row does
   not change width as an entry moves up and down the list. */
button.crypto-reorder:disabled,
button.crypto-remove:disabled {
    color: var(--color-foreground-disabled);
    opacity: 0.4;
}

.crypto-add-pair {
    margin-top: 4px;
}

.crypto-pair-slash {
    color: var(--color-foreground-muted);
}

/* ===== Location dialog ===== */

/* The window is only a transparent frame; the dialog inside it is what is
   painted, and the backdrop is a separate full-screen surface under it. */
.location-window {
    background: transparent;
}

.location-backdrop {
    background-color: rgba(0, 0, 0, 0.45);
}

/* Its own window, so it inherits nothing: the typography every other surface
   gets from `.popover-surface` has to be stated here too. */
.location-dialog {
    background-color: var(--color-popover);
    border: 1px solid var(--color-surface-border);
    border-radius: var(--radius-popover);
    box-shadow: var(--shadow-modal);
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--dialog-font-size);
    font-weight: 400;
    padding: 16px;
}

.location-title {
    font-size: 1.2em;
    font-weight: 700;
}

entry.location-search,
entry.location-coordinate {
    min-height: 32px;
    padding: 4px 14px;
    background: none;
    background-color: var(--color-card);
    border: 1px solid transparent;
    border-radius: var(--radius-card);
    box-shadow: none;
    color: var(--color-foreground);
}

entry.location-search:focus-within,
entry.location-coordinate:focus-within {
    border-color: var(--color-accent);
    outline: none;
}

/* Leading search icons have their own allocation; reserve a visible gap
   before the query so the caret never crowds the icon. */
entry.location-search > image.left,
entry.chooser-search > image.left {
    margin-right: 6px;
}

/* Empty until a search returns something, so it must not reserve anything of
   its own — a negative margin here makes GTK measure the box at less than
   zero and complain about it on every frame. */
.location-results {
    background: transparent;
}

button.location-result {
    min-height: 30px;
    padding: 2px 10px;
    background: none;
    border: 1px solid transparent;
    box-shadow: none;
    border-radius: var(--radius-card);
    color: var(--color-foreground);
}

button.location-result:hover {
    background-color: var(--color-widget-hover);
}

/* The border is on every row, transparent until one is picked, so choosing
   a place cannot change the height of the list. */
button.location-result-selected {
    background-color: var(--color-widget-checked);
    border-color: var(--color-accent);
}

.location-error {
    color: var(--color-foreground-muted);
    font-size: 0.9em;
}

.location-advanced {
    color: var(--color-foreground-muted);
}

.location-actions {
    margin-top: 4px;
}

/* `background: none` first, then the colour: the stock theme paints buttons
   with a background *image*, and setting only background-color leaves that
   gradient on top of it. Every other button in the panel does the same. */
button.dialog-button {
    min-height: 30px;
    padding: 2px 16px;
    background: none;
    background-color: var(--color-card);
    border: none;
    box-shadow: none;
    border-radius: 9999px;
    color: var(--color-foreground);
}

button.dialog-button:hover {
    background-color: var(--color-widget-hover);
}

/* A button with nothing to do says so: the Add in the crypto settings is
   disabled for a pair that is already shown, and a full-contrast label on it
   would look like a control that simply did not work. */
button.dialog-button:disabled {
    background-color: transparent;
    color: var(--color-foreground-disabled);
}

button.dialog-button-primary {
    background-color: var(--color-accent);
    color: var(--color-on-accent);
    font-weight: 700;
}

button.dialog-button-primary:hover {
    background-color: var(--color-accent);
    opacity: 0.9;
}

/* ===== Launcher ===== */

.launcher-window {
    background: transparent;
    box-shadow: none;
}

/* The opaque palette color is the safe default until compositor blur is usable.
   A 20% painted tint keeps the blurred backdrop mapped for click-away dismissal. */
.launcher-backdrop {
    background-color: var(--color-launcher-backdrop);
}

.launcher-backdrop-blurred {
    background-color: rgba(128, 128, 128, 0.2);
}

.launcher {
    background: transparent;
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--launcher-font-size);
    padding: 20px;
}

entry.launcher-search {
    min-height: 52px;
    /* Keep the insertion caret comfortably inside the pill outline. */
    padding: 6px 20px;
    background: none;
    background-color: var(--color-surface);
    border: 1px solid var(--color-foreground-muted);
    border-radius: 9999px;
    box-shadow: none;
    color: var(--color-foreground);
    font-size: 1.1em;
}

entry.launcher-search:focus-within {
    border-color: var(--color-accent);
    outline: none;
}

button.launcher-filter {
    min-height: 32px;
    padding: 2px 12px;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: 9999px;
    color: var(--color-foreground-muted);
    font-size: 0.95em;
}

button.launcher-filter:hover,
button.launcher-filter-selected {
    background-color: var(--color-widget-checked);
    color: var(--color-foreground);
}

.launcher-scroll,
.launcher-results,
.launcher-list {
    background: transparent;
}

.launcher-section {
    margin-top: 6px;
    color: var(--color-foreground-muted);
    font-size: 0.95em;
    font-weight: 700;
}

flowbox.launcher-grid {
    border-spacing: 8px;
}

button.launcher-item {
    min-width: 148px;
    min-height: 118px;
    padding: 10px;
    background: none;
    border: 1px solid transparent;
    box-shadow: none;
    border-radius: var(--radius-card);
    color: var(--color-foreground);
}

button.launcher-item:hover,
button.launcher-item-selected {
    background-color: var(--color-widget-hover);
    border-color: var(--color-accent);
}

button.launcher-item.launcher-row {
    min-width: 0;
    min-height: 44px;
    padding: 6px 10px;
}

button.launcher-item.launcher-row .launcher-icon {
    -gtk-icon-size: 24px;
}

.launcher-icon {
    -gtk-icon-size: 72px;
}

.launcher-item-title {
    font-size: 1.06em;
    font-weight: 700;
}

.launcher-item b {
    color: var(--color-accent);
}

.launcher-item-subtitle,
.launcher-status {
    color: var(--color-foreground-muted);
    font-size: 0.95em;
}

.launcher-item-subtitle {
    font-style: italic;
}

/* ===== Standalone chooser ===== */

/* Choosers share the dialog vocabulary but run in their own process, so their
   classes spell out the complete surface rather than relying on a panel root. */
.chooser-window {
    background: transparent;
    box-shadow: none;
}

.chooser-backdrop {
    background-color: rgba(0, 0, 0, 0.45);
}

.chooser-dialog {
    background-color: var(--color-dialog-surface);
    border: 1px solid var(--color-surface-border);
    border-radius: var(--radius-popover);
    box-shadow: var(--shadow-modal);
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--dialog-font-size);
    font-weight: 400;
    padding: 16px;
}

.chooser-title {
    font-size: 1.2em;
    font-weight: 700;
}

.chooser-message,
.chooser-subtitle,
.chooser-empty {
    color: var(--color-foreground-muted);
    font-size: 0.95em;
}

.chooser-message,
.chooser-subtitle,
.chooser-empty,
.chooser-mode {
    font-style: italic;
}

entry.chooser-search {
    min-height: 32px;
    padding: 4px 14px;
    background: none;
    background-color: var(--color-card);
    border: 1px solid transparent;
    border-radius: var(--radius-card);
    box-shadow: none;
    color: var(--color-foreground);
}

entry.chooser-search:focus-within {
    border-color: var(--color-accent);
    outline: none;
}

.chooser-results {
    background: transparent;
}

button.chooser-result {
    min-height: 44px;
    padding: 8px 10px;
    background: none;
    border: 1px solid transparent;
    box-shadow: none;
    border-radius: var(--radius-card);
    color: var(--color-foreground);
}

button.chooser-result:hover {
    background-color: var(--color-widget-hover);
}

button.chooser-result:focus-visible,
button.chooser-result-selected:focus-visible {
    border-color: var(--color-accent);
    outline: none;
}

/* The marker for the applied item and keyboard selection stay independent:
   browsing a new item never makes the desktop look as if it already changed. */
button.chooser-result-selected {
    background-color: var(--color-widget-checked);
    border-color: var(--color-accent);
}

button.chooser-result-current {
    border-style: solid;
}

.chooser-label {
    font-weight: 700;
}

.chooser-mode,
.chooser-current {
    color: var(--color-foreground-muted);
    font-size: 0.9em;
}

.chooser-current {
    color: var(--color-accent);
    font-weight: 700;
}

.chooser-preview,
.chooser-theme-sample {
    background-color: var(--color-card);
    border-radius: var(--radius-card);
}

.chooser-theme-sample {
    padding: 8px;
}

.chooser-preview-image,
.chooser-swatch {
    border-radius: 8px;
}

.chooser-actions {
    margin-top: 4px;
}

/* ===== Pinentry ===== */

window.pinentry-window {
    background: transparent;
    box-shadow: none;
}

.pinentry-backdrop {
    background-color: rgba(0, 0, 0, 0.52);
}

.pinentry-dialog {
    background-color: var(--color-dialog-surface);
    border: 1px solid var(--color-surface-border);
    border-radius: var(--radius-popover);
    box-shadow: var(--shadow-modal);
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--dialog-font-size);
    padding: 16px;
}

.pinentry-title {
    font-size: 1.15em;
    font-weight: 700;
}

.pinentry-context,
.pinentry-description {
    color: var(--color-foreground-muted);
    font-size: 0.95em;
}

.pinentry-context {
    font-style: italic;
}

.pinentry-error,
.pinentry-caps-lock {
    color: var(--color-state-urgent);
}

entry.pinentry-entry {
    min-height: 38px;
    /* Match dialog fields while leaving room for the trailing peek icon. */
    padding: 6px 14px;
}

.pinentry-actions {
    margin-top: 4px;
}

/* ===== Media ===== */

/* The card is hidden outright when no player is on the bus, so it needs no
   empty state of its own — the column simply closes up. It wears nothing but
   `.control-panel-card`, deliberately: it used to carry a padding of its own
   that was 2px tighter than every other card in the column, which is exactly
   the kind of difference nobody can name and everybody can see. */

/* The art is drawn by rounded-picture, which clips it in snapshot(); the
   placeholder behind it carries the same radius so the corner never changes
   shape as a cover arrives. It is painted a step lighter than the card it sits
   on, because a square in the card's own colour is a square nobody can see. */
.media-art-placeholder {
    background-color: var(--color-widget-hover);
    border-radius: 12px;
    color: var(--color-foreground-disabled);
    -gtk-icon-size: 32px;
}

.media-title {
    font-weight: 700;
}

.media-artist {
    color: var(--color-foreground-muted);
}

/* The transport is a centred row of its own now, not a tail on the artist
   line, so the gap between the buttons is what separates them rather than a
   box spacing that also has to serve the rows above. Same shape as
   `.crypto-entry`: the container states the rhythm, the widget states none. */
.media-controls > *:not(:last-child) {
    margin-right: 8px;
}

button.media-control {
    min-width: 28px;
    min-height: 28px;
    padding: 0;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: 9999px;
    color: var(--color-foreground);
    -gtk-icon-size: 16px;
}

button.media-control:hover {
    background-color: var(--color-widget-hover);
}

button.media-control:active {
    background-color: var(--color-widget-pressed);
}

button.media-control-primary {
    -gtk-icon-size: 20px;
}

/* A control the player cannot honour is dimmed rather than hidden: the row
   must not change shape as tracks come and go. */
button.media-control:disabled {
    background: none;
    opacity: 0.3;
}

/* A thin GNOME-style slider: the filled part is the accent colour, and the
   thumb is only as big as it has to be to grab. */
scale.media-seek {
    min-height: 16px;
    padding: 0;
    margin: 0;
}

scale.media-seek trough {
    min-height: 4px;
    background-color: var(--color-card);
    border: none;
    border-radius: 9999px;
}

/* Adwaita paints the filled part with a 1px blue border as well as a blue
   background, and then pulls it out again with a matching -1px margin. At 4px
   tall that border is half the bar, so the fill read as Adwaita's blue with a
   thread of the accent down the middle of it; and with the border gone, the
   margin alone would leave the fill fatter than the trough it sits in. Both
   have to go, which is the same lesson the knob below and `.qs-meter` learned
   separately. */
scale.media-seek highlight {
    min-height: 4px;
    background-color: var(--color-accent);
    border: none;
    margin: 0;
    border-radius: 9999px;
}

scale.media-seek slider {
    min-width: 12px;
    min-height: 12px;
    margin: -5px;
    /* Adwaita paints the knob with a light-theme gradient image plus a
       shadow; on a dark surface whatever is not zeroed bleeds through as a
       white fringe around the thumb. Flat colour, nothing else. */
    background-image: none;
    background-color: var(--color-foreground);
    border: none;
    border-radius: 9999px;
    box-shadow: none;
    outline-color: transparent;
}

.media-time {
    margin-top: 2px;
}

.media-time-label {
    font-size: 0.85em;
    color: var(--color-foreground-muted);
    font-feature-settings: "tnum";
}

/* Top-right, on the title's line. The gap is so a long title ellipsizes
   before it reaches the buttons rather than touching them. */
.media-switcher {
    margin-left: 8px;
}

/* Every button carries the ring's border, transparent until it is the active
   one, so switching players cannot change the size of the row. */
button.media-switcher-button {
    min-width: 24px;
    min-height: 24px;
    padding: 2px;
    background: none;
    border: 2px solid transparent;
    box-shadow: none;
    border-radius: 9999px;
}

button.media-switcher-button:hover {
    background-color: var(--color-widget-hover);
}

button.media-switcher-active {
    border-color: var(--color-accent);
}

.media-switcher-initial {
    font-size: 0.85em;
    font-weight: 700;
    color: var(--color-foreground-muted);
}

/* ===== Mail ===== */

/* The count lives in the tooltip, not on the bar: an envelope is the same
   width whether it is three messages or three hundred, and a widget that
   changes width every time mail arrives moves everything beside it. */
.notmuch-icon {
    -gtk-icon-size: var(--icon-size);
}

.notmuch-popover {
    min-width: 320px;
}

.notmuch-header {
    min-height: 28px;
    margin-bottom: 4px;
}

/* Tabular figures, so a count crossing a digit boundary does not shuffle the
   header's title sideways. */
.notmuch-count {
    font-weight: 700;
    color: var(--color-foreground-muted);
    font-feature-settings: "tnum";
}

.notmuch-row {
    border-radius: var(--radius-card);
    padding: 6px 8px;
}

.notmuch-row:hover {
    background-color: var(--color-widget-hover);
}

.notmuch-sender {
    font-weight: 700;
}

.notmuch-subject {
    font-size: 0.92em;
    color: var(--color-foreground-muted);
}

.notmuch-time {
    font-size: 0.85em;
    color: var(--color-foreground-muted);
    font-variant-numeric: tabular-nums;
}

/* How many unread messages one conversation holds. Only drawn past one, which
   is what stops the header's message count and this list's length looking
   like they disagree. */
.notmuch-matched {
    font-size: 0.85em;
    font-weight: 700;
    color: var(--color-foreground-muted);
    background-color: var(--color-card);
    border-radius: 9999px;
    padding: 0 6px;
}

.notmuch-list {
    margin: 0;
}

.notmuch-more {
    font-size: 0.85em;
    color: var(--color-foreground-disabled);
    padding: 0 8px;
}

/* ===== Notification history ===== */

.notification-list {
    background: transparent;
}

.notification-header {
    min-height: 28px;
    margin-bottom: 8px;
}

/* Drawn from scratch, like every button in Quick Settings and for the same
   reason: Adwaita's `background-image` paints on top of a background *colour*,
   so a rule that sets only the colour changes nothing on screen. */
button.notification-clear-all,
button.notification-group-clear,
button.notification-close,
button.notification-group-header {
    background-image: none;
    box-shadow: none;
    text-shadow: none;
    outline: none;
}

button.notification-clear-all,
button.notification-group-clear,
button.notification-close {
    min-width: 24px;
    min-height: 24px;
    padding: 0 8px;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-card);
    color: var(--color-foreground-muted);
}

button.notification-clear-all:hover,
button.notification-group-clear:hover,
button.notification-close:hover {
    background-color: var(--color-widget-hover);
    color: var(--color-foreground);
}

button.notification-group-clear,
button.notification-close {
    padding: 0;
}

.notification-group {
    padding: 4px;
}

/* The header is a button so the whole strip expands the group, but it must not
   look like one until the pointer is on it. */
button.notification-group-header {
    padding: 6px 8px;
    background: none;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-card);
    color: var(--color-foreground);
}

button.notification-group-header:hover {
    background-color: var(--color-widget-hover);
}

/* A group of one cannot expand, so it never offers the affordance either. */
button.notification-group-header.notification-group-single:hover {
    background: none;
}

.notification-icon {
    -gtk-icon-size: 24px;
}

.notification-app {
    font-weight: 700;
}

/* The line under a closed group's name: what arrived, in the same weight and
   colour the body of a row uses, because that is what it is. */
.notification-preview {
    font-size: 0.92em;
    color: var(--color-foreground-muted);
}

/* The same chip the crypto popover puts a 24h change in: a tinted pill at
   0.85em with tabular figures, so 1 and 11 are the same shape and a group
   whose count ticks over does not shuffle the chevron beside it. */
.notification-count {
    font-size: 0.85em;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    color: var(--color-foreground-muted);
    background-color: var(--color-card);
    border-radius: 9999px;
    min-width: 12px;
    padding: 1px 8px;
}

.notification-chevron {
    -gtk-icon-size: 16px;
    color: var(--color-foreground-muted);
}

/* No side margins: the rows line up with the header above them on both edges.
   The stack used to be inset four pixels on each side, which put its summaries
   to the left of the application name and its close buttons eight pixels
   inside the clear button they sit under — two rails where a card has one. */
.notification-group-list {
    margin-top: 2px;
}

/* Right padding is the close button's own: with none of its own here the ✕
   lands exactly under the group's clear button. */
.notification-row {
    padding: 6px 0 6px 8px;
    border-radius: var(--radius-card);
}

/* A row is a way back to the application that sent it, so it answers the
   pointer the same way the group header above it does. */
.notification-row:hover {
    background-color: var(--color-widget-hover);
}

.notification-summary {
    font-weight: 500;
}

.notification-body {
    font-size: 0.92em;
    color: var(--color-foreground-muted);
}

/* Muted rather than disabled. It is the only thing on the row saying *when*,
   and at 40% over a translucent popover it was the faintest thing in the
   panel; a size below the summary is enough to keep it secondary. Tabular
   because it changes under itself every minute — "1m ago" to "11m ago" must
   not shift the ellipsis in the summary beside it. */
.notification-time {
    font-size: 0.85em;
    font-variant-numeric: tabular-nums;
    color: var(--color-foreground-muted);
}

/* ===== Notification banners ===== */

window.toast-window {
    background: transparent;
}

.toast-stack {
    background: transparent;
}

.toast {
    background-color: var(--color-popover);
    border: 1px solid var(--color-surface-border);
    border-radius: var(--radius-popover);
    box-shadow: var(--shadow-popover);
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--font-size);
    font-weight: 400;
    padding: 12px 14px;
}

/* A banner that will not go away by itself says so with the state colour
   rather than with more chrome. */
.toast.toast-critical {
    border-color: var(--color-state-urgent);
}

.toast-icon {
    -gtk-icon-size: 32px;
}

.toast-summary {
    font-weight: 700;
}

.toast-body {
    color: var(--color-foreground-muted);
}

/* 24, which is what the ✕ on a history row measures. The two are the same
   affordance on the same notification and stood two pixels apart. */
button.toast-close {
    min-width: 24px;
    min-height: 24px;
    padding: 0;
    background: none;
    background-image: none;
    border: none;
    box-shadow: none;
    text-shadow: none;
    outline: none;
    border-radius: 9999px;
    color: var(--color-foreground-muted);
}

button.toast-close:hover {
    background-color: var(--color-widget-hover);
    color: var(--color-foreground);
}

/* The row keeps its own top gap so a banner with no actions is not padded
   for buttons it does not have. */
.toast-actions {
    margin-top: 2px;
}

/* 24 of content and 4px above and below is 32 on screen — a pill a pointer
   can hit without aiming, and the same height as the panel's own small
   controls. GTK adds padding to `min-height` rather than taking it out of it,
   so the 26 this asked for really was 26. */
button.toast-action {
    min-height: 24px;
    padding: 4px 14px;
    background-color: var(--color-card);
    background-image: none;
    border: none;
    box-shadow: none;
    text-shadow: none;
    outline: none;
    border-radius: 9999px;
    color: var(--color-foreground);
    font-size: 0.92em;
    font-weight: 500;
}

button.toast-action:hover {
    background-color: var(--color-widget-hover);
}

button.toast-action:active {
    background-color: var(--color-widget-pressed);
}

/* ===== System tray ===== */

/* The pill holds the icons; each icon is its own button inside it, so hovering
   one lights that one rather than the whole tray. */
.tray .content {
    padding-left: 4px;
    padding-right: 4px;
}

.tray-icons {
    /* No spacing of its own: the buttons carry their own padding, and a gap on
       top of it would leave dead ground between two hover targets. */
    padding: 0;
}

.tray-item,
.tray-overflow {
    padding: 2px 4px;
    border-radius: 6px;
}

.tray-item:hover,
.tray-overflow:hover {
    background-color: var(--color-widget-hover);
}

.tray-item:active,
.tray-overflow:active {
    background-color: var(--color-widget-pressed);
}

.tray-item-icon {
    color: var(--color-foreground);
}

/* An item that wants attention: the icon takes the warning colour, and the
   tint behind it is what the pulse animates. */
.tray-item-attention {
    color: var(--color-state-warning);
}

.tray-item-tint {
    border-radius: 6px;
}

/* Only an item that is shouting is tinted; the box is there on every item so
   the pulse has something to animate without a relayout. */
.tray-item-shouting .tray-item-tint {
    background-color: var(--color-state-warning-fill);
}

.tray-overflow-popover {
    padding: 8px;
}

.tray-overflow-grid {
    padding: 0;
}

/* ===== Tray menus ===== */

.tray-menu {
    padding: 6px;
    min-width: 180px;
}

.tray-menu-list {
    padding: 0;
}

.tray-menu-row,
.tray-menu-back {
    padding: 6px 8px;
    border-radius: 8px;
    color: var(--color-foreground);
}

.tray-menu-row:hover,
.tray-menu-back:hover {
    background-color: var(--color-widget-hover);
}

.tray-menu-row:active,
.tray-menu-back:active {
    background-color: var(--color-widget-pressed);
}

/* A row the application has switched off: dimmed, and with no hover at all,
   because a hover on something inert is a promise the row cannot keep. */
.tray-menu-row.disabled,
.tray-menu-row.disabled:hover,
.tray-menu-row.disabled .tray-menu-label {
    background-color: transparent;
    color: var(--color-foreground-disabled);
}

.tray-menu-label {
    color: inherit;
}

.tray-menu-mark,
.tray-menu-icon {
    color: var(--color-foreground-muted);
}

.tray-menu-row .tray-menu-mark {
    color: var(--color-accent);
}

.tray-menu-chevron {
    color: var(--color-foreground-muted);
}

.tray-menu-separator {
    background-color: var(--color-surface-border);
    margin: 4px 8px;
    min-height: 1px;
}

.tray-menu-back {
    color: var(--color-foreground-muted);
    margin-bottom: 2px;
}

.tray-menu-empty {
    padding: 12px;
    color: var(--color-foreground-muted);
}

/* ===== Tooltip ===== */

window.tooltip-window {
    background: transparent;
}

.tooltip-surface {
    background-color: var(--color-surface);
    border: 1px solid var(--color-surface-border);
    border-radius: var(--radius-surface);
    padding: 6px 10px;
}

.tooltip-label {
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground);
}

/* ===== The volume/brightness capsule ===== */

window.osd-window {
    background: transparent;
}

/* GNOME 42's pill: one row, fully rounded, nothing else on it. The bar and
   the icon are drawn from Rust (see surfaces/osd_bar.rs), so the only thing
   CSS decides here is the surface it all sits on. */
.osd-capsule {
    background-color: var(--color-popover);
    border: 1px solid var(--color-surface-border);
    border-radius: 9999px;
    box-shadow: var(--shadow-popover);
    color: var(--color-foreground);
    font-family: var(--font-family);
    font-size: var(--font-size);
    padding: 16px var(--osd-padding);
}

.osd-icon {
    -gtk-icon-size: 24px;
    color: var(--color-foreground);
}

/* The custom widget reads its own `color` for the unfilled track; the fill
   itself is the configured accent, which CSS cannot hand to Rust. */
osd-bar {
    color: var(--color-foreground);
}

.osd-caption {
    color: var(--color-foreground-muted);
}

.osd-value {
    color: var(--color-foreground);
    font-variant-numeric: tabular-nums;
}

/* ===== Quick Settings — the bar button ===== */

.qs-indicator {
    /* The gap between status icons is tighter than the gap between widgets:
       they are one pill's worth of information, not a row of buttons. */
    margin: 0;
}

.qs-indicator > * {
    margin-right: 4px;
}

.qs-indicator > *:last-child {
    margin-right: 0;
}

.qs-icon {
    -gtk-icon-size: var(--icon-size);
    color: var(--color-foreground);
}

/* A battery that is low and on nothing but itself. */
.qs-icon-urgent {
    color: var(--color-state-urgent);
}

/* A tunnel that is up, or any other state the user chose rather than read. */
.qs-icon-accent {
    color: var(--color-accent);
}

/* The microphone-in-use dot. Not an icon: a dot is what GNOME draws, and it
   has to read at a glance on a 36px bar. */
.qs-privacy-dot {
    background-color: var(--color-state-warning);
    border-radius: 9999px;
    min-width: 8px;
    min-height: 8px;
}

/* ===== Quick Settings — the panel ===== */

.qs-panel {
    padding: 0;
}

.qs-scroll {
    background: transparent;
}

.qs-content {
    padding: 12px;
}

.qs-content > * {
    margin-bottom: 8px;
}

.qs-content > *:last-child {
    margin-bottom: 0;
}

/* --- Header --- */

.qs-header {
    min-height: 40px;
}

/* Every button in the panel is drawn from scratch.
 *
 * Adwaita gives `button` a `background-image` and a `box-shadow`, and a
 * background *image* paints on top of a background *colour* — so a rule that
 * sets only `background-color` leaves the theme's own grey on screen and
 * changes nothing visible. It cost an afternoon: the charge-limit buttons
 * carried the accent class, the class matched, and the pill stayed grey.
 * Clearing both on the base rule of each button is what makes the colours
 * below the ones that actually land, in every state. */
.qs-battery-pill,
.qs-round-button,
.qs-slider-icon,
.qs-chooser,
.qs-device-row,
.qs-toggle,
.qs-toggle-expand,
.qs-radio-row,
.qs-network-row,
.qs-pair-row,
.qs-password-entry,
.qs-password-button,
.qs-vpn-row,
.qs-limit-button {
    background-image: none;
    box-shadow: none;
    text-shadow: none;
    outline: none;
}

/* Keyboard focus, which the reset above took away along with Adwaita's
 * background images: `outline: none` is what draws a focus ring in GTK4, so
 * every control in the panel moved the focus and drew nothing. The power rows
 * are the worst of it — they are the one thing here that has to be *held*
 * down, they take Enter and space so that a keyboard can do it, and there was
 * no way to see which row was about to be held.
 *
 * `.keyboard <control>:focus` rather than `<control>:focus-visible`, which is
 * the rule this wants and cannot have. GTK stamps the state behind
 * `:focus-visible` from what it knows about a window the compositor has
 * activated, and a layer surface is never activated, so the selector matched
 * nothing here however the panel was driven — and setting the window's
 * `focus-visible` property by hand does not rescue it. `.keyboard` is put on
 * the popover window by the first key pressed in it and taken off by the next
 * click, so a panel driven by a pointer still draws no rings.
 *
 * The offset is negative so the ring is inside the control's own edge and
 * never overlaps the row above it. */
.keyboard .qs-battery-pill:focus,
.keyboard .qs-round-button:focus,
.keyboard .qs-slider-icon:focus,
.keyboard .qs-chooser:focus,
.keyboard .qs-device-row:focus,
.keyboard .qs-toggle:focus,
.keyboard .qs-toggle-expand:focus,
.keyboard .qs-radio-row:focus,
.keyboard .qs-network-row:focus,
.keyboard .qs-pair-row:focus,
.keyboard .qs-password-button:focus,
.keyboard .qs-vpn-row:focus,
.keyboard .qs-limit-button:focus,
.keyboard .qs-power-row:focus,
/* The control panel is the same kind of surface with the same problem: its
   notification controls cleared Adwaita's ring along with its backgrounds and
   drew nothing in its place, so tabbing through the history moved a focus
   nobody could see. The row's close button is faded until it is hovered — the
   ring is how a keyboard says where it is. */
.keyboard .notification-clear-all:focus,
.keyboard .notification-group-header:focus,
.keyboard .notification-group-clear:focus,
.keyboard .notification-close:focus,
/* The media card cleared its backgrounds the same way and was left out of this
   list, so the transport was the one part of the panel a keyboard could reach
   and not see. */
.keyboard .media-control:focus,
.keyboard .media-switcher-button:focus,
.keyboard .dnd-row switch:focus {
    outline: 2px solid var(--color-accent);
    outline-offset: -2px;
}

/* On something already filled with the accent, the ring has to be the colour
   drawn *on* the accent or it is invisible. */
.keyboard .qs-toggle-pill.checked .qs-toggle:focus,
.keyboard .qs-toggle-pill.checked .qs-toggle-expand:focus,
.keyboard .qs-limit-button.checked:focus,
.keyboard .qs-password-button.checked:focus {
    outline-color: var(--color-on-accent);
}

.qs-battery-pill {
    background-color: var(--color-card);
    border: none;
    border-radius: 9999px;
    padding: 6px 12px;
    color: var(--color-foreground);
}

.qs-battery-pill:hover {
    background-color: var(--color-widget-hover);
}

.qs-battery-pill.checked {
    background-color: var(--color-widget-checked);
}

.qs-battery-percent {
    font-variant-numeric: tabular-nums;
    margin-left: 6px;
}

/* 28 + 4px of padding on each side is 36 on screen, which is what the battery
   pill beside it measures. GTK's `min-height` is the *content* box and padding
   is added to it, so a round button asking for 32 was drawing 40 and standing
   4px taller than the pill it shares a row with. */
.qs-round-button {
    background-color: var(--color-card);
    border: none;
    border-radius: 9999px;
    min-width: 28px;
    min-height: 28px;
    padding: 4px;
    color: var(--color-foreground);
    -gtk-icon-size: var(--icon-size);
}

.qs-round-button:hover {
    background-color: var(--color-widget-hover);
}

.qs-round-button:active,
.qs-round-button.checked {
    background-color: var(--color-widget-checked);
}

/* --- Sliders --- */

/* The block sits a little away from the header above it: the sliders are a
   different kind of control and reading them as one group matters. */
.qs-sliders {
    margin-top: 4px;
}

.qs-slider-row {
    min-height: 36px;
}

/* 28 and 4px of padding is 36 on screen, the same round control as the header's
   two buttons — and the height `.qs-slider-row` above asks for. At 32 it was
   the only 40px control in the panel and the row's own rule never applied.
   Worn by a button on the two rows that mute and by a plain image on the one
   that does not; the box is the same either way, which is what keeps the icon
   column straight. */
.qs-slider-icon {
    background: transparent;
    border: none;
    border-radius: 9999px;
    min-width: 28px;
    min-height: 28px;
    padding: 4px;
    color: var(--color-foreground);
    -gtk-icon-size: var(--icon-size);
}

.qs-slider-icon:hover {
    background-color: var(--color-widget-hover);
}

.qs-slider-icon:disabled {
    color: var(--color-foreground-disabled);
}

.qs-slider trough {
    background-color: var(--color-card);
    border: none;
    border-radius: 9999px;
    min-height: 6px;
}

/* The same borders the seek bar's fill had, for the same reason. See
   scale.media-seek highlight. */
.qs-slider highlight {
    background-color: var(--color-accent);
    border: none;
    margin: 0;
    border-radius: 9999px;
    min-height: 6px;
}

.qs-slider slider {
    /* Same as the seek bar's knob: Adwaita's gradient image and shadow are
       drawn for a light surface and read as a ragged white halo on this one.
       See scale.media-seek slider. */
    background-image: none;
    background-color: var(--color-foreground);
    border: none;
    border-radius: 9999px;
    box-shadow: none;
    outline-color: transparent;
    min-width: 16px;
    min-height: 16px;
    margin: -6px;
}

.qs-slider:disabled highlight {
    background-color: var(--color-foreground-disabled);
}

/* 24 and 2px of padding is 28 on screen, which is what the chevron on a toggle
   pill measures. The panel has two chevrons that mean the same thing and they
   were four pixels apart in size. */
.qs-chooser {
    background: transparent;
    border: none;
    border-radius: 9999px;
    min-width: 24px;
    min-height: 24px;
    padding: 2px;
    color: var(--color-foreground-muted);
    -gtk-icon-size: var(--icon-size);
}

.qs-chooser:hover {
    background-color: var(--color-widget-hover);
    color: var(--color-foreground);
}

.qs-device-list {
    padding: 4px 0 4px 40px;
}

/* Every list row in the panel is the same row: 28 of content and 8px of
   padding, which is 44 on screen. There are three of them — Bluetooth devices,
   networks, output devices — and until this rule and the one below agreed, a
   panel that had both open showed 44px rows under one pill and 52px rows under
   the next, indented two pixels differently. */
.qs-device-row {
    background: transparent;
    border: none;
    border-radius: var(--radius-card);
    min-height: 28px;
    padding: 8px 12px;
    color: var(--color-foreground);
}

.qs-device-row:hover {
    background-color: var(--color-widget-hover);
}

.qs-device-name {
    color: inherit;
}

.qs-device-mark {
    color: var(--color-accent);
    -gtk-icon-size: var(--icon-size);
}

/* --- Toggle grid --- */

.qs-grid {
    margin-top: 4px;
}

.qs-grid-row {
    min-height: 48px;
}

/* The pill is a box holding two buttons, not one button holding another: a
   GtkButton inside a GtkButton can never be clicked, because the outer one
   claims the click in the capture phase and GTK then cancels every gesture
   below it. So the shape and the fill live on the box, and each half paints
   only its own hover. */
.qs-toggle-pill {
    background-color: var(--color-card);
    border-radius: 24px;
    min-height: 48px;
    color: var(--color-foreground);
}

/* Checked is the accent fill, which is how every GNOME quick-settings toggle
   says it is on — the icon does not change. */
.qs-toggle-pill.checked {
    background-color: var(--color-accent);
    color: var(--color-on-accent);
}

/* 40 plus 4px of padding top and bottom is the 48 the pill around it asks
   for. GTK adds padding to `min-height` rather than taking it out of it, so a
   body asking for 48 as well made every pill in the grid 56 — half a row
   taller than GNOME's, eight pixels at a time, four times down the panel. */
.qs-toggle {
    background-color: transparent;
    border: none;
    border-radius: 24px;
    min-height: 40px;
    padding: 4px 12px;
    color: inherit;
}

/* A body with a chevron beside it gives up its right padding to the chevron's
   margin, so the label has exactly the room it had when the chevron was inside
   the body — and the two halves meet, the way a split control should. */
.qs-toggle-split {
    padding-right: 0;
}

.qs-toggle:hover {
    background-color: var(--color-widget-hover);
}

.qs-toggle:disabled {
    color: var(--color-foreground-disabled);
}

.qs-toggle-icon {
    -gtk-icon-size: var(--icon-size);
    color: inherit;
    margin-right: 8px;
}

.qs-toggle-label {
    color: inherit;
    font-weight: 700;
}

.qs-toggle-subtitle {
    color: inherit;
    font-size: 12px;
    opacity: 0.7;
}

/* The margin is what the body's right padding used to give it, now that the
   chevron sits beside the body rather than inside it. */
.qs-toggle-expand {
    background: transparent;
    border: none;
    border-radius: 9999px;
    min-width: 28px;
    min-height: 28px;
    margin-right: 12px;
    padding: 0;
    color: inherit;
    -gtk-icon-size: var(--icon-size);
}

.qs-toggle-expand:hover {
    background-color: var(--color-widget-hover);
}

/* The fourth list row in the panel, and the same 44 as the other three. Left
   to its content it came out at 40, so a panel with Power Mode open and any
   other list open showed two lists whose rows were four pixels apart. */
.qs-radio-row {
    background: transparent;
    border: none;
    border-radius: var(--radius-card);
    min-height: 28px;
    padding: 8px 12px;
    color: var(--color-foreground);
}

.qs-radio-row:hover {
    background-color: var(--color-widget-hover);
}

.qs-radio-mark {
    color: var(--color-accent);
    -gtk-icon-size: var(--icon-size);
}

/* --- Network and VPN lists --- */

/* A header rather than a title: the scanning spinner lives in it, and a
   spinner that appeared beside the first row would push the list down. */
.qs-list-header {
    padding: 2px 12px 4px 12px;
    color: var(--color-foreground-muted);
    font-size: 12px;
}

/* The same 44px row as `.qs-device-row`. A VPN row carries two lines and comes
   out taller than that on its own, which is honest — it is a taller row. */
.qs-network-row,
.qs-pair-row,
.qs-vpn-row {
    background: transparent;
    border: none;
    border-radius: var(--radius-card);
    padding: 8px 12px;
    color: var(--color-foreground);
    min-height: 28px;
}

.qs-network-row:hover,
.qs-pair-row:hover,
.qs-vpn-row:hover {
    background-color: var(--color-widget-hover);
}

.qs-network-row:disabled,
.qs-pair-row:disabled,
.qs-vpn-row:disabled {
    color: var(--color-foreground-disabled);
}

/* The padlock is a hint, not a heading: it sits beside the name at the same
   size as the signal icon and a step down in contrast. */
.qs-network-badge {
    color: var(--color-foreground-muted);
    -gtk-icon-size: var(--icon-size);
}

/* The password box, opened under the row it belongs to. Indented to the same
   place the row's text starts, so it reads as part of that row. */
.qs-password-row {
    padding: 4px 12px 8px 12px;
}

.qs-password-entry {
    background-color: var(--color-card);
    border: 1px solid var(--color-surface-border);
    border-radius: var(--radius-card);
    color: var(--color-foreground);
    min-height: 32px;
    padding: 2px 8px;
}

.qs-password-entry:focus-within {
    border-color: var(--color-accent);
    /* Adwaita's focus ring is its own blue and ignores the border colour, so
       it is named here as well; a stock-blue ring inside an accent-green panel
       is the one thing on it that looks like it came from another program. */
    outline-color: var(--color-accent);
}

.qs-password-button {
    background-color: var(--color-card);
    border: none;
    border-radius: var(--radius-card);
    padding: 6px 14px;
    color: var(--color-foreground);
}

.qs-password-button:hover {
    background-color: var(--color-widget-hover);
}

.qs-password-button.checked {
    background-color: var(--color-accent);
    color: var(--color-on-accent);
}

/* The wired row: a statement, not a control. No hover, no pointer.
   48 on screen — the same height as a pill in the grid above it, which is what
   a row of that width should be. It was 60, and a statement standing taller
   than every control in the panel read as the most important thing in it. */
.qs-status-row {
    background-color: var(--color-card);
    border-radius: var(--radius-card);
    padding: 12px;
    color: var(--color-foreground);
    min-height: 24px;
}

/* --- Bluetooth --- */

/* A device's battery, in tabular figures so 85% and 100% are the same width
   and the switch beside them does not shuffle sideways once a minute. */
.qs-device-battery {
    color: var(--color-foreground-muted);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
}

/* GTK's own switch, restyled. Adwaita paints "on" in its *own* accent — a
   blue that has nothing to do with the panel's — and every other control in
   the panel that means "on" is the configured accent. */
.qs-device-switch {
    background-image: none;
    box-shadow: none;
    background-color: var(--color-widget-hover);
    border: none;
    border-radius: 9999px;
    margin-left: 2px;
}

.qs-device-switch:checked {
    background-color: var(--color-accent);
}

.qs-device-switch > slider {
    background-image: none;
    box-shadow: none;
    background-color: var(--color-foreground);
    border: none;
    border-radius: 9999px;
}

.qs-device-switch:disabled {
    background-color: var(--color-card);
}

/* The pairing box: a question somebody else asked, so it is tinted rather
   than silent — it is the one thing in the panel the user did not open. */
.qs-pairing-row {
    background-color: var(--color-widget-hover);
    border-radius: var(--radius-card);
    padding: 10px 12px;
    margin: 4px 0;
}

/* The code has to be readable across a desk at the same time as the phone
   showing it, which is what the size and the letter-spacing are for. */
.qs-pairing-code {
    color: var(--color-foreground);
    font-size: 24px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    letter-spacing: 4px;
    margin: 2px 0;
}

/* --- Cards --- */

.qs-card {
    background-color: var(--color-card);
    border-radius: var(--radius-card);
    padding: 12px;
}

.qs-card-title {
    color: var(--color-foreground);
    font-weight: 700;
}

/* Tabular figures: every card line in the panel is mostly numbers that move —
   "62% · Discharging", "2h 15m remaining", "Charging stops at 80%" — and with
   proportional ones the whole line shuffles sideways once a minute. */
.qs-card-line {
    color: var(--color-foreground-muted);
    font-variant-numeric: tabular-nums;
}

.qs-limit-row {
    margin-top: 4px;
}

.qs-limit-button {
    background-color: var(--color-widget-hover);
    border: none;
    border-radius: 9999px;
    padding: 6px 14px;
    color: var(--color-foreground);
}

.qs-limit-button:hover {
    background-color: var(--color-widget-checked);
}

.qs-limit-button.checked {
    background-color: var(--color-accent);
    color: var(--color-on-accent);
}

.qs-limit-button:disabled {
    background-color: var(--color-card);
    color: var(--color-foreground-disabled);
}

/* Muted rather than disabled. This is the panel's secondary text — "No
   networks found", "Bluetooth is off", "Enter the password for Cafe", the udev
   rule that would make the charge-limit buttons work — and every one of those
   is something the user is meant to read. At the disabled 40% they were the
   faintest thing on the panel, which said "ignore me" about the one line that
   explained what had happened. */
.qs-hint {
    color: var(--color-foreground-muted);
    font-size: 12px;
}

/* The updates card is one line and a subtitle: a statement, not a control.
   24 of content inside the card's own 12px padding is 48 with one line on it,
   and grows by the second line rather than reserving room for one. */
.qs-updates {
    min-height: 24px;
}

/* --- Resource overview --- */

.qs-resources {
    padding: 12px;
}

.qs-meter-row {
    min-height: 22px;
}

/* GTK's own level bar, restyled: the theme's default is a segmented
   discrete-mode look that reads as a battery gauge rather than a usage bar. */
.qs-meter {
    min-height: 6px;
}

.qs-meter trough {
    background-color: var(--color-widget-hover);
    border: none;
    border-radius: 9999px;
    min-height: 6px;
}

.qs-meter block.filled {
    background-color: var(--color-accent);
    border: none;
    border-radius: 9999px;
    min-height: 6px;
}

/* A reading worth looking at looks like one. The card does not *act* on this
   — that is the system_monitor widget's job, with its own thresholds — but a
   disk at 97% should not be the same colour as one at 12%. */
.qs-meter-warning block.filled {
    background-color: var(--color-state-warning);
}

/* Tabular figures and a fixed width, so 9% becoming 10% does not shorten the
   bar beside it by a character. */
.qs-meter-value {
    color: var(--color-foreground-muted);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    min-width: 84px;
}

/* --- Power section --- */

.qs-power-row {
    background-color: var(--color-card);
    border-radius: var(--radius-card);
    min-height: 44px;
    padding: 0;
    color: var(--color-foreground);
}

.qs-power-row:hover {
    background-color: var(--color-widget-hover);
}

/* The fill's width is set from Rust, a frame at a time; CSS only says what
   colour it is and that it is clipped to the row's corners. */
.qs-power-fill {
    background-color: var(--color-accent);
    border-radius: var(--radius-card);
}

/* With motion off there is no fill to watch, so the row itself carries the
   confirming state for the same 650ms. */
.qs-power-row.confirming {
    background-color: var(--color-state-warning-fill);
}

/* --- Inline failures --- */

/* Indented to where the text it is explaining starts. Every card, row and list
   in the panel insets its text by 12px; a caption hanging at the panel's own
   edge was the one line in it that started somewhere else. */
.inline-error {
    color: var(--color-state-urgent);
    font-size: 12px;
    margin-top: 2px;
    padding: 0 12px;
}

/* Except inside something that has already done the inset. The battery card
   and the password box both pad themselves by 12, so a caption in one of them
   takes the indent twice and hangs a step to the right of the lines it belongs
   under — and the password prompt wears this class only on a retry, so it
   would have jumped sideways at the moment it turned red. */
.qs-card .inline-error,
.qs-password-row .inline-error {
    padding: 0;
}
"#;

thread_local! {
    /// The provider currently installed on the default display.
    static PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

/// Install the configuration's stylesheet and this application's GTK polarity.
///
/// The new provider is added before the old one is removed, so a reload never
/// leaves a frame with unstyled widgets.
pub fn apply(config: &Config) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    if let Some(settings) = gtk4::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(config.theme.mode == "dark");
    }
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&generate(config));

    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let previous = PROVIDER.with(|cell| cell.borrow_mut().replace(provider));
    if let Some(previous) = previous {
        gtk4::style_context_remove_provider_for_display(&display, &previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_bar_keeps_the_exclusive_zone_tight() {
        let mut config = Config::default();
        config.bar.padding = 4;
        assert_eq!(window_height(&config), 36 + 8);

        config.bar.background_opacity = 0.0;
        assert_eq!(window_height(&config), 36 + 4);
    }

    #[test]
    fn widget_radius_becomes_a_pill_at_fifty_percent() {
        let mut config = Config::default();
        assert_eq!(widget_radius(&config), "9999px");

        config.widgets.border_radius = 25;
        assert_eq!(widget_radius(&config), "9px");

        config.widgets.border_radius = 0;
        assert_eq!(widget_radius(&config), "0px");
    }

    #[test]
    fn metrics_scale_with_the_bar_height() {
        assert_eq!(widget_height(36), 24);
        assert_eq!(font_size(36), 14);
        assert_eq!(dialog_font_size(36), 16);
        assert_eq!(launcher_font_size(36), 20);
        assert!(launcher_font_size(48) > launcher_font_size(36));
        assert!(dialog_font_size(48) > dialog_font_size(36));
        assert!(widget_height(48) > widget_height(36));
        assert!(font_size(48) > font_size(36));
    }

    #[test]
    fn standalone_dialogs_use_opaque_palette_surface() {
        for (mode, surface) in [("light", "#dce6f0"), ("dark", "#18212a")] {
            let mut config = Config::default();
            config.theme.mode = mode.into();
            config.theme.palette.surface = Some(surface.into());
            config.widgets.popover_background_opacity = Some(0.76);
            let css = generate(&config);

            assert!(css.contains(&format!("--color-dialog-surface: {surface};")));
            for selector in [".chooser-dialog", ".pinentry-dialog"] {
                assert!(css.contains(&format!(
                    "{selector} {{\n    background-color: var(--color-dialog-surface);"
                )));
            }
        }
    }

    #[test]
    fn modal_windows_leave_shadow_to_their_rounded_content() {
        let css = generate(&Config::default());
        for selector in [
            "window.launcher-window",
            "window.chooser-window",
            "window.pinentry-window",
        ] {
            assert!(
                css.contains(&format!("{selector},")) || css.contains(&format!("{selector} {{"))
            );
        }
        assert!(css.contains("--dialog-font-size: 16px;"));
        assert!(css.contains(".chooser-message,\n.chooser-subtitle,"));
    }

    #[test]
    fn text_entries_keep_the_caret_clear_of_their_edges_and_icons() {
        let css = generate(&Config::default());
        for (selector, padding) in [
            ("entry.launcher-search", "padding: 6px 20px;"),
            ("entry.chooser-search", "padding: 4px 14px;"),
            ("entry.pinentry-entry", "padding: 6px 14px;"),
        ] {
            let rule = css
                .split_once(&format!("{selector} {{"))
                .and_then(|(_, rest)| rest.split_once('}'))
                .map(|(rule, _)| rule)
                .expect("entry selector must have a CSS rule");
            assert!(rule.contains(padding), "{selector} lost its text inset");
        }

        let location_rule = css
            .split_once("entry.location-search,\nentry.location-coordinate {")
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(rule, _)| rule)
            .expect("location entry selector must have a CSS rule");
        assert!(location_rule.contains("padding: 4px 14px;"));
        assert!(css.contains(
            "entry.location-search > image.left,\nentry.chooser-search > image.left {\n    margin-right: 6px;"
        ));
    }

    #[test]
    fn shadows_are_light_and_use_the_compact_shared_rules() {
        let css = generate(&Config::default());
        assert!(css.contains("--color-popover-shadow: rgba(0, 0, 0, 0.28);"));
        assert!(css.contains("--shadow-popover: 0 1px 4px var(--color-popover-shadow);"));
        assert!(css.contains("--shadow-modal: 0 4px 14px var(--color-popover-shadow);"));

        for selector in [".location-dialog", ".chooser-dialog", ".pinentry-dialog"] {
            let rule = css
                .split_once(&format!("{selector} {{"))
                .and_then(|(_, rest)| rest.split_once('}'))
                .map(|(rule, _)| rule)
                .expect("modal selector must have a CSS rule");
            assert!(rule.contains("box-shadow: var(--shadow-modal);"));
        }
        for selector in [".popover-surface", ".toast", ".osd-capsule"] {
            let rule = css
                .split_once(&format!("{selector} {{"))
                .and_then(|(_, rest)| rest.split_once('}'))
                .map(|(rule, _)| rule)
                .expect("elevated selector must have a CSS rule");
            assert!(rule.contains("box-shadow: var(--shadow-popover);"));
        }
    }
}
