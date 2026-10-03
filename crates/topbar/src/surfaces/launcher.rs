//! The panel-owned full-screen application, window and filename launcher.
//!
//! The launcher owns a centered input surface and a full-screen backdrop. Its
//! result model uses stable identities rather than row indexes, so a
//! file-discovery or compositor update can redraw the list without changing
//! what Enter means.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::SystemTime;

use gio::prelude::*;
use gio_unix::DesktopAppInfo;
use gtk4::prelude::*;
use gtk4::{
    Align, Button, Entry, Image, Label, Orientation, PolicyType, ScrolledWindow, Window, gdk, glib,
};
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use topbar_core::Config;
use topbar_services::file_search::RESULT_LIMIT;
use topbar_services::{
    Application, ApplicationsState, FileEntry, FileSearchState, Services, WindowsSnapshot,
    rank_match,
};

use crate::anim::{Animation, AnimationParams, Easing};
use crate::bridge::{self, BindingGuard};
use crate::style::{classes, icons};
use crate::surfaces::{modal, search};
use crate::wayland::blur::BlurAttachment;

const MAX_WIDTH: i32 = 1120;
const SEARCH_WIDTH: i32 = 640;
const MAX_SCROLL_HEIGHT: i32 = 720;
const MIN_SCROLL_HEIGHT: i32 = 120;
const COMPACT_OUTPUT_HEIGHT: i32 = 900;
const LARGE_OUTPUT_MARGIN: i32 = 48;
/// The launcher owns a 24px CSS blur with an 8px downward offset.
const COMPACT_OUTPUT_MARGIN: i32 = 36;
const LAUNCHER_PADDING: i32 = 20;
const SEARCH_HEIGHT: i32 = 52;
const FILTER_HEIGHT: i32 = 32;
const STATUS_HEIGHT: i32 = 24;
const ROOT_GAP: i32 = 12;
/// Reserve room for GTK CSS sizing and text wraps at fractional output scales.
const OUTPUT_HEADROOM: i32 = 144;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LauncherLayout {
    margin: i32,
    scroll_min_height: i32,
    scroll_max_height: i32,
}

/// Keep the foreground surface inside the output even when a laptop panel has
/// little vertical room. The result area becomes scrollable before any row can
/// extend below the screen edge.
fn launcher_layout(output_height: i32) -> LauncherLayout {
    let margin = if output_height < COMPACT_OUTPUT_HEIGHT {
        COMPACT_OUTPUT_MARGIN
    } else {
        LARGE_OUTPUT_MARGIN
    };
    let fixed_height =
        2 * LAUNCHER_PADDING + SEARCH_HEIGHT + FILTER_HEIGHT + STATUS_HEIGHT + 3 * ROOT_GAP;
    let scroll_max_height =
        (output_height - 2 * margin - fixed_height - OUTPUT_HEADROOM).clamp(1, MAX_SCROLL_HEIGHT);
    LauncherLayout {
        margin,
        scroll_min_height: scroll_max_height.min(MIN_SCROLL_HEIGHT),
        scroll_max_height,
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Launcher>>> = const { RefCell::new(None) };
    static NEXT_INSTANCE: Cell<u64> = const { Cell::new(1) };
}

/// Handle a launcher IPC visibility action.
pub fn dispatch(
    action: topbar_core::ipc::VisibilityAction,
    services: &Services,
    config: &Config,
) -> bool {
    use topbar_core::ipc::VisibilityAction;
    match action {
        VisibilityAction::Show => show(services, config),
        VisibilityAction::Hide => {
            dismiss();
            false
        }
        VisibilityAction::Toggle => {
            if CURRENT.with_borrow(|current| current.is_some()) {
                dismiss();
                false
            } else {
                show(services, config)
            }
        }
    }
}

/// Close a launcher without activating its selection.
pub fn dismiss() {
    let launcher = CURRENT.with_borrow_mut(|current| current.take());
    if let Some(launcher) = launcher {
        launcher.window.set_visible(false);
        launcher.backdrop.set_visible(false);
        launcher.window.close();
        launcher.backdrop.close();
    }
}

fn current_is(instance: u64) -> bool {
    CURRENT.with_borrow(|current| {
        current
            .as_ref()
            .is_some_and(|launcher| launcher.instance == instance)
    })
}

fn dismiss_instance(instance: u64) {
    if current_is(instance) {
        dismiss();
    }
}

fn status_instance(instance: u64, message: &str) {
    CURRENT.with_borrow(|current| {
        if let Some(launcher) = current
            .as_ref()
            .filter(|launcher| launcher.instance == instance)
        {
            launcher.set_global_message(message);
        }
    });
}

fn show(services: &Services, config: &Config) -> bool {
    if let Some(launcher) = CURRENT.with_borrow(|current| current.clone()) {
        launcher.window.present();
        launcher.search.grab_focus();
        return true;
    }
    let Some(input) = modal::claim_input() else {
        return false;
    };
    modal::close_popovers();
    let display = match gdk::Display::default() {
        Some(display) => display,
        None => return false,
    };
    let focused_output = services
        .compositor
        .workspaces()
        .borrow()
        .focused_output
        .clone();
    let monitors = display.monitors();
    let monitor = focused_output
        .as_deref()
        .and_then(|focused_output| {
            (0..monitors.n_items())
                .filter_map(|index| monitors.item(index).and_downcast::<gdk::Monitor>())
                .find(|monitor| monitor.connector().as_deref() == Some(focused_output))
        })
        .or_else(|| monitors.item(0).and_downcast::<gdk::Monitor>());
    let launcher = Launcher::new(services.clone(), config.clone(), monitor.as_ref(), input);
    CURRENT.with_borrow_mut(|current| *current = Some(Rc::clone(&launcher)));
    launcher.open();
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Applications,
    Actions,
    Windows,
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Applications,
    Windows,
    Files,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SectionMessages {
    applications: Option<String>,
    windows: Option<String>,
    files: Option<String>,
}

#[derive(Default)]
struct SectionStatusLabels {
    applications: Option<Label>,
    windows: Option<Label>,
    files: Option<Label>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SectionPresence {
    applications: bool,
    windows: bool,
    files: bool,
}

impl SectionStatusLabels {
    fn insert(&mut self, section: Section, label: Label) {
        match section {
            Section::Applications => self.applications = Some(label),
            Section::Windows => self.windows = Some(label),
            Section::Files => self.files = Some(label),
        }
    }

    fn update(&self, messages: &SectionMessages) {
        for (label, message) in [
            (&self.applications, messages.applications.as_deref()),
            (&self.windows, messages.windows.as_deref()),
            (&self.files, messages.files.as_deref()),
        ] {
            if let Some(label) = label {
                label.set_text(message.unwrap_or_default());
                label.set_visible(message.is_some());
            }
        }
    }

    fn presence(&self) -> SectionPresence {
        SectionPresence {
            applications: self.applications.is_some(),
            windows: self.windows.is_some(),
            files: self.files.is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardFocus {
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NavigationBounds {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl NavigationBounds {
    fn center_x(self) -> f32 {
        self.x + self.width / 2.0
    }

    fn center_y(self) -> f32 {
        self.y + self.height / 2.0
    }
}

#[derive(Clone)]
struct NavigationTarget {
    id: String,
    window_labels: Option<(Label, Label)>,
    window_icon: Option<Image>,
    button: Button,
}

#[derive(Clone, PartialEq, Eq)]
struct FileResult {
    entry: FileEntry,
    modified: Option<SystemTime>,
    size: Option<u64>,
}

#[derive(Clone, PartialEq, Eq)]
enum ResultItem {
    Application(Application),
    Window {
        id: u64,
        app_id: String,
        title: String,
        context: String,
        icon: Option<String>,
    },
    File(FileResult),
    Theme,
    Wallpaper,
}

impl ResultItem {
    fn id(&self) -> String {
        match self {
            Self::Application(app) => format!("app:{}", app.desktop_id),
            Self::Window { id, .. } => format!("window:{id}"),
            Self::File(file) => format!("file:{}", hex_identity(&file.entry.identity())),
            Self::Theme => "action:theme".to_string(),
            Self::Wallpaper => "action:wallpaper".to_string(),
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Application(app) => app.name.clone(),
            Self::Window { title, .. } => title.clone(),
            Self::File(file) => file.entry.basename(),
            Self::Theme => "Choose theme".into(),
            Self::Wallpaper => "Choose wallpaper".into(),
        }
    }

    fn subtitle(&self) -> String {
        match self {
            Self::Application(app) => app
                .generic_name
                .clone()
                .unwrap_or_else(|| app.desktop_id.clone()),
            Self::Window {
                app_id, context, ..
            } => format!("{app_id} · {context}"),
            Self::File(file) => file.entry.home_relative_path(),
            Self::Theme => "Appearance".into(),
            Self::Wallpaper => "Appearance".into(),
        }
    }
}

struct Launcher {
    _input: topbar_services::ipc::InputLock,
    services: Services,
    config: Config,
    window: Window,
    backdrop: Window,
    // Holds the Wayland effect for the backdrop's entire wl_surface.
    _backdrop_blur: BlurAttachment,
    root: gtk4::Box,
    container_motion: Animation,
    search: Entry,
    filters: Vec<(Filter, Button)>,
    scroll: ScrolledWindow,
    scroll_motion: Animation,
    scroll_target: Rc<Cell<Option<f64>>>,
    results: gtk4::Box,
    status: Label,
    section_statuses: RefCell<SectionStatusLabels>,
    window_grid: RefCell<Option<gtk4::FlowBox>>,
    global_message: RefCell<Option<String>>,
    filter: Cell<Filter>,
    instance: u64,
    drawn_query: RefCell<Option<String>>,
    drawn_filter: Cell<Filter>,
    selected_id: RefCell<Option<String>>,
    items: RefCell<Vec<ResultItem>>,
    selected_index: Cell<usize>,
    navigation_targets: RefCell<Vec<NavigationTarget>>,
    file_matches: RefCell<Vec<FileResult>>,
    file_query: RefCell<Option<String>>,
    file_generation: Cell<u64>,
    /// The last catalog revision that was ranked for this surface. Discovery
    /// progress and warnings update labels without starting another search.
    file_catalog_revision: Cell<u64>,
    bindings: RefCell<Vec<BindingGuard>>,
}

impl Launcher {
    fn new(
        services: Services,
        config: Config,
        monitor: Option<&gdk::Monitor>,
        input: topbar_services::ipc::InputLock,
    ) -> Rc<Self> {
        // This has its own namespace so niri can apply a full-screen effect to
        // the backdrop without also applying it to the centered launcher.
        let backdrop = modal::backdrop(
            monitor,
            "topbar-launcher-backdrop",
            classes::LAUNCHER_BACKDROP,
        );
        let backdrop_blur = modal::attach_backdrop_blur(&backdrop);
        if backdrop_blur.is_available()
            && let Some(surface) = backdrop.child()
        {
            surface.add_css_class(classes::LAUNCHER_BACKDROP_BLURRED);
        }
        let window = modal::centered_window(monitor, "topbar-launcher");
        window.set_layer(gtk4_layer_shell::Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.add_css_class(classes::LAUNCHER_WINDOW);

        let available_width = monitor
            .map(|monitor| {
                monitor
                    .geometry()
                    .width()
                    .saturating_sub(2 * COMPACT_OUTPUT_MARGIN)
                    .max(1)
            })
            .unwrap_or(MAX_WIDTH);
        let layout = launcher_layout(
            monitor
                .map(|monitor| monitor.geometry().height())
                .unwrap_or(1080),
        );
        let root = gtk4::Box::new(Orientation::Vertical, 12);
        root.add_css_class(classes::LAUNCHER);
        root.set_size_request(available_width.min(MAX_WIDTH), -1);
        root.set_margin_top(layout.margin);
        root.set_margin_bottom(layout.margin);
        root.set_margin_start(COMPACT_OUTPUT_MARGIN);
        root.set_margin_end(COMPACT_OUTPUT_MARGIN);

        let search = Entry::new();
        search.add_css_class(classes::LAUNCHER_SEARCH);
        search.set_placeholder_text(Some("Search applications, actions, windows, and files"));
        search.set_width_chars(48);
        search.set_max_width_chars(48);
        search.set_halign(Align::Center);
        search.set_size_request(available_width.min(SEARCH_WIDTH), -1);
        root.append(&search);

        let filter_row = gtk4::Box::new(Orientation::Horizontal, 6);
        filter_row.add_css_class(classes::LAUNCHER_FILTERS);
        filter_row.set_halign(Align::Center);
        let mut filters = Vec::new();
        for (filter, label) in [
            (Filter::All, "All"),
            (Filter::Applications, "Applications"),
            (Filter::Actions, "Actions"),
            (Filter::Windows, "Windows"),
            (Filter::Files, "Files"),
        ] {
            let button = Button::with_label(label);
            button.add_css_class(classes::LAUNCHER_FILTER);
            filter_row.append(&button);
            filters.push((filter, button));
        }
        root.append(&filter_row);

        let scroll = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .vexpand(true)
            .propagate_natural_height(true)
            .min_content_height(layout.scroll_min_height)
            .max_content_height(layout.scroll_max_height)
            .build();
        scroll.add_css_class(classes::LAUNCHER_SCROLL);
        let results = gtk4::Box::new(Orientation::Vertical, 8);
        results.add_css_class(classes::LAUNCHER_RESULTS);
        scroll.set_child(Some(&results));
        root.append(&scroll);

        let status = Label::new(None);
        status.add_css_class(classes::LAUNCHER_STATUS);
        status.set_xalign(0.0);
        status.set_wrap(true);
        root.append(&status);
        window.set_child(Some(&root));

        let container_motion = Animation::new(&root);
        let scroll_motion = Animation::new(&scroll);
        let file_catalog_revision = services.files.current().catalog_revision;
        let launcher = Rc::new(Self {
            _input: input,
            services,
            config,
            window,
            backdrop,
            _backdrop_blur: backdrop_blur,
            root,
            container_motion,
            search,
            filters,
            scroll,
            scroll_motion,
            scroll_target: Rc::new(Cell::new(None)),
            results,
            status,
            section_statuses: RefCell::new(SectionStatusLabels::default()),
            window_grid: RefCell::new(None),
            global_message: RefCell::new(None),
            filter: Cell::new(Filter::All),
            instance: NEXT_INSTANCE.with(|next| {
                let instance = next.get();
                next.set(instance.wrapping_add(1));
                instance
            }),
            drawn_query: RefCell::new(None),
            drawn_filter: Cell::new(Filter::All),
            selected_id: RefCell::new(None),
            items: RefCell::new(Vec::new()),
            selected_index: Cell::new(0),
            navigation_targets: RefCell::new(Vec::new()),
            file_matches: RefCell::new(Vec::new()),
            file_query: RefCell::new(None),
            file_generation: Cell::new(0),
            file_catalog_revision: Cell::new(file_catalog_revision),
            bindings: RefCell::new(Vec::new()),
        });
        let monitors = gtk4::prelude::WidgetExt::display(&launcher.window).monitors();
        monitors.connect_items_changed({
            let instance = launcher.instance;
            let window = launcher.window.clone();
            move |_, _, _, _| {
                let Some(surface_monitor) = window.monitor() else {
                    return;
                };
                let display = gtk4::prelude::WidgetExt::display(&window);
                let present = (0..display.monitors().n_items())
                    .filter_map(|index| {
                        display
                            .monitors()
                            .item(index)
                            .and_downcast::<gdk::Monitor>()
                    })
                    .any(|candidate| candidate == surface_monitor);
                if !present {
                    dismiss_instance(instance);
                }
            }
        });
        launcher.install_handlers();
        launcher.install_bindings();
        launcher
    }

    fn open(&self) {
        let files = self.services.files.handle();
        topbar_services::Runtime::handle().spawn(async move {
            let _ = files.opened().await;
        });
        self.backdrop.present();
        self.window.present();
        self.root.set_opacity(0.0);
        let root = self.root.clone();
        self.container_motion.start(
            AnimationParams::new(200).with_easing(Easing::EaseOutCubic),
            Box::new(move |progress| root.set_opacity(progress)),
            None,
        );
        self.search.grab_focus();
        self.render();
    }

    fn install_handlers(self: &Rc<Self>) {
        search::install(&self.window, {
            let entry = self.search.downgrade();
            move |_| entry.upgrade().map(|entry| entry.upcast())
        });
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_released(|_, _, _, _| dismiss());
        self.backdrop.add_controller(click);
        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
        wheel.connect_scroll({
            let launcher = Rc::downgrade(self);
            move |_, _, _| {
                if let Some(launcher) = launcher.upgrade() {
                    launcher.scroll_motion.cancel();
                    launcher.scroll_target.set(None);
                }
                glib::Propagation::Proceed
            }
        });
        self.scroll.add_controller(wheel);

        self.window.connect_close_request(|_| {
            CURRENT.with_borrow_mut(|current| current.take());
            glib::Propagation::Proceed
        });
        self.search.connect_changed({
            let launcher = Rc::downgrade(self);
            move |_| {
                if let Some(launcher) = launcher.upgrade() {
                    launcher.clear_global_message();
                    if launcher.shows_files() {
                        launcher.queue_file_search();
                    }
                    launcher.render();
                }
            }
        });
        for (filter, button) in &self.filters {
            let launcher = Rc::downgrade(self);
            let filter = *filter;
            button.connect_clicked(move |_| {
                if let Some(launcher) = launcher.upgrade() {
                    launcher.filter.set(filter);
                    launcher.clear_global_message();
                    if launcher.shows_files() {
                        launcher.queue_file_search();
                    }
                    launcher.render();
                }
            });
        }
        let keys = gtk4::EventControllerKey::new();
        // Dedicated search Up/Down and tile navigation; ordinary editors keep
        // their caret keys and buttons keep their own activation.
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let launcher = Rc::downgrade(self);
            move |_, key, _, modifiers| {
                let Some(launcher) = launcher.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if search::nested_focus(&launcher.window) {
                    return glib::Propagation::Proceed;
                }
                if search::shortcut(modifiers) || modifiers.contains(gdk::ModifierType::SHIFT_MASK)
                {
                    return glib::Propagation::Proceed;
                }
                let in_search = search::focused(&launcher.search);
                let on_tile = launcher
                    .navigation_targets
                    .borrow()
                    .iter()
                    .any(|target| search::focused(&target.button));
                match key {
                    gdk::Key::Escape => dismiss(),
                    gdk::Key::Down if in_search || on_tile => {
                        launcher.move_selection(Direction::Down)
                    }
                    gdk::Key::Up if in_search || on_tile => launcher.move_selection(Direction::Up),
                    gdk::Key::Right if on_tile => launcher.move_selection(Direction::Right),
                    gdk::Key::Left if on_tile => launcher.move_selection(Direction::Left),
                    gdk::Key::Return | gdk::Key::KP_Enter if in_search => {
                        launcher.activate_selected()
                    }
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        });
        self.window.add_controller(keys);
    }

    fn install_bindings(self: &Rc<Self>) {
        let applications = bridge::bind_state(&self.results, self.services.applications.state(), {
            let launcher = Rc::downgrade(self);
            move |_, _| {
                if let Some(launcher) = launcher.upgrade() {
                    launcher.render();
                }
            }
        });
        let windows = bridge::bind_state(&self.results, self.services.compositor.windows(), {
            let launcher = Rc::downgrade(self);
            move |_, _| {
                if let Some(launcher) = launcher.upgrade()
                    && (launcher.filter.get() == Filter::Windows
                        || (!launcher.search.text().is_empty()
                            && launcher.filter.get() == Filter::All))
                {
                    launcher.render();
                }
            }
        });
        self.bindings.borrow_mut().extend([applications, windows]);
        let files = bridge::bind_state(&self.results, self.services.files.state(), {
            let launcher = Rc::downgrade(self);
            move |_, files| {
                if let Some(launcher) = launcher.upgrade()
                    && launcher.shows_files()
                {
                    let revision_changed = catalog_revision_changed(
                        &launcher.file_catalog_revision,
                        files.catalog_revision,
                    );
                    // Warm refreshes change only progress or warning metadata
                    // until their final atomic catalog swap. Keep the current
                    // rows while those labels update.
                    launcher.refresh_status();
                    if revision_changed {
                        launcher.queue_file_search();
                    }
                }
            }
        });
        self.bindings.borrow_mut().push(files);
    }

    /// Ask the service runtime to apply its shared 75ms debounce and rank the
    /// query outside GTK. A later keystroke invalidates this answer by number.
    fn queue_file_search(self: &Rc<Self>) {
        let generation = self.file_generation.get().wrapping_add(1);
        self.file_generation.set(generation);
        let query = self.search.text().to_string();
        if query.is_empty() {
            *self.file_query.borrow_mut() = None;
            self.file_matches.borrow_mut().clear();
            return;
        }
        let query_changed = self.file_query.borrow().as_deref() != Some(query.as_str());
        if query_changed {
            *self.file_query.borrow_mut() = Some(query.clone());
            // Never show paths for the previous query while the debounced
            // answer for the new one is still in flight.
            self.file_matches.borrow_mut().clear();
        }
        let search = self.services.files.handle();
        let answer =
            topbar_services::Runtime::handle().spawn(async move { search.search(query).await });
        let launcher = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(Ok(matches)) = answer.await else {
                return;
            };
            let Some(launcher) = launcher.upgrade() else {
                return;
            };
            if launcher.file_generation.get() != generation {
                return;
            }
            let mut matches = matches
                .into_iter()
                .map(|matched| FileResult {
                    entry: matched.entry,
                    modified: matched.modified,
                    size: matched.size,
                })
                .collect::<Vec<_>>();
            let selected_file = launcher
                .selected_id
                .borrow()
                .as_deref()
                .and_then(|selected_id| {
                    launcher
                        .items
                        .borrow()
                        .iter()
                        .find(|item| item.id() == selected_id)
                        .and_then(|item| match item {
                            ResultItem::File(file) => Some(file.clone()),
                            _ => None,
                        })
                });
            pin_selected_file_result(&mut matches, selected_file);
            launcher.refresh_status();
            if *launcher.file_matches.borrow() == matches {
                return;
            }
            *launcher.file_matches.borrow_mut() = matches;
            launcher.render();
        });
    }

    fn shows_files(&self) -> bool {
        !self.search.text().is_empty() && matches!(self.filter.get(), Filter::All | Filter::Files)
    }

    fn move_selection(&self, direction: Direction) {
        let from_search = search::focused(&self.search);
        let targets = self.navigation_targets.borrow();
        let remembered = self.selected_id.borrow();
        let selected_id = targets
            .iter()
            .find(|target| search::focused(&target.button))
            .map(|target| target.id.as_str())
            .or(remembered.as_deref());
        let Some(selected_id) = selected_id else {
            return;
        };
        let Some(next_id) = spatial_target(
            &targets,
            &self.results.clone().upcast(),
            selected_id,
            direction,
        ) else {
            return;
        };
        drop(remembered);
        let next = stable_index(&self.items.borrow(), Some(&next_id));
        for target in targets.iter() {
            if target.id == next_id {
                target.button.add_css_class(classes::LAUNCHER_ITEM_SELECTED);
                self.scroll_selected_into_view(&target.button);
            } else {
                target
                    .button
                    .remove_css_class(classes::LAUNCHER_ITEM_SELECTED);
            }
        }
        self.selected_index.set(next);
        if keyboard_focus_after_navigation() == KeyboardFocus::Search && from_search {
            self.search.grab_focus();
        } else if let Some(target) = targets.iter().find(|target| target.id == next_id) {
            target.button.grab_focus();
        }
        *self.selected_id.borrow_mut() = Some(next_id);
    }

    fn scroll_selected_into_view(&self, button: &Button) {
        // Compare content coordinates with the adjustment's content offset.
        let Some(bounds) = button.compute_bounds(&self.results) else {
            return;
        };
        let adjustment = self.scroll.vadjustment();
        let current = adjustment.value();
        let page = adjustment.page_size();
        let target_start = f64::from(bounds.y());
        let target_end = target_start + f64::from(bounds.height());
        let base = self.scroll_target.get().unwrap_or(current);
        let Some(value) = scroll_value_for_bounds(base, page, target_start, target_end) else {
            return;
        };
        let target = value.clamp(
            adjustment.lower(),
            (adjustment.upper() - page).max(adjustment.lower()),
        );
        self.scroll_target.set(Some(target));
        let on_done = Rc::clone(&self.scroll_target);
        self.scroll_motion.start(
            AnimationParams::new(180).with_easing(Easing::EaseOutCubic),
            Box::new(move |progress| adjustment.set_value(current + (target - current) * progress)),
            Some(Box::new(move || on_done.set(None))),
        );
    }

    fn render(&self) {
        let query = self.search.text().to_string();
        let applications = self.services.applications.state().borrow().clone();
        let files = self.services.files.current();
        let windows = self.services.compositor.windows().borrow().clone();
        let old_id = self.selected_id.borrow().clone();
        let items = Self::collect(
            self.filter.get(),
            &query,
            &applications,
            &windows,
            &self.file_matches.borrow(),
            [
                self.config.appearance.theme_command.is_some(),
                self.config.appearance.wallpaper_command.is_some(),
            ],
        );
        let same_view = self.drawn_query.borrow().as_deref() == Some(query.as_str())
            && self.drawn_filter.get() == self.filter.get();
        let same_results = same_view && *self.items.borrow() == items;
        let messages = section_messages(self.filter.get(), &query, &applications, &files, &windows);
        let same_sections = !section_structure_changed(
            self.section_statuses.borrow().presence(),
            section_presence(&items, &messages),
        );
        if same_results && same_sections {
            self.update_status(&query, !items.is_empty(), &messages);
            return;
        }
        if same_view
            && same_sections
            && self.filter.get() == Filter::Windows
            && self.reconcile_windows(&items)
        {
            self.items.replace(items);
            self.update_status(&query, !self.items.borrow().is_empty(), &messages);
            return;
        }
        *self.drawn_query.borrow_mut() = Some(query.clone());
        self.drawn_filter.set(self.filter.get());
        self.items.replace(items);
        let selected = stable_index(&self.items.borrow(), old_id.as_deref());
        let fallback_id = self.items.borrow().get(selected).map(ResultItem::id);
        self.draw(&query, &applications, &files, &windows);
        let targets = self.navigation_targets.borrow();
        let selected = targets
            .iter()
            .find(|target| Some(target.id.as_str()) == old_id.as_deref())
            .or_else(|| {
                // A blank view with no retained selection starts at Frequent,
                // even when it repeats an application in the ordinary grid.
                (query.is_empty()
                    && matches!(self.filter.get(), Filter::All | Filter::Applications))
                .then(|| targets.first())
                .flatten()
                .filter(|target| target.id.starts_with("frequent:"))
            })
            .or_else(|| {
                targets
                    .iter()
                    .find(|target| Some(target.id.as_str()) == fallback_id.as_deref())
            })
            .or_else(|| targets.first());
        let selected_id = selected.map(|target| {
            target.button.add_css_class(classes::LAUNCHER_ITEM_SELECTED);
            target.id.clone()
        });
        self.selected_index
            .set(stable_index(&self.items.borrow(), selected_id.as_deref()));
        *self.selected_id.borrow_mut() = selected_id;
    }

    fn set_global_message(&self, message: &str) {
        *self.global_message.borrow_mut() = Some(message.to_string());
        self.status.set_text(message);
    }

    fn clear_global_message(&self) {
        self.global_message.borrow_mut().take();
    }

    fn collect(
        filter: Filter,
        query: &str,
        applications: &ApplicationsState,
        windows: &WindowsSnapshot,
        file_matches: &[FileResult],
        enabled_actions: [bool; 2],
    ) -> Vec<ResultItem> {
        let wants = |kind| filter == Filter::All || filter == kind;
        let mut output = Vec::new();
        if wants(Filter::Applications) {
            let mut apps = applications.entries.clone();
            if !query.is_empty() {
                apps.retain(|app| app.match_score(query).is_some());
                apps.sort_by(|left, right| {
                    right
                        .match_score(query)
                        .unwrap()
                        .score
                        .cmp(&left.match_score(query).unwrap().score)
                        .then_with(|| left.desktop_id.cmp(&right.desktop_id))
                });
            }
            output.extend(apps.into_iter().map(ResultItem::Application));
        }
        if wants(Filter::Actions) && (!query.is_empty() || filter == Filter::Actions) {
            for (item, enabled) in [
                (ResultItem::Theme, enabled_actions[0]),
                (ResultItem::Wallpaper, enabled_actions[1]),
            ] {
                if enabled && (query.is_empty() || rank_match(query, &item.title()).is_some()) {
                    output.push(item);
                }
            }
        }
        if wants(Filter::Windows)
            && (!query.is_empty() || filter == Filter::Windows)
            && windows.connected
        {
            let mut matches = windows
                .windows
                .iter()
                .filter_map(|window| {
                    let application =
                        unambiguous_application(&applications.entries, &window.app_id);
                    if query.is_empty() {
                        return Some((0, window, application));
                    }
                    let app_name = application.map(|app| app.name.as_str());
                    let app_match = rank_match(query, &window.app_id)
                        .into_iter()
                        .chain(app_name.and_then(|name| rank_match(query, name)))
                        .max_by_key(|matched| matched.score);
                    let title_match = rank_match(query, &window.title);
                    let score = app_match
                        .into_iter()
                        .chain(title_match)
                        .map(|matched| matched.score)
                        .max()?;
                    Some((score, window, application))
                })
                .collect::<Vec<_>>();
            matches.sort_by(|(left_score, left, _), (right_score, right, _)| {
                right_score
                    .cmp(left_score)
                    .then_with(|| left.id.cmp(&right.id))
            });
            output.extend(matches.into_iter().map(|(_, window, application)| {
                ResultItem::Window {
                    id: window.id,
                    app_id: window.app_id.clone(),
                    icon: application.and_then(|app| app.icon.clone()),
                    title: window.title.clone(),
                    context: [window.workspace.as_deref(), window.output.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · "),
                }
            }));
        }
        if !query.is_empty() && wants(Filter::Files) {
            output.extend(file_matches.iter().cloned().map(ResultItem::File));
        }
        output
    }

    /// ponytail: update existing window buttons in place; rebuild only when
    /// the Windows section appears or disappears.
    fn reconcile_windows(&self, items: &[ResultItem]) -> bool {
        let Some(grid) = self.window_grid.borrow().clone() else {
            return false;
        };
        if items.is_empty() {
            return false;
        }
        let order_changed = !self
            .items
            .borrow()
            .iter()
            .map(ResultItem::id)
            .eq(items.iter().map(ResultItem::id));
        let order: HashMap<_, _> = items
            .iter()
            .enumerate()
            .map(|(position, item)| (item.id(), position))
            .collect();
        self.navigation_targets.borrow_mut().retain(|target| {
            if order.contains_key(&target.id) {
                return true;
            }
            if let Some(child) = target.button.parent() {
                grid.remove(&child);
            }
            false
        });
        for item in items {
            let id = item.id();
            let target = self
                .navigation_targets
                .borrow()
                .iter()
                .find(|target| target.id == id)
                .cloned();
            if let Some(target) = target {
                if !self.items.borrow().contains(item) {
                    let (title, subtitle) = target.window_labels.as_ref().expect("window labels");
                    title.set_markup(&highlight(&item.title(), &self.search.text()));
                    subtitle.set_markup(&highlight(&item.subtitle(), &self.search.text()));
                }
                if let ResultItem::Window { icon, .. } = item {
                    let icon_changed = self
                        .items
                        .borrow()
                        .iter()
                        .find(|old| old.id() == id)
                        .and_then(|old| match old {
                            ResultItem::Window { icon, .. } => Some(icon),
                            _ => None,
                        })
                        != Some(icon);
                    if icon_changed {
                        set_window_icon(
                            target.window_icon.as_ref().expect("window icon"),
                            icon.as_deref(),
                        );
                    }
                }
            } else {
                let button = self.item_button(item.clone(), false);
                grid.insert(&button, -1);
            }
        }
        if order_changed {
            let positions: HashMap<_, _> = self
                .navigation_targets
                .borrow()
                .iter()
                .filter_map(|target| {
                    order
                        .get(&target.id)
                        .map(|position| (target.button.as_ptr() as usize, *position))
                })
                .collect();
            grid.set_sort_func(move |left, right| {
                let position = |child: &gtk4::FlowBoxChild| {
                    child
                        .child()
                        .and_then(|button| positions.get(&(button.as_ptr() as usize)).copied())
                        .unwrap_or(usize::MAX)
                };
                position(left).cmp(&position(right)).into()
            });
        }
        let old_id = self.selected_id.borrow().clone();
        let targets = self.navigation_targets.borrow();
        let selected = old_id
            .as_deref()
            .and_then(|id| targets.iter().find(|target| target.id == id))
            .or_else(|| targets.iter().find(|target| target.id == items[0].id()));
        let selected_id = selected.map(|target| {
            target.button.add_css_class(classes::LAUNCHER_ITEM_SELECTED);
            target.id.clone()
        });
        self.selected_index
            .set(stable_index(items, selected_id.as_deref()));
        *self.selected_id.borrow_mut() = selected_id;
        true
    }

    fn draw(
        &self,
        query: &str,
        applications: &ApplicationsState,
        files: &FileSearchState,
        windows: &WindowsSnapshot,
    ) {
        self.scroll_motion.cancel();
        self.scroll_target.set(None);
        self.window_grid.borrow_mut().take();
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }
        self.navigation_targets.borrow_mut().clear();
        *self.section_statuses.borrow_mut() = SectionStatusLabels::default();
        for (filter, button) in &self.filters {
            button.remove_css_class(classes::LAUNCHER_FILTER_SELECTED);
            if *filter == self.filter.get() {
                button.add_css_class(classes::LAUNCHER_FILTER_SELECTED);
            }
        }
        let section_messages =
            section_messages(self.filter.get(), query, applications, files, windows);
        if query.is_empty()
            && (self.filter.get() == Filter::All || self.filter.get() == Filter::Applications)
        {
            let frequent = self.services.launcher_usage.frequent(
                applications
                    .entries
                    .iter()
                    .map(|app| app.desktop_id.as_str()),
                chrono::Utc::now().timestamp(),
            );
            let frequent = frequent
                .into_iter()
                .filter_map(|id| {
                    applications
                        .entries
                        .iter()
                        .find(|app| app.desktop_id == id)
                        .cloned()
                })
                .collect::<Vec<_>>();
            if !frequent.is_empty() {
                self.add_section(
                    Section::Applications,
                    "Frequent",
                    frequent.into_iter().map(ResultItem::Application).collect(),
                    None,
                    true,
                );
            }
            self.add_section(
                Section::Applications,
                "Applications",
                self.items.borrow().clone(),
                section_messages.applications.as_deref(),
                false,
            );
        } else {
            let items = self.items.borrow().clone();
            self.add_section(
                Section::Applications,
                "Applications",
                items
                    .iter()
                    .filter(|item| matches!(item, ResultItem::Application(_)))
                    .cloned()
                    .collect(),
                section_messages.applications.as_deref(),
                false,
            );
            self.add_list_section(
                None,
                "Actions",
                items
                    .iter()
                    .filter(|item| matches!(item, ResultItem::Theme | ResultItem::Wallpaper))
                    .cloned()
                    .collect(),
                None,
            );
            self.add_section(
                Section::Windows,
                "Windows",
                items
                    .iter()
                    .filter(|item| matches!(item, ResultItem::Window { .. }))
                    .cloned()
                    .collect(),
                section_messages.windows.as_deref(),
                false,
            );
            self.add_list_section(
                Some(Section::Files),
                "Files",
                items
                    .iter()
                    .filter(|item| matches!(item, ResultItem::File(_)))
                    .cloned()
                    .collect(),
                section_messages.files.as_deref(),
            );
        }
        self.update_status(query, !self.items.borrow().is_empty(), &section_messages);
    }

    /// Refresh text-only progress and warning state without disturbing result
    /// rows that GTK is already laying out.
    fn refresh_status(&self) {
        let query = self.search.text().to_string();
        let applications = self.services.applications.state().borrow().clone();
        let files = self.services.files.current();
        let windows = self.services.compositor.windows().borrow().clone();
        let messages = section_messages(self.filter.get(), &query, &applications, &files, &windows);
        let desired_presence = section_presence(&self.items.borrow(), &messages);
        if section_structure_changed(self.section_statuses.borrow().presence(), desired_presence) {
            // A warning with no matching rows needs a new local section, while
            // clearing that warning removes its now-empty section. All other
            // catalog batches only update an existing label and leave rows in
            // place, which avoids visible file-search flicker.
            self.render();
            return;
        }
        self.update_status(&query, !self.items.borrow().is_empty(), &messages);
    }

    fn update_status(&self, query: &str, has_items: bool, section_messages: &SectionMessages) {
        let status = global_status_message(query, has_items, self.global_message.borrow().clone());
        self.status.set_text(status.as_deref().unwrap_or_default());
        self.section_statuses.borrow().update(section_messages);
    }

    fn add_section(
        &self,
        section: Section,
        title: &str,
        items: Vec<ResultItem>,
        message: Option<&str>,
        frequent: bool,
    ) {
        if items.is_empty() && message.is_none() {
            return;
        }
        let heading = Label::new(Some(title));
        heading.add_css_class(classes::LAUNCHER_SECTION);
        heading.set_xalign(0.0);
        self.results.append(&heading);
        self.add_section_status(section, message);
        if items.is_empty() {
            return;
        }
        let grid = gtk4::FlowBox::new();
        grid.add_css_class(classes::LAUNCHER_GRID);
        grid.set_selection_mode(gtk4::SelectionMode::None);
        grid.set_max_children_per_line(6);
        grid.set_min_children_per_line(1);
        for item in items {
            let button = self.item_button(item, frequent);
            grid.insert(&button, -1);
        }
        if section == Section::Windows {
            *self.window_grid.borrow_mut() = Some(grid.clone());
        }
        self.results.append(&grid);
    }

    fn item_button(&self, item: ResultItem, frequent: bool) -> Button {
        let button = Button::new();
        button.add_css_class(classes::LAUNCHER_ITEM);
        let is_file = matches!(&item, ResultItem::File(_));
        let body = gtk4::Box::new(
            if is_file {
                Orientation::Horizontal
            } else {
                Orientation::Vertical
            },
            if is_file { 10 } else { 4 },
        );
        let mut window_icon = None;
        if is_file {
            button.add_css_class(classes::LAUNCHER_FILE_ROW);
        } else {
            let icon = match &item {
                ResultItem::Application(app) => app
                    .icon
                    .as_deref()
                    .and_then(|serialized| gio::Icon::for_string(serialized).ok())
                    .map(|icon| Image::from_gicon(&icon))
                    .unwrap_or_else(|| Image::from_icon_name("application-x-executable")),
                ResultItem::Window { icon, .. } => {
                    let image = Image::new();
                    set_window_icon(&image, icon.as_deref());
                    window_icon = Some(image.clone());
                    image
                }
                ResultItem::Theme => Image::from_icon_name(icons::THEME),
                ResultItem::Wallpaper => Image::from_icon_name(icons::WALLPAPER),
                ResultItem::File(_) => unreachable!("file rows do not have icons"),
            };
            icon.add_css_class(classes::LAUNCHER_ICON);
            body.append(&icon);
        }
        let title = Label::new(None);
        title.add_css_class(classes::LAUNCHER_ITEM_TITLE);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title.set_max_width_chars(if is_file { 28 } else { 22 });
        title.set_markup(&highlight(&item.title(), &self.search.text()));
        body.append(&title);
        let subtitle = Label::new(None);
        subtitle.add_css_class(classes::LAUNCHER_ITEM_SUBTITLE);
        subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        subtitle.set_max_width_chars(if is_file { 70 } else { 22 });
        subtitle.set_markup(&highlight(&item.subtitle(), &self.search.text()));
        body.append(&subtitle);
        if let ResultItem::File(file) = &item {
            title.set_xalign(0.0);
            subtitle.set_xalign(0.0);
            subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
            subtitle.set_hexpand(true);
            let mut metadata = file
                .size
                .map(|size| glib::format_size(size).to_string())
                .unwrap_or_default();
            if let Some(modified) = file.modified {
                if !metadata.is_empty() {
                    metadata.push_str(" · ");
                }
                metadata.push_str(
                    &chrono::DateTime::<chrono::Local>::from(modified)
                        .format("%Y-%m-%d %H:%M")
                        .to_string(),
                );
            }
            if !metadata.is_empty() {
                let label = Label::new(Some(&metadata));
                label.add_css_class(classes::LAUNCHER_FILE_META);
                body.append(&label);
            }
        }
        button.set_child(Some(&body));
        let click_item = item.clone();
        button.connect_clicked(move |_| {
            CURRENT.with_borrow(|current| {
                if let Some(launcher) = current.as_ref() {
                    let current_item = if matches!(click_item, ResultItem::Window { .. }) {
                        launcher
                            .items
                            .borrow()
                            .iter()
                            .find(|item| item.id() == click_item.id())
                            .cloned()
                    } else {
                        Some(click_item.clone())
                    };
                    if let Some(item) = current_item {
                        launcher.activate(item);
                    }
                }
            });
        });
        let secondary = gtk4::GestureClick::new();
        secondary.set_button(gdk::BUTTON_SECONDARY);
        secondary.connect_released({
            let item = item.clone();
            move |gesture, _, _, _| {
                let Some(widget) = gesture.widget() else {
                    return;
                };
                match &item {
                    ResultItem::Application(app) => app_menu(&widget, app.clone()),
                    ResultItem::File(file) => file_menu(&widget, file.entry.clone()),
                    _ => {}
                }
            }
        });
        button.add_controller(secondary);
        self.navigation_targets.borrow_mut().push(NavigationTarget {
            id: navigation_id(&item, frequent),
            window_labels: matches!(&item, ResultItem::Window { .. }).then_some((title, subtitle)),
            window_icon,
            button: button.clone(),
        });
        button
    }

    fn add_list_section(
        &self,
        section: Option<Section>,
        title: &str,
        items: Vec<ResultItem>,
        message: Option<&str>,
    ) {
        if items.is_empty() && message.is_none() {
            return;
        }
        let heading = Label::new(Some(title));
        heading.add_css_class(classes::LAUNCHER_SECTION);
        heading.set_xalign(0.0);
        self.results.append(&heading);
        if let Some(section) = section {
            self.add_section_status(section, message);
        }
        if items.is_empty() {
            return;
        }
        let list = gtk4::Box::new(Orientation::Vertical, 4);
        list.add_css_class(classes::LAUNCHER_LIST);
        for item in items {
            let button = self.item_button(item, false);
            button.add_css_class(classes::LAUNCHER_ROW);
            list.append(&button);
        }
        self.results.append(&list);
    }

    fn add_section_status(&self, section: Section, message: Option<&str>) {
        let label = Label::new(message);
        label.add_css_class(classes::LAUNCHER_STATUS);
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_visible(message.is_some());
        self.results.append(&label);
        self.section_statuses.borrow_mut().insert(section, label);
    }

    fn activate_selected(&self) {
        if let Some(item) = self.items.borrow().get(self.selected_index.get()).cloned() {
            self.activate(item);
        }
    }

    fn activate(&self, item: ResultItem) {
        if let ResultItem::Application(app) = &item {
            self.launch_application(app.clone());
            return;
        }
        if let ResultItem::File(file) = &item {
            self.open_file(file.entry.clone());
            return;
        }
        let services = self.services.clone();
        let config = self.config.clone();
        let result_id = item.id();
        let instance = self.instance;
        topbar_services::Runtime::handle().spawn(async move {
            let usage_id = match &item {
                ResultItem::Window { app_id, .. } => {
                    let applications = services.applications.state().borrow().clone();
                    unambiguous_application_id(&applications.entries, app_id)
                }
                _ => None,
            };
            let outcome = match item.clone() {
                ResultItem::Window { id, .. } => {
                    services.compositor.handle().focus_window(id).await
                }
                ResultItem::Theme => run_appearance(config.appearance.theme_command).await,
                ResultItem::Wallpaper => run_appearance(config.appearance.wallpaper_command).await,
                ResultItem::Application(_) => {
                    unreachable!("applications return before service activation")
                }
                ResultItem::File(_) => unreachable!("files return before service activation"),
            };
            if outcome.is_ok()
                && let Some(desktop_id) = usage_id
            {
                services
                    .launcher_usage
                    .record(&desktop_id, chrono::Utc::now().timestamp());
            }
            glib::idle_add_once(move || match outcome {
                Ok(()) => dismiss_instance(instance),
                Err(error) => status_instance(instance, &format!("{result_id}: {error}")),
            });
        });
    }

    fn launch_application(&self, app: Application) {
        let Some(info) = gio::AppInfo::all()
            .into_iter()
            .find(|info| info.id().as_deref().is_some_and(|id| id == app.desktop_id))
        else {
            self.status.set_text("Application is no longer installed.");
            return;
        };
        let Some(info) = info.downcast_ref::<DesktopAppInfo>() else {
            self.status
                .set_text("Could not resolve application desktop entry.");
            return;
        };
        let info = info.clone();
        let context = gtk4::prelude::WidgetExt::display(&self.window).app_launch_context();
        let services = self.services.clone();
        let desktop_id = app.desktop_id.clone();
        let instance = self.instance;
        glib::spawn_future_local(async move {
            let outcome = match exec_app_info(&info).await {
                Ok(exec_info) => exec_info
                    .launch_uris_future(&[], Some(&context))
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error),
            };
            match outcome {
                Ok(()) => {
                    services
                        .launcher_usage
                        .record(&desktop_id, chrono::Utc::now().timestamp());
                    dismiss_instance(instance);
                }
                Err(error) => {
                    status_instance(instance, &format!("Could not launch application: {error}"));
                }
            }
        });
    }

    fn open_file(&self, file: FileEntry) {
        let uri = gio::File::for_path(file.path()).uri();
        let context = gtk4::prelude::WidgetExt::display(&self.window).app_launch_context();
        let instance = self.instance;
        gio::AppInfo::launch_default_for_uri_async(
            &uri,
            Some(&context),
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(()) => dismiss_instance(instance),
                Err(error) => status_instance(instance, &format!("Could not open file: {error}")),
            },
        );
    }
}

/// GIO normally selects D-Bus activation for DBusActivatable entries. Rebuild
/// those entries with just that flag disabled so GIO still parses Exec and
/// Terminal and provides startup notification, but launches a child. GIO's
/// keyfile constructor does not retain a filename, so %k is unavailable here.
async fn exec_app_info(info: &DesktopAppInfo) -> Result<DesktopAppInfo, String> {
    if !info.boolean("DBusActivatable") {
        return Ok(info.clone());
    }
    let filename = info
        .filename()
        .ok_or("Application desktop entry has no filename")?;
    let (contents, _) = gio::File::for_path(&filename)
        .load_contents_future()
        .await
        .map_err(|error| format!("Could not read {}: {error}", filename.display()))?;
    let key_file = glib::KeyFile::new();
    key_file
        .load_from_data(
            std::str::from_utf8(&contents).map_err(|error| {
                format!("Invalid desktop entry {}: {error}", filename.display())
            })?,
            glib::KeyFileFlags::NONE,
        )
        .map_err(|error| format!("Invalid desktop entry {}: {error}", filename.display()))?;
    if !key_file
        .string("Desktop Entry", "Exec")
        .is_ok_and(|exec| !exec.trim().is_empty())
    {
        return Err(format!("No Exec command in {}", filename.display()));
    }
    key_file.set_boolean("Desktop Entry", "DBusActivatable", false);
    DesktopAppInfo::from_keyfile(&key_file).ok_or_else(|| {
        format!(
            "Could not load application desktop entry {}",
            filename.display()
        )
    })
}

fn app_menu(anchor: &gtk4::Widget, app: Application) {
    let menu = gtk4::Popover::new();
    let launch = Button::with_label("Launch New");
    launch.add_css_class(classes::DIALOG_BUTTON);
    launch.connect_clicked(move |_| {
        CURRENT.with_borrow(|current| {
            if let Some(launcher) = current.as_ref() {
                launcher.activate(ResultItem::Application(app.clone()));
            }
        })
    });
    menu.set_child(Some(&launch));
    menu.set_parent(anchor);
    menu.popup();
}

fn file_menu(anchor: &gtk4::Widget, file: FileEntry) {
    let menu = gtk4::Popover::new();
    let actions = gtk4::Box::new(Orientation::Vertical, 4);
    for label in ["Open", "Show in Files", "Copy Path"] {
        let action = Button::with_label(label);
        action.add_css_class(classes::DIALOG_BUTTON);
        let file = file.clone();
        let anchor = anchor.clone();
        action.connect_clicked(move |_| match label {
            "Open" => CURRENT.with_borrow(|current| {
                if let Some(launcher) = current.as_ref() {
                    launcher.open_file(file.clone());
                }
            }),
            "Show in Files" => {
                let path = file.path().to_path_buf();
                let launcher = CURRENT.with_borrow(|current| {
                    current
                        .as_ref()
                        .map(|launcher| (launcher.services.clone(), launcher.instance))
                });
                if let Some((services, instance)) = launcher {
                    topbar_services::Runtime::handle().spawn(async move {
                        if let Err(error) = services.applications.reveal_path(path).await {
                            glib::idle_add_once(move || {
                                status_instance(instance, &error.to_string())
                            });
                        }
                    });
                }
            }
            "Copy Path" => gtk4::prelude::WidgetExt::display(&anchor)
                .clipboard()
                .set_text(&file.display_path()),
            _ => unreachable!(),
        });
        actions.append(&action);
    }
    menu.set_child(Some(&actions));
    menu.set_parent(anchor);
    menu.popup();
}

fn normalize_identity(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('@')
        .strip_suffix(".desktop")
        .unwrap_or(value.trim().trim_start_matches('@'))
        .to_ascii_lowercase()
}

fn unambiguous_application<'a>(
    applications: &'a [Application],
    app_id: &str,
) -> Option<&'a Application> {
    let normalized = normalize_identity(app_id);
    let mut matches = applications.iter().filter(|application| {
        application
            .identities()
            .any(|identity| normalize_identity(identity) == normalized)
    });
    let application = matches.next()?;
    matches.next().is_none().then_some(application)
}

fn unambiguous_application_id(applications: &[Application], app_id: &str) -> Option<String> {
    unambiguous_application(applications, app_id).map(|app| app.desktop_id.clone())
}

fn set_window_icon(image: &Image, serialized: Option<&str>) {
    if let Some(icon) = serialized.and_then(|value| gio::Icon::for_string(value).ok()) {
        image.set_from_gicon(&icon);
    } else {
        image.set_icon_name(Some("window-symbolic"));
    }
}

/// Stable file IDs retain every original pathname byte, including invalid UTF-8.
fn hex_identity(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Render deterministic fuzzy-match positions without trusting result text as
/// markup. Labels remain ordinary accessible text; Pango only paints emphasis.
fn highlight(text: &str, query: &str) -> String {
    let positions = rank_match(query, text)
        .map(|matched| matched.positions)
        .unwrap_or_default();
    text.chars()
        .enumerate()
        .map(|(index, character)| {
            let escaped = glib::markup_escape_text(&character.to_string());
            if positions.contains(&index) {
                format!("<b>{escaped}</b>")
            } else {
                escaped.to_string()
            }
        })
        .collect()
}

/// Retain a selected file while a cold catalog grows and changes its top 30.
///
/// A pinned result replaces the ranked tail, so search still exposes at most
/// the service limit. The caller supplies no selected value after a query
/// change, which naturally releases the pin for the new query.
fn pin_selected_result<T: Clone + PartialEq>(
    matches: &mut Vec<T>,
    selected: Option<T>,
    limit: usize,
) {
    let Some(selected) = selected else {
        return;
    };
    if matches.contains(&selected) {
        return;
    }
    if limit == 0 {
        matches.clear();
        return;
    }
    if matches.len() >= limit {
        matches.truncate(limit - 1);
    }
    matches.push(selected);
}

fn pin_selected_file_result(matches: &mut Vec<FileResult>, selected: Option<FileResult>) {
    let selected = selected.filter(|selected| {
        !matches
            .iter()
            .any(|matched| matched.entry == selected.entry)
    });
    pin_selected_result(matches, selected, RESULT_LIMIT);
}

/// Return whether a service update contains a new visible catalog.
fn catalog_revision_changed(last_seen: &Cell<u64>, current: u64) -> bool {
    last_seen.replace(current) != current
}

fn navigation_id(item: &ResultItem, frequent: bool) -> String {
    if frequent {
        format!("frequent:{}", item.id())
    } else {
        item.id()
    }
}

/// Keep an existing selection when an asynchronous result update reorders rows.
fn stable_index(items: &[ResultItem], selected_id: Option<&str>) -> usize {
    selected_id
        .map(|id| id.strip_prefix("frequent:").unwrap_or(id))
        .and_then(|selected_id| items.iter().position(|item| item.id() == selected_id))
        .unwrap_or(0)
}

fn section_messages(
    filter: Filter,
    query: &str,
    applications: &ApplicationsState,
    files: &FileSearchState,
    windows: &WindowsSnapshot,
) -> SectionMessages {
    let wants = |kind| filter == Filter::All || filter == kind;
    SectionMessages {
        applications: wants(Filter::Applications).then(|| {
            applications
                .warning
                .clone()
                .or_else(|| applications.loading.then(|| "Applications are refreshing.".into()))
        })
        .flatten(),
        windows: (wants(Filter::Windows)
            && (!query.is_empty() || filter == Filter::Windows)
            && !windows.connected)
            .then(|| {
                if query.is_empty() {
                    "Windows are unavailable while niri reconnects.".into()
                } else {
                    "Window search is unavailable while niri reconnects.".into()
                }
            }),
        files: (!query.is_empty() && wants(Filter::Files))
            .then(|| {
                files.warning.clone().or_else(|| {
                    files.partial.then(|| {
                        "File search has partial coverage because its catalog limit was reached.".into()
                    })
                })
            })
            .flatten()
            .or_else(|| {
                (!query.is_empty() && wants(Filter::Files) && files.discovering)
                    .then(|| "Files are still being indexed.".into())
            }),
    }
}

fn global_status_message(
    query: &str,
    has_items: bool,
    activation_message: Option<String>,
) -> Option<String> {
    activation_message
        .or_else(|| (!query.is_empty() && !has_items).then(|| "No matching results.".into()))
}

fn section_presence(items: &[ResultItem], messages: &SectionMessages) -> SectionPresence {
    SectionPresence {
        applications: messages.applications.is_some()
            || items
                .iter()
                .any(|item| matches!(item, ResultItem::Application(_))),
        windows: messages.windows.is_some()
            || items
                .iter()
                .any(|item| matches!(item, ResultItem::Window { .. })),
        files: messages.files.is_some()
            || items.iter().any(|item| matches!(item, ResultItem::File(_))),
    }
}

fn section_structure_changed(current: SectionPresence, desired: SectionPresence) -> bool {
    current != desired
}

fn keyboard_focus_after_navigation() -> KeyboardFocus {
    KeyboardFocus::Search
}

fn spatial_target(
    targets: &[NavigationTarget],
    container: &gtk4::Widget,
    selected_id: &str,
    direction: Direction,
) -> Option<String> {
    let bounds = targets
        .iter()
        .filter_map(|target| {
            target.button.compute_bounds(container).map(|bounds| {
                (
                    target.id.as_str(),
                    NavigationBounds {
                        x: bounds.x(),
                        y: bounds.y(),
                        width: bounds.width(),
                        height: bounds.height(),
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    spatial_target_for_bounds(&bounds, selected_id, direction).map(str::to_string)
}

fn spatial_target_for_bounds<'a>(
    bounds: &[(&'a str, NavigationBounds)],
    selected_id: &str,
    direction: Direction,
) -> Option<&'a str> {
    let current = bounds
        .iter()
        .find_map(|(id, bounds)| (*id == selected_id).then_some(*bounds))?;
    spatial_candidate(
        current,
        bounds
            .iter()
            .filter_map(|(id, bounds)| (*id != selected_id).then_some((*id, *bounds))),
        direction,
    )
}

fn spatial_candidate<'a>(
    current: NavigationBounds,
    candidates: impl Iterator<Item = (&'a str, NavigationBounds)>,
    direction: Direction,
) -> Option<&'a str> {
    candidates
        .filter_map(|(id, candidate)| {
            let (primary, perpendicular, overlap) = match direction {
                Direction::Up => (
                    current.center_y() - candidate.center_y(),
                    (current.center_x() - candidate.center_x()).abs(),
                    ranges_overlap(
                        current.x,
                        current.x + current.width,
                        candidate.x,
                        candidate.x + candidate.width,
                    ),
                ),
                Direction::Down => (
                    candidate.center_y() - current.center_y(),
                    (current.center_x() - candidate.center_x()).abs(),
                    ranges_overlap(
                        current.x,
                        current.x + current.width,
                        candidate.x,
                        candidate.x + candidate.width,
                    ),
                ),
                Direction::Left => (
                    current.center_x() - candidate.center_x(),
                    (current.center_y() - candidate.center_y()).abs(),
                    ranges_overlap(
                        current.y,
                        current.y + current.height,
                        candidate.y,
                        candidate.y + candidate.height,
                    ),
                ),
                Direction::Right => (
                    candidate.center_x() - current.center_x(),
                    (current.center_y() - candidate.center_y()).abs(),
                    ranges_overlap(
                        current.y,
                        current.y + current.height,
                        candidate.y,
                        candidate.y + candidate.height,
                    ),
                ),
            };
            (primary > 0.0).then_some((!overlap, primary, perpendicular, id))
        })
        .min_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.total_cmp(&right.1))
                .then_with(|| left.2.total_cmp(&right.2))
                .then_with(|| left.3.cmp(right.3))
        })
        .map(|(_, _, _, id)| id)
}

fn ranges_overlap(start: f32, end: f32, other_start: f32, other_end: f32) -> bool {
    start < other_end && other_start < end
}

fn scroll_value_for_bounds(current: f64, page: f64, start: f64, end: f64) -> Option<f64> {
    if start < current {
        Some(start)
    } else if end > current + page {
        Some(end - page)
    } else {
        None
    }
}

async fn run_appearance(command: Option<Vec<String>>) -> Result<(), topbar_services::SvcError> {
    let command = command.ok_or_else(|| {
        topbar_services::SvcError::Rejected("appearance action is not configured".into())
    })?;
    topbar_services::proc::run_argv(&command).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_exec_launches_even_when_dbus_activatable_and_preserves_normal_percent_k() {
        let root = std::env::temp_dir().join(format!(
            "topbar-desktop-exec-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let executable = root.join("record");
        std::fs::write(&executable, "printf '%s' \"$1\" > \"$2\"\n").unwrap();
        let context = glib::MainContext::new();
        let main_loop = glib::MainLoop::new(Some(&context), false);
        let root_for_launch = root.clone();
        let loop_for_launch = main_loop.clone();
        context
            .with_thread_default(|| {
                context.spawn_local(async move {
                    for (name, activatable, argument) in [
                        ("org.example.Direct", true, "exec-not-dbus"),
                        ("org.example.Normal", false, "%k"),
                    ] {
                        let desktop = root_for_launch.join(format!("{name}.desktop"));
                        let marker = root_for_launch.join(format!("{name}.marker"));
                        std::fs::write(
                            &desktop,
                            format!(
                                "[Desktop Entry]\nType=Application\nName={name}\nExec=sh {} {argument} {}\nDBusActivatable={activatable}\n",
                                glib::shell_quote(&executable).to_string_lossy(),
                                glib::shell_quote(&marker).to_string_lossy(),
                            ),
                        )
                        .unwrap();
                        let info = DesktopAppInfo::from_filename(&desktop).unwrap();
                        exec_app_info(&info)
                            .await
                            .unwrap()
                            .launch_uris_future(&[], gio::AppLaunchContext::NONE)
                            .await
                            .unwrap();
                        // GIO reports a successful spawn, not that the child has
                        // already written its output.
                        let mut actual = None;
                        for _ in 0..100 {
                            actual = std::fs::read_to_string(&marker).ok();
                            if actual.is_some() {
                                break;
                            }
                            glib::timeout_future(std::time::Duration::from_millis(10)).await;
                        }
                        assert_eq!(
                            actual.as_deref(),
                            Some(if activatable {
                                "exec-not-dbus"
                            } else {
                                desktop.to_str().unwrap()
                            })
                        );
                    }
                    loop_for_launch.quit();
                });
                main_loop.run();
            })
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normalizes_only_explicit_desktop_identity_variants() {
        assert_eq!(
            normalize_identity("org.Example.App.desktop"),
            "org.example.app"
        );
        assert_eq!(normalize_identity("@org.Example.App"), "org.example.app");
    }

    #[test]
    fn results_follow_visible_priority_and_keep_file_and_window_identities() {
        let applications = ApplicationsState {
            entries: vec![Application {
                desktop_id: "theme-editor.desktop".into(),
                name: "Theme Editor".into(),
                generic_name: None,
                executable: "editor".into(),
                keywords: Vec::new(),
                aliases: Vec::new(),
                icon: None,
            }],
            ..Default::default()
        };
        let windows = WindowsSnapshot {
            connected: true,
            windows: vec![topbar_services::WindowView {
                id: 42,
                app_id: "theme-editor".into(),
                title: "Theme draft".into(),
                workspace: None,
                output: None,
                focused_at_ms: None,
            }],
        };
        let files = [FileResult {
            entry: FileEntry::new("/tmp/theme-draft.txt".into()),
            modified: None,
            size: None,
        }];
        let results = Launcher::collect(
            Filter::All,
            "theme",
            &applications,
            &windows,
            &files,
            [true, false],
        );
        let ids = results.iter().map(ResultItem::id).collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "app:theme-editor.desktop",
                "action:theme",
                "window:42",
                "file:2f746d702f7468656d652d64726166742e747874",
            ],
        );
        assert_eq!(stable_index(&results, None), 0);
        assert_eq!(stable_index(&results, Some("window:42")), 2);
        assert_eq!(
            Launcher::collect(
                Filter::Actions,
                "theme",
                &applications,
                &windows,
                &files,
                [true, false]
            )
            .iter()
            .map(ResultItem::id)
            .collect::<Vec<_>>(),
            ["action:theme"],
        );
    }

    #[test]
    fn blank_actions_tab_lists_only_configured_actions_without_changing_all() {
        let applications = ApplicationsState {
            entries: vec![Application {
                desktop_id: "editor.desktop".into(),
                name: "Editor".into(),
                generic_name: None,
                executable: "editor".into(),
                keywords: Vec::new(),
                aliases: Vec::new(),
                icon: None,
            }],
            ..Default::default()
        };
        let windows = WindowsSnapshot::default();
        let ids = |filter, enabled| {
            Launcher::collect(filter, "", &applications, &windows, &[], enabled)
                .iter()
                .map(ResultItem::id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids(Filter::Actions, [true, true]),
            ["action:theme", "action:wallpaper"]
        );
        assert_eq!(ids(Filter::Actions, [true, false]), ["action:theme"]);
        assert_eq!(ids(Filter::Actions, [false, true]), ["action:wallpaper"]);
        assert!(ids(Filter::Actions, [false, false]).is_empty());
        assert_eq!(ids(Filter::All, [true, true]), ["app:editor.desktop"]);
    }

    #[test]
    fn blank_windows_tab_tracks_open_windows_without_changing_all() {
        let applications = ApplicationsState {
            entries: vec![Application {
                desktop_id: "editor.desktop".into(),
                name: "Editor".into(),
                generic_name: None,
                executable: "editor".into(),
                keywords: Vec::new(),
                aliases: Vec::new(),
                icon: None,
            }],
            ..Default::default()
        };
        let mut windows = WindowsSnapshot {
            connected: true,
            windows: vec![topbar_services::WindowView {
                id: 42,
                app_id: "editor".into(),
                title: "Draft".into(),
                workspace: Some("work".into()),
                output: Some("HDMI-A-1".into()),
                focused_at_ms: None,
            }],
        };
        let ids = |filter, windows: &WindowsSnapshot| {
            Launcher::collect(filter, "", &applications, windows, &[], [true, true])
                .iter()
                .map(ResultItem::id)
                .collect::<Vec<_>>()
        };
        let items = Launcher::collect(
            Filter::Windows,
            "",
            &applications,
            &windows,
            &[],
            [true, true],
        );
        assert_eq!(
            items.iter().map(ResultItem::id).collect::<Vec<_>>(),
            ["window:42"]
        );
        assert!(
            matches!(&items[0], ResultItem::Window { title, context, .. }
            if title == "Draft" && context == "work · HDMI-A-1")
        );
        assert_eq!(ids(Filter::All, &windows), ["app:editor.desktop"]);

        windows.windows.push(topbar_services::WindowView {
            id: 7,
            app_id: "browser".into(),
            title: "Browser".into(),
            workspace: None,
            output: None,
            focused_at_ms: None,
        });
        assert_eq!(ids(Filter::Windows, &windows), ["window:7", "window:42"]);
        windows.connected = false;
        assert!(ids(Filter::Windows, &windows).is_empty());
        assert_eq!(
            section_messages(
                Filter::Windows,
                "",
                &applications,
                &FileSearchState::default(),
                &windows
            )
            .windows
            .as_deref(),
            Some("Windows are unavailable while niri reconnects.")
        );
        assert_eq!(
            section_messages(
                Filter::All,
                "",
                &applications,
                &FileSearchState::default(),
                &windows
            )
            .windows,
            None
        );
    }

    #[test]
    fn result_updates_keep_the_same_stable_selection() {
        let rows = [ResultItem::Wallpaper, ResultItem::Theme];
        assert_eq!(stable_index(&rows, Some("action:theme")), 1);
        assert_eq!(stable_index(&rows, Some("gone")), 0);
    }

    #[test]
    fn duplicate_application_rows_keep_distinct_focus_and_navigation() {
        let application_item = |id: &str| {
            ResultItem::Application(Application {
                desktop_id: id.into(),
                name: id.into(),
                generic_name: None,
                executable: "editor".into(),
                keywords: Vec::new(),
                aliases: Vec::new(),
                icon: None,
            })
        };
        let app = application_item("editor.desktop");
        let frequent = navigation_id(&app, true);
        let first = application_item("aardvark.desktop");
        let other = application_item("other.desktop");
        let first_id = navigation_id(&first, false);
        let application = navigation_id(&app, false);
        let next = navigation_id(&other, false);
        let bounds = |x, y| NavigationBounds {
            x,
            y,
            width: 80.0,
            height: 80.0,
        };
        let targets = [
            (frequent.as_str(), bounds(100.0, 0.0)),
            (first_id.as_str(), bounds(0.0, 150.0)),
            (application.as_str(), bounds(100.0, 150.0)),
            (next.as_str(), bounds(200.0, 150.0)),
        ];

        assert_eq!(targets.iter().filter(|(id, _)| *id == frequent).count(), 1);
        assert_eq!(
            targets.iter().filter(|(id, _)| *id == application).count(),
            1
        );
        assert_eq!(stable_index(&[first, app, other], Some(&frequent)), 1);
        assert_eq!(
            spatial_target_for_bounds(&targets, &frequent, Direction::Down),
            Some(application.as_str())
        );
        assert_eq!(
            spatial_target_for_bounds(&targets, &application, Direction::Right),
            Some(next.as_str())
        );
    }

    #[test]
    fn a_cold_catalog_update_pins_the_selected_file_at_the_result_tail() {
        let mut ranked = (0..RESULT_LIMIT).collect::<Vec<_>>();
        pin_selected_result(&mut ranked, Some(99), RESULT_LIMIT);
        assert_eq!(ranked.len(), RESULT_LIMIT);
        assert_eq!(ranked.last(), Some(&99));
        assert!(!ranked.contains(&(RESULT_LIMIT - 1)));

        // A result already in the ranked set retains its normal position.
        let mut already_ranked = vec![3, 2, 1];
        pin_selected_result(&mut already_ranked, Some(2), RESULT_LIMIT);
        assert_eq!(already_ranked, vec![3, 2, 1]);
    }

    #[test]
    fn a_new_modified_date_does_not_duplicate_the_selected_file() {
        let selected = FileResult {
            entry: FileEntry::new("/tmp/theme-draft.txt".into()),
            modified: None,
            size: None,
        };
        let mut refreshed = vec![FileResult {
            modified: Some(SystemTime::UNIX_EPOCH),
            size: Some(1024),
            ..selected.clone()
        }];
        pin_selected_file_result(&mut refreshed, Some(selected));
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].modified, Some(SystemTime::UNIX_EPOCH));
        assert_eq!(refreshed[0].size, Some(1024));
    }

    #[test]
    fn metadata_updates_do_not_rerank_the_same_catalog_revision() {
        let revision = Cell::new(4);
        assert!(!catalog_revision_changed(&revision, 4));
        assert!(!catalog_revision_changed(&revision, 4));
        assert!(catalog_revision_changed(&revision, 5));
        assert!(!catalog_revision_changed(&revision, 5));
    }

    #[test]
    fn file_identity_does_not_lossily_merge_non_utf8_paths() {
        assert_ne!(hex_identity(&[0xff]), hex_identity(&[0xfe]));
    }

    #[test]
    fn ambiguous_window_alias_does_not_earn_usage() {
        let app = |id: &str| Application {
            desktop_id: id.into(),
            name: id.into(),
            generic_name: None,
            executable: "ignored".into(),
            keywords: Vec::new(),
            aliases: vec!["shared".into()],
            icon: None,
        };
        assert_eq!(
            unambiguous_application_id(&[app("one.desktop"), app("two.desktop")], "shared"),
            None
        );
    }

    #[test]
    fn window_icons_follow_unambiguous_catalog_matches_and_updates() {
        let app = |id: &str, aliases: Vec<String>, icon: Option<&str>| Application {
            desktop_id: id.into(),
            name: id.into(),
            generic_name: None,
            executable: id.into(),
            keywords: Vec::new(),
            aliases,
            icon: icon.map(str::to_owned),
        };
        let window = |id, app_id: &str| topbar_services::WindowView {
            id,
            app_id: app_id.into(),
            title: "Document".into(),
            workspace: None,
            output: None,
            focused_at_ms: None,
        };
        let mut applications = ApplicationsState {
            entries: vec![
                app("org.example.editor.desktop", vec![], Some("editor-icon")),
                app("first.desktop", vec!["shared".into()], Some("first-icon")),
                app("second.desktop", vec!["shared".into()], Some("second-icon")),
            ],
            ..Default::default()
        };
        let windows = WindowsSnapshot {
            connected: true,
            windows: vec![
                window(1, "@org.Example.Editor"),
                window(2, "unknown"),
                window(3, "shared"),
            ],
        };
        let icons = |applications: &ApplicationsState| {
            Launcher::collect(Filter::Windows, "", applications, &windows, &[], [false; 2])
                .into_iter()
                .map(|item| match item {
                    ResultItem::Window { icon, .. } => icon,
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            icons(&applications),
            [Some("editor-icon".into()), None, None]
        );
        assert!(gio::Icon::for_string("editor-icon").is_ok());
        applications.entries[0].icon = Some("replacement-icon".into());
        assert_eq!(icons(&applications)[0].as_deref(), Some("replacement-icon"));
        applications.entries[0].icon = None;
        assert_eq!(icons(&applications)[0], None);
        applications.entries.push(app(
            "other.desktop",
            vec!["org.example.editor".into()],
            Some("other-icon"),
        ));
        assert_eq!(icons(&applications), [None, None, None]);
    }

    #[test]
    fn highlighting_escapes_labels_and_marks_fuzzy_positions() {
        assert_eq!(highlight("A<B", "ab"), "<b>A</b>&lt;<b>B</b>");
    }

    #[test]
    fn compact_outputs_cap_the_scroller_before_rows_leave_the_screen() {
        let layout = launcher_layout(768);
        assert_eq!(layout.margin, COMPACT_OUTPUT_MARGIN);
        assert_eq!(layout.scroll_max_height, 368);
        assert_eq!(
            2 * layout.margin
                + 2 * LAUNCHER_PADDING
                + SEARCH_HEIGHT
                + FILTER_HEIGHT
                + STATUS_HEIGHT
                + 3 * ROOT_GAP
                + layout.scroll_max_height,
            768 - OUTPUT_HEADROOM
        );
    }

    #[test]
    fn very_short_outputs_reduce_the_scroll_minimum_too() {
        let layout = launcher_layout(320);
        assert_eq!(layout.scroll_min_height, layout.scroll_max_height);
        assert!(layout.scroll_max_height < MIN_SCROLL_HEIGHT);
    }

    #[test]
    fn section_messages_keep_discovery_and_failures_with_their_source() {
        let messages = section_messages(
            Filter::All,
            "report",
            &ApplicationsState {
                loading: true,
                ..Default::default()
            },
            &FileSearchState {
                discovering: true,
                warning: Some("Files could not read one directory.".into()),
                ..Default::default()
            },
            &WindowsSnapshot {
                connected: false,
                ..Default::default()
            },
        );
        assert_eq!(
            messages.applications.as_deref(),
            Some("Applications are refreshing.")
        );
        assert_eq!(
            messages.windows.as_deref(),
            Some("Window search is unavailable while niri reconnects.")
        );
        assert_eq!(
            messages.files.as_deref(),
            Some("Files could not read one directory.")
        );
    }

    #[test]
    fn section_structure_changes_only_when_a_local_section_appears_or_disappears() {
        let current = SectionPresence::default();
        let warning = SectionMessages {
            files: Some("Files could not read one directory.".into()),
            ..Default::default()
        };
        let appeared = section_presence(&[], &warning);
        assert!(appeared.files);
        assert!(section_structure_changed(current, appeared));
        assert!(!section_structure_changed(appeared, appeared));
        assert!(section_structure_changed(
            appeared,
            SectionPresence::default()
        ));
    }

    #[test]
    fn keyboard_navigation_leaves_typing_in_the_search_entry() {
        assert_eq!(keyboard_focus_after_navigation(), KeyboardFocus::Search);
    }

    #[test]
    fn global_status_does_not_replace_an_activation_failure() {
        let status = global_status_message(
            "missing",
            false,
            Some("Could not open file: no handler".to_string()),
        );
        assert_eq!(status.as_deref(), Some("Could not open file: no handler"));
        assert_eq!(
            global_status_message("missing", false, None).as_deref(),
            Some("No matching results.")
        );
    }

    #[test]
    fn spatial_navigation_uses_the_actual_grid_columns() {
        let bounds = |x, y| NavigationBounds {
            x,
            y,
            width: 80.0,
            height: 80.0,
        };
        let tiles = [
            ("a", bounds(0.0, 0.0)),
            ("b", bounds(100.0, 0.0)),
            ("c", bounds(200.0, 0.0)),
            ("d", bounds(0.0, 100.0)),
            ("e", bounds(100.0, 100.0)),
        ];
        assert_eq!(
            spatial_candidate(
                tiles[1].1,
                tiles.iter().copied().filter(|(id, _)| *id != "b"),
                Direction::Down
            ),
            Some("e")
        );
        assert_eq!(
            spatial_candidate(
                tiles[4].1,
                tiles.iter().copied().filter(|(id, _)| *id != "e"),
                Direction::Up
            ),
            Some("b")
        );
    }

    #[test]
    fn spatial_navigation_crosses_action_row_window_grid_and_file_row() {
        let targets = [
            (
                "action",
                NavigationBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 300.0,
                    height: 44.0,
                },
            ),
            (
                "window",
                NavigationBounds {
                    x: 0.0,
                    y: 52.0,
                    width: 148.0,
                    height: 118.0,
                },
            ),
            (
                "file",
                NavigationBounds {
                    x: 0.0,
                    y: 190.0,
                    width: 300.0,
                    height: 30.0,
                },
            ),
        ];
        assert_eq!(
            spatial_candidate(
                targets[1].1,
                targets.iter().copied().filter(|(id, _)| *id != "window"),
                Direction::Up
            ),
            Some("action")
        );
        assert_eq!(
            spatial_candidate(
                targets[1].1,
                targets.iter().copied().filter(|(id, _)| *id != "window"),
                Direction::Down
            ),
            Some("file")
        );
    }

    #[test]
    fn selected_result_scrolls_using_content_coordinates() {
        assert_eq!(
            scroll_value_for_bounds(300.0, 200.0, 520.0, 550.0),
            Some(350.0)
        );
        assert_eq!(
            scroll_value_for_bounds(300.0, 200.0, 250.0, 280.0),
            Some(250.0)
        );
        assert_eq!(scroll_value_for_bounds(300.0, 200.0, 320.0, 360.0), None);
    }
}
