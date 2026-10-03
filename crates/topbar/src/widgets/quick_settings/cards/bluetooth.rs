//! The Bluetooth device list, the devices a scan found, and the pairing row
//! that opens inside either of them.
//!
//! ```text
//! ┌───────────────────┬───────────────────┐
//! │ ᛒ WH-1000XM4  ⌄ │ …                 │   the pill in the grid
//! └───────────────────┴───────────────────┘
//!   🎧 WH-1000XM4        85%    [ ●──]     ← connected, with its battery
//!   🖱 MX Master                 [──○]
//!   ⌨  Magic Keyboard           [ ◜◝ ]     ← connecting
//!   Available devices                ◜◝    ← header, with the scan spinner
//!   📱 Pixel 8                             ← the whole row pairs
//!     Confirm this code matches on Pixel 8
//!     ┌────────┐
//!     │ 000042 │   Cancel   Confirm       ← the same row either way
//!     └────────┘
//! ```
//!
//! **Two kinds of row, each with one meaning.** A paired device is a box with a
//! switch in it: the switch is the control and the row itself does nothing. A
//! device a scan found is a *button* with no switch: the whole row is the
//! control, and clicking it pairs. Neither line is ever two things at once —
//! that was the reason the paired row is not a button, and it is the reason the
//! found row is nothing but one.
//!
//! Rows are **rebuilt only when the list changes shape**. A snapshot arrives
//! whenever a battery level moves, which for a connected headset is every
//! minute or so; tearing the rows down and building them again at that rate
//! would make a switch flip under the pointer.
//!
//! The switches are pessimistic. `Connect` on a device that is switched off
//! takes BlueZ the better part of ten seconds to give up on, so the row spins
//! for exactly that long and the switch does not move until BlueZ says the
//! device did. A switch that flipped and flipped back eight seconds later would
//! be a control that lied for eight seconds.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Button, Image, Label, Orientation, Spinner, Switch};
use topbar_services::{BtDevice, BtState, Services};

use crate::anim::ripple;
use crate::style::{classes, icons};
use crate::surfaces::inline::names;
use crate::surfaces::search::ChoiceFilter;
use crate::widgets::quick_settings::{attempt, set_text};

/// Gap between the parts of a row.
const GAP: i32 = 10;

/// Whether the "Available devices" group is drawn at all.
///
/// While the list is open, always — a header with a spinner and nothing under
/// it is how a scan says it is still looking. And afterwards for as long as
/// something unpaired is still in the list, which is what a pairing that
/// outlived the scan that found it looks like.
fn shows_found(state: &BtState) -> bool {
    state.browsing || state.devices.iter().any(|device| !device.paired)
}

/// What the list looks like right now, as one comparable string.
///
/// The battery percentage is deliberately **not** in it: it is drawn into a
/// label the render pass already updates, and putting it here would rebuild
/// every row each time a headset reported one point less. Neither is
/// [`BtState::scanning`], which only turns a spinner that is already there.
fn signature(state: &BtState) -> String {
    let mut signature = format!(
        "{}:{}:{}:{}",
        state.available,
        state.powered,
        state.powering,
        shows_found(state)
    );
    for device in &state.devices {
        signature.push('\u{1}');
        signature.push_str(&format!(
            "{}\u{2}{}\u{2}{}\u{2}{}\u{2}{}\u{2}{}",
            device.path,
            device.alias,
            icons::bluetooth_device(device.icon),
            // A device that has just been paired stops being a button and
            // becomes a row with a switch, which is a change of shape.
            device.paired,
            device.connected,
            device.pending
        ));
    }
    // A prompt arriving or leaving moves the row it is attached to.
    match &state.prompt {
        Some(prompt) => {
            signature.push('\u{1}');
            signature.push_str(&prompt.path);
        }
        None => signature.push('\u{1}'),
    }
    signature
}

/// The parts of one row that change without the list changing shape.
struct Row {
    path: String,
    /// The row itself, so the pairing box can be moved underneath it.
    line: gtk4::Widget,
    /// The battery level, on a paired row.
    battery: Option<Label>,
    /// The connect switch — `None` on a row that is something to pair rather
    /// than something to switch, where the whole line is the control.
    switch: Option<Switch>,
    spinner: Spinner,
    /// Raised while the render path is writing into the switch, so the
    /// `state-set` it causes is not mistaken for the user flipping it.
    updating: std::cell::Cell<bool>,
}

/// The device list: what is paired, and what a scan turned up.
pub struct BluetoothList {
    root: gtk4::Box,
    list: gtk4::Box,
    filter: Rc<ChoiceFilter>,
    /// "Available devices", with the scan spinner in it. Re-parented into the
    /// list on a rebuild, between the paired rows and the found ones.
    header: gtk4::Box,
    scanning: Spinner,
    /// "Looking for devices…", under a header with nothing beneath it yet.
    searching: Label,
    /// "Bluetooth is off", where the rows would be.
    off: Label,
    /// "No devices are paired", likewise.
    empty: Label,
    rows: RefCell<Vec<Rc<Row>>>,
    /// What the rows were last built from.
    built: RefCell<String>,
    /// The pairing box, re-parented under whichever row wants it.
    pairing: Rc<PairingBox>,
    services: Services,
}

impl BluetoothList {
    /// Build the list.
    pub fn new(services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.add_css_class(classes::QS_DEVICE_LIST);
        let filter = ChoiceFilter::new(&root, "Filter Bluetooth devices");

        let list = gtk4::Box::new(Orientation::Vertical, 2);
        root.append(&list);

        // Built once and moved about, the same way the pairing box is: the
        // header belongs *between* two groups of rows the list rebuilds, and
        // rebuilding it alongside them would throw away a turning spinner.
        let header = gtk4::Box::new(Orientation::Horizontal, GAP);
        header.add_css_class(classes::QS_LIST_HEADER);
        let title = Label::new(Some("Available devices"));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        header.append(&title);
        let scanning = Spinner::new();
        scanning.set_visible(false);
        header.append(&scanning);

        // Its text is written on every render: a burst that ran out having
        // found nothing has to stop saying it is still looking.
        let searching = Label::new(None);
        searching.add_css_class(classes::QS_HINT);
        searching.set_xalign(0.0);

        // A radio that is off has no rows to show, and says why rather than
        // being a gap the user has to work out the meaning of.
        let off = Label::new(Some("Bluetooth is off"));
        off.add_css_class(classes::QS_HINT);
        off.set_xalign(0.0);
        off.set_visible(false);
        root.append(&off);

        let empty = Label::new(Some("No devices are paired"));
        empty.add_css_class(classes::QS_HINT);
        empty.set_xalign(0.0);
        empty.set_visible(false);
        root.append(&empty);

        let pairing = PairingBox::new(services);

        Rc::new(Self {
            root,
            list,
            filter,
            header,
            scanning,
            searching,
            off,
            empty,
            rows: RefCell::new(Vec::new()),
            built: RefCell::new(String::new()),
            pairing,
            services: services.clone(),
        })
    }

    /// The widget to put in the section.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Draw the list from `state`.
    pub fn render(self: &Rc<Self>, state: &BtState) {
        let signature = signature(state);
        if *self.built.borrow() != signature {
            self.rebuild(state);
            *self.built.borrow_mut() = signature;
        }

        for (row, device) in self.rows.borrow().iter().zip(&state.devices) {
            debug_assert_eq!(row.path, device.path);
            if let Some(battery) = &row.battery {
                render_battery(battery, device);
            }
            row.spinner
                .set_opacity(if device.pending { 1.0 } else { 0.0 });
            if device.pending {
                row.spinner.start();
            } else {
                row.spinner.stop();
            }
            // Insensitive rather than gone while BlueZ decides: the control
            // stays where it was, says it is not taking another press, and
            // comes back the moment the answer lands. On a found row the
            // *whole row* is the control, so that is what goes quiet.
            let live = state.powered && !device.pending;
            let Some(switch) = &row.switch else {
                row.line.set_sensitive(live);
                continue;
            };
            // The switch is only touched when it disagrees, and the guard stops
            // the write being read back as the user flipping it.
            if switch.is_active() != device.connected {
                row.updating.set(true);
                switch.set_active(device.connected);
                row.updating.set(false);
            }
            // `state` is the *confirmed* half of a GtkSwitch, and the handler
            // below deliberately refuses to move it. This is the other end of
            // that contract: BlueZ said what the device is doing, so the switch
            // is told. Without it the two halves disagree for ever and the
            // switch draws in its in-between look.
            switch.set_state(device.connected);
            switch.set_sensitive(live);
        }

        self.scanning.set_visible(state.scanning);
        if state.scanning {
            self.scanning.start();
        } else {
            self.scanning.stop();
        }
        // The burst is bounded, so "looking" is a thing the list stops being.
        set_text(
            &self.searching,
            if state.scanning {
                "Looking for devices…"
            } else {
                "No devices found"
            },
        );

        self.list.set_visible(state.powered);
        self.off.set_visible(!state.powered);
        // Only about the *paired* group: a scan's own emptiness is said under
        // its own header, and two "there is nothing here" lines one above the
        // other would be the panel saying it twice.
        self.empty.set_visible(
            state.powered
                && state.devices.iter().all(|device| !device.paired)
                && !shows_found(state),
        );
        self.pairing.render(state, self);
        self.filter.set_visible(state.powered);
        self.filter
            .pin(state.prompt.as_ref().map(|prompt| prompt.path.as_str()));
    }

    /// Rebuild every row.
    fn rebuild(self: &Rc<Self>, state: &BtState) {
        // Both of these are re-parented into the list, so they have to come out
        // before the rows around them are dropped — GTK asserts otherwise.
        self.pairing.detach();
        detach(&self.header);
        detach(&self.searching);

        self.filter.clear_rows();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let mut rows = Vec::new();
        let found = shows_found(state);
        let mut headed = false;
        for device in &state.devices {
            // The header goes in front of the first unpaired row, which is why
            // `order` guarantees the unpaired ones are a contiguous run at the
            // end rather than a flag scattered through the list.
            if found && !headed && !device.paired {
                self.list.append(&self.header);
                headed = true;
            }
            let (widget, row) = self.row(device);
            self.list.append(&widget);
            let address = device
                .path
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches("dev_");
            let mut text = String::with_capacity(device.alias.len() + 1 + address.len());
            text.push_str(&device.alias);
            text.push(' ');
            text.extend(
                address
                    .chars()
                    .map(|character| if character == '_' { ':' } else { character }),
            );
            self.filter.add(&widget, text, Some(&device.path));
            rows.push(row);
        }
        if found && !headed {
            // Nothing turned up yet. The header and the line under it are what
            // say the scan is running rather than finished.
            self.list.append(&self.header);
            self.list.append(&self.searching);
        }
        *self.rows.borrow_mut() = rows;
    }

    /// One device's row: a switch for what is paired, a button for what is not.
    fn row(self: &Rc<Self>, device: &BtDevice) -> (gtk4::Widget, Rc<Row>) {
        if device.paired {
            self.paired_row(device)
        } else {
            self.found_row(device)
        }
    }

    /// A device a scan found: the whole row pairs with it.
    fn found_row(self: &Rc<Self>, device: &BtDevice) -> (gtk4::Widget, Rc<Row>) {
        let button = Button::new();
        button.add_css_class(classes::QS_PAIR_ROW);

        let line = gtk4::Box::new(Orientation::Horizontal, GAP);
        line.set_valign(Align::Center);

        let icon = Image::from_icon_name(icons::bluetooth_device(device.icon));
        icon.add_css_class(classes::QS_ICON);
        line.append(&icon);

        let name = Label::new(Some(&device.alias));
        name.add_css_class(classes::QS_DEVICE_NAME);
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        line.append(&name);

        // Always measured, only its opacity moves — the same rule the paired
        // row's spinner follows, and for the same reason: a pairing takes as
        // long as the user takes to look at the other screen, and a row that
        // changed width when it started would twitch under the pointer.
        let spinner = Spinner::new();
        spinner.set_opacity(0.0);
        line.append(&spinner);

        button.set_child(Some(&line));
        ripple::install(&button);
        button.connect_clicked({
            let list = Rc::downgrade(self);
            let path = device.path.clone();
            move |_| {
                let Some(list) = list.upgrade() else { return };
                let bluetooth = list.services.bluetooth.handle().clone();
                let path = path.clone();
                // Answers when the pair, the trust and the connect have all
                // finished, which for a device that wants a code confirmed is
                // however long the user takes — the row spins throughout, and
                // the confirmation opens underneath it.
                attempt(names::BLUETOOTH, async move { bluetooth.pair(path).await });
            }
        });

        let row = Rc::new(Row {
            path: device.path.clone(),
            line: button.clone().upcast(),
            battery: None,
            switch: None,
            spinner,
            updating: std::cell::Cell::new(false),
        });
        (button.upcast(), row)
    }

    /// A paired device's row.
    fn paired_row(self: &Rc<Self>, device: &BtDevice) -> (gtk4::Widget, Rc<Row>) {
        // A box rather than a button: the switch is the control, and a row that
        // was *also* clickable would give one line two different meanings.
        let line = gtk4::Box::new(Orientation::Horizontal, GAP);
        line.add_css_class(classes::QS_DEVICE_ROW);
        line.set_valign(Align::Center);

        let icon = Image::from_icon_name(icons::bluetooth_device(device.icon));
        icon.add_css_class(classes::QS_ICON);
        line.append(&icon);

        let name = Label::new(Some(&device.alias));
        name.add_css_class(classes::QS_DEVICE_NAME);
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        line.append(&name);

        let battery = Label::new(None);
        battery.add_css_class(classes::QS_DEVICE_BATTERY);
        battery.set_visible(false);
        line.append(&battery);

        // The spinner sits beside the switch and is always measured — only its
        // opacity moves. It used to take the switch's place, and swapping a
        // 44px switch for a 16px spinner shortened the row's right-hand side by
        // the difference: the battery percentage jumped sideways, and the
        // control the user had just flipped disappeared for as long as BlueZ
        // took to answer, which for a headset that is out of range is ten
        // seconds of a row with nothing on it.
        let spinner = Spinner::new();
        spinner.set_opacity(0.0);
        spinner.set_valign(Align::Center);
        line.append(&spinner);

        let switch = Switch::new();
        switch.add_css_class(classes::QS_DEVICE_SWITCH);
        switch.set_valign(Align::Center);
        line.append(&switch);

        let row = Rc::new(Row {
            path: device.path.clone(),
            line: line.clone().upcast(),
            battery: Some(battery),
            switch: Some(switch.clone()),
            spinner,
            updating: std::cell::Cell::new(false),
        });
        if let Some(battery) = &row.battery {
            render_battery(battery, device);
        }

        switch.connect_state_set({
            let list = Rc::downgrade(self);
            let row = Rc::downgrade(&row);
            move |_, wanted| {
                let (Some(list), Some(row)) = (list.upgrade(), row.upgrade()) else {
                    return gtk4::glib::Propagation::Proceed;
                };
                if row.updating.get() {
                    return gtk4::glib::Propagation::Proceed;
                }
                let bluetooth = list.services.bluetooth.handle().clone();
                let path = row.path.clone();
                attempt(names::BLUETOOTH, async move {
                    if wanted {
                        bluetooth.connect(path).await
                    } else {
                        bluetooth.disconnect(path).await
                    }
                });
                // Refusing the default handler is what makes this switch
                // *asynchronous*: GTK leaves `state` where it was, and the
                // render pass sets it once BlueZ has said what the device is
                // actually doing. That is what makes a failed connect revert
                // rather than lie for the ten seconds BlueZ takes to give up.
                gtk4::glib::Propagation::Stop
            }
        });

        (line.upcast(), row)
    }

    /// Put the pairing box under the row for `path`, or at the end.
    fn place_pairing(&self, path: Option<&str>) {
        let Some(path) = path else {
            self.pairing.detach();
            return;
        };
        let after = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.path == path)
            .map(|row| row.line.clone());
        self.pairing.attach(&self.list, after.as_ref());
    }
}

/// Take a widget out of whatever box it is in.
fn detach(child: &impl IsA<gtk4::Widget>) {
    if let Some(parent) = child.as_ref().parent()
        && let Some(parent) = parent.downcast_ref::<gtk4::Box>()
    {
        parent.remove(child);
    }
}

/// Write a device's battery level, or take the label away.
fn render_battery(label: &Label, device: &BtDevice) {
    match device.battery_pct {
        // Only while connected: a headset in a case reports whatever it last
        // said, and a stale number beside an idle row reads as a live one.
        Some(percent) if device.connected => {
            set_text(label, &format!("{percent}%"));
            label.set_visible(true);
        }
        _ => label.set_visible(false),
    }
}

/// The inline pairing box.
///
/// One widget, moved about, rather than one per row — the same arrangement the
/// Wi-Fi password box uses, and for a related reason: there is at most one
/// pairing in flight, and a box per device would be a dozen widgets waiting for
/// something that happens twice a year.
struct PairingBox {
    root: gtk4::Box,
    question: Label,
    code: Label,
    buttons: gtk4::Box,
    /// The device the box is currently open for.
    path: RefCell<Option<String>>,
}

impl PairingBox {
    /// Build it, closed.
    fn new(services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 6);
        root.add_css_class(classes::QS_PAIRING_ROW);
        root.set_visible(false);

        let question = Label::new(None);
        question.add_css_class(classes::QS_HINT);
        question.set_xalign(0.0);
        question.set_wrap(true);
        root.append(&question);

        // The code is the whole point of the row: it has to be readable across
        // a desk at the same time as the phone showing it.
        let code = Label::new(None);
        code.add_css_class(classes::QS_PAIRING_CODE);
        code.set_halign(Align::Center);
        code.set_visible(false);
        root.append(&code);

        let buttons = gtk4::Box::new(Orientation::Horizontal, 6);
        buttons.set_halign(Align::End);
        let cancel = Button::with_label("Cancel");
        cancel.add_css_class(classes::QS_PASSWORD_BUTTON);
        ripple::install(&cancel);
        buttons.append(&cancel);
        let confirm = Button::with_label("Confirm");
        confirm.add_css_class(classes::QS_PASSWORD_BUTTON);
        confirm.add_css_class(classes::CHECKED);
        ripple::install(&confirm);
        buttons.append(&confirm);
        root.append(&buttons);

        for (button, yes) in [(&cancel, false), (&confirm, true)] {
            button.connect_clicked({
                let bluetooth = services.bluetooth.handle().clone();
                move |_| {
                    let bluetooth = bluetooth.clone();
                    attempt(names::BLUETOOTH, async move {
                        if yes {
                            bluetooth.confirm_pairing().await
                        } else {
                            bluetooth.cancel_pairing().await
                        }
                    });
                }
            });
        }

        Rc::new(Self {
            root,
            question,
            code,
            buttons,
            path: RefCell::new(None),
        })
    }

    /// Show or hide the box, and say what it is asking.
    fn render(self: &Rc<Self>, state: &BtState, list: &Rc<BluetoothList>) {
        let Some(prompt) = &state.prompt else {
            if self.path.borrow().is_some() {
                *self.path.borrow_mut() = None;
                self.root.set_visible(false);
                list.place_pairing(None);
            }
            return;
        };

        *self.path.borrow_mut() = Some(prompt.path.clone());
        set_text(&self.question, &prompt.question());
        match &prompt.code {
            Some(code) => {
                set_text(&self.code, code);
                self.code.set_visible(true);
            }
            None => self.code.set_visible(false),
        }
        // A code the user is told to type on the other device has nothing to
        // answer, so it gets no buttons rather than two that do nothing.
        self.buttons.set_visible(prompt.answerable());
        self.root.set_visible(true);
        list.place_pairing(Some(&prompt.path));
    }

    /// Put it into `list` directly after `after`, or at the end of the list.
    ///
    /// The sibling is named by *widget* rather than by index. The list has a
    /// header in the middle of it, so a row's position in `rows` and its
    /// position among the box's children are two different numbers — and
    /// counting one as if it were the other put the confirmation under the
    /// wrong device.
    fn attach(&self, list: &gtk4::Box, after: Option<&gtk4::Widget>) {
        match self.root.parent() {
            Some(parent) if parent.downcast_ref::<gtk4::Box>() == Some(list) => {}
            _ => {
                detach(&self.root);
                list.append(&self.root);
            }
        }
        if let Some(sibling) = after {
            list.reorder_child_after(&self.root, Some(sibling));
        }
    }

    /// Take it out of whatever it is in.
    fn detach(&self) {
        detach(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topbar_services::{IconKind, PairingPrompt, PromptKind};

    fn device(alias: &str, connected: bool) -> BtDevice {
        BtDevice {
            path: format!("/org/bluez/hci0/dev_{alias}"),
            alias: alias.to_string(),
            icon: IconKind::Headset,
            connected,
            paired: true,
            battery_pct: None,
            pending: false,
        }
    }

    fn powered(devices: Vec<BtDevice>) -> BtState {
        BtState {
            available: true,
            powered: true,
            devices,
            ..BtState::default()
        }
    }

    #[test]
    fn the_list_is_rebuilt_when_its_shape_changes_and_not_when_a_battery_moves() {
        let mut state = powered(vec![device("Buds", true), device("Mouse", false)]);
        state.devices[0].battery_pct = Some(85);
        let before = signature(&state);

        // A headset losing a percent changes a label, not a row.
        state.devices[0].battery_pct = Some(84);
        assert_eq!(signature(&state), before, "no rebuild for a battery tick");

        // A row starting to spin does change the shape.
        let mut spinning = state.clone();
        spinning.devices[1].pending = true;
        assert_ne!(signature(&spinning), before);

        // And so does one connecting.
        let mut connected = state.clone();
        connected.devices[1].connected = true;
        assert_ne!(signature(&connected), before);
    }

    #[test]
    fn switching_the_radio_off_changes_the_list_the_user_is_shown() {
        let on = powered(vec![device("Buds", true)]);
        let mut off = on.clone();
        off.powered = false;
        assert_ne!(signature(&on), signature(&off));
    }

    #[test]
    fn opening_the_scan_puts_a_header_in_the_list_before_it_has_found_anything() {
        let closed = powered(vec![device("Buds", true)]);
        let open = BtState {
            browsing: true,
            ..closed.clone()
        };
        assert!(!shows_found(&closed));
        assert!(
            shows_found(&open),
            "a scan says so before it finds anything"
        );
        assert_ne!(signature(&closed), signature(&open));

        // The spinner is not a rebuild: it is already in the header, and only
        // its own visibility moves.
        let looking = BtState {
            scanning: true,
            ..open.clone()
        };
        assert_eq!(signature(&looking), signature(&open));
    }

    #[test]
    fn a_device_that_finished_pairing_is_a_different_row() {
        let mut found = device("Pixel", false);
        found.paired = false;
        let scanning = BtState {
            browsing: true,
            ..powered(vec![device("Buds", true), found.clone()])
        };

        // A button with no switch becomes a box with one, so the list has to
        // be rebuilt rather than have its labels updated.
        let mut paired = scanning.clone();
        paired.devices[1].paired = true;
        assert_ne!(signature(&scanning), signature(&paired));

        // And the group survives the scan that found it, so a pairing still in
        // flight does not lose the row it is happening under.
        let settled = BtState {
            browsing: false,
            ..scanning.clone()
        };
        assert!(shows_found(&settled));
    }

    #[test]
    fn a_pairing_question_arriving_moves_the_row_it_belongs_to() {
        let state = powered(vec![device("Pixel", false)]);
        let mut asking = state.clone();
        asking.prompt = Some(PairingPrompt {
            path: state.devices[0].path.clone(),
            alias: "Pixel".into(),
            code: Some("000042".into()),
            kind: PromptKind::Confirm,
        });
        assert_ne!(signature(&state), signature(&asking));
    }
}
