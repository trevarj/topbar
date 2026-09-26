//! Who owns a popover, and who is allowed to open it.
//!
//! A widget hands [`attach`] a builder and gets a handle back. The builder
//! runs **once**, the first time the popover is opened; from then on the same
//! widget tree is re-parented into the host on every open and unparented on
//! every close. Nothing is rebuilt per cycle, which is what keeps a popover
//! that is opened a thousand times from allocating a thousand widget trees —
//! and dropping the handle (a bar rebuild, a monitor going away) drops the
//! content, and with it every [`BindingGuard`](crate::bridge::BindingGuard)
//! inside it.
//!
//! Every handle also registers itself under its widget name and connector, so
//! `topbar popover show clock` has something to address. M8 wires the
//! IPC server to [`dispatch`]; the map it needs exists now.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gtk4::prelude::*;
use topbar_core::ipc::PopoverAction;
use tracing::{debug, info, warn};

use crate::bar::BarContext;
use crate::style::classes;
use crate::surfaces::layer_popover::{Anchored, LayerPopover};

/// Environment variable that auto-opens a widget's popover after start-up.
const SMOKE_ENV: &str = "TOPBAR_SMOKE_OPEN";
/// How long the smoke hook waits for the bar to settle before opening.
const SMOKE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
/// Gap between toggles when the smoke hook is asked for repeat cycles.
///
/// Comfortably longer than either animation: the nested niri the smoke test
/// runs in renders in software, and a screenshot of a half-drawn popover
/// proves nothing.
const SMOKE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);

/// The content of one widget's popover.
///
/// Implementors are built once and kept for the widget's lifetime, so they own
/// their own state and re-render on demand rather than being reconstructed.
pub trait PopoverContent {
    /// The widget parented into the popover host.
    fn root(&self) -> gtk4::Widget;

    /// Re-render from current state.
    ///
    /// Called every time the popover opens. Retained content would otherwise
    /// show whatever was true when it was last on screen.
    fn refresh(&self);

    /// The popover has left the screen.
    ///
    /// The mirror of [`PopoverContent::refresh`], for content that costs
    /// something to keep up to date — the media card polls a player for its
    /// position, and there is no sense in doing that for a card nobody can
    /// see. Most content has nothing to do here.
    fn closed(&self) {}
}

/// A widget's popover: its builder, its retained content, and its host.
struct Entry {
    name: String,
    host: Rc<LayerPopover>,
    anchor: gtk4::Widget,
    build: Box<dyn Fn() -> Rc<dyn PopoverContent>>,
    content: RefCell<Option<Rc<dyn PopoverContent>>>,
}

impl Entry {
    /// The content, building it on first use.
    fn content(&self) -> Rc<dyn PopoverContent> {
        if let Some(content) = self.content.borrow().as_ref() {
            return Rc::clone(content);
        }
        debug!("building `{}` popover content", self.name);
        let content = (self.build)();
        content.root().add_css_class(classes::POPOVER_SURFACE);
        content
            .root()
            .add_css_class(&format!("{}-popover", self.name.replace('_', "-")));
        *self.content.borrow_mut() = Some(Rc::clone(&content));
        content
    }

    /// What the host needs to put this popover on screen.
    fn anchored(&self) -> Anchored {
        let content = self.content();
        let root = content.root();
        let closing = Rc::clone(&content);
        Anchored {
            name: self.name.clone(),
            content: root,
            anchor: self.anchor.clone(),
            refresh: Rc::new(move || content.refresh()),
            closed: Rc::new(move || closing.closed()),
        }
    }

    fn open(&self) {
        if !crate::surfaces::modal::input_available() {
            return;
        }
        self.host.open(self.anchored());
    }

    fn close(&self) {
        if self.host.is_open(&self.name) {
            self.host.close();
        }
    }

    fn toggle(&self) {
        if !self.host.is_open(&self.name) && !crate::surfaces::modal::input_available() {
            return;
        }
        self.host.toggle(self.anchored());
    }
}

/// A widget's claim on its popover.
///
/// Keep it alive for as long as the widget lives: dropping it unregisters the
/// popover and releases the content it was holding.
pub struct PopoverHandle {
    entry: Rc<Entry>,
}

impl Drop for PopoverHandle {
    fn drop(&mut self) {
        // A rebuilt widget must not leave its old tree on screen.
        self.entry.close();
        unregister(&self.entry);
    }
}

/// Give `anchor` a popover built by `build`.
///
/// A primary-button gesture is installed on the anchor: clicking it opens the
/// popover, and clicking it again — including part-way through the close
/// animation — closes it. While it is open the anchor wears
/// [`classes::CHECKED`], which the host releases when the close animation
/// finishes.
pub fn attach<F>(
    context: &BarContext,
    name: &str,
    anchor: &impl IsA<gtk4::Widget>,
    build: F,
) -> PopoverHandle
where
    F: Fn() -> Rc<dyn PopoverContent> + 'static,
{
    let entry = Rc::new(Entry {
        name: name.to_string(),
        host: context.popovers(),
        anchor: anchor.as_ref().clone(),
        build: Box::new(build),
        content: RefCell::new(None),
    });

    let click = gtk4::GestureClick::new();
    click.set_button(gtk4::gdk::BUTTON_PRIMARY);
    // Released rather than pressed, so the press-state feedback on the button
    // completes before the surface it belongs to changes underneath it.
    click.connect_released({
        let entry = Rc::downgrade(&entry);
        move |_, _, _, _| {
            if let Some(entry) = entry.upgrade() {
                entry.toggle();
            }
        }
    });
    anchor.as_ref().add_controller(click);

    register(&context.connector, &entry);
    PopoverHandle { entry }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// One registered popover, held weakly so a dropped handle cannot be revived.
struct Registration {
    connector: String,
    entry: Weak<Entry>,
}

thread_local! {
    /// Every attached popover, in registration order.
    ///
    /// A `Vec` rather than a map: there is one entry per widget per monitor,
    /// so the whole list fits in a cache line's worth of pointers and lookups
    /// are rare (an IPC command, or the smoke hook).
    static REGISTRY: RefCell<Vec<Registration>> = const { RefCell::new(Vec::new()) };
}

fn register(connector: &str, entry: &Rc<Entry>) {
    REGISTRY.with_borrow_mut(|registry| {
        registry.retain(|registration| registration.entry.strong_count() > 0);
        registry.push(Registration {
            connector: connector.to_string(),
            entry: Rc::downgrade(entry),
        });
    });
    debug!("popover `{}` registered on {connector}", entry.name);
}

fn unregister(entry: &Rc<Entry>) {
    REGISTRY.with_borrow_mut(|registry| {
        registry.retain(|registration| {
            registration
                .entry
                .upgrade()
                .is_some_and(|registered| !Rc::ptr_eq(&registered, entry))
        });
    });
}

/// Apply an IPC popover action, returning whether anything answered it.
///
/// `connector` narrows the search to one monitor — M8 will pass the focused
/// output's connector so `topbar popover show clock` opens on the
/// monitor the user is looking at. With no connector, or one that matches
/// nothing, the first registered popover for that widget answers.
pub fn dispatch(action: &PopoverAction, connector: Option<&str>) -> bool {
    match action {
        // A registered smoke action first, so `topbar popover show
        // quick-settings-wifi-password` reaches something that is not a
        // popover at all. That is how a driver sequences several steps inside
        // one session: `TOPBAR_SMOKE_OPEN` fires exactly once, at start-up.
        // Debug builds only — `smoke_action` registers nothing otherwise.
        PopoverAction::Show(widget) if smoke_action(widget) => true,
        PopoverAction::Show(widget) => with_entry(widget, connector, Entry::open),
        PopoverAction::Toggle(widget) => with_entry(widget, connector, Entry::toggle),
        PopoverAction::Hide(Some(widget)) => with_entry(widget, connector, Entry::close),
        PopoverAction::Hide(None) => close_all(),
    }
}

/// Close whatever popover is on screen, returning whether there was one.
///
/// Used by anything that sends the user somewhere else: activating a
/// notification in the control panel raises another application's window, and a
/// panel left open over it is standing in front of what was just asked for.
pub fn close_all() -> bool {
    let mut closed = false;
    for entry in live_entries() {
        entry.close();
        closed = true;
    }
    closed
}

/// Run `action` on the named widget's popover.
///
/// Widget names are written with underscores everywhere inside the panel; the
/// CLI's hyphens are normalised here so `quick-settings` finds
/// `quick_settings`.
fn with_entry(widget: &str, connector: Option<&str>, action: impl Fn(&Entry)) -> bool {
    let wanted = widget.replace('-', "_");
    let matching: Vec<Rc<Entry>> = REGISTRY.with_borrow(|registry| {
        registry
            .iter()
            .filter(|registration| {
                connector.is_none_or(|connector| registration.connector == connector)
            })
            .filter_map(|registration| registration.entry.upgrade())
            .filter(|entry| entry.name == wanted)
            .collect()
    });

    // A connector that matches nothing falls back to any monitor rather than
    // silently doing nothing: a popover the user asked for should appear.
    let Some(entry) = matching.first() else {
        return match connector {
            Some(_) => with_entry(widget, None, action),
            None => {
                warn!("no popover registered for `{widget}`");
                false
            }
        };
    };
    action(entry);
    true
}

/// One thing the smoke hook can open that is not a popover.
type SmokeAction = (String, Rc<dyn Fn()>);

thread_local! {
    /// Things the smoke hook can open that are not popovers.
    ///
    /// The location dialog is a modal on a layer surface of its own, so it is
    /// not in the popover registry and `TOPBAR_SMOKE_OPEN` has no other way to
    /// reach it. Later milestones with their own surfaces register here too.
    static SMOKE_ACTIONS: RefCell<Vec<SmokeAction>> = const { RefCell::new(Vec::new()) };
}

/// Let `TOPBAR_SMOKE_OPEN=<name>` run `action`.
///
/// Debug builds only, and the last registration for a name wins — with two
/// monitors there are two weather widgets, and either one's dialog is as good
/// as the other's for a screenshot.
pub fn register_smoke_action(name: &str, action: impl Fn() + 'static) {
    if !cfg!(debug_assertions) {
        return;
    }
    SMOKE_ACTIONS.with_borrow_mut(|actions| {
        actions.retain(|(registered, _)| registered != name);
        actions.push((name.to_string(), Rc::new(action)));
    });
}

/// Run a registered smoke action, if `name` names one.
fn smoke_action(name: &str) -> bool {
    let action = SMOKE_ACTIONS.with_borrow(|actions| {
        actions
            .iter()
            .find(|(registered, _)| registered == name)
            .map(|(_, action)| Rc::clone(action))
    });
    match action {
        Some(action) => {
            action();
            true
        }
        None => false,
    }
}

/// Every popover whose widget is still alive.
fn live_entries() -> Vec<Rc<Entry>> {
    REGISTRY.with_borrow(|registry| {
        registry
            .iter()
            .filter_map(|registration| registration.entry.upgrade())
            .collect()
    })
}

/// Drive a widget's popover from the environment: `TOPBAR_SMOKE_OPEN`.
///
/// The nested-niri smoke test has no way to click anything — there is no
/// synthetic pointer input in the dev shell — so this is how a screenshot of
/// an *open* popover gets taken before M8's IPC server exists.
///
/// The value is `<widget>` to open it a second after start-up and leave it
/// open, or `<widget>:<n>` for `n` toggles [`SMOKE_INTERVAL`] apart. An even
/// count ends closed, which is how teardown is checked; an odd one ends open,
/// which is how the reopen-onto-retained-content path is caught in a
/// screenshot without having to race the animation. Debug builds only; the
/// packaged binary ignores the variable entirely.
pub fn install_smoke_hook() {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(value) = std::env::var_os(SMOKE_ENV) else {
        return;
    };
    let Some(value) = value.to_str() else {
        warn!("{SMOKE_ENV} is not valid UTF-8");
        return;
    };

    let (widget, toggles) = match value.split_once(':') {
        Some((widget, count)) => match count.parse::<u32>() {
            Ok(0) | Err(_) => {
                warn!("{SMOKE_ENV}={value}: `{count}` is not a positive toggle count");
                return;
            }
            Ok(toggles) => (widget.to_string(), toggles),
        },
        None => (value.to_string(), 1),
    };

    info!("{SMOKE_ENV}={value}: {toggles} toggle(s) of `{widget}`, from {SMOKE_DELAY:?}");

    gtk4::glib::timeout_add_local_once(SMOKE_DELAY, move || {
        // Registered actions first: `weather-setup` is a dialog, not a
        // popover, and there is nothing to toggle afterwards.
        if smoke_action(&widget) {
            return;
        }
        if !dispatch(&PopoverAction::Show(widget.clone()), None) {
            warn!("{SMOKE_ENV}: `{widget}` has no popover");
            return;
        }
        let mut left = toggles - 1;
        if left == 0 {
            return;
        }
        gtk4::glib::timeout_add_local(SMOKE_INTERVAL, move || {
            dispatch(&PopoverAction::Toggle(widget.clone()), None);
            left -= 1;
            if left == 0 {
                gtk4::glib::ControlFlow::Break
            } else {
                gtk4::glib::ControlFlow::Continue
            }
        });
    });
}
