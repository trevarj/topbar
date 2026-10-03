//! The Wi-Fi list, the password row, the VPN list, and the wired statement.
//!
//! ```text
//! ┌───────────────────┬───────────────────┐
//! │ 📶 Usadba      ⌄ │ 🔒 Work        ⌄ │   two pills in the grid
//! └───────────────────┴───────────────────┘
//!   Available networks              ◜◝     ← header, with the scan spinner
//!   📶 Usadba              🔒    ✓
//!   📶 Cafe                🔒
//!     [ ••••••••••• ]  Cancel  Connect     ← opened under the row it is for
//!   📶 Airport                    ◜◝
//! ┌───────────────────────────────────────┐
//! │ 🖧  Wired · 1 Gb/s                     │   a statement, not a control
//! └───────────────────────────────────────┘
//! ```
//!
//! The list is **rebuilt only when its shape changes**. A snapshot arrives
//! every time one access point's signal moves, which in a busy place is several
//! a second; tearing down and rebuilding a dozen rows at that rate would make
//! the panel flicker and would throw away the password entry the user is
//! halfway through typing into. So each render compares a signature of what the
//! rows *are* — names, and the flags that change what a row looks like — and
//! only touches the icons and marks when it matches.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Button, Image, Label, Orientation, PasswordEntry, Spinner};
use topbar_services::{ApView, NetworkState, Pending, Services, VpnView};

use crate::anim::ripple;
use crate::style::{classes, icons};
use crate::surfaces::inline::names;
use crate::surfaces::popovers;
use crate::surfaces::search::ChoiceFilter;
use crate::widgets::quick_settings::{WIDGET_NAME, attempt, attempt_then, set_icon, set_text};

/// Gap between the parts of a row.
const GAP: i32 = 10;

/// What the Wi-Fi list looks like right now, as one comparable string.
///
/// Signal strength is in it as the *bucket* rather than the percentage: the
/// icon is what a change of bucket changes, and rebuilding the list because a
/// reading went from 71 to 70 would be a rebuild nobody could see.
fn wifi_signature(state: &NetworkState) -> String {
    let mut signature = format!("{}:{}", state.wifi.present, state.wifi.enabled);
    for ap in &state.wifi.list {
        signature.push('\u{1}');
        signature.push_str(&format!(
            "{}\u{2}{}\u{2}{}\u{2}{}\u{2}{}\u{2}{}",
            ap.ssid, ap.bucket, ap.secured, ap.known, ap.active, ap.connecting
        ));
    }
    signature
}

/// The same for the VPN list.
fn vpn_signature(state: &NetworkState) -> String {
    let mut signature = String::new();
    for profile in &state.vpn {
        signature.push('\u{1}');
        signature.push_str(&format!(
            "{}\u{2}{}\u{2}{}\u{2}{}\u{2}{}",
            profile.uuid,
            profile.id,
            profile.kind.label(),
            profile.active,
            profile.pending
        ));
    }
    signature
}

/// The parts of one network row that change without the list changing shape.
struct Row {
    ssid: String,
    icon: Image,
    mark: Image,
    spinner: Spinner,
}

/// The list of networks in range, and the password row that opens inside it.
pub struct WifiList {
    root: gtk4::Box,
    header: gtk4::Box,
    scanning: Spinner,
    list: gtk4::Box,
    filter: Rc<ChoiceFilter>,
    empty: Label,
    rows: RefCell<Vec<Row>>,
    /// What the rows were last built from.
    built: RefCell<String>,
    /// The password box, re-parented under whichever row wants it.
    password: Rc<PasswordBox>,
    services: Services,
}

impl WifiList {
    /// Build the list.
    pub fn new(services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.add_css_class(classes::QS_DEVICE_LIST);

        let header = gtk4::Box::new(Orientation::Horizontal, GAP);
        header.add_css_class(classes::QS_LIST_HEADER);
        let title = Label::new(Some("Available networks"));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        header.append(&title);
        let scanning = Spinner::new();
        scanning.set_visible(false);
        header.append(&scanning);
        root.append(&header);
        let filter = ChoiceFilter::new(&root, "Filter networks");

        let list = gtk4::Box::new(Orientation::Vertical, 2);
        root.append(&list);

        // A list with nothing in it says so, rather than being a gap the user
        // has to decide the meaning of.
        let empty = Label::new(Some("No networks found"));
        empty.add_css_class(classes::QS_HINT);
        empty.set_xalign(0.0);
        empty.set_visible(false);
        root.append(&empty);

        let password = PasswordBox::new(services);

        Rc::new(Self {
            root,
            header,
            scanning,
            list,
            filter,
            empty,
            rows: RefCell::new(Vec::new()),
            built: RefCell::new(String::new()),
            password,
            services: services.clone(),
        })
    }

    /// The widget to put in the section.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Draw the list from `state`.
    pub fn render(self: &Rc<Self>, state: &NetworkState) {
        self.scanning.set_visible(state.wifi.scanning);
        if state.wifi.scanning {
            self.scanning.start();
        } else {
            self.scanning.stop();
        }
        self.header.set_visible(state.wifi.enabled);

        let signature = wifi_signature(state);
        if *self.built.borrow() != signature {
            self.rebuild(state);
            *self.built.borrow_mut() = signature;
        }

        for (row, ap) in self.rows.borrow().iter().zip(&state.wifi.list) {
            set_icon(&row.icon, icons::wifi_signal(ap.bucket));
            row.mark.set_opacity(if ap.active { 1.0 } else { 0.0 });
            row.spinner
                .set_opacity(if ap.connecting { 1.0 } else { 0.0 });
            if ap.connecting {
                row.spinner.start();
            } else {
                row.spinner.stop();
            }
        }

        self.empty
            .set_visible(state.wifi.enabled && state.wifi.list.is_empty());
        self.password.render(state, self);
        self.filter.set_visible(state.wifi.enabled);
        self.filter
            .pin(state.prompt.as_ref().map(|prompt| prompt.ssid.as_str()));
    }

    /// Rebuild every row.
    fn rebuild(self: &Rc<Self>, state: &NetworkState) {
        // The password box is re-parented into the list, so it has to be taken
        // out before the rows around it are dropped — GTK asserts otherwise.
        self.password.detach();

        self.filter.clear_rows();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut rows = Vec::new();

        for ap in &state.wifi.list {
            let (row, parts) = self.row(ap);
            self.list.append(&row);
            rows.push(parts);
        }
        *self.rows.borrow_mut() = rows;
    }

    /// One network's row.
    fn row(self: &Rc<Self>, ap: &ApView) -> (Button, Row) {
        let button = Button::new();
        button.add_css_class(classes::QS_NETWORK_ROW);

        let line = gtk4::Box::new(Orientation::Horizontal, GAP);
        line.set_valign(Align::Center);

        let icon = Image::from_icon_name(icons::wifi_signal(ap.bucket));
        icon.add_css_class(classes::QS_ICON);
        line.append(&icon);

        let name = Label::new(Some(&ap.ssid));
        name.add_css_class(classes::QS_DEVICE_NAME);
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        line.append(&name);

        if ap.secured {
            let badge = Image::from_icon_name(icons::WIFI_LOCKED);
            badge.add_css_class(classes::QS_NETWORK_BADGE);
            line.append(&badge);
        }

        // The spinner and the checkmark both live in the row from the start and
        // only their *opacity* moves, so joining a network does not make the
        // padlock beside them shift sideways. Visibility would: an invisible
        // GTK widget is not measured, so a spinner that appeared on a press
        // shoved the badge to its left by its own width and the row twitched at
        // exactly the moment the user was watching it.
        let spinner = Spinner::new();
        spinner.set_opacity(0.0);
        line.append(&spinner);

        let mark = Image::from_icon_name(icons::SELECTED);
        mark.add_css_class(classes::QS_DEVICE_MARK);
        mark.set_opacity(0.0);
        line.append(&mark);

        button.set_child(Some(&line));
        ripple::install(&button);
        button.connect_clicked({
            let list = Rc::downgrade(self);
            let ssid = ap.ssid.clone();
            let active = ap.active;
            move |_| {
                let Some(list) = list.upgrade() else { return };
                list.activate(&ssid, active);
            }
        });

        self.filter.add(&button, ap.ssid.clone(), None);
        (
            button,
            Row {
                ssid: ap.ssid.clone(),
                icon,
                mark,
                spinner,
            },
        )
    }

    /// What a click on one row means.
    ///
    /// Leaving is unambiguous. Joining is too, in this design: there is no
    /// "enter the password first" step, because the password row only exists
    /// once NetworkManager has asked for one — which it does for a network with
    /// no saved key and does not for one with a key that still works.
    fn activate(self: &Rc<Self>, ssid: &str, active: bool) {
        let network = self.services.network.handle().clone();
        if active {
            attempt(names::WIFI, async move { network.disconnect_wifi().await });
            return;
        }
        let ssid = ssid.to_string();
        attempt(names::WIFI, async move { network.connect(ssid).await });
    }

    /// Put the password box under the row for `ssid`, or take it away.
    fn place_password(&self, ssid: Option<&str>) {
        let Some(ssid) = ssid else {
            self.password.detach();
            return;
        };
        let position = self
            .rows
            .borrow()
            .iter()
            .position(|row| row.ssid == ssid)
            .map(|index| index + 1);
        self.password.attach(&self.list, position);
    }
}

/// The inline password box.
///
/// One widget, moved about, rather than one per row: a `PasswordEntry` carries
/// what the user has typed, and building a fresh one on every snapshot would
/// throw away a half-typed key every time an access point's signal moved.
struct PasswordBox {
    root: gtk4::Box,
    prompt: Label,
    entry: PasswordEntry,
    connect: Button,
    /// The network the box is currently open for.
    ssid: RefCell<Option<String>>,
    services: Services,
}

impl PasswordBox {
    /// Build it, closed.
    fn new(services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 6);
        root.add_css_class(classes::QS_PASSWORD_ROW);
        root.set_visible(false);

        let prompt = Label::new(None);
        prompt.add_css_class(classes::QS_HINT);
        prompt.set_xalign(0.0);
        prompt.set_wrap(true);
        root.append(&prompt);

        let entry = PasswordEntry::new();
        entry.add_css_class(classes::QS_PASSWORD_ENTRY);
        entry.set_show_peek_icon(true);
        root.append(&entry);

        let actions = gtk4::Box::new(Orientation::Horizontal, 6);
        actions.set_halign(Align::End);
        let cancel = Button::with_label("Cancel");
        cancel.add_css_class(classes::QS_PASSWORD_BUTTON);
        ripple::install(&cancel);
        actions.append(&cancel);
        let connect = Button::with_label("Connect");
        connect.add_css_class(classes::QS_PASSWORD_BUTTON);
        connect.add_css_class(classes::CHECKED);
        // Ripples like everything else in the panel: Connect is the one button
        // here whose answer can take ten seconds to arrive, so a press that
        // showed nothing was the press most likely to be made twice.
        ripple::install(&connect);
        actions.append(&connect);
        root.append(&actions);

        let password = Rc::new(Self {
            root,
            prompt,
            entry,
            connect,
            ssid: RefCell::new(None),
            services: services.clone(),
        });

        password.connect.connect_clicked({
            let password = Rc::downgrade(&password);
            move |_| {
                if let Some(password) = password.upgrade() {
                    password.submit();
                }
            }
        });
        password.entry.connect_activate({
            let password = Rc::downgrade(&password);
            move |_| {
                if let Some(password) = password.upgrade() {
                    password.submit();
                }
            }
        });
        cancel.connect_clicked({
            let network = services.network.handle().clone();
            move |_| {
                let network = network.clone();
                attempt(names::WIFI, async move { network.cancel_prompt().await });
            }
        });

        password
    }

    /// Show or hide the box, and say what it is for.
    fn render(self: &Rc<Self>, state: &NetworkState, list: &Rc<WifiList>) {
        let Some(prompt) = &state.prompt else {
            if self.ssid.borrow().is_some() {
                self.close();
                list.place_password(None);
            }
            return;
        };

        let reopened = self.ssid.borrow().as_deref() != Some(prompt.ssid.as_str());
        if reopened {
            *self.ssid.borrow_mut() = Some(prompt.ssid.clone());
            self.entry.set_text("");
        }

        // The wording carries the whole of what went wrong: NetworkManager
        // asking a second time *is* "that password was refused", and there is
        // no other signal on the bus that says so while the card is still
        // trying.
        set_text(
            &self.prompt,
            &if prompt.is_retry() {
                format!("Authentication failed — try again for {}", prompt.ssid)
            } else {
                format!("Enter the password for {}", prompt.ssid)
            },
        );
        if prompt.is_retry() {
            self.prompt.add_css_class(classes::INLINE_ERROR);
        } else {
            self.prompt.remove_css_class(classes::INLINE_ERROR);
        }

        self.root.set_visible(true);
        list.place_password(Some(&prompt.ssid));
        if reprompted(prompt.attempt) {
            self.entry.set_text("");
        }
        self.entry.grab_focus();
    }

    /// Send what was typed.
    fn submit(self: &Rc<Self>) {
        let typed = self.entry.text().to_string();
        if typed.is_empty() {
            return;
        }
        let network = self.services.network.handle().clone();
        // The entry is cleared here rather than when the answer comes back:
        // whatever happens next, the key must not sit in a widget waiting to be
        // read out of a screenshot.
        self.entry.set_text("");
        attempt(names::WIFI, async move {
            network
                .submit_secret(topbar_services::Secret::new(typed))
                .await
        });
    }

    /// Stop showing it.
    fn close(&self) {
        *self.ssid.borrow_mut() = None;
        self.entry.set_text("");
        self.root.set_visible(false);
    }

    /// Put it into `list` at `position`, or at the end.
    fn attach(&self, list: &gtk4::Box, position: Option<usize>) {
        if let Some(parent) = self.root.parent()
            && let Some(parent) = parent.downcast_ref::<gtk4::Box>()
        {
            if parent == list {
                reorder(list, &self.root, position);
                return;
            }
            parent.remove(&self.root);
        }
        list.append(&self.root);
        reorder(list, &self.root, position);
    }

    /// Take it out of whatever it is in.
    fn detach(&self) {
        if let Some(parent) = self.root.parent()
            && let Some(parent) = parent.downcast_ref::<gtk4::Box>()
        {
            parent.remove(&self.root);
        }
    }
}

/// Whether an attempt count means the last password was refused.
fn reprompted(attempt: u32) -> bool {
    attempt > 1
}

/// Move `child` to `position` inside `list`.
fn reorder(list: &gtk4::Box, child: &gtk4::Box, position: Option<usize>) {
    let Some(position) = position else { return };
    let mut sibling = list.first_child();
    let mut index = 0;
    while let Some(current) = sibling {
        if index + 1 == position {
            list.reorder_child_after(child, Some(&current));
            return;
        }
        sibling = current.next_sibling();
        index += 1;
    }
}

/// The list of VPN profiles.
pub struct VpnList {
    root: gtk4::Box,
    list: gtk4::Box,
    filter: Rc<ChoiceFilter>,
    /// Each row's spinner and accent mark, beside the profile it belongs to.
    rows: RefCell<Vec<(String, Image, Spinner)>>,
    built: RefCell<String>,
    /// `[widgets.quick_settings] vpn_close_on_connect`.
    close_on_connect: bool,
    services: Services,
}

impl VpnList {
    /// Build the list.
    pub fn new(services: &Services, close_on_connect: bool) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 2);
        root.add_css_class(classes::QS_DEVICE_LIST);
        let filter = ChoiceFilter::new(&root, "Filter VPN profiles");
        let list = gtk4::Box::new(Orientation::Vertical, 2);
        root.append(&list);
        Rc::new(Self {
            root,
            list,
            filter,
            rows: RefCell::new(Vec::new()),
            built: RefCell::new(String::new()),
            close_on_connect,
            services: services.clone(),
        })
    }

    /// The widget to put in the section.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Draw the list from `state`.
    pub fn render(self: &Rc<Self>, state: &NetworkState) {
        let signature = vpn_signature(state);
        if *self.built.borrow() != signature {
            self.rebuild(state);
            *self.built.borrow_mut() = signature;
        }
        for ((uuid, mark, spinner), profile) in self.rows.borrow().iter().zip(&state.vpn) {
            debug_assert_eq!(uuid, &profile.uuid);
            mark.set_opacity(if profile.active { 1.0 } else { 0.0 });
            spinner.set_opacity(if profile.pending { 1.0 } else { 0.0 });
            if profile.pending {
                spinner.start();
            } else {
                spinner.stop();
            }
        }
        self.filter.apply();
    }

    /// Rebuild every row.
    fn rebuild(self: &Rc<Self>, state: &NetworkState) {
        self.filter.clear_rows();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut rows = Vec::new();
        for profile in &state.vpn {
            let (row, parts) = self.row(profile);
            self.list.append(&row);
            rows.push(parts);
        }
        *self.rows.borrow_mut() = rows;
    }

    /// One profile's row.
    fn row(self: &Rc<Self>, profile: &VpnView) -> (Button, (String, Image, Spinner)) {
        let button = Button::new();
        button.add_css_class(classes::QS_VPN_ROW);
        // A tunnel something else raised is shown so the user knows their
        // traffic is going somewhere, and is not the panel's to switch.
        button.set_sensitive(profile.switchable());

        let line = gtk4::Box::new(Orientation::Horizontal, GAP);
        line.set_valign(Align::Center);

        let icon = Image::from_icon_name(if profile.active {
            icons::VPN
        } else {
            icons::VPN_DISCONNECTED
        });
        icon.add_css_class(classes::QS_ICON);
        line.append(&icon);

        let text = gtk4::Box::new(Orientation::Vertical, 0);
        text.set_hexpand(true);
        let name = Label::new(Some(&profile.id));
        name.add_css_class(classes::QS_DEVICE_NAME);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        text.append(&name);
        let kind = Label::new(Some(profile.kind.label()));
        kind.add_css_class(classes::QS_HINT);
        kind.set_xalign(0.0);
        text.append(&kind);
        line.append(&text);

        // Measured whether or not it is spinning, like the checkmark beside it
        // and the Wi-Fi row's: a tunnel that starts coming up must not shove
        // the profile's name sideways at the moment it is being watched.
        let spinner = Spinner::new();
        spinner.set_opacity(0.0);
        line.append(&spinner);

        let mark = Image::from_icon_name(icons::SELECTED);
        mark.add_css_class(classes::QS_DEVICE_MARK);
        mark.set_opacity(0.0);
        line.append(&mark);

        button.set_child(Some(&line));
        ripple::install(&button);
        button.connect_clicked({
            let network = self.services.network.handle().clone();
            let uuid = profile.uuid.clone();
            let active = profile.active;
            // v1 had this option and it never fired: the close was gated behind
            // a legacy auth-dialog flag that a WireGuard profile never set. Here
            // it means what it says — the panel closes once the tunnel is
            // actually up, and stays open with the error under the row when it
            // is not.
            let close = self.close_on_connect && !active;
            move |_| {
                let network = network.clone();
                let uuid = uuid.clone();
                attempt_then(
                    names::VPN,
                    async move { network.set_vpn(uuid, !active).await },
                    move || {
                        if close {
                            popovers::dispatch(
                                &topbar_core::ipc::PopoverAction::Hide(Some(
                                    WIDGET_NAME.to_string(),
                                )),
                                None,
                            );
                        }
                    },
                );
            }
        });

        self.filter
            .add(&button, format!("{} {}", profile.id, profile.uuid), None);
        (button, (profile.uuid.clone(), mark, spinner))
    }
}

/// The wired row: what the cable is doing, stated once.
pub struct WiredRow {
    root: gtk4::Box,
    icon: Image,
    label: Label,
}

impl WiredRow {
    /// Build it, hidden.
    pub fn new() -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Horizontal, GAP);
        root.add_css_class(classes::QS_STATUS_ROW);
        root.set_visible(false);

        let icon = Image::from_icon_name(icons::WIRED);
        icon.add_css_class(classes::QS_ICON);
        root.append(&icon);

        let label = Label::new(Some("Wired"));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        root.append(&label);

        Rc::new(Self { root, icon, label })
    }

    /// The widget to put in the panel.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Draw it from `state`.
    ///
    /// Hidden unless there is a cable doing something. A machine with an
    /// Ethernet port and nothing plugged into it has nothing to say about it,
    /// and a permanent "Wired — disconnected" row on every laptop would be the
    /// opposite of the quiet the panel is supposed to be.
    pub fn render(&self, state: &NetworkState) {
        self.root.set_visible(state.wired.connected);
        if !state.wired.connected {
            return;
        }
        set_icon(&self.icon, icons::WIRED);
        set_text(&self.label, &wired_label(state));
    }
}

/// What the wired row says.
fn wired_label(state: &NetworkState) -> String {
    let name = state.wired.id.as_deref().unwrap_or("Wired");
    match state.wired.speed_label() {
        Some(speed) => format!("{name} · {speed}"),
        None => name.to_string(),
    }
}

/// What the collapsed Wi-Fi pill says under its title.
pub fn wifi_subtitle(state: &NetworkState) -> &'static str {
    if !state.wifi.enabled {
        return "Off";
    }
    if state.wifi.active.is_some() {
        return "Connected";
    }
    "Not connected"
}

/// The icon on the collapsed Wi-Fi pill.
pub fn wifi_icon(state: &NetworkState) -> &'static str {
    if !state.wifi.enabled {
        return icons::WIFI_DISABLED;
    }
    match &state.wifi.active {
        Some(active) => icons::wifi_signal(active.bucket),
        None => icons::WIFI_OFFLINE,
    }
}

/// What the collapsed Wi-Fi pill is titled.
///
/// The network the user is on, when there is one: that is the answer to the
/// question they opened the panel to ask, and "Wi-Fi" underneath says what kind
/// of answer it is.
pub fn wifi_title(state: &NetworkState) -> String {
    state
        .wifi
        .active
        .as_ref()
        .map_or_else(|| "Wi-Fi".to_string(), |active| active.ssid.clone())
}

/// Whether the Wi-Fi pill should read as switched on.
pub fn wifi_checked(state: &NetworkState) -> bool {
    state.wifi.enabled
}

/// Whether the radio is mid-switch, so the pill should not be pressed again.
pub fn radio_busy(state: &NetworkState) -> bool {
    matches!(state.pending, Some(Pending::Radio))
}

/// The one profile a click on the VPN pill's body should switch, if there is
/// exactly one to switch.
///
/// A machine with a single tunnel does not need a list to open: the pill is the
/// switch, which is what every other pill in the grid is. A machine with two
/// does, because "on" would be ambiguous.
pub fn lone_vpn(state: &NetworkState) -> Option<&VpnView> {
    let switchable: Vec<&VpnView> = state
        .vpn
        .iter()
        .filter(|profile| profile.switchable())
        .collect();
    match switchable.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// What the collapsed VPN pill is titled.
///
/// One profile is named; several are counted, because six names cannot fit in
/// half a panel and the list underneath is where they belong.
pub fn vpn_title(state: &NetworkState) -> String {
    let active: Vec<&VpnView> = state.vpn.iter().filter(|profile| profile.active).collect();
    match active.as_slice() {
        [] if state.vpn.len() == 1 => state.vpn[0].id.clone(),
        [] => "VPN".to_string(),
        [one] => one.id.clone(),
        many => format!("{} tunnels", many.len()),
    }
}

/// What it says underneath.
///
/// The subtitle says what kind of answer the title is — "Balanced / Power
/// Mode", "Usadba / Connected" — so when the title is already the word "VPN"
/// there is nothing left for it to add, and a pill reading "VPN" over
/// "VPN · Off" said the same thing twice in the space of two lines.
pub fn vpn_subtitle(state: &NetworkState) -> &'static str {
    let on = state.vpn.iter().any(|profile| profile.active);
    if vpn_title(state) == "VPN" {
        return if on { "On" } else { "Off" };
    }
    if on { "VPN · On" } else { "VPN · Off" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topbar_services::{VpnKind, WifiState, WiredState};

    fn ap(ssid: &str, bucket: u8, active: bool) -> ApView {
        ApView {
            ssid: ssid.to_string(),
            strength: bucket * 25,
            bucket,
            secured: true,
            known: true,
            active,
            connecting: false,
        }
    }

    fn wifi(state: WifiState) -> NetworkState {
        NetworkState {
            wifi: state,
            ..NetworkState::default()
        }
    }

    #[test]
    fn the_pill_names_the_network_and_says_what_kind_of_name_it_is() {
        let connected = wifi(WifiState {
            present: true,
            enabled: true,
            active: Some(ap("Usadba", 4, true)),
            list: vec![ap("Usadba", 4, true)],
            ..WifiState::default()
        });
        assert_eq!(wifi_title(&connected), "Usadba");
        assert_eq!(wifi_subtitle(&connected), "Connected");
        assert_eq!(
            wifi_icon(&connected),
            "network-wireless-signal-excellent-symbolic"
        );
        assert!(wifi_checked(&connected));

        let idle = wifi(WifiState {
            present: true,
            enabled: true,
            ..WifiState::default()
        });
        assert_eq!(wifi_title(&idle), "Wi-Fi");
        assert_eq!(wifi_subtitle(&idle), "Not connected");
        assert_eq!(wifi_icon(&idle), "network-wireless-offline-symbolic");

        let off = wifi(WifiState {
            present: true,
            ..WifiState::default()
        });
        assert_eq!(wifi_subtitle(&off), "Off");
        assert_eq!(wifi_icon(&off), "network-wireless-disabled-symbolic");
        assert!(!wifi_checked(&off));
    }

    #[test]
    fn the_wired_row_reads_like_the_label_on_the_box() {
        let mut state = NetworkState {
            wired: WiredState {
                present: true,
                carrier: true,
                connected: true,
                id: Some("Wired connection 1".into()),
                speed_mbps: 1000,
            },
            ..NetworkState::default()
        };
        assert_eq!(wired_label(&state), "Wired connection 1 · 1 Gb/s");

        state.wired.speed_mbps = 0;
        assert_eq!(
            wired_label(&state),
            "Wired connection 1",
            "a driver that will not say its speed says nothing"
        );

        state.wired.id = None;
        assert_eq!(wired_label(&state), "Wired");
    }

    fn vpn(id: &str, active: bool) -> VpnView {
        VpnView {
            id: id.to_string(),
            uuid: id.to_string(),
            kind: VpnKind::WireGuard,
            active,
            pending: false,
        }
    }

    #[test]
    fn a_machine_with_one_tunnel_switches_it_from_the_pill() {
        let one = NetworkState {
            vpn: vec![vpn("Work", false)],
            ..NetworkState::default()
        };
        assert_eq!(lone_vpn(&one).map(|p| p.id.as_str()), Some("Work"));

        let two = NetworkState {
            vpn: vec![vpn("Work", false), vpn("Home", false)],
            ..NetworkState::default()
        };
        assert!(lone_vpn(&two).is_none(), "two is a list, not a switch");

        // A tunnel the panel cannot switch does not count towards the one.
        let mut external = VpnView {
            kind: topbar_services::VpnKind::External,
            ..vpn("tun0", true)
        };
        external.active = true;
        let one_plus_external = NetworkState {
            vpn: vec![vpn("Work", false), external],
            ..NetworkState::default()
        };
        assert_eq!(
            lone_vpn(&one_plus_external).map(|p| p.id.as_str()),
            Some("Work")
        );

        assert!(lone_vpn(&NetworkState::default()).is_none());
    }

    #[test]
    fn the_vpn_pill_names_one_tunnel_and_counts_several() {
        let one = NetworkState {
            vpn: vec![vpn("Work", false)],
            ..NetworkState::default()
        };
        assert_eq!(vpn_title(&one), "Work", "a single profile is named");
        assert_eq!(vpn_subtitle(&one), "VPN · Off");

        let several = NetworkState {
            vpn: vec![vpn("Work", false), vpn("Home", false)],
            ..NetworkState::default()
        };
        assert_eq!(vpn_title(&several), "VPN");
        assert_eq!(
            vpn_subtitle(&several),
            "Off",
            "a pill titled VPN does not need a subtitle that says VPN"
        );

        let up = NetworkState {
            vpn: vec![vpn("Work", true), vpn("Home", false)],
            ..NetworkState::default()
        };
        assert_eq!(vpn_title(&up), "Work", "whatever is up is what it says");
        assert_eq!(vpn_subtitle(&up), "VPN · On");

        let both = NetworkState {
            vpn: vec![vpn("Work", true), vpn("Home", true)],
            ..NetworkState::default()
        };
        assert_eq!(vpn_title(&both), "2 tunnels");
    }

    #[test]
    fn the_list_is_rebuilt_when_its_shape_changes_and_not_when_a_signal_moves() {
        let mut state = wifi(WifiState {
            present: true,
            enabled: true,
            list: vec![ap("Home", 3, false), ap("Cafe", 2, false)],
            ..WifiState::default()
        });
        let before = wifi_signature(&state);

        // A reading that does not cross a bucket boundary changes no icon.
        state.wifi.list[0].strength += 3;
        assert_eq!(wifi_signature(&state), before, "no rebuild for a wobble");

        // One that does.
        state.wifi.list[0].bucket = 4;
        assert_ne!(wifi_signature(&state), before);

        // And so does a row starting to spin.
        let mut spinning = state.clone();
        spinning.wifi.list[1].connecting = true;
        assert_ne!(wifi_signature(&spinning), wifi_signature(&state));
    }

    #[test]
    fn the_vpn_list_is_rebuilt_when_a_row_starts_or_stops_spinning() {
        let state = NetworkState {
            vpn: vec![vpn("Work", false)],
            ..NetworkState::default()
        };
        let mut pending = state.clone();
        pending.vpn[0].pending = true;
        assert_ne!(vpn_signature(&state), vpn_signature(&pending));
    }

    #[test]
    fn a_second_ask_is_what_puts_the_authentication_error_up() {
        assert!(!reprompted(1));
        assert!(reprompted(2));
    }

    #[test]
    fn the_radio_is_busy_only_while_it_is_being_switched() {
        let idle = NetworkState::default();
        assert!(!radio_busy(&idle));
        let switching = NetworkState {
            pending: Some(Pending::Radio),
            ..NetworkState::default()
        };
        assert!(radio_busy(&switching));
        let joining = NetworkState {
            pending: Some(Pending::Wifi {
                ssid: "Home".into(),
            }),
            ..NetworkState::default()
        };
        assert!(!radio_busy(&joining));
    }
}
