//! The location dialog: a modal on a layer surface of its own.
//!
//! ```text
//! window .location-window          layer Overlay, no anchors, so centred
//! └── .location-dialog             the modal itself
//!
//! window .location-window          the dimmed backdrop, full screen
//! └── .location-backdrop           a click on it cancels
//! ```
//!
//! A `GtkWindow` would be an xdg-toplevel, which under a layer-shell panel
//! means the compositor decides where it goes and the bar is not its parent.
//! Two layer surfaces put it exactly in the middle of the monitor the user
//! clicked on, above everything, with the rest of the screen dimmed — which is
//! what "modal" looks like.
//!
//! Nothing is committed until Save, so Cancel, Escape and a click on the
//! backdrop are all the same operation: close and change nothing.
//!
//! # Environment
//!
//! `TOPBAR_SMOKE_QUERY` seeds the search box and runs the search as soon as
//! the dialog opens. There is no synthetic pointer or keyboard in the dev
//! shell, so it is the only way the visual smoke run can screenshot a dialog
//! with results in it. Debug builds only.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{Align, Button, Entry, Expander, Label, Orientation, Window, gdk, glib, pango};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use topbar_core::config::WeatherConfig;
use topbar_services::weather::valid_coordinates;
use topbar_services::{GeocodeResult, Runtime, Services};
use tracing::{debug, warn};

use crate::bridge::{self, ActionScope};
use crate::style::classes;
use crate::surfaces::popovers;

/// Where this dialog's failures are reported.
const SCOPE: ActionScope = ActionScope::Toast { widget: "weather" };
/// How long the dialog waits after a keystroke before searching.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Shortest query worth sending. One letter matches half the planet.
const MIN_QUERY: usize = 2;
/// Width of the dialog, in pixels.
const WIDTH: i32 = 400;
/// Seeds the search box for the visual smoke run.
const SMOKE_QUERY_ENV: &str = "TOPBAR_SMOKE_QUERY";

/// The hint the dialog opens with.
const HINT: &str = "Search for a city or enter coordinates.";
/// Shown while a search is out.
const SEARCHING: &str = "Searching…";
/// Shown when Save is pressed with nothing to save. Ported from v1.
const EMPTY_QUERY: &str = "Enter a city to search.";
/// Shown when the geocoder matched nothing. Ported from v1.
const NO_RESULTS: &str = "No city found.";
/// Shown when the search request itself failed. Ported from v1.
const SEARCH_FAILED: &str = "City search failed.";
/// Shown for coordinates that are not numbers. Ported from v1.
const NOT_NUMBERS: &str = "Latitude and longitude must be numbers.";
/// Shown for coordinates that are not on Earth. Ported from v1.
const OUT_OF_RANGE: &str = "Latitude or longitude is out of range.";

thread_local! {
    /// The dialog on screen. There is at most one, and opening it again while
    /// it is up simply brings it forward.
    static CURRENT: RefCell<Option<Rc<Dialog>>> = const { RefCell::new(None) };
}

/// Open the location dialog on the monitor `anchor` is on.
pub fn present(config: &WeatherConfig, services: &Services, anchor: &impl IsA<gtk4::Widget>) {
    if let Some(dialog) = CURRENT.with_borrow(|current| current.clone()) {
        dialog.window.present();
        return;
    }

    // The gear that opened this usually lives inside a popover, and a menu
    // still on screen behind a modal reads as two things being open at once.
    close_popovers();
    // Again once the event that got us here has finished propagating. The
    // other way in is a click on the widget itself, which the popover's own
    // gesture is also watching: claiming the sequence should stop it, but
    // "should" is not something to leave a stray menu behind a modal on.
    glib::idle_add_local_once(close_popovers);

    let dialog = Dialog::new(config, services, monitor_of(anchor).as_ref());
    CURRENT.with_borrow_mut(|current| *current = Some(Rc::clone(&dialog)));
    dialog.open();
}

/// Take down whatever popover is on screen.
fn close_popovers() {
    popovers::dispatch(&topbar_core::ipc::PopoverAction::Hide(None), None);
}

/// Close whatever is open. Used by the panel's own teardown and by Escape.
fn dismiss() {
    let dialog = CURRENT.with_borrow_mut(|current| current.take());
    if let Some(dialog) = dialog {
        dialog.window.set_visible(false);
        dialog.backdrop.set_visible(false);
        dialog.window.close();
        dialog.backdrop.close();
    }
}

/// The monitor a widget is being displayed on.
fn monitor_of(anchor: &impl IsA<gtk4::Widget>) -> Option<gdk::Monitor> {
    let widget = anchor.as_ref();
    let display = widget.display();
    let surface = widget
        .root()
        .and_then(|root| root.downcast::<Window>().ok())
        .and_then(|window| window.surface());

    surface
        .and_then(|surface| display.monitor_at_surface(&surface))
        .or_else(|| display.monitors().item(0).and_downcast::<gdk::Monitor>())
}

/// The dialog and everything it is holding on to.
struct Dialog {
    window: Window,
    backdrop: Window,
    search: Entry,
    results: gtk4::Box,
    status: Label,
    latitude: Entry,
    longitude: Entry,
    advanced: Expander,
    /// The place the user picked, so Save can keep its name.
    selected: RefCell<Option<GeocodeResult>>,
    /// The pending debounce timer, cancelled by the next keystroke.
    timer: RefCell<Option<glib::SourceId>>,
    /// Bumped by every search, so a slow answer cannot overwrite a newer one.
    generation: Cell<u64>,
    services: Services,
}

impl Dialog {
    fn new(
        config: &WeatherConfig,
        services: &Services,
        monitor: Option<&gdk::Monitor>,
    ) -> Rc<Self> {
        // The backdrop is built first so the compositor stacks it below the
        // dialog: layer surfaces in one layer keep their creation order.
        let backdrop = build_backdrop(monitor);
        let window = build_window(monitor);

        let root = gtk4::Box::new(Orientation::Vertical, 10);
        root.add_css_class(classes::LOCATION_DIALOG);
        root.set_size_request(WIDTH, -1);
        // The CSS shadow belongs to the dialog, not its rectangular window.
        // Reserve enough space for its 24px blur on every side.
        root.set_margin_start(32);
        root.set_margin_end(32);
        root.set_margin_top(32);
        root.set_margin_bottom(32);

        let title = Label::new(Some("Weather location"));
        title.add_css_class(classes::LOCATION_TITLE);
        title.set_xalign(0.0);

        let search = Entry::new();
        search.add_css_class(classes::LOCATION_SEARCH);
        search.set_placeholder_text(Some("Search for a city"));
        search.set_primary_icon_name(Some("system-search-symbolic"));

        let results = gtk4::Box::new(Orientation::Vertical, 2);
        results.add_css_class(classes::LOCATION_RESULTS);

        let status = Label::new(Some(HINT));
        status.add_css_class(classes::LOCATION_ERROR);
        status.set_xalign(0.0);
        status.set_wrap(true);

        // --- Advanced -------------------------------------------------------
        let coordinates = gtk4::Box::new(Orientation::Horizontal, 8);
        coordinates.set_margin_top(8);

        let latitude = coordinate_entry("Latitude");
        let longitude = coordinate_entry("Longitude");
        if let (Some(lat), Some(lon)) = (config.latitude, config.longitude) {
            latitude.set_text(&lat.to_string());
            longitude.set_text(&lon.to_string());
        }
        coordinates.append(&latitude);
        coordinates.append(&longitude);

        let advanced = Expander::new(Some("Advanced"));
        advanced.add_css_class(classes::LOCATION_ADVANCED);
        advanced.set_child(Some(&coordinates));

        // --- actions --------------------------------------------------------
        let actions = gtk4::Box::new(Orientation::Horizontal, 8);
        actions.add_css_class(classes::LOCATION_ACTIONS);
        actions.set_halign(Align::End);

        let cancel = Button::with_label("Cancel");
        cancel.add_css_class(classes::DIALOG_BUTTON);
        let save = Button::with_label("Save");
        save.add_css_class(classes::DIALOG_BUTTON);
        save.add_css_class(classes::DIALOG_BUTTON_PRIMARY);
        actions.append(&cancel);
        actions.append(&save);

        root.append(&title);
        root.append(&search);
        root.append(&results);
        root.append(&status);
        root.append(&advanced);
        root.append(&actions);
        window.set_child(Some(&root));

        let dialog = Rc::new(Self {
            window,
            backdrop,
            search,
            results,
            status,
            latitude,
            longitude,
            advanced,
            selected: RefCell::new(None),
            timer: RefCell::new(None),
            generation: Cell::new(0),
            services: services.clone(),
        });

        dialog.wire(&cancel, &save);
        dialog
    }

    /// Connect everything that reacts to the user.
    fn wire(self: &Rc<Self>, cancel: &Button, save: &Button) {
        // Type-ahead: every keystroke restarts the timer, and only the pause
        // sends a request. A search per character would be five requests for
        // "Paris" and a rate limit for the sixth.
        self.search.connect_changed({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.schedule_search();
                }
            }
        });
        // Enter searches at once rather than waiting out the debounce.
        self.search.connect_activate({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.search_now();
                }
            }
        });

        cancel.connect_clicked(|_| dismiss());
        save.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.save();
                }
            }
        });

        // Escape and a click on the backdrop are both Cancel.
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(|_, key, _, _| {
            if key == gdk::Key::Escape {
                dismiss();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);

        let click = gtk4::GestureClick::new();
        click.set_button(0);
        click.connect_released(|_, _, _, _| dismiss());
        self.backdrop.add_controller(click);
    }

    /// Put it on screen.
    fn open(self: &Rc<Self>) {
        // Order matters: the backdrop maps first so it stays underneath.
        self.backdrop.set_visible(true);
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        self.window.present();
        self.search.grab_focus();
        self.seed_from_environment();
    }

    /// Debug hook: run one search as though the user had typed it.
    fn seed_from_environment(self: &Rc<Self>) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Some(query) = std::env::var_os(SMOKE_QUERY_ENV).and_then(|q| q.into_string().ok())
        else {
            return;
        };
        if query.trim().is_empty() {
            return;
        }
        debug!("{SMOKE_QUERY_ENV}={query}: searching as though it had been typed");
        self.search.set_text(&query);
        self.search_now();
    }

    /// Restart the type-ahead timer.
    fn schedule_search(self: &Rc<Self>) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(DEBOUNCE, move || {
            if let Some(dialog) = weak.upgrade() {
                *dialog.timer.borrow_mut() = None;
                dialog.search_now();
            }
        });
        *self.timer.borrow_mut() = Some(timer);
    }

    /// Search for whatever is in the entry.
    fn search_now(self: &Rc<Self>) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }

        let query = self.search.text().trim().to_string();
        // Below the minimum the dialog goes quiet rather than complaining:
        // the user is still typing, and an error under a half-typed word is
        // noise. Save is where an empty query becomes a message.
        if query.chars().count() < MIN_QUERY {
            self.clear_results();
            self.set_status(HINT);
            return;
        }

        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.set_status(SEARCHING);

        let handle = self.services.weather.handle().clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let found = Runtime::handle()
                .spawn(async move { handle.search(query).await })
                .await;

            let Some(dialog) = weak.upgrade() else {
                return;
            };
            // A slower earlier search must not overwrite a newer one's results.
            if dialog.generation.get() != generation {
                return;
            }

            match found {
                Ok(Ok(results)) if results.is_empty() => {
                    dialog.clear_results();
                    dialog.set_status(NO_RESULTS);
                }
                Ok(Ok(results)) => {
                    dialog.show_results(&results);
                    dialog.set_status(HINT);
                }
                Ok(Err(error)) => {
                    warn!("the weather location search failed: {error}");
                    dialog.clear_results();
                    dialog.set_status(SEARCH_FAILED);
                }
                Err(error) => {
                    warn!("the weather location search task failed: {error}");
                    dialog.clear_results();
                    dialog.set_status(SEARCH_FAILED);
                }
            }
        });
    }

    /// Draw the places the geocoder found.
    fn show_results(self: &Rc<Self>, results: &[GeocodeResult]) {
        self.clear_results();
        for result in results {
            let row = Button::with_label(&result.label);
            row.add_css_class(classes::LOCATION_RESULT);
            if let Some(label) = row.child().and_downcast::<Label>() {
                label.set_xalign(0.0);
                label.set_ellipsize(pango::EllipsizeMode::End);
            }
            row.connect_clicked({
                let weak = Rc::downgrade(self);
                let result = result.clone();
                move |row| {
                    if let Some(dialog) = weak.upgrade() {
                        dialog.select(row, &result);
                    }
                }
            });
            self.results.append(&row);
        }
    }

    /// Take one of them.
    fn select(self: &Rc<Self>, row: &Button, result: &GeocodeResult) {
        let mut child = self.results.first_child();
        while let Some(widget) = child {
            widget.remove_css_class(classes::LOCATION_RESULT_SELECTED);
            child = widget.next_sibling();
        }
        row.add_css_class(classes::LOCATION_RESULT_SELECTED);

        // The coordinate entries follow the selection, so Advanced always
        // shows what is about to be saved and Save has one thing to read.
        self.latitude.set_text(&result.latitude.to_string());
        self.longitude.set_text(&result.longitude.to_string());
        *self.selected.borrow_mut() = Some(result.clone());
        self.set_status(HINT);
    }

    fn clear_results(&self) {
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }
    }

    fn set_status(&self, text: &str) {
        if self.status.text() != text {
            self.status.set_text(text);
        }
    }

    /// Commit whatever the entries say.
    fn save(self: &Rc<Self>) {
        let latitude = self.latitude.text().trim().to_string();
        let longitude = self.longitude.text().trim().to_string();

        if latitude.is_empty() && longitude.is_empty() {
            self.set_status(EMPTY_QUERY);
            // A user who never opened Advanced has no idea the coordinates are
            // where Save looks, so open it for them.
            self.advanced.set_expanded(true);
            return;
        }

        let (Ok(latitude), Ok(longitude)) = (latitude.parse::<f64>(), longitude.parse::<f64>())
        else {
            self.set_status(NOT_NUMBERS);
            self.advanced.set_expanded(true);
            return;
        };
        if !valid_coordinates(latitude, longitude) {
            self.set_status(OUT_OF_RANGE);
            self.advanced.set_expanded(true);
            return;
        }

        // Keep the place's name only while the coordinates are still its own:
        // editing them by hand makes "Moscow" a lie.
        let label = self
            .selected
            .borrow()
            .as_ref()
            .filter(|result| {
                same_coordinate(result.latitude, latitude)
                    && same_coordinate(result.longitude, longitude)
            })
            .map(|result| result.label.clone())
            .unwrap_or_default();

        let handle = self.services.weather.handle().clone();
        bridge::act(SCOPE, async move {
            handle.set_manual(latitude, longitude, label).await
        });
        dismiss();
    }
}

/// Whether two coordinates are the same place, to the API's own precision.
fn same_coordinate(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-6
}

/// One of the two Advanced entries.
fn coordinate_entry(placeholder: &str) -> Entry {
    let entry = Entry::new();
    entry.add_css_class(classes::LOCATION_COORDINATE);
    entry.set_placeholder_text(Some(placeholder));
    entry.set_hexpand(true);
    entry.set_input_purpose(gtk4::InputPurpose::Number);
    entry
}

/// The dialog's own layer surface: centred, above everything.
fn build_window(monitor: Option<&gdk::Monitor>) -> Window {
    let window = Window::builder().decorated(false).resizable(false).build();
    window.add_css_class(classes::LOCATION_WINDOW);

    window.init_layer_shell();
    window.set_namespace(Some("topbar-dialog"));
    // Overlay rather than Top: a modal the user has to answer belongs above
    // the panel's own menus, and above a fullscreen window.
    window.set_layer(Layer::Overlay);
    if let Some(monitor) = monitor {
        window.set_monitor(Some(monitor));
    }
    // No anchors at all, which is how layer-shell says "centre me".
    window.set_exclusive_zone(0);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    window
}

/// The dimmed surface behind it.
fn build_backdrop(monitor: Option<&gdk::Monitor>) -> Window {
    let window = Window::builder().decorated(false).build();
    window.add_css_class(classes::LOCATION_WINDOW);

    window.init_layer_shell();
    window.set_namespace(Some("topbar-dialog-backdrop"));
    window.set_layer(Layer::Overlay);
    if let Some(monitor) = monitor {
        window.set_monitor(Some(monitor));
    }
    // Covering the bar too, unlike a popover's catcher: a modal that leaves
    // the panel clickable is not modal.
    window.set_exclusive_zone(-1);
    for edge in [
        gtk4_layer_shell::Edge::Top,
        gtk4_layer_shell::Edge::Bottom,
        gtk4_layer_shell::Edge::Left,
        gtk4_layer_shell::Edge::Right,
    ] {
        window.set_anchor(edge, true);
    }
    window.set_keyboard_mode(KeyboardMode::None);

    let surface = gtk4::Box::new(Orientation::Vertical, 0);
    surface.add_css_class(classes::LOCATION_BACKDROP);
    surface.set_hexpand(true);
    surface.set_vexpand(true);
    window.set_child(Some(&surface));
    window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_are_compared_at_the_precision_the_api_reports() {
        assert!(same_coordinate(55.75222, 55.752_220_1));
        assert!(!same_coordinate(55.75222, 55.7523));
    }

    #[test]
    fn every_message_the_dialog_can_show_is_a_sentence() {
        for message in [
            HINT,
            SEARCHING,
            EMPTY_QUERY,
            NO_RESULTS,
            SEARCH_FAILED,
            NOT_NUMBERS,
            OUT_OF_RANGE,
        ] {
            assert!(!message.is_empty());
            assert!(
                message.ends_with('.') || message.ends_with('…'),
                "`{message}` is not written as a sentence"
            );
        }
    }

    #[test]
    fn a_query_shorter_than_the_minimum_is_not_worth_a_request() {
        assert_eq!(MIN_QUERY, 2);
        assert!("P".chars().count() < MIN_QUERY);
        assert!("Pa".chars().count() >= MIN_QUERY);
    }
}
