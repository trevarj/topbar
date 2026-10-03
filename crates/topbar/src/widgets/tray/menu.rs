//! An application's own menu, drawn as one of the panel's popovers.
//!
//! ```text
//! ┌────────────────────────────┐   ┌────────────────────────────┐
//! │  Open Window               │   │ ‹  More                    │  ← back row
//! │  ✓ Show Notifications      │   │  About                     │
//! │  ────────────────────────  │   │  Report a Bug              │
//! │  ● Online                  │   └────────────────────────────┘
//! │  ○ Away                    │
//! │  Not Available             │  ← disabled: dimmed, inert
//! │  More                    › │
//! └────────────────────────────┘
//! ```
//!
//! Submenus open **in place**, with a row back to where they came from, rather
//! than as a second surface hanging off the first. The panel has exactly one
//! popover on screen at a time by construction, and a tray menu three levels
//! deep is common enough — every mail client has one — that a cascade would
//! spend the whole monitor.
//!
//! The rows are rebuilt on every open, and deliberately: a tray menu is built
//! by the application when it is asked for, and a remembered one would show
//! whatever was true the last time the user looked. Only the popover *shell*
//! is retained, which is what the framework's retention rule is actually about.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gtk4::prelude::*;
use gtk4::{Align, Button, Image, Label, Orientation, Separator};
use topbar_services::{MenuEvent, MenuKind, MenuNode, Services, ToggleKind};

use crate::bridge::{self, ActionScope};
use crate::style::classes;
use crate::surfaces::popovers::PopoverContent;
use crate::surfaces::search::ChoiceFilter;

use super::WIDGET_NAME;

/// The chevron on a row that leads somewhere.
const SUBMENU_ICON: &str = "go-next-symbolic";
/// The arrow on the row that leads back.
const BACK_ICON: &str = "go-previous-symbolic";
/// A set checkmark.
const CHECKED_ICON: &str = "object-select-symbolic";
/// A set radio button.
const RADIO_ICON: &str = "radio-checked-symbolic";
/// A toggle whose application does not know which way it is.
const MIXED_ICON: &str = "list-remove-symbolic";
/// Icon size on a menu row.
const ROW_ICON: i32 = 16;
/// What a menu with nothing in it says.
const EMPTY_LABEL: &str = "No menu";

/// One application's menu, as a popover.
pub struct TrayMenu {
    root: gtk4::Box,
    /// The row that leads back out of a submenu.
    back: gtk4::Box,
    back_label: Label,
    /// Where the rows go.
    list: gtk4::Box,
    filter: Rc<ChoiceFilter>,
    /// The item whose menu this is, and the menu it last fetched.
    showing: RefCell<Option<Showing>>,
    /// How deep into the menu the user has gone: the id of each submenu
    /// entered, so a refetched layout can be walked back down to where they
    /// were rather than snapping to the top.
    path: RefCell<Vec<i32>>,
    services: Services,
    me: Weak<Self>,
}

/// The menu currently on screen.
struct Showing {
    /// The tray item it belongs to.
    id: String,
    /// The layout, as it was last fetched.
    menu: MenuNode,
}

impl TrayMenu {
    /// Build the popover. One per tray widget, reused by every item.
    pub fn new(services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.add_css_class(classes::TRAY_MENU);

        let (back, back_label) = back_row();
        root.append(&back);
        let filter = ChoiceFilter::new(&root, "Filter menu choices");
        filter.search.set_visible(false);

        let list = gtk4::Box::new(Orientation::Vertical, 0);
        list.add_css_class(classes::TRAY_MENU_LIST);
        root.append(&list);

        Rc::new_cyclic(|me| Self {
            root,
            back,
            back_label,
            list,
            filter,
            showing: RefCell::new(None),
            path: RefCell::new(Vec::new()),
            services: services.clone(),
            me: me.clone(),
        })
    }

    /// Whose menu is on screen, if any.
    pub fn showing(&self) -> Option<String> {
        self.showing.borrow().as_ref().map(|open| open.id.clone())
    }

    /// Point the popover at `id` and fetch its menu.
    ///
    /// Called just before the popover is opened, so the surface has rows in it
    /// on the frame it appears rather than growing as the answer arrives.
    pub fn aim_at(&self, id: &str) {
        *self.path.borrow_mut() = Vec::new();
        self.filter.search.set_text("");
        let mut showing = self.showing.borrow_mut();
        if showing.as_ref().is_none_or(|open| open.id != id) {
            *showing = Some(Showing {
                id: id.to_string(),
                menu: MenuNode::default(),
            });
        }
        drop(showing);
        self.fetch();
    }

    /// The item's menu has gone: close down whatever is drawn.
    pub fn forget(&self) {
        *self.showing.borrow_mut() = None;
        *self.path.borrow_mut() = Vec::new();
    }

    /// Ask the application for its menu, then draw it.
    fn fetch(&self) {
        let Some(id) = self.showing() else {
            return;
        };
        let handle = self.services.tray.handle().clone();
        let me = self.me.clone();
        bridge::request(
            ActionScope::Toast {
                widget: WIDGET_NAME,
            },
            async move { handle.menu(&id).await },
            move |menu| {
                let Some(this) = me.upgrade() else {
                    return;
                };
                if let Some(showing) = this.showing.borrow_mut().as_mut() {
                    showing.menu = menu;
                }
                this.draw();
            },
        );
    }

    /// Draw the level the user is looking at.
    fn draw(&self) {
        self.filter.clear_rows();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let showing = self.showing.borrow();
        let Some(showing) = showing.as_ref() else {
            self.back.set_visible(false);
            return;
        };

        // Walk back down to where the user was. A refetched layout may have
        // lost the submenu they were in, in which case they come back up to
        // whatever level still exists rather than being shown an empty one.
        let mut level = &showing.menu;
        let mut reached = Vec::new();
        for id in self.path.borrow().iter() {
            match level.children.iter().find(|child| child.id == *id) {
                Some(child) => {
                    level = child;
                    reached.push(*id);
                }
                None => break,
            }
        }
        let depth = reached.len();
        if *self.path.borrow() != reached {
            self.filter.search.set_text("");
        }
        *self.path.borrow_mut() = reached;
        let choices = level.rows().any(|entry| entry.toggle != ToggleKind::None);
        self.filter.search.set_visible(choices);

        self.back.set_visible(depth > 0);
        if depth > 0 {
            self.back_label.set_text(&level.label);
        }

        if level.is_empty() {
            let empty = Label::new(Some(EMPTY_LABEL));
            empty.add_css_class(classes::TRAY_MENU_EMPTY);
            self.list.append(&empty);
            return;
        }

        let id = showing.id.clone();
        for entry in level.rows() {
            match entry.kind {
                MenuKind::Separator => {
                    let rule = Separator::new(Orientation::Horizontal);
                    rule.add_css_class(classes::TRAY_MENU_SEPARATOR);
                    self.list.append(&rule);
                }
                MenuKind::Standard => {
                    let row = self.row(entry, &id, choices);
                    if entry.toggle != ToggleKind::None {
                        self.filter.add(&row, entry.label.clone(), None);
                    }
                    self.list.append(&row);
                }
            }
        }
        self.filter.apply();
    }

    /// Build one row.
    fn row(&self, entry: &MenuNode, item: &str, choices: bool) -> gtk4::Widget {
        let row = gtk4::Box::new(Orientation::Horizontal, 8);
        row.add_css_class(classes::TRAY_MENU_ROW);

        // The mark column is present on every row of a menu that has any, so
        // the labels of a radio group line up with the ones around them.
        let mark = Image::new();
        mark.set_pixel_size(ROW_ICON);
        mark.add_css_class(classes::TRAY_MENU_MARK);
        match (entry.toggle, entry.toggle_state) {
            (ToggleKind::None, _) => mark.set_visible(false),
            (_, topbar_services::ToggleState::On) => {
                mark.set_icon_name(Some(if entry.toggle == ToggleKind::Radio {
                    RADIO_ICON
                } else {
                    CHECKED_ICON
                }))
            }
            (_, topbar_services::ToggleState::Indeterminate) => {
                mark.set_icon_name(Some(MIXED_ICON));
            }
            // Off: the column is held open, but nothing is drawn in it.
            (_, topbar_services::ToggleState::Off) => mark.set_icon_name(None),
        }
        row.append(&mark);

        if let Some(name) = entry.icon_name.as_deref() {
            let icon = Image::from_icon_name(name);
            icon.set_pixel_size(ROW_ICON);
            icon.add_css_class(classes::TRAY_MENU_ICON);
            row.append(&icon);
        }

        let label = Label::new(Some(&entry.label));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.add_css_class(classes::TRAY_MENU_LABEL);
        row.append(&label);

        if entry.has_submenu {
            let chevron = Image::from_icon_name(SUBMENU_ICON);
            chevron.set_pixel_size(ROW_ICON);
            chevron.add_css_class(classes::TRAY_MENU_CHEVRON);
            chevron.set_halign(Align::End);
            row.append(&chevron);
        }

        if choices {
            let role = match entry.toggle {
                ToggleKind::Radio => gtk4::AccessibleRole::MenuItemRadio,
                ToggleKind::Checkmark => gtk4::AccessibleRole::MenuItemCheckbox,
                ToggleKind::None => gtk4::AccessibleRole::MenuItem,
            };
            let button = Button::builder().accessible_role(role).build();
            button.add_css_class(classes::TRAY_MENU_ROW);
            if entry.toggle != ToggleKind::None {
                button.add_css_class(classes::PICKER_OPTION);
            }
            row.remove_css_class(classes::TRAY_MENU_ROW);
            button.set_child(Some(&row));
            button.set_sensitive(entry.enabled);
            if !entry.enabled {
                button.add_css_class(classes::DISABLED);
            }
            button.update_property(&[gtk4::accessible::Property::Label(&entry.label)]);
            if entry.toggle != ToggleKind::None {
                let checked = match entry.toggle_state {
                    topbar_services::ToggleState::On => gtk4::AccessibleTristate::True,
                    topbar_services::ToggleState::Off => gtk4::AccessibleTristate::False,
                    topbar_services::ToggleState::Indeterminate => gtk4::AccessibleTristate::Mixed,
                };
                button.update_state(&[gtk4::accessible::State::Checked(checked)]);
            }
            button.connect_clicked({
                let me = self.me.clone();
                let item = item.to_string();
                let entry_id = entry.id;
                let submenu = entry.has_submenu;
                move |_| {
                    if let Some(this) = me.upgrade() {
                        if submenu {
                            this.enter(entry_id);
                        } else {
                            this.choose(&item, entry_id);
                        }
                    }
                }
            });
            return button.upcast();
        }
        if !entry.enabled {
            // Dimmed *and* inert: a row the application has switched off must
            // not send an event when it is clicked.
            row.add_css_class(classes::DISABLED);
            return row.upcast();
        }

        row.set_cursor_from_name(Some("pointer"));
        let click = gtk4::GestureClick::new();
        click.set_button(gtk4::gdk::BUTTON_PRIMARY);
        click.connect_released({
            let me = self.me.clone();
            let item = item.to_string();
            let entry_id = entry.id;
            let submenu = entry.has_submenu;
            move |_, _, _, _| {
                let Some(this) = me.upgrade() else {
                    return;
                };
                if submenu {
                    this.enter(entry_id);
                } else {
                    this.choose(&item, entry_id);
                }
            }
        });
        row.add_controller(click);
        row.upcast()
    }

    /// Go into a submenu.
    ///
    /// The application is told first: several build their submenus only when
    /// they are hovered, and one that has just been asked to will have sent a
    /// fresh layout by the time it is fetched again.
    fn enter(&self, entry_id: i32) {
        self.filter.search.set_text("");
        self.path.borrow_mut().push(entry_id);
        self.draw();
        self.send(entry_id, MenuEvent::Hovered);
    }

    /// Walk into the last submenu on the level on screen.
    ///
    /// The smoke run's only way in: there is no synthetic pointer in the dev
    /// shell, so a submenu that only a click can reach could never be
    /// photographed.
    pub fn enter_last_submenu(&self) {
        let target = self.showing.borrow().as_ref().and_then(|showing| {
            showing
                .menu
                .rows()
                .filter(|row| row.has_submenu)
                .map(|row| row.id)
                .last()
        });
        if let Some(id) = target {
            self.enter(id);
        }
    }

    /// Come back out of one.
    fn leave(&self) {
        self.filter.search.set_text("");
        self.path.borrow_mut().pop();
        self.draw();
    }

    /// Choose a row: tell the application, then get out of its way.
    fn choose(&self, item: &str, entry_id: i32) {
        self.send(entry_id, MenuEvent::Clicked);
        crate::surfaces::popovers::dispatch(&topbar_core::ipc::PopoverAction::Hide(None), None);
        let _ = item;
    }

    /// Send one dbusmenu event for the item on screen.
    fn send(&self, entry_id: i32, event: MenuEvent) {
        let Some(id) = self.showing() else {
            return;
        };
        let handle = self.services.tray.handle().clone();
        bridge::act(
            ActionScope::Toast {
                widget: WIDGET_NAME,
            },
            async move { handle.menu_event(&id, entry_id, event).await },
        );
    }
}

impl PopoverContent for TrayMenu {
    fn root(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    /// Fetch the menu again every time the popover appears.
    fn refresh(&self) {
        self.fetch();
    }

    fn closed(&self) {
        *self.path.borrow_mut() = Vec::new();
        self.filter.search.set_text("");
    }
}

/// The row that leads back out of a submenu.
fn back_row() -> (gtk4::Box, Label) {
    let row = gtk4::Box::new(Orientation::Horizontal, 8);
    row.add_css_class(classes::TRAY_MENU_BACK);
    row.set_visible(false);
    row.set_cursor_from_name(Some("pointer"));

    let arrow = Image::from_icon_name(BACK_ICON);
    arrow.set_pixel_size(ROW_ICON);
    row.append(&arrow);

    let label = Label::new(None);
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row.append(&label);

    (row, label)
}

/// Wire the back row to the menu that owns it.
///
/// Separate from [`back_row`] because the menu does not exist yet when its own
/// widgets are being built.
pub fn connect_back(menu: &Rc<TrayMenu>) {
    let click = gtk4::GestureClick::new();
    click.set_button(gtk4::gdk::BUTTON_PRIMARY);
    click.connect_released({
        let me = Rc::downgrade(menu);
        move |_, _, _, _| {
            if let Some(menu) = me.upgrade() {
                menu.leave();
            }
        }
    });
    menu.back.add_controller(click);
}
