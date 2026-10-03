//! Where the panel's controls are, so a smoke run can click them.
//!
//! A pointer-driven driver holding a table of coordinates measured off a
//! screenshot starts clicking empty space the first time a padding changes —
//! which looks exactly like the dead-control bugs those runs exist to catch. So
//! the panel says where its controls are and the driver reads it back out of
//! the log.
//!
//! Quick Settings has said this since M9 and said it about itself. Everything
//! else the panel puts on a layer surface — the control panel, the banners, the
//! capsule — had no way to say it at all, so [`surfaces`] walks every mapped
//! layer window instead of one named panel. The line format is the same either
//! way, only the prefix differs, because the readers are the same six lines of
//! Python in two drivers.
//!
//! Debug builds only: nothing here is compiled into a packaged panel.

use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, LayerShell};

use crate::style::classes;
use crate::surfaces::{layer_popover, popovers};

/// Classes worth a line even though the widget is not a control.
///
/// A driver has to be able to *point* at things it cannot press: a banner it
/// hovers to stall the timer, a history row it hovers to reveal the close
/// button, the empty state it measures the centring of. The power rows are here
/// because they are overlays with a gesture on them rather than buttons.
///
/// The unread dot is here for a different reason again: it is not a control at
/// all, but it is the one thing on the bar that answers "is there anything in
/// the panel", and it is only answerable *before* the panel is opened, because
/// opening it is what clears the dot. [`tree`] skips invisible widgets, so a
/// line for it in a dump is the assertion.
const LOCATABLE: &[&str] = &[
    classes::WIDGET,
    classes::QS_POWER_ROW,
    classes::TOAST,
    classes::NOTIFICATION_ROW,
    classes::NOTIFICATION_GROUP,
    classes::EMPTY_STATE,
    classes::CLOCK_UNSEEN,
    classes::PICKER_OPTION,
    classes::QS_DEVICE_ROW,
    classes::TRAY_MENU_BACK,
    classes::CHOOSER_EMPTY,
];

/// Register `topbar popover show surface-dump`.
///
/// One action for every surface at once rather than one per widget: what a
/// driver wants after a click is "where is everything now", and a banner that
/// arrived while a panel was open belongs in the same answer as the panel.
pub fn install() {
    popovers::register_smoke_action("surface-dump", || {
        // A marker first: the reader wants the *last* dump, and the block it
        // is about to parse has to be a whole one.
        tracing::info!("ui-dump: begin");
        surfaces();
        tracing::info!("ui-dump: end");
    });
}

/// Log every control on every layer surface the panel currently has mapped.
fn surfaces() {
    // By index rather than through the typed iterator: GTK reports the
    // toplevel list's item type as `GtkWidget`, and asking gio for `GtkWindow`s
    // out of it asserts.
    let toplevels = gtk4::Window::toplevels();
    for index in 0..toplevels.n_items() {
        let Some(window) = toplevels
            .item(index)
            .and_then(|object| object.downcast::<gtk4::Window>().ok())
        else {
            continue;
        };
        if !LayerShell::is_layer_window(&window) || !window.is_visible() {
            continue;
        }
        window_tree(&window, "ui-dump");
    }
}

/// Dump one standalone chooser too; it deliberately has no panel IPC service.
pub fn window_tree(window: &gtk4::Window, prefix: &str) {
    let (origin_x, origin_y) = origin(window);
    tracing::info!(
        "{prefix}: surface \"{}\" {origin_x} {origin_y} {} {}",
        LayerShell::namespace(window).unwrap_or_default(),
        window.width(),
        window.height(),
    );
    tree(
        &window.clone().upcast(),
        window,
        (origin_x, origin_y),
        prefix,
    );
}

/// Where `window` sits on the monitor, in logical pixels.
///
/// Worked out from the layer-shell state rather than asked for, because there
/// is nothing to ask: a layer surface has no position, only anchors and
/// margins, and the compositor resolves them. Three cases cover every surface
/// the panel has — anchored to an edge (the popovers, top-left), anchored to
/// one edge of an axis and therefore centred on the other (the banners and the
/// capsule), centred on both axes (the dialogs), and stretched across both (the
/// click-catcher). Zero-zone surfaces centre within the area below the bar.
fn origin(window: &gtk4::Window) -> (i32, i32) {
    let (monitor_width, monitor_height) = monitor_size(window);
    // Only a surface that asks for a zone of exactly zero is pushed below
    // everyone else's: that is what "zero" means in layer-shell. A negative
    // zone ignores them, and a positive one belongs to the surface that
    // *reserved* the space — the bar, which is at the edge itself and would
    // otherwise be reported one bar height below where it is.
    let reserved = if LayerShell::exclusive_zone(window) == 0 {
        layer_popover::bar_height()
    } else {
        0
    };

    let x = match (
        LayerShell::is_anchor(window, Edge::Left),
        LayerShell::is_anchor(window, Edge::Right),
    ) {
        (true, false) => LayerShell::margin(window, Edge::Left),
        (false, true) => monitor_width - window.width() - LayerShell::margin(window, Edge::Right),
        _ => (monitor_width - window.width()) / 2,
    };
    let y = match (
        LayerShell::is_anchor(window, Edge::Top),
        LayerShell::is_anchor(window, Edge::Bottom),
    ) {
        (false, true) => {
            monitor_height - window.height() - LayerShell::margin(window, Edge::Bottom)
        }
        (false, false) => reserved + (monitor_height - reserved - window.height()) / 2,
        _ => reserved + LayerShell::margin(window, Edge::Top),
    };
    (x, y)
}

/// The monitor `window` is on, in logical pixels.
fn monitor_size(window: &gtk4::Window) -> (i32, i32) {
    let geometry = window
        .surface()
        .and_then(|surface| {
            gtk4::gdk::Display::default().and_then(|display| display.monitor_at_surface(&surface))
        })
        .map(|monitor| monitor.geometry());
    match geometry {
        Some(geometry) => (geometry.width(), geometry.height()),
        None => (0, 0),
    }
}

/// Walk `widget` and its children, logging every control's screen rectangle.
///
/// Only the things a driver can act on are worth a line: it is looking for "the
/// Wi-Fi chevron", not the four boxes that chevron is nested in.
pub fn tree(widget: &gtk4::Widget, window: &gtk4::Window, origin: (i32, i32), prefix: &str) {
    if !widget.is_visible() {
        return;
    }
    let interactive = widget.is::<gtk4::Button>()
        || widget.is::<gtk4::Scale>()
        || widget.is::<gtk4::Expander>()
        || widget.is::<gtk4::Switch>()
        || widget.is::<gtk4::MenuButton>()
        || widget.is::<gtk4::Editable>()
        || LOCATABLE.iter().any(|class| widget.has_css_class(class));
    if interactive && let Some(bounds) = widget.compute_bounds(window) {
        // The text on it as well as the classes it wears: four pills in the
        // grid are the same widget with the same classes, and a driver that
        // could only say "the fourth one" would click the wrong control the
        // first time a machine turned out to have no VPN profiles. Every line
        // of it, because a notification row is a summary, an age and a body.
        tracing::info!(
            "{prefix}: {} [{}] \"{}\" {} {} {} {} sensitive={} focused={}",
            widget.type_().name(),
            widget.css_classes().join("."),
            labels_of(widget).join(" · "),
            origin.0 + bounds.x().round() as i32,
            origin.1 + bounds.y().round() as i32,
            bounds.width().round() as i32,
            bounds.height().round() as i32,
            widget.is_sensitive(),
            crate::surfaces::search::focused(widget),
        );
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        tree(&current, window, origin, prefix);
        child = current.next_sibling();
    }
}

/// Every visible label under `widget`, which is what a control is called.
fn labels_of(widget: &gtk4::Widget) -> Vec<String> {
    if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
        return vec![label.text().to_string()];
    }
    if let Some(entry) = widget.downcast_ref::<gtk4::SearchEntry>() {
        return vec![
            entry.placeholder_text().unwrap_or_default().to_string(),
            entry.text().to_string(),
        ];
    }
    if let Some(entry) = widget.downcast_ref::<gtk4::Entry>()
        && [
            classes::CHOOSER_SEARCH,
            classes::LAUNCHER_SEARCH,
            classes::LOCATION_SEARCH,
        ]
        .iter()
        .any(|class| widget.has_css_class(class))
    {
        return vec![
            entry.placeholder_text().unwrap_or_default().to_string(),
            entry.text().to_string(),
        ];
    }
    let mut found = Vec::new();
    let mut child = widget.first_child();
    while let Some(current) = child {
        if current.is_visible() {
            match current.clone().downcast::<gtk4::Label>() {
                Ok(label) => found.push(label.text().to_string()),
                Err(_) => found.extend(labels_of(&current)),
            }
        }
        child = current.next_sibling();
    }
    if found.is_empty()
        && widget.is::<gtk4::Button>()
        && let Some(tooltip) = widget.tooltip_text()
    {
        found.push(tooltip.to_string());
    }
    found
}
