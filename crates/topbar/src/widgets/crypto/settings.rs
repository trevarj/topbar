//! The popover's second view: which prices to show, and in what order.
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ ←  Prices                                    │
//! │ Assets                                       │
//! │  ₿  Bitcoin              ↑  ↓        [ on  ] │
//! │  Ξ  Ethereum             ↑  ↓        [ on  ] │
//! │  ɱ  Monero               ↑  ↓        [ off ] │
//! │ Pairs                                        │
//! │  Ξ₿ ETH / BTC            ↑  ↓            ×   │
//! │  [ETH ▾]  /  [BTC ▾]                   Add   │
//! └──────────────────────────────────────────────┘
//! ```
//!
//! Every control applies immediately: a switch flipped or a pair added is a
//! [`set_entries`](topbar_services::CryptoHandle::set_entries) call, which
//! writes `state.json` and republishes, and the bar and the price view redraw
//! from that one snapshot. There is deliberately **no Save button** — see the
//! module comment on [`apply`].
//!
//! The list operations themselves are pure functions at the bottom of this
//! file, so what a click does is testable without a display.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Button, Label, Orientation, Switch};
use topbar_services::{Asset, Entry, Services};

use crate::style::classes;
use crate::surfaces::search::{ChoiceFilter, SearchSelector};
use crate::widgets::crypto::{emblem, icons};

/// Logo size on a settings row. A pair's two coins each take 5/8 of this.
const ROW_ICON: i32 = 20;
/// The button that goes back to the prices.
const BACK_ICON: &str = "go-previous-symbolic";
/// Move an entry one place towards the left of the bar.
const UP_ICON: &str = "go-up-symbolic";
/// Move it one place towards the right.
const DOWN_ICON: &str = "go-down-symbolic";
/// Take an entry off the bar.
const REMOVE_ICON: &str = "window-close-symbolic";

/// The settings view.
pub struct Settings {
    root: gtk4::Box,
    /// One per supported asset, in [`Asset::ALL`] order.
    assets: Vec<AssetRow>,
    _asset_filter: Rc<ChoiceFilter>,
    /// Where the pair rows are appended and removed.
    pairs: gtk4::Box,
    /// The pair rows currently in `pairs`.
    pair_rows: RefCell<Vec<PairRow>>,
    numerator: Rc<SearchSelector>,
    denominator: Rc<SearchSelector>,
    add: Button,
    /// The entries the view was last drawn from.
    entries: RefCell<Vec<Entry>>,
    /// Set while the view is writing its own controls, so the signals that
    /// fire do not read as the user having clicked something.
    updating: Rc<Cell<bool>>,
    services: Services,
}

impl Settings {
    /// Build the settings view. `back` is run when its back button is pressed.
    pub fn new(services: &Services, back: impl Fn() + 'static) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 6);
        root.add_css_class(classes::CRYPTO_SETTINGS);

        // --- header ---------------------------------------------------------
        let header = gtk4::Box::new(Orientation::Horizontal, 8);
        header.add_css_class(classes::CRYPTO_HEADER);

        let back_button = icon_button(BACK_ICON, classes::CRYPTO_BACK);
        back_button.set_tooltip_text(Some("Back to prices"));
        back_button.connect_clicked(move |_| back());

        let title = Label::new(Some("Prices"));
        title.add_css_class(classes::CARD_TITLE);
        title.set_xalign(0.0);
        title.set_hexpand(true);

        header.append(&back_button);
        header.append(&title);
        root.append(&header);

        // --- the three assets -----------------------------------------------
        root.append(&section_label("Assets"));
        let asset_scope = gtk4::Box::new(Orientation::Vertical, 2);
        let asset_filter = ChoiceFilter::new(&asset_scope, "Filter assets");
        root.append(&asset_scope);

        let updating = Rc::new(Cell::new(false));
        let assets: Vec<AssetRow> = Asset::ALL
            .iter()
            .map(|asset| {
                let row = AssetRow::new(*asset);
                row.root.add_css_class(classes::PICKER_OPTION);
                asset_scope.append(&row.root);
                asset_filter.add(
                    &row.root,
                    format!("{} {}", asset.name(), asset.symbol()),
                    None,
                );
                row
            })
            .collect();
        asset_filter.apply();

        // --- the pairs ------------------------------------------------------
        root.append(&section_label("Pairs"));

        let pairs = gtk4::Box::new(Orientation::Vertical, 2);
        root.append(&pairs);

        let add_row = gtk4::Box::new(Orientation::Horizontal, 6);
        add_row.add_css_class(classes::CRYPTO_ADD_PAIR);

        let names: Vec<&str> = Asset::ALL.iter().map(|asset| asset.symbol()).collect();
        let numerator = SearchSelector::new("Numerator asset", &names);
        let denominator = SearchSelector::new("Denominator asset", &names);
        // Ethereum over Bitcoin: the pair the user's own script printed, and
        // the one they are most likely to want a second of.
        numerator.set_selected(1);
        denominator.set_selected(0);

        let separator = Label::new(Some("/"));
        separator.add_css_class(classes::CRYPTO_PAIR_SLASH);

        let add = Button::with_label("Add");
        add.add_css_class(classes::DIALOG_BUTTON);
        add.set_halign(Align::End);
        add.set_hexpand(true);

        add_row.append(numerator.root());
        add_row.append(&separator);
        add_row.append(denominator.root());
        add_row.append(&add);
        root.append(&add_row);

        let settings = Rc::new(Self {
            root,
            assets,
            _asset_filter: asset_filter,
            pairs,
            pair_rows: RefCell::new(Vec::new()),
            numerator,
            denominator,
            add,
            entries: RefCell::new(Vec::new()),
            updating,
            services: services.clone(),
        });

        settings.connect();
        settings
    }

    /// The widget to put in the popover.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Wire every control to the entry list it edits.
    fn connect(self: &Rc<Self>) {
        for row in &self.assets {
            let asset = row.asset;

            row.switch.connect_active_notify({
                let weak = Rc::downgrade(self);
                move |switch| {
                    let Some(settings) = weak.upgrade() else {
                        return;
                    };
                    if settings.updating.get() {
                        return;
                    }
                    settings.apply(toggle_single(
                        &settings.entries.borrow(),
                        asset,
                        switch.is_active(),
                    ));
                }
            });

            row.up.connect_clicked(self.mover(Entry::Single(asset), -1));
            row.down
                .connect_clicked(self.mover(Entry::Single(asset), 1));
        }

        let refresh_add = {
            let weak = Rc::downgrade(self);
            move || {
                if let Some(settings) = weak.upgrade() {
                    settings.sync_add_button();
                }
            }
        };
        self.numerator.connect_selected({
            let refresh_add = refresh_add.clone();
            move |_| refresh_add()
        });
        self.denominator.connect_selected(move |_| refresh_add());

        self.add.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(settings) = weak.upgrade() else {
                    return;
                };
                let (base, quote) = settings.chosen_pair();
                settings.apply(add_pair(&settings.entries.borrow(), base, quote));
            }
        });
    }

    /// A click handler that moves `entry` by `delta` places.
    fn mover(self: &Rc<Self>, entry: Entry, delta: isize) -> impl Fn(&Button) + 'static {
        let weak = Rc::downgrade(self);
        move |_| {
            if let Some(settings) = weak.upgrade() {
                settings.apply(move_entry(&settings.entries.borrow(), entry, delta));
            }
        }
    }

    /// Which pair the two selectors are naming.
    fn chosen_pair(&self) -> (Asset, Asset) {
        let pick = |selector: &Rc<SearchSelector>| {
            Asset::ALL
                .get(selector.selected() as usize)
                .copied()
                .unwrap_or(Asset::Btc)
        };
        (pick(&self.numerator), pick(&self.denominator))
    }

    /// Send an edited list to the service, which persists and republishes it.
    ///
    /// The view does not mirror the change locally: it redraws when the new
    /// snapshot arrives, so what is on screen is always what was actually
    /// saved rather than what was optimistically assumed.
    fn apply(&self, entries: Vec<Entry>) {
        if *self.entries.borrow() == entries {
            return;
        }
        let handle = self.services.crypto.handle().clone();
        crate::bridge::act(
            crate::bridge::ActionScope::Toast { widget: "crypto" },
            async move { handle.set_entries(entries).await },
        );
    }

    /// Draw `entries`.
    pub fn render(self: &Rc<Self>, entries: &[Entry]) {
        self.entries.borrow_mut().clear();
        self.entries.borrow_mut().extend_from_slice(entries);

        // The switches are about to be written to; the signals that fire from
        // that are the view talking to itself, not the user.
        self.updating.set(true);
        for row in &self.assets {
            let entry = Entry::Single(row.asset);
            let position = entries.iter().position(|candidate| *candidate == entry);
            row.switch.set_active(position.is_some());
            set_movable(&row.up, &row.down, position, entries.len());
        }
        self.updating.set(false);

        let pairs: Vec<Entry> = entries
            .iter()
            .copied()
            .filter(|entry| matches!(entry, Entry::Pair(..)))
            .collect();
        self.sync_pair_rows(&pairs);
        for row in self.pair_rows.borrow().iter() {
            let position = entries.iter().position(|entry| *entry == row.entry);
            set_movable(&row.up, &row.down, position, entries.len());
        }

        self.sync_add_button();
    }

    /// Rebuild the pair rows if they are not the pairs in `pairs`.
    fn sync_pair_rows(self: &Rc<Self>, pairs: &[Entry]) {
        {
            let rows = self.pair_rows.borrow();
            if rows.len() == pairs.len()
                && rows
                    .iter()
                    .zip(pairs)
                    .all(|(row, entry)| row.entry == *entry)
            {
                return;
            }
        }

        let mut rows = self.pair_rows.borrow_mut();
        for row in rows.drain(..) {
            self.pairs.remove(&row.root);
        }
        for entry in pairs {
            let row = PairRow::new(*entry);
            row.up.connect_clicked(self.mover(*entry, -1));
            row.down.connect_clicked(self.mover(*entry, 1));
            row.remove.connect_clicked({
                let weak = Rc::downgrade(self);
                let entry = *entry;
                move |_| {
                    if let Some(settings) = weak.upgrade() {
                        settings.apply(remove(&settings.entries.borrow(), entry));
                    }
                }
            });
            self.pairs.append(&row.root);
            rows.push(row);
        }
    }

    /// Grey the Add button out for a pair that cannot be added.
    fn sync_add_button(&self) {
        let (base, quote) = self.chosen_pair();
        let entries = self.entries.borrow();
        self.add.set_sensitive(can_add(&entries, base, quote));
        self.add.set_tooltip_text(match (base == quote, ()) {
            (true, ()) => Some("A pair needs two different assets"),
            (false, ()) if !can_add(&entries, base, quote) => Some("That pair is already shown"),
            _ => None,
        });
    }
}

/// One of the three asset rows.
struct AssetRow {
    asset: Asset,
    root: gtk4::Box,
    switch: Switch,
    up: Button,
    down: Button,
}

impl AssetRow {
    fn new(asset: Asset) -> Self {
        let root = gtk4::Box::new(Orientation::Horizontal, 10);
        root.add_css_class(classes::CRYPTO_SETTING_ROW);

        let logo = icons::image(asset, ROW_ICON);
        logo.add_css_class(classes::CRYPTO_ICON);

        let name = Label::new(Some(asset.name()));
        name.set_xalign(0.0);
        name.set_hexpand(true);

        let up = icon_button(UP_ICON, classes::CRYPTO_REORDER);
        up.set_tooltip_text(Some("Move left"));
        let down = icon_button(DOWN_ICON, classes::CRYPTO_REORDER);
        down.set_tooltip_text(Some("Move right"));

        let switch = Switch::new();
        switch.set_valign(Align::Center);

        root.append(&logo);
        root.append(&name);
        root.append(&up);
        root.append(&down);
        root.append(&switch);

        Self {
            asset,
            root,
            switch,
            up,
            down,
        }
    }
}

/// One configured pair.
struct PairRow {
    entry: Entry,
    root: gtk4::Box,
    up: Button,
    down: Button,
    remove: Button,
}

impl PairRow {
    fn new(entry: Entry) -> Self {
        let root = gtk4::Box::new(Orientation::Horizontal, 10);
        root.add_css_class(classes::CRYPTO_SETTING_ROW);

        let name = Label::new(Some(&entry.label()));
        name.set_xalign(0.0);
        name.set_hexpand(true);

        let up = icon_button(UP_ICON, classes::CRYPTO_REORDER);
        up.set_tooltip_text(Some("Move left"));
        let down = icon_button(DOWN_ICON, classes::CRYPTO_REORDER);
        down.set_tooltip_text(Some("Move right"));
        let remove = icon_button(REMOVE_ICON, classes::CRYPTO_REMOVE);
        remove.set_tooltip_text(Some("Stop showing this pair"));

        root.append(&emblem(entry, ROW_ICON));
        root.append(&name);
        root.append(&up);
        root.append(&down);
        root.append(&remove);

        Self {
            entry,
            root,
            up,
            down,
            remove,
        }
    }
}

/// Enable or disable a row's arrows for its place in a list of `total`.
///
/// An entry that is not in the list at all has nowhere to move, and the ends
/// of the list have nowhere to move in one direction.
fn set_movable(up: &Button, down: &Button, position: Option<usize>, total: usize) {
    let Some(position) = position else {
        up.set_sensitive(false);
        down.set_sensitive(false);
        return;
    };
    up.set_sensitive(position > 0);
    down.set_sensitive(position + 1 < total);
}

/// A section heading inside the settings view.
fn section_label(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class(classes::CRYPTO_SECTION);
    label.set_xalign(0.0);
    label
}

/// A flat, round icon button.
fn icon_button(name: &str, class: &str) -> Button {
    let button = Button::new();
    button.add_css_class(class);
    button.set_child(Some(&gtk4::Image::from_icon_name(name)));
    button
}

// ---------------------------------------------------------------------------
// What each control does to the list — pure, so it can be tested without GTK
// ---------------------------------------------------------------------------

/// Turn one asset's dollar price on or off.
///
/// On appends: a newly enabled asset goes to the right-hand end of the bar,
/// where a new thing belongs, and the arrows are there to move it.
pub fn toggle_single(entries: &[Entry], asset: Asset, on: bool) -> Vec<Entry> {
    let entry = Entry::Single(asset);
    if on {
        return add(entries, entry);
    }
    remove(entries, entry)
}

/// Add a pair, if it is one that can be added.
pub fn add_pair(entries: &[Entry], base: Asset, quote: Asset) -> Vec<Entry> {
    if !can_add(entries, base, quote) {
        return entries.to_vec();
    }
    add(entries, Entry::Pair(base, quote))
}

/// Whether `base/quote` is a pair this list does not already have.
pub fn can_add(entries: &[Entry], base: Asset, quote: Asset) -> bool {
    base != quote && !entries.contains(&Entry::Pair(base, quote))
}

/// Append `entry`, unless it is already there.
fn add(entries: &[Entry], entry: Entry) -> Vec<Entry> {
    if entries.contains(&entry) {
        return entries.to_vec();
    }
    let mut entries = entries.to_vec();
    entries.push(entry);
    entries
}

/// Take `entry` off the list.
pub fn remove(entries: &[Entry], entry: Entry) -> Vec<Entry> {
    entries
        .iter()
        .copied()
        .filter(|candidate| *candidate != entry)
        .collect()
}

/// Move `entry` `delta` places, clamped to the ends of the list.
pub fn move_entry(entries: &[Entry], entry: Entry, delta: isize) -> Vec<Entry> {
    let mut entries = entries.to_vec();
    let Some(from) = entries.iter().position(|candidate| *candidate == entry) else {
        return entries;
    };
    let to = from
        .saturating_add_signed(delta)
        .min(entries.len().saturating_sub(1));
    if to == from {
        return entries;
    }
    let entry = entries.remove(from);
    entries.insert(to, entry);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        vec![
            Entry::Single(Asset::Btc),
            Entry::Single(Asset::Eth),
            Entry::Pair(Asset::Eth, Asset::Btc),
        ]
    }

    #[test]
    fn switching_an_asset_on_adds_it_to_the_end() {
        assert_eq!(
            toggle_single(&entries(), Asset::Xmr, true),
            vec![
                Entry::Single(Asset::Btc),
                Entry::Single(Asset::Eth),
                Entry::Pair(Asset::Eth, Asset::Btc),
                Entry::Single(Asset::Xmr),
            ]
        );
    }

    #[test]
    fn switching_an_asset_off_takes_only_its_own_entry() {
        assert_eq!(
            toggle_single(&entries(), Asset::Eth, false),
            vec![
                Entry::Single(Asset::Btc),
                Entry::Pair(Asset::Eth, Asset::Btc),
            ],
            "the pair is a different entry and stays"
        );
    }

    #[test]
    fn switching_something_on_twice_changes_nothing() {
        assert_eq!(toggle_single(&entries(), Asset::Btc, true), entries());
        assert_eq!(toggle_single(&entries(), Asset::Xmr, false), entries());
    }

    #[test]
    fn a_pair_can_only_be_added_once_and_never_with_itself() {
        assert!(can_add(&entries(), Asset::Xmr, Asset::Btc));
        assert!(!can_add(&entries(), Asset::Eth, Asset::Btc));
        assert!(!can_add(&entries(), Asset::Btc, Asset::Btc));

        assert_eq!(add_pair(&entries(), Asset::Eth, Asset::Btc), entries());
        assert_eq!(add_pair(&entries(), Asset::Btc, Asset::Btc), entries());
        assert_eq!(
            add_pair(&entries(), Asset::Xmr, Asset::Btc).len(),
            entries().len() + 1
        );
    }

    #[test]
    fn removing_takes_exactly_one_entry() {
        assert_eq!(
            remove(&entries(), Entry::Pair(Asset::Eth, Asset::Btc)),
            vec![Entry::Single(Asset::Btc), Entry::Single(Asset::Eth)]
        );
        assert_eq!(
            remove(&entries(), Entry::Pair(Asset::Xmr, Asset::Btc)),
            entries(),
            "removing what is not there is not an edit"
        );
    }

    #[test]
    fn an_entry_moves_one_place_at_a_time() {
        assert_eq!(
            move_entry(&entries(), Entry::Single(Asset::Eth), -1),
            vec![
                Entry::Single(Asset::Eth),
                Entry::Single(Asset::Btc),
                Entry::Pair(Asset::Eth, Asset::Btc),
            ]
        );
        assert_eq!(
            move_entry(&entries(), Entry::Single(Asset::Eth), 1),
            vec![
                Entry::Single(Asset::Btc),
                Entry::Pair(Asset::Eth, Asset::Btc),
                Entry::Single(Asset::Eth),
            ]
        );
    }

    #[test]
    fn the_ends_of_the_list_are_the_ends() {
        assert_eq!(
            move_entry(&entries(), Entry::Single(Asset::Btc), -1),
            entries()
        );
        assert_eq!(
            move_entry(&entries(), Entry::Pair(Asset::Eth, Asset::Btc), 1),
            entries()
        );
    }

    #[test]
    fn moving_something_that_is_not_shown_changes_nothing() {
        assert_eq!(
            move_entry(&entries(), Entry::Single(Asset::Xmr), -1),
            entries()
        );
        assert_eq!(move_entry(&[], Entry::Single(Asset::Btc), 1), Vec::new());
    }

    #[test]
    fn the_arrows_are_dead_at_the_ends_and_for_what_is_not_shown() {
        // Position, list length, then whether up and down should be live.
        let table = [
            (Some(0), 3, (false, true)),
            (Some(1), 3, (true, true)),
            (Some(2), 3, (true, false)),
            (Some(0), 1, (false, false)),
            (None, 3, (false, false)),
        ];
        for (position, total, (up, down)) in table {
            assert_eq!(
                (
                    position.is_some_and(|position| position > 0),
                    position.is_some_and(|position: usize| position + 1 < total),
                ),
                (up, down),
                "at {position:?} of {total}"
            );
        }
    }
}
