//! GTK-local type-ahead and fuzzy choice filtering. Services only see activation.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Button, Label, MenuButton, Orientation, Popover, SearchEntry, Widget, gdk, glib};
use topbar_services::rank_match;

use crate::style::classes;

pub fn matches(query: &str, label: &str) -> bool {
    let query = query.trim();
    query.is_empty() || rank_match(query, label).is_some()
}

pub fn focused(widget: &impl IsA<Widget>) -> bool {
    widget
        .as_ref()
        .root()
        .and_then(|root| root.focus())
        .is_some_and(|focus| focus == *widget.as_ref() || focus.is_ancestor(widget))
}

pub fn nested_focus(scope: &impl IsA<Widget>) -> bool {
    let mut focus = scope.as_ref().root().and_then(|root| root.focus());
    while let Some(widget) = focus {
        if widget == *scope.as_ref() {
            return false;
        }
        if widget.is::<Popover>() || widget.has_css_class(classes::PICKER_SCOPE) {
            return true;
        }
        focus = widget.parent();
    }
    false
}

pub fn shortcut(modifiers: gdk::ModifierType) -> bool {
    modifiers.intersects(
        gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::META_MASK
            | gdk::ModifierType::HYPER_MASK,
    )
}

fn text_modifiers(state: gdk::ModifierType, consumed: gdk::ModifierType) -> gdk::ModifierType {
    state - (consumed & (gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK))
}

fn routes_key(key: gdk::Key, modifiers: gdk::ModifierType, protected: bool, empty: bool) -> bool {
    if protected || shortcut(modifiers) {
        return false;
    }
    if key == gdk::Key::BackSpace {
        return !empty;
    }
    if matches!(key,
        gdk::Key::space | gdk::Key::KP_Space | gdk::Key::Menu
        | gdk::Key::Tab | gdk::Key::KP_Tab | gdk::Key::ISO_Left_Tab
        | gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Escape
        | gdk::Key::Up | gdk::Key::Down | gdk::Key::Left | gdk::Key::Right
        | gdk::Key::Home | gdk::Key::End | gdk::Key::Page_Up | gdk::Key::Page_Down
        | gdk::Key::KP_Up | gdk::Key::KP_Down | gdk::Key::KP_Left | gdk::Key::KP_Right
        | gdk::Key::KP_Home | gdk::Key::KP_End | gdk::Key::KP_Page_Up | gdk::Key::KP_Page_Down
        | gdk::Key::Insert | gdk::Key::Delete | gdk::Key::KP_Insert | gdk::Key::KP_Delete
        | gdk::Key::ISO_Level3_Shift | gdk::Key::ISO_Level5_Shift
        | gdk::Key::Mode_switch | gdk::Key::Num_Lock | gdk::Key::Scroll_Lock
        | gdk::Key::Pause | gdk::Key::Print | gdk::Key::Sys_Req | gdk::Key::Break | gdk::Key::Help
    ) || (gdk::Key::F1..=gdk::Key::F35).contains(&key)
        || (gdk::Key::KP_F1..=gdk::Key::KP_F4).contains(&key)
        || (gdk::Key::Shift_L..=gdk::Key::Hyper_R).contains(&key)
        // Extended hardware accelerators have no text; Unicode keyvals do.
        || (key > gdk::Key::Delete && key != gdk::Key::VoidSymbol && key.to_unicode().is_none())
    {
        return false;
    }
    // Unknown non-navigation events belong to the native IM context too:
    // an initial IME/compose/dead-key event need not contain a Unicode scalar.
    true
}

/// Forward the real event to GtkText, keeping compose/preedit and Unicode editing native.
/// Only this scope's choices are owned; nested pickers and ordinary editors keep their keys.
pub fn install(scope: &impl IsA<Widget>, target: impl Fn(gdk::Key) -> Option<Widget> + 'static) {
    scope.add_css_class(classes::PICKER_SCOPE);
    let scope_weak = scope.as_ref().downgrade();
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let forwarded = Rc::new(RefCell::new(None::<(u32, glib::WeakRef<Widget>)>));
    keys.connect_key_pressed({
        let forwarded = forwarded.clone();
        move |controller, key, code, modifiers| {
            let Some(scope) = scope_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let mut focus = scope.root().and_then(|root| root.focus());
            let mut protected = false;
            let mut owned = false;
            while let Some(widget) = focus {
                if widget == scope {
                    owned = true;
                    break;
                }
                if widget.is::<gtk4::Editable>()
                    || widget.is::<gtk4::PasswordEntry>()
                    || widget.is::<gtk4::TextView>()
                    || widget.is::<Popover>()
                    || widget.has_css_class(classes::PICKER_SCOPE)
                {
                    protected = true;
                    break;
                }
                focus = widget.parent();
            }
            // Consumed layout modifiers (e.g. AltGr) produce text, not shortcuts.
            let event = controller
                .current_event()
                .and_then(|event| event.downcast::<gdk::KeyEvent>().ok());
            let consumed = event.as_ref().map_or(gdk::ModifierType::empty(), |event| {
                event.consumed_modifiers()
            });
            let protected =
                protected || !owned || event.as_ref().is_some_and(|event| event.is_modifier());
            let modifiers = text_modifiers(modifiers, consumed);
            if !routes_key(key, modifiers, protected, false) {
                return glib::Propagation::Proceed;
            }
            let Some(target) =
                target(key).filter(|widget| widget.is_mapped() && widget.is_sensitive())
            else {
                return glib::Propagation::Proceed;
            };
            let Some(editable) = target.dynamic_cast_ref::<gtk4::Editable>() else {
                return glib::Propagation::Proceed;
            };
            if !editable.is_editable() || (key == gdk::Key::BackSpace && editable.text().is_empty())
            {
                return glib::Propagation::Proceed;
            }
            // Focus before changed handlers can remove the old result row. Grab-focus
            // may select all; type-ahead always continues at the query's tail.
            target.grab_focus();
            editable.set_position(-1);
            let end = editable.position();
            editable.select_region(end, end);
            let delegate = editable
                .delegate()
                .and_then(|delegate| delegate.dynamic_cast::<Widget>().ok())
                .unwrap_or(target);
            *forwarded.borrow_mut() = Some((code, delegate.downgrade()));
            controller.forward(&delegate);
            glib::Propagation::Stop
        }
    });
    keys.connect_key_released(move |controller, _, code, _| {
        let target = forwarded.borrow_mut().take();
        if let Some((pressed, target)) = target
            && pressed == code
            && let Some(target) = target.upgrade()
        {
            controller.forward(&target);
        }
    });
    scope.add_controller(keys);
}

struct FilterRow {
    widget: glib::WeakRef<Widget>,
    label: String,
    key: Option<String>,
}

/// Retain rows in service order and filter visibility, never their identity or state.
pub struct ChoiceFilter {
    pub search: SearchEntry,
    rows: RefCell<Vec<FilterRow>>,
    pinned: RefCell<Option<String>>,
    empty: Label,
    dirty: Cell<bool>,
}

impl ChoiceFilter {
    pub fn new(scope: &gtk4::Box, label: &str) -> Rc<Self> {
        let search = SearchEntry::new();
        search.set_placeholder_text(Some(label));
        search.add_css_class(classes::PICKER_SEARCH);
        search.update_property(&[gtk4::accessible::Property::Label(label)]);
        scope.append(&search);
        let empty = Label::new(Some("No matching choices"));
        empty.set_visible(false);
        scope.append(&empty);
        let filter = Rc::new(Self {
            search,
            rows: RefCell::new(Vec::new()),
            pinned: RefCell::new(None),
            empty,
            dirty: Cell::new(true),
        });
        filter.search.connect_changed({
            let weak = Rc::downgrade(&filter);
            move |_| {
                if let Some(filter) = weak.upgrade() {
                    filter.dirty.set(true);
                    filter.apply();
                }
            }
        });
        install(scope, {
            let search = filter.search.downgrade();
            move |_| search.upgrade().map(|entry| entry.upcast())
        });
        filter
    }

    pub fn set_visible(&self, visible: bool) {
        if self.search.is_visible() != visible {
            self.search.set_visible(visible);
            self.dirty.set(true);
        }
        self.apply();
    }

    pub fn clear_rows(&self) {
        if self
            .rows
            .borrow()
            .iter()
            .any(|row| row.widget.upgrade().is_some_and(|row| focused(&row)))
        {
            self.search.grab_focus();
        }
        self.rows.borrow_mut().clear();
        self.empty.set_visible(false);
        self.dirty.set(true);
    }

    /// A prompt can pin a stable key; otherwise its identity is the label.
    pub fn add(&self, widget: &impl IsA<Widget>, label: impl Into<String>, key: Option<&str>) {
        self.rows.borrow_mut().push(FilterRow {
            widget: widget.as_ref().downgrade(),
            label: label.into(),
            key: key.map(str::to_owned),
        });
        self.dirty.set(true);
    }

    /// Keep the option owning an active password/pairing prompt accessible.
    pub fn pin(&self, key: Option<&str>) {
        if self.pinned.borrow().as_deref() != key {
            *self.pinned.borrow_mut() = key.map(str::to_owned);
            self.dirty.set(true);
        }
        self.apply();
    }

    pub fn apply(&self) {
        if !self.dirty.replace(false) {
            return;
        }
        let query = self.search.text();
        let pinned = self.pinned.borrow();
        let rows = self.rows.borrow();
        let mut visible = false;
        for row in rows.iter() {
            if let Some(widget) = row.widget.upgrade() {
                let show = pinned.as_deref() == Some(row.key.as_deref().unwrap_or(&row.label))
                    || matches(&query, &row.label);
                widget.set_visible(show);
                visible |= show;
            }
        }
        self.empty.set_visible(
            self.search.is_visible() && !query.is_empty() && !rows.is_empty() && !visible,
        );
    }
}

type SelectedCallback = Box<dyn Fn(u32)>;

/// Native button/popover composition. A filtered position is never a selection.
pub struct SearchSelector {
    root: MenuButton,
    choices: Vec<String>,
    selected: Cell<u32>,
    callbacks: RefCell<Vec<SelectedCallback>>,
}

impl SearchSelector {
    pub fn new(label: &str, choices: &[&str]) -> Rc<Self> {
        let root = MenuButton::new();
        root.add_css_class(classes::PICKER_SELECTOR);
        root.update_property(&[gtk4::accessible::Property::Label(label)]);
        root.add_css_class(classes::PICKER_SCOPE);
        let popover = Popover::new();
        let content = gtk4::Box::new(Orientation::Vertical, 4);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(8);
        content.set_margin_end(8);
        let filter = ChoiceFilter::new(&content, label);
        let list = gtk4::Box::new(Orientation::Vertical, 2);
        let scroll = gtk4::ScrolledWindow::new();
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_max_content_height(300);
        scroll.set_propagate_natural_height(true);
        scroll.set_child(Some(&list));
        content.append(&scroll);
        popover.set_child(Some(&content));
        root.set_popover(Some(&popover));
        let selector = Rc::new(Self {
            root,
            choices: choices.iter().map(|choice| (*choice).to_owned()).collect(),
            selected: Cell::new(0),
            callbacks: RefCell::new(Vec::new()),
        });
        install(selector.root(), {
            let root = selector.root.downgrade();
            let entry = filter.search.downgrade();
            move |key| {
                let entry = entry.upgrade()?;
                if key == gdk::Key::BackSpace && (!entry.is_mapped() || entry.text().is_empty()) {
                    return None;
                }
                root.upgrade()?.popup();
                Some(entry.upcast())
            }
        });
        for (index, choice) in choices.iter().enumerate() {
            let button = Button::with_label(choice);
            button.add_css_class(classes::PICKER_OPTION);
            filter.add(&button, *choice, None);
            button.connect_clicked({
                let weak = Rc::downgrade(&selector);
                let popover = popover.downgrade();
                move |_| {
                    if let Some(selector) = weak.upgrade() {
                        let changed = selector.selected.replace(index as u32) != index as u32;
                        selector.update_label();
                        if let Some(popover) = popover.upgrade() {
                            popover.popdown();
                        }
                        if changed {
                            for callback in selector.callbacks.borrow().iter() {
                                callback(index as u32);
                            }
                        }
                    }
                }
            });
            list.append(&button);
        }
        let navigation = gtk4::EventControllerKey::new();
        navigation.set_propagation_phase(gtk4::PropagationPhase::Capture);
        navigation.connect_key_pressed({
            let filter = filter.clone();
            let popover = popover.downgrade();
            move |_, key, _, modifiers| {
                if shortcut(modifiers) || modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
                    return glib::Propagation::Proceed;
                }
                if key == gdk::Key::Escape {
                    if let Some(popover) = popover.upgrade() {
                        popover.popdown();
                    }
                    return glib::Propagation::Stop;
                }
                if !matches!(key, gdk::Key::Up | gdk::Key::Down) {
                    return glib::Propagation::Proceed;
                }
                let rows = filter.rows.borrow();
                let current = rows
                    .iter()
                    .position(|row| row.widget.upgrade().is_some_and(|row| focused(&row)));
                let next = if key == gdk::Key::Down {
                    rows.iter()
                        .skip(current.map_or(0, |index| index + 1))
                        .find_map(|row| {
                            row.widget
                                .upgrade()
                                .filter(|row| row.is_visible() && row.is_sensitive())
                        })
                } else {
                    rows.iter()
                        .take(current.unwrap_or(rows.len()))
                        .rev()
                        .find_map(|row| {
                            row.widget
                                .upgrade()
                                .filter(|row| row.is_visible() && row.is_sensitive())
                        })
                };
                if let Some(next) = next {
                    next.grab_focus();
                }
                glib::Propagation::Stop
            }
        });
        content.add_controller(navigation);
        // Signals own the filter; no application-side selector or GTK-private model.
        // Reset on show, not during the containing window's teardown.
        popover.connect_show(move |_| {
            filter.search.set_text("");
            filter.apply();
            filter.search.grab_focus();
        });
        selector.update_label();
        selector
    }

    pub fn root(&self) -> &MenuButton {
        &self.root
    }
    pub fn selected(&self) -> u32 {
        self.selected.get()
    }
    pub fn set_selected(&self, index: u32) {
        if (index as usize) < self.choices.len() {
            self.selected.set(index);
            self.update_label();
        }
    }
    pub fn connect_selected(&self, callback: impl Fn(u32) + 'static) {
        self.callbacks.borrow_mut().push(Box::new(callback));
    }
    fn update_label(&self) {
        let value = self
            .choices
            .get(self.selected.get() as usize)
            .map_or("", String::as_str);
        self.root.set_label(value);
        self.root
            .update_property(&[gtk4::accessible::Property::Description(value)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_preserves_editors_shortcuts_and_activation() {
        let none = gdk::ModifierType::empty();
        for key in [
            gdk::Key::Left,
            gdk::Key::Home,
            gdk::Key::Return,
            gdk::Key::space,
            gdk::Key::Escape,
        ] {
            assert!(!routes_key(key, none, false, false));
        }
        for modifier in [
            gdk::ModifierType::CONTROL_MASK,
            gdk::ModifierType::ALT_MASK,
            gdk::ModifierType::SUPER_MASK,
            gdk::ModifierType::META_MASK,
            gdk::ModifierType::HYPER_MASK,
        ] {
            assert!(!routes_key(gdk::Key::a, modifier, false, false));
        }
        assert!(!routes_key(gdk::Key::a, none, true, false)); // editor or nested scope
        assert!(!routes_key(gdk::Key::BackSpace, none, false, true));
        assert!(routes_key(gdk::Key::BackSpace, none, false, false));
        assert!(routes_key(
            gdk::Key::a,
            gdk::ModifierType::SHIFT_MASK,
            false,
            true
        ));
        assert!(routes_key(gdk::Key::dead_acute, none, false, true));
        assert!(routes_key(gdk::Key::Multi_key, none, false, true));
        assert!(routes_key(gdk::Key::Hangul, none, false, true));
        assert!(routes_key(gdk::Key::Henkan, none, false, true));
        assert!(!routes_key(gdk::Key::F1, none, false, false));
        assert!(!routes_key(gdk::Key::AudioRaiseVolume, none, false, false));
        assert!(!routes_key(gdk::Key::Shift_L, none, false, false));
        let altgr = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK;
        assert!(routes_key(
            gdk::Key::a,
            text_modifiers(altgr, altgr),
            false,
            true
        ));
        assert!(!routes_key(
            gdk::Key::a,
            text_modifiers(gdk::ModifierType::SUPER_MASK, gdk::ModifierType::SUPER_MASK),
            false,
            true
        ));
    }
}
