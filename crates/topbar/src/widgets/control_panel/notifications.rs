//! The control panel's notifications column: GNOME's message list.
//!
//! ```text
//! header        "Notifications" + Clear, only while there is something to clear
//! scroll
//! └── list
//!     └── group   app icon, name, newest summary, count, chevron, clear
//!         └── rows  summary, body, age, close   (while expanded)
//! empty         the designed state for a column with nothing in it
//! dnd           Do Not Disturb
//! ```
//!
//! Expansion is per-open state, which is what v1 did and what GNOME does: a
//! group the user opened last time comes back collapsed, because the list is a
//! place to catch up rather than a tree to navigate. It is *per open*, though,
//! and not per render — the cards are thrown away and rebuilt whenever anything
//! arrives, and a group that closed itself because a message landed in another
//! application is a group that shut under the reader's hand.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use chrono::{DateTime, Local};
use gtk4::prelude::*;
use gtk4::{
    Align, Button, Entry, EventControllerMotion, GestureClick, Image, Label, Orientation,
    PolicyType, ScrolledWindow, Switch, gdk, pango,
};
use topbar_services::{
    CloseReason, GroupView, NotifState, NotificationSound, NotificationView, Services,
};

use crate::anim::{Animation, AnimationParams, Easing, RotateBox, ripple};
use crate::bridge::{self, ActionScope, BindingGuard};
use crate::style::{classes, icons};
use crate::surfaces::search::SearchSelector;
use crate::widgets::expander::{REVEAL_MS, Section};
use crate::widgets::notifications::{
    self as notifications, ROW_ICON, absolute_time, icon, markup, relative_time,
};

/// Adwaita's bell. There is no plain `notifications-symbolic` in Adwaita 50,
/// and `notifications-disabled-symbolic` would read as "DND is on" rather than
/// "nothing has arrived".
const EMPTY_ICON: &str = "preferences-system-notifications-symbolic";
/// Tallest the list grows before it starts scrolling.
const MAX_LIST_HEIGHT: i32 = 460;
/// Half a turn, which takes the chevron from pointing down to pointing up.
///
/// The same turn Quick Settings' expanders make, over the same duration: one
/// arrow that turns rather than two that swap places.
const CHEVRON_TURN: f32 = 180.0;
/// Where this column's failures are reported.
const SCOPE: ActionScope = ActionScope::Toast {
    widget: "notifications",
};

const SOUND_CHOICES: &[(&str, NotificationSound)] = &[
    ("Off", NotificationSound::Off),
    ("Soft bell", NotificationSound::SoftBell),
    ("Soft chime", NotificationSound::SoftChime),
    ("Message", NotificationSound::Message),
    ("Instant message", NotificationSound::MessageNewInstant),
    ("Information", NotificationSound::DialogInformation),
    ("Volume change", NotificationSound::AudioVolumeChange),
    ("Device added", NotificationSound::DeviceAdded),
    ("Device removed", NotificationSound::DeviceRemoved),
    ("Custom", NotificationSound::Custom),
];

/// The notifications column.
pub struct Column {
    root: gtk4::Box,
    /// Header title and Clear, hidden while there is nothing to clear.
    header: gtk4::Box,
    clear_all: Button,
    /// The history, one child per application group.
    list: gtk4::Box,
    /// The scroller around it, hidden along with the list.
    ///
    /// Hiding the list alone is not enough: the scroller is what claims the
    /// column's spare height, and an empty one still claimed half of it —
    /// which pushed the "No Notifications" state, itself asking for the other
    /// half, into the bottom of the column instead of its middle.
    scroll: ScrolledWindow,
    /// Shown while [`Column::list`] is empty.
    empty: gtk4::Box,
    /// Do Not Disturb.
    dnd: Switch,
    /// Fuzzy sound selector, including Off and Custom.
    sound: Rc<SearchSelector>,
    custom_sound_row: gtk4::Box,
    custom_sound_path: Entry,
    custom_sound_apply: Button,
    /// Only preference changes overwrite an in-progress path edit.
    sound_preferences: RefCell<Option<(NotificationSound, Option<String>)>>,
    /// Set while the switch is being driven from a snapshot, so echoing the
    /// service's own state back at it does not look like a user toggle.
    syncing: Rc<Cell<bool>>,
    /// Whether the column is on screen, which is what makes a render a read.
    on_screen: Cell<bool>,
    /// Every age label on screen, with the moment it is counting from.
    ///
    /// Retained rather than searched for: the minute tick has to be cheap, and
    /// walking the widget tree once a minute to find labels by class would be
    /// both slower and easier to get wrong.
    ages: RefCell<Vec<(i64, Label)>>,
    /// The groups the user has opened, by group key, until the panel closes.
    ///
    /// Held here rather than on the cards because the cards do not survive a
    /// notification arriving. Cleared on every open, which is what makes
    /// expansion per-open state.
    expanded: Rc<RefCell<HashSet<String>>>,
    services: Services,
    binding: RefCell<Option<BindingGuard>>,
}

impl Column {
    /// Build the column and subscribe it to the daemon.
    pub fn new(services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.add_css_class(classes::CONTROL_PANEL_COLUMN);

        let header = gtk4::Box::new(Orientation::Horizontal, 8);
        header.add_css_class(classes::NOTIFICATION_HEADER);

        let title = Label::new(Some("Notifications"));
        title.add_css_class(classes::CARD_TITLE);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        header.append(&title);

        let clear_all = Button::with_label("Clear");
        clear_all.add_css_class(classes::NOTIFICATION_CLEAR_ALL);
        clear_all.set_focus_on_click(false);
        ripple::install(&clear_all);
        header.append(&clear_all);

        let list = gtk4::Box::new(Orientation::Vertical, 8);
        list.add_css_class(classes::NOTIFICATION_LIST);

        let scroll = ScrolledWindow::new();
        scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(MAX_LIST_HEIGHT);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));

        let empty = gtk4::Box::new(Orientation::Vertical, 12);
        empty.add_css_class(classes::EMPTY_STATE);
        empty.set_vexpand(true);
        empty.set_valign(Align::Center);
        empty.set_halign(Align::Center);

        let placeholder = Image::from_icon_name(EMPTY_ICON);
        placeholder.add_css_class(classes::EMPTY_STATE_ICON);
        empty.append(&placeholder);

        let caption = Label::new(Some("No Notifications"));
        caption.add_css_class(classes::EMPTY_STATE_LABEL);
        empty.append(&caption);

        let dnd_row = gtk4::Box::new(Orientation::Horizontal, 8);
        dnd_row.add_css_class(classes::DND_ROW);

        let dnd_label = Label::new(Some("Do Not Disturb"));
        dnd_label.add_css_class(classes::DND_LABEL);
        dnd_label.set_xalign(0.0);
        dnd_label.set_hexpand(true);

        let dnd = Switch::new();
        dnd.add_css_class(classes::NOTIFICATION_DND);
        dnd.set_valign(Align::Center);

        dnd_row.append(&dnd_label);
        dnd_row.append(&dnd);

        let sound_row = gtk4::Box::new(Orientation::Horizontal, 8);
        sound_row.add_css_class(classes::DND_ROW);
        let sound_label = Label::new(Some("_Notification sound"));
        sound_label.set_use_underline(true);
        sound_label.add_css_class(classes::DND_LABEL);
        sound_label.set_xalign(0.0);
        sound_label.set_hexpand(true);
        let sound = SearchSelector::new(
            "Notification sound",
            &SOUND_CHOICES
                .iter()
                .map(|(label, _)| *label)
                .collect::<Vec<_>>(),
        );
        sound.root().add_css_class(classes::NOTIFICATION_SOUND);
        sound.root().set_valign(Align::Center);
        sound_label.set_mnemonic_widget(Some(sound.root()));
        sound_row.append(&sound_label);
        sound_row.append(sound.root());

        let custom_sound_row = gtk4::Box::new(Orientation::Horizontal, 8);
        custom_sound_row.add_css_class(classes::DND_ROW);
        custom_sound_row.set_visible(false);
        let custom_sound_path = Entry::new();
        custom_sound_path.add_css_class(classes::NOTIFICATION_SOUND_PATH);
        custom_sound_path.set_placeholder_text(Some("/absolute/path/to/sound"));
        custom_sound_path
            .set_tooltip_text(Some("Custom notification sound: absolute local file path"));
        custom_sound_path.update_property(&[gtk4::accessible::Property::Label(
            "Custom notification sound file",
        )]);
        custom_sound_path.set_hexpand(true);
        custom_sound_path.set_width_chars(16);
        let custom_sound_apply = Button::with_label("Apply");
        custom_sound_apply.add_css_class(classes::NOTIFICATION_SOUND_APPLY);
        custom_sound_apply.add_css_class(classes::DIALOG_BUTTON);
        ripple::install(&custom_sound_apply);
        custom_sound_row.append(&custom_sound_path);
        custom_sound_row.append(&custom_sound_apply);

        root.append(&header);
        root.append(&scroll);
        root.append(&empty);
        root.append(&dnd_row);
        root.append(&sound_row);
        root.append(&custom_sound_row);

        let column = Rc::new(Self {
            root,
            header,
            clear_all,
            list,
            scroll,
            empty,
            dnd,
            sound,
            custom_sound_row,
            custom_sound_path,
            custom_sound_apply,
            sound_preferences: RefCell::new(None),
            syncing: Rc::new(Cell::new(false)),
            on_screen: Cell::new(false),
            ages: RefCell::new(Vec::new()),
            expanded: Rc::new(RefCell::new(HashSet::new())),
            services: services.clone(),
            binding: RefCell::new(None),
        });

        column.clear_all.connect_clicked({
            let handle = services.notifications.handle().clone();
            move |_| {
                let handle = handle.clone();
                bridge::act(SCOPE, async move { handle.clear_all().await });
            }
        });

        column.dnd.connect_state_set({
            let handle = services.notifications.handle().clone();
            let syncing = Rc::clone(&column.syncing);
            move |_, wanted| {
                if !syncing.get() {
                    let handle = handle.clone();
                    bridge::act(SCOPE, async move { handle.set_dnd(wanted).await });
                }
                gtk4::glib::Propagation::Proceed
            }
        });

        column.sound.connect_selected({
            let weak = Rc::downgrade(&column);
            move |selected| {
                let Some(column) = weak.upgrade() else {
                    return;
                };
                if !column.syncing.get() {
                    let sound = SOUND_CHOICES
                        .get(selected as usize)
                        .map_or(NotificationSound::Off, |(_, sound)| *sound);
                    column
                        .custom_sound_row
                        .set_visible(sound == NotificationSound::Custom);
                    if sound == NotificationSound::Custom {
                        column.custom_sound_path.grab_focus();
                    } else {
                        let handle = column.services.notifications.handle().clone();
                        bridge::act(SCOPE, async move { handle.set_sound(sound).await });
                    }
                }
            }
        });

        column.custom_sound_apply.connect_clicked({
            let weak = Rc::downgrade(&column);
            move |_| {
                if let Some(column) = weak.upgrade() {
                    column.apply_custom_sound();
                }
            }
        });
        column.custom_sound_path.connect_activate({
            let weak = Rc::downgrade(&column);
            move |_| {
                if let Some(column) = weak.upgrade() {
                    column.apply_custom_sound();
                }
            }
        });

        let binding = bridge::bind_state(&column.root, services.notifications.state(), {
            let column = Rc::downgrade(&column);
            move |_, state| {
                if let Some(column) = column.upgrade() {
                    column.render(state);
                }
            }
        });
        *column.binding.borrow_mut() = Some(binding);

        column
    }

    fn apply_custom_sound(&self) {
        let path = self.custom_sound_path.text().to_string();
        let handle = self.services.notifications.handle().clone();
        bridge::act(SCOPE, async move { handle.set_custom_sound(path).await });
    }

    /// The widget to put in the panel's left column.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Re-render from current state.
    ///
    /// Called on every open as well as on every change, so a panel that has
    /// been sitting closed for an hour never shows an hour-old list — and the
    /// ages in it are recomputed at the same moment.
    pub fn refresh(&self) {
        // Opening is what makes expansion per-open: whatever was left open an
        // hour ago comes back closed, and everything after this point in the
        // session is the user's own doing.
        self.expanded.borrow_mut().clear();

        // Opening the panel is what "seen" means, and it goes on meaning it
        // until the panel closes again; `render` is where the mark is made.
        self.on_screen.set(true);

        let receiver = self.services.notifications.state();
        let state = receiver.borrow().clone();
        self.render(&state);
    }

    /// The panel has left the screen, so a render is no longer a read.
    pub fn closed(&self) {
        self.on_screen.set(false);
    }

    /// Re-time every row, on the clock's minute tick.
    pub fn retime(&self, now: DateTime<Local>) {
        for (timestamp, label) in self.ages.borrow().iter() {
            let text = relative_time(*timestamp, now);
            if label.text() != text {
                label.set_text(&text);
            }
        }
    }

    /// Draw `state`.
    fn render(&self, state: &NotifState) {
        self.syncing.set(true);
        if self.dnd.is_active() != state.dnd {
            self.dnd.set_active(state.dnd);
        }
        let mut preferences = self.sound_preferences.borrow_mut();
        if preferences
            .as_ref()
            .is_none_or(|(sound, path)| *sound != state.sound || *path != state.custom_sound_path)
        {
            let selected = SOUND_CHOICES
                .iter()
                .position(|(_, sound)| *sound == state.sound)
                .unwrap_or(0) as u32;
            self.sound.set_selected(selected);
            self.custom_sound_row
                .set_visible(state.sound == NotificationSound::Custom);
            self.custom_sound_path
                .set_text(state.custom_sound_path.as_deref().unwrap_or_default());
            *preferences = Some((state.sound, state.custom_sound_path.clone()));
        }
        self.syncing.set(false);

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.ages.borrow_mut().clear();

        let now = Local::now();
        for group in &state.history {
            self.list.append(&self.build_group(group, now));
        }

        let empty = state.history.is_empty();
        self.list.set_visible(!empty);
        // The scroller with it: see the field's own note. An empty column is a
        // designed state and it has to be centred in the whole column.
        self.scroll.set_visible(!empty);
        // A Clear button over an empty list is a button that does nothing.
        self.header.set_visible(!empty);
        self.empty.set_visible(empty);

        if counts_as_read(self.on_screen.get(), state.unseen_count) {
            let handle = self.services.notifications.handle().clone();
            bridge::act(SCOPE, async move { handle.mark_seen().await });
        }
    }

    /// One application's notifications, as a collapsible card.
    fn build_group(&self, group: &GroupView, now: DateTime<Local>) -> gtk4::Box {
        let card = gtk4::Box::new(Orientation::Vertical, 0);
        card.add_css_class(classes::CARD);
        card.add_css_class(classes::NOTIFICATION_GROUP);

        let rows = gtk4::Box::new(Orientation::Vertical, 4);
        rows.add_css_class(classes::NOTIFICATION_GROUP_LIST);
        for notification in &group.notifications {
            rows.append(&self.build_row(notification, now));
        }

        // One arrow that turns, rather than two that swap: the chevron is the
        // handle on the stack below it, so it moves with it.
        let arrow = Image::from_icon_name(icons::EXPAND);
        arrow.add_css_class(classes::NOTIFICATION_CHEVRON);
        let chevron = RotateBox::new();
        chevron.set_child(&arrow);
        chevron.set_valign(Align::Center);

        // The whole header is the expander, so hitting the app name works.
        let expander = Button::new();
        expander.add_css_class(classes::NOTIFICATION_GROUP_HEADER);
        expander.set_focus_on_click(false);
        expander.set_hexpand(true);

        let content = gtk4::Box::new(Orientation::Horizontal, 8);
        let app_icon = icon::image(&group.newest().icon, ROW_ICON);
        app_icon.add_css_class(classes::NOTIFICATION_ICON);
        app_icon.set_valign(Align::Center);
        content.append(&app_icon);

        let text = gtk4::Box::new(Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.set_valign(Align::Center);

        let name = Label::new(Some(&group.app_name));
        name.add_css_class(classes::NOTIFICATION_APP);
        name.set_xalign(0.0);
        name.set_ellipsize(pango::EllipsizeMode::End);
        text.append(&name);

        // What is inside a closed group, on one line. A collapsed stack that
        // showed nothing but a name and a number is a list that has to be
        // opened five times before it can be read, and "3" is not the thing
        // the user came to catch up on. It goes when the group opens, in the
        // same frame the rows arrive, so the surface is resized once — and the
        // first row it uncovers says the same thing in full.
        let preview = Label::new(Some(group.newest().summary.as_str()));
        preview.add_css_class(classes::NOTIFICATION_PREVIEW);
        preview.set_xalign(0.0);
        preview.set_ellipsize(pango::EllipsizeMode::End);
        preview.set_single_line_mode(true);
        preview.set_visible(group.count() > 1);
        text.append(&preview);
        content.append(&text);

        if group.count() > 1 {
            let count = Label::new(Some(&group.count().to_string()));
            count.add_css_class(classes::NOTIFICATION_COUNT);
            count.set_valign(Align::Center);
            content.append(&count);
        }

        content.append(&chevron);
        expander.set_child(Some(&content));
        // Every button in the panel answers a press. This one is the largest
        // click target in the column and was the only one of its size that
        // showed nothing at all when it was hit.
        ripple::install(&expander);

        let header = gtk4::Box::new(Orientation::Horizontal, 2);
        header.append(&expander);

        let clear = Button::from_icon_name("user-trash-symbolic");
        clear.add_css_class(classes::NOTIFICATION_GROUP_CLEAR);
        clear.set_focus_on_click(false);
        clear.set_valign(Align::Center);
        clear.set_tooltip_text(Some(&format!("Clear {}", group.app_name)));
        ripple::install(&clear);
        clear.connect_clicked({
            let handle = self.services.notifications.handle().clone();
            let key = group.key.clone();
            move |button| {
                // The card is about to go; a second click on the way out would
                // close notifications that arrived in between.
                button.set_sensitive(false);
                let handle = handle.clone();
                let key = key.clone();
                bridge::act(SCOPE, async move { handle.clear_group(key).await });
            }
        });
        header.append(&clear);

        card.append(&header);

        // A group of one is its own summary, so it is simply open: expanding
        // it would show the same line again, and there is nothing to animate.
        if group.count() == 1 {
            chevron.set_visible(false);
            expander.set_sensitive(false);
            expander.add_css_class(classes::NOTIFICATION_GROUP_SINGLE);
            card.append(&rows);
            return card;
        }

        // The same slot Quick Settings' cards open in, for the same reason: the
        // panel is on a layer surface, so the height it asks the compositor for
        // changes once per toggle and what animates is where the rows are
        // painted inside the space that is already there.
        let section = Section::new(&rows);
        card.append(section.root());

        // A card the user had open before this rebuild comes back open, and
        // comes back open *silently*: the rebuild is a notification arriving
        // somewhere else, not a click, and nothing the user did should look
        // like it is being replayed at them.
        if self.expanded.borrow().contains(&group.key) {
            section.expand_now();
            preview.set_visible(false);
            chevron.set_angle(CHEVRON_TURN);
        }

        let rotation = Animation::new(&chevron);
        expander.connect_clicked({
            let open = Rc::clone(&self.expanded);
            let key = group.key.clone();
            move |_| {
                let expanded = !section.is_expanded();
                section.set_expanded(expanded);
                preview.set_visible(!expanded);
                turn(&chevron, &rotation, expanded);

                if expanded {
                    open.borrow_mut().insert(key.clone());
                } else {
                    open.borrow_mut().remove(&key);
                }
            }
        });

        card
    }

    /// One notification inside a group.
    fn build_row(&self, notification: &NotificationView, now: DateTime<Local>) -> gtk4::Box {
        let row = gtk4::Box::new(Orientation::Horizontal, 8);
        row.add_css_class(classes::NOTIFICATION_ROW);

        let text = gtk4::Box::new(Orientation::Vertical, 2);
        text.set_hexpand(true);

        let top = gtk4::Box::new(Orientation::Horizontal, 6);
        let summary = Label::new(Some(&notification.summary));
        summary.add_css_class(classes::NOTIFICATION_SUMMARY);
        summary.set_xalign(0.0);
        summary.set_hexpand(true);
        summary.set_ellipsize(pango::EllipsizeMode::End);
        summary.set_single_line_mode(true);
        // Baselines, not boxes: the age is drawn a size smaller than the
        // summary beside it, and aligning the two boxes at the top left it
        // sitting visibly high on the line.
        summary.set_valign(Align::Baseline);
        top.append(&summary);

        let age = Label::new(Some(&relative_time(notification.timestamp, now)));
        age.add_css_class(classes::NOTIFICATION_TIME);
        age.set_valign(Align::Baseline);
        age.set_tooltip_text(Some(&absolute_time(notification.timestamp)));
        self.ages
            .borrow_mut()
            .push((notification.timestamp, age.clone()));
        top.append(&age);
        text.append(&top);

        if !notification.body.is_empty() {
            let body = Label::new(None);
            body.add_css_class(classes::NOTIFICATION_BODY);
            body.set_xalign(0.0);
            body.set_wrap(true);
            body.set_wrap_mode(pango::WrapMode::WordChar);
            body.set_lines(2);
            body.set_ellipsize(pango::EllipsizeMode::End);
            markup::apply(&body, &notification.body);
            text.append(&body);
        }
        row.append(&text);

        let close = Button::from_icon_name("window-close-symbolic");
        close.add_css_class(classes::NOTIFICATION_CLOSE);
        close.set_focus_on_click(false);
        ripple::install(&close);
        close.set_valign(Align::Start);
        close.connect_clicked({
            let handle = self.services.notifications.handle().clone();
            let id = notification.id;
            move |button| {
                button.set_sensitive(false);
                let handle = handle.clone();
                bridge::act(SCOPE, async move {
                    handle.dismiss(id, CloseReason::Dismissed).await
                });
            }
        });
        row.append(&close);
        reveal_on_hover(&row, &close);

        // Clicking the row is the way back to whatever sent it: the sender's
        // default action if it offered one, its window either way, and the
        // notification off the list — a message the user has followed is a
        // message they have read. The panel goes with it, because it is now
        // standing in front of the window it just raised.
        let activate = GestureClick::new();
        activate.set_button(gdk::BUTTON_PRIMARY);
        activate.connect_released({
            let services = self.services.clone();
            let notification = notification.clone();
            move |gesture, _, _, _| {
                let surface = gesture
                    .widget()
                    .and_then(|widget| widget.root().and_then(|root| root.surface()));
                if !notifications::activate(&services, &notification, surface, SCOPE) {
                    let handle = services.notifications.handle().clone();
                    let id = notification.id;
                    bridge::act(SCOPE, async move {
                        handle.dismiss(id, CloseReason::Dismissed).await
                    });
                }
                crate::surfaces::popovers::close_all();
            }
        });
        row.add_controller(activate);

        row
    }
}

/// Whether drawing this state into the column counts as reading it.
///
/// Anything drawn into an *open* column has been seen: it is in the list under
/// the reader's eyes. Without that, a notification arriving while the panel is
/// open lights the unread dot on the very button the panel is hanging off — and
/// leaves it lit after the panel is closed on a list that was read as it
/// arrived. The mark clears `unseen_count`, so the render it causes finds
/// nothing left to do and the loop ends after one pass.
fn counts_as_read(on_screen: bool, unseen: usize) -> bool {
    on_screen && unseen > 0
}

/// Show a row's close button under the pointer, and nowhere else.
///
/// What GNOME's message list does, and a history of sixty rows is why: sixty
/// permanent ✕s read as sixty things to do rather than as a list to catch up
/// on. The button keeps its slot at every moment — it is faded, not hidden, so
/// revealing it cannot reflow the summary beside it — and it stays sensitive,
/// so the keyboard can still reach it. Focus reveals it too: a Tab that landed
/// on something invisible would be worse than a ✕ that was always there.
fn reveal_on_hover(row: &gtk4::Box, close: &Button) {
    fn set_revealed(button: &Button, revealed: bool) {
        button.set_opacity(f64::from(u8::from(revealed)));
    }

    set_revealed(close, false);
    let hovered = Rc::new(Cell::new(false));

    let motion = EventControllerMotion::new();
    motion.connect_enter({
        let close = close.clone();
        let hovered = Rc::clone(&hovered);
        move |_, _, _| {
            hovered.set(true);
            set_revealed(&close, true);
        }
    });
    motion.connect_leave({
        let close = close.clone();
        let hovered = Rc::clone(&hovered);
        move |_| {
            hovered.set(false);
            set_revealed(&close, close.has_focus());
        }
    });
    row.add_controller(motion);

    // On the button rather than through a captured clone of it: a handler that
    // held a reference to the widget it is connected to is a reference cycle,
    // and the row it is in would outlive the panel.
    close.connect_notify_local(Some("has-focus"), move |button, _| {
        set_revealed(button, hovered.get() || button.has_focus());
    });
}

/// Turn a group's chevron the way its stack is going.
///
/// Half a turn over exactly as long as the rows take, from wherever the arrow
/// currently is: a group closed halfway through opening turns its arrow back
/// from there rather than snapping upright first.
fn turn(chevron: &RotateBox, rotation: &Animation, expanded: bool) {
    let start = chevron.angle();
    let target = if expanded { CHEVRON_TURN } else { 0.0 };
    let distance = f64::from((target - start).abs()) / f64::from(CHEVRON_TURN);
    let duration = (REVEAL_MS as f64 * distance).round() as u64;

    let chevron = chevron.clone();
    rotation.start(
        AnimationParams::new(duration).with_easing(if expanded {
            Easing::EaseOutCubic
        } else {
            Easing::EaseInCubic
        }),
        Box::new(move |progress| chevron.set_angle(start + (target - start) * progress as f32)),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_on_screen_reads_what_it_draws() {
        // The panel is open and something has just landed in the list.
        assert!(counts_as_read(true, 1));
        // Nothing new: no call, and so no render loop.
        assert!(!counts_as_read(true, 0));
        // The panel is shut. The whole point of the count is that this is the
        // case that leaves the dot on the bar lit.
        assert!(!counts_as_read(false, 3));
    }
}
