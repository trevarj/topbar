//! The power section: four rows, each of which has to be held.
//!
//! ```text
//! ┌────────────────────────────────┐
//! │████████░░░░░░  Suspend         │  ← the fill grows while it is held
//! ├────────────────────────────────┤
//! │                Restart         │
//! │                Shut Down       │
//! │                Log Out         │
//! └────────────────────────────────┘
//! ```
//!
//! Three of them go to logind over D-Bus. Logging out does not: under niri the
//! session *is* the compositor, and asking it to quit is both the correct call
//! and the one that lets it close cleanly.
//!
//! Every row reports inline. A refused shutdown — polkit wanting a password
//! the panel cannot ask for — belongs under the row that asked for it, not in
//! a banner over somebody's work.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Image, Label, Orientation, Overlay};
use topbar_services::{PowerAction, Services};

use crate::bridge::BindingGuard;
use crate::style::{classes, icons};
use crate::surfaces::inline::{self, names};
use crate::widgets::quick_settings::attempt;
use crate::widgets::quick_settings::hold::HoldRow;

/// One row in the section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// Sleep, through logind.
    Suspend,
    /// Restart, through logind.
    Restart,
    /// Power off, through logind.
    ShutDown,
    /// Quit the compositor, through niri.
    LogOut,
}

/// Top to bottom: least destructive first.
///
/// Suspend is the one people reach for daily and Log Out the one they reach
/// for by accident, so the order is also a guard: the row nearest the pointer
/// when the section opens is the one that costs least to get wrong.
pub const ROWS: &[Row] = &[Row::Suspend, Row::Restart, Row::ShutDown, Row::LogOut];

impl Row {
    /// The label on the row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Suspend => "Suspend",
            Self::Restart => "Restart",
            Self::ShutDown => "Shut Down",
            Self::LogOut => "Log Out",
        }
    }

    /// The inline slot it reports into.
    pub fn slot(self) -> &'static str {
        match self {
            Self::Suspend => names::SUSPEND,
            Self::Restart => names::RESTART,
            Self::ShutDown => names::SHUT_DOWN,
            Self::LogOut => names::LOG_OUT,
        }
    }

    /// The logind action, where there is one.
    pub fn action(self) -> Option<PowerAction> {
        match self {
            Self::Suspend => Some(PowerAction::Suspend),
            Self::Restart => Some(PowerAction::Restart),
            Self::ShutDown => Some(PowerAction::ShutDown),
            // The compositor's business, not logind's.
            Self::LogOut => None,
        }
    }

    /// The symbolic icon for it.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Suspend => icons::first_available(icons::SUSPEND),
            Self::Restart => icons::RESTART,
            Self::ShutDown => icons::SHUT_DOWN,
            Self::LogOut => icons::LOG_OUT,
        }
    }
}

/// The power section.
pub struct PowerSection {
    root: gtk4::Box,
    /// One per row, keeping its gestures and its frame-clock callback alive.
    holds: Vec<Rc<HoldRow>>,
    /// The row surfaces, in [`ROWS`] order, for the smoke hook.
    ///
    /// Kept only in a debug build: the packaged panel has no way to paint a
    /// hold it is not performing, and no reason to carry the vectors that
    /// would let it.
    #[cfg(debug_assertions)]
    overlays: Vec<Overlay>,
    /// Their fills, likewise.
    #[cfg(debug_assertions)]
    fills: Vec<gtk4::Box>,
    _slots: Vec<inline::InlineSlot>,
    _bindings: Vec<BindingGuard>,
}

impl PowerSection {
    /// Build the section.
    pub fn new(services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 4);
        let mut holds = Vec::new();
        let mut slots = Vec::new();
        #[cfg(debug_assertions)]
        let mut overlays = Vec::new();
        #[cfg(debug_assertions)]
        let mut fills = Vec::new();

        for row in ROWS {
            let built = build_row(*row, services);
            root.append(&built.widget);
            holds.push(built.hold);
            slots.push(built.slot);
            #[cfg(debug_assertions)]
            {
                overlays.push(built.overlay);
                fills.push(built.fill);
            }
        }

        Rc::new(Self {
            root,
            holds,
            #[cfg(debug_assertions)]
            overlays,
            #[cfg(debug_assertions)]
            fills,
            _slots: slots,
            _bindings: Vec::new(),
        })
    }

    /// The widget to put in the panel.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Cancel any hold in progress.
    ///
    /// Runs when the panel closes: a row left half-filled because the popover
    /// was dismissed mid-press must not still be counting down.
    pub fn cancel_holds(&self) {
        for hold in &self.holds {
            hold.cancel();
        }
    }

    /// Start a hold on `row` without a pointer.
    ///
    /// The smoke run has no synthetic input, so this is how a photograph of a
    /// half-filled row gets taken. Debug builds reach it through
    /// `TOPBAR_SMOKE_OPEN`; nothing else calls it.
    #[cfg(debug_assertions)]
    pub fn begin_hold(&self, row: Row) {
        if let Some(hold) = self.row(row) {
            hold.begin();
        }
    }

    /// How far through a hold on `row` is, `0.0..=1.0`.
    ///
    /// Read by the same smoke hook, so the log can say the fill really was
    /// part-way across when the screenshot was taken rather than leaving that
    /// to whoever looks at the picture.
    #[cfg(debug_assertions)]
    pub fn hold_progress(&self, row: Row) -> f64 {
        self.row(row).map_or(0.0, |hold| hold.state().progress())
    }

    /// Paint `row` as though it were `fraction` of the way through a hold,
    /// without starting one.
    ///
    /// The smoke run cannot photograph a real hold: the capture waits for two
    /// identical frames and a fill that is moving never gives it one, while a
    /// hold left running to the end would reach `logind` on the **system**
    /// bus — the developer's own. So the fill is painted at a fixed fraction
    /// instead. Everything on screen is the real thing: the real overlay, the
    /// real accent, the real width arithmetic. The only thing that is not
    /// happening is the countdown.
    #[cfg(debug_assertions)]
    pub fn paint_hold(&self, row: Row, fraction: f64) {
        let Some(index) = ROWS.iter().position(|candidate| *candidate == row) else {
            return;
        };
        let Some(overlay) = self.overlays.get(index) else {
            return;
        };
        let Some(fill) = self.fills.get(index) else {
            return;
        };
        let width = (f64::from(overlay.width()) * fraction.clamp(0.0, 1.0)).round() as i32;
        fill.set_size_request(width, -1);
    }

    /// The hold belonging to `row`.
    #[cfg(debug_assertions)]
    fn row(&self, row: Row) -> Option<&Rc<HoldRow>> {
        let index = ROWS.iter().position(|candidate| *candidate == row)?;
        self.holds.get(index)
    }
}

/// One built row and everything that has to outlive it.
struct BuiltRow {
    widget: gtk4::Box,
    /// The row surface and its fill, which only the smoke hook keeps.
    #[cfg(debug_assertions)]
    overlay: Overlay,
    #[cfg(debug_assertions)]
    fill: gtk4::Box,
    hold: Rc<HoldRow>,
    slot: inline::InlineSlot,
}

/// Build one row: the fill behind it, the content on top, and the gesture.
fn build_row(row: Row, services: &Services) -> BuiltRow {
    let column = gtk4::Box::new(Orientation::Vertical, 0);

    let overlay = Overlay::new();
    overlay.add_css_class(classes::QS_POWER_ROW);
    // The fill has to be clipped to the row's rounded corners, or it paints
    // square ends over them as it reaches the edge.
    overlay.set_overflow(gtk4::Overflow::Hidden);

    let fill = gtk4::Box::new(Orientation::Horizontal, 0);
    fill.add_css_class(classes::QS_POWER_FILL);
    fill.set_halign(Align::Start);
    fill.set_valign(Align::Fill);
    fill.set_size_request(0, -1);
    overlay.set_child(Some(&fill));

    let content = gtk4::Box::new(Orientation::Horizontal, 8);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(10);
    content.set_margin_bottom(10);

    let icon = Image::from_icon_name(row.icon());
    icon.add_css_class(classes::QS_ICON);
    content.append(&icon);

    let label = Label::new(Some(row.label()));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    content.append(&label);

    overlay.add_overlay(&content);
    overlay.set_measure_overlay(&content, true);
    // Focusable so Enter and space can hold it: a power menu that only worked
    // with a pointer would be one nobody could use from the keyboard.
    overlay.set_focusable(true);

    column.append(&overlay);
    let (error, slot) = inline::slot(row.slot());
    column.append(&error);

    let hold = HoldRow::attach(&overlay, &fill, {
        let services = services.clone();
        move || fire(row, &services)
    });

    BuiltRow {
        widget: column,
        #[cfg(debug_assertions)]
        overlay,
        #[cfg(debug_assertions)]
        fill,
        hold,
        slot,
    }
}

/// A hold completed: do the thing it was holding for.
fn fire(row: Row, services: &Services) {
    match row.action() {
        Some(action) => {
            let power = services.power.clone();
            attempt(row.slot(), async move { power.act(action).await });
        }
        None => {
            let compositor = services.compositor.handle();
            attempt(
                row.slot(),
                async move { compositor.quit_compositor().await },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_run_from_least_to_most_destructive() {
        assert_eq!(
            ROWS,
            &[Row::Suspend, Row::Restart, Row::ShutDown, Row::LogOut]
        );
    }

    #[test]
    fn three_rows_go_to_logind_and_logging_out_goes_to_the_compositor() {
        assert_eq!(Row::Suspend.action(), Some(PowerAction::Suspend));
        assert_eq!(Row::Restart.action(), Some(PowerAction::Restart));
        assert_eq!(Row::ShutDown.action(), Some(PowerAction::ShutDown));
        assert_eq!(
            Row::LogOut.action(),
            None,
            "under niri, logging out is the compositor quitting"
        );
    }

    #[test]
    fn every_row_has_a_slot_of_its_own_to_report_into() {
        let slots: Vec<&str> = ROWS.iter().map(|row| row.slot()).collect();
        let unique: std::collections::BTreeSet<&&str> = slots.iter().collect();
        assert_eq!(
            unique.len(),
            slots.len(),
            "two rows sharing a slot would show each other's failures"
        );
    }

    #[test]
    fn every_row_is_labelled_and_iconed() {
        for row in ROWS {
            assert!(!row.label().is_empty());
            assert!(row.icon().ends_with("-symbolic"));
        }
    }
}
