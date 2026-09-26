//! The panel-owned full-screen application, window and filename launcher.
//!
//! The launcher intentionally owns one layer surface at a time.  Its result
//! model uses stable identities rather than row indexes, so a file-discovery or
//! compositor update can redraw the list without changing what Enter means.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gio::prelude::*;
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
use crate::style::classes;
use crate::surfaces::modal;
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
    Windows,
    Files,
    Actions,
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
    button: Button,
}

#[derive(Clone)]
enum ResultItem {
    Application(Application),
    Window {
        id: u64,
        app_id: String,
        title: String,
        context: String,
    },
    File(FileEntry),
    Theme,
    Wallpaper,
}

impl ResultItem {
    fn id(&self) -> String {
        match self {
            Self::Application(app) => format!("app:{}", app.desktop_id),
            Self::Window { id, .. } => format!("window:{id}"),
            Self::File(file) => format!("file:{}", hex_identity(&file.identity())),
            Self::Theme => "action:theme".to_string(),
            Self::Wallpaper => "action:wallpaper".to_string(),
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Application(app) => app.name.clone(),
            Self::Window { title, .. } => title.clone(),
            Self::File(file) => file.basename(),
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
            Self::File(file) => file.home_relative_path(),
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
    results: gtk4::Box,
    status: Label,
    section_statuses: RefCell<SectionStatusLabels>,
    global_message: RefCell<Option<String>>,
    filter: Cell<Filter>,
    instance: u64,
    selected_id: RefCell<Option<String>>,
    items: RefCell<Vec<ResultItem>>,
    selected_index: Cell<usize>,
    navigation_targets: RefCell<Vec<NavigationTarget>>,
    file_matches: RefCell<Vec<FileEntry>>,
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
        // The backdrop shares the launcher namespace: the niri rule that owns
        // wallpaper xray/blur applies to the actual full-screen surface.
        let backdrop = modal::backdrop(monitor, "topbar-launcher", classes::LAUNCHER_BACKDROP);
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
        search.set_placeholder_text(Some("Search applications, windows, files, and actions"));
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
            (Filter::Windows, "Windows"),
            (Filter::Files, "Files"),
            (Filter::Actions, "Actions"),
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
            results,
            status,
            section_statuses: RefCell::new(SectionStatusLabels::default()),
            global_message: RefCell::new(None),
            filter: Cell::new(Filter::All),
            instance: NEXT_INSTANCE.with(|next| {
                let instance = next.get();
                next.set(instance.wrapping_add(1));
                instance
            }),
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
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_released(|_, _, _, _| dismiss());
        self.backdrop.add_controller(click);

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
        // Handle navigation and activation before the focused entry consumes
        // keys such as Return; unhandled typing still reaches the entry.
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let launcher = Rc::downgrade(self);
            move |_, key, _, modifiers| {
                let Some(launcher) = launcher.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                match key {
                    gdk::Key::Escape => dismiss(),
                    gdk::Key::Down => launcher.move_selection(Direction::Down),
                    gdk::Key::Up => launcher.move_selection(Direction::Up),
                    gdk::Key::Right => launcher.move_selection(Direction::Right),
                    gdk::Key::Left => launcher.move_selection(Direction::Left),
                    gdk::Key::Return | gdk::Key::KP_Enter => launcher
                        .activate_selected(modifiers.contains(gdk::ModifierType::CONTROL_MASK)),
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
                if let Some(launcher) = launcher.upgrade() {
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
                .map(|matched| matched.entry)
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
            pin_selected_result(&mut matches, selected_file, RESULT_LIMIT);
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
        let selected_id = self.selected_id.borrow().clone();
        let Some(selected_id) = selected_id.as_deref() else {
            return;
        };
        let targets = self.navigation_targets.borrow();
        let Some(next_id) = spatial_target(
            &targets,
            &self.results.clone().upcast(),
            selected_id,
            direction,
        ) else {
            return;
        };
        let next = stable_index(&self.items.borrow(), Some(&next_id));
        for target in targets.iter() {
            if target.id == next_id {
                target.button.add_css_class(classes::LAUNCHER_ITEM_SELECTED);
                self.scroll_selected_into_view(&target.button);
            } else if target.id == selected_id {
                target
                    .button
                    .remove_css_class(classes::LAUNCHER_ITEM_SELECTED);
            }
        }
        self.selected_index.set(next);
        *self.selected_id.borrow_mut() = Some(next_id);
        if keyboard_focus_after_navigation() == KeyboardFocus::Search && !self.search.has_focus() {
            self.search.grab_focus();
            self.search.set_position(-1);
        }
    }

    fn scroll_selected_into_view(&self, button: &Button) {
        // Compare content coordinates with the adjustment's content offset.
        let Some(bounds) = button.compute_bounds(&self.results) else {
            return;
        };
        let adjustment = self.scroll.vadjustment();
        let current = adjustment.value();
        let target_start = f64::from(bounds.y());
        let target_end = target_start + f64::from(bounds.height());
        if let Some(value) =
            scroll_value_for_bounds(current, adjustment.page_size(), target_start, target_end)
        {
            adjustment.set_value(value);
        }
    }

    fn render(&self) {
        let query = self.search.text().to_string();
        let applications = self.services.applications.state().borrow().clone();
        let files = self.services.files.current();
        let windows = self.services.compositor.windows().borrow().clone();
        let old_id = self.selected_id.borrow().clone();
        let items = self.collect(&query, &applications, &windows);
        self.items.replace(items);
        let selected = stable_index(&self.items.borrow(), old_id.as_deref());
        self.selected_index.set(selected);
        *self.selected_id.borrow_mut() = self.items.borrow().get(selected).map(ResultItem::id);
        self.draw(&query, &applications, &files, &windows);
    }

    fn set_global_message(&self, message: &str) {
        *self.global_message.borrow_mut() = Some(message.to_string());
        self.status.set_text(message);
    }

    fn clear_global_message(&self) {
        self.global_message.borrow_mut().take();
    }

    fn collect(
        &self,
        query: &str,
        applications: &ApplicationsState,
        windows: &WindowsSnapshot,
    ) -> Vec<ResultItem> {
        let wants = |kind| self.filter.get() == Filter::All || self.filter.get() == kind;
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
        if !query.is_empty() && wants(Filter::Windows) && windows.connected {
            let mut matches = windows
                .windows
                .iter()
                .filter_map(|window| {
                    let app_name = applications
                        .entries
                        .iter()
                        .find(|app| {
                            app.identities().any(|identity| {
                                normalize_identity(identity) == normalize_identity(&window.app_id)
                            })
                        })
                        .map(|app| app.name.as_str());
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
                    Some((score, window))
                })
                .collect::<Vec<_>>();
            matches.sort_by(|(left_score, left), (right_score, right)| {
                right_score
                    .cmp(left_score)
                    .then_with(|| left.id.cmp(&right.id))
            });
            output.extend(matches.into_iter().map(|(_, window)| {
                ResultItem::Window {
                    id: window.id,
                    app_id: window.app_id.clone(),
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
            output.extend(
                self.file_matches
                    .borrow()
                    .iter()
                    .cloned()
                    .map(ResultItem::File),
            );
        }
        if !query.is_empty() && wants(Filter::Actions) {
            for item in [ResultItem::Theme, ResultItem::Wallpaper] {
                if rank_match(query, &item.title()).is_some() && self.action_enabled(&item) {
                    output.push(item);
                }
            }
        }
        output
    }

    fn draw(
        &self,
        query: &str,
        applications: &ApplicationsState,
        files: &FileSearchState,
        windows: &WindowsSnapshot,
    ) {
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
                );
            }
            self.add_section(
                Section::Applications,
                "Applications",
                self.items.borrow().clone(),
                section_messages.applications.as_deref(),
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
            );
            self.add_list_section(
                Some(Section::Windows),
                "Windows",
                items
                    .iter()
                    .filter(|item| matches!(item, ResultItem::Window { .. }))
                    .cloned()
                    .collect(),
                section_messages.windows.as_deref(),
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
            let selected = self.selected_id.borrow().as_ref() == Some(&item.id());
            let button = self.item_button(item, selected);
            grid.insert(&button, -1);
        }
        self.results.append(&grid);
    }

    fn item_button(&self, item: ResultItem, selected: bool) -> Button {
        let button = Button::new();
        button.add_css_class(classes::LAUNCHER_ITEM);
        if selected {
            button.add_css_class(classes::LAUNCHER_ITEM_SELECTED);
        }
        let body = gtk4::Box::new(Orientation::Vertical, 4);
        let icon = match &item {
            ResultItem::Application(app) => app
                .icon
                .as_deref()
                .and_then(|serialized| gio::Icon::for_string(serialized).ok())
                .map(|icon| Image::from_gicon(&icon))
                .unwrap_or_else(|| Image::from_icon_name("application-x-executable")),
            ResultItem::Window { .. } => Image::from_icon_name("window-symbolic"),
            ResultItem::File(_) => Image::from_icon_name("text-x-generic-symbolic"),
            ResultItem::Theme => Image::from_icon_name("preferences-desktop-theme-symbolic"),
            ResultItem::Wallpaper => Image::from_icon_name("image-x-generic-symbolic"),
        };
        icon.add_css_class(classes::LAUNCHER_ICON);
        body.append(&icon);
        let title = Label::new(None);
        title.add_css_class(classes::LAUNCHER_ITEM_TITLE);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title.set_max_width_chars(22);
        title.set_markup(&highlight(&item.title(), &self.search.text()));
        body.append(&title);
        let subtitle = Label::new(None);
        subtitle.add_css_class(classes::LAUNCHER_ITEM_SUBTITLE);
        subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        subtitle.set_max_width_chars(22);
        subtitle.set_markup(&highlight(&item.subtitle(), &self.search.text()));
        body.append(&subtitle);
        button.set_child(Some(&body));
        let interaction_motion = Animation::new(&button);
        let pointer = gtk4::EventControllerMotion::new();
        pointer.connect_enter({
            let button = button.clone();
            let animation = interaction_motion.clone();
            move |_, _, _| {
                let button = button.clone();
                animation.start(
                    AnimationParams::new(120).with_easing(Easing::EaseOutCubic),
                    Box::new(move |progress| button.set_opacity(0.92 + 0.08 * progress)),
                    None,
                );
            }
        });
        pointer.connect_leave({
            let button = button.clone();
            move |_| button.set_opacity(1.0)
        });
        button.add_controller(pointer);
        let click_item = item.clone();
        button.connect_clicked(move |_| {
            CURRENT.with_borrow(|current| {
                if let Some(launcher) = current.as_ref() {
                    launcher.activate(click_item.clone(), false);
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
                    ResultItem::File(file) => file_menu(&widget, file.clone()),
                    _ => {}
                }
            }
        });
        button.add_controller(secondary);
        self.navigation_targets.borrow_mut().push(NavigationTarget {
            id: item.id(),
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
            let selected = self.selected_id.borrow().as_ref() == Some(&item.id());
            let button = self.item_button(item, selected);
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

    fn action_enabled(&self, item: &ResultItem) -> bool {
        match item {
            ResultItem::Theme => self.config.appearance.theme_command.is_some(),
            ResultItem::Wallpaper => self.config.appearance.wallpaper_command.is_some(),
            _ => true,
        }
    }

    fn activate_selected(&self, fresh: bool) {
        if let Some(item) = self.items.borrow().get(self.selected_index.get()).cloned() {
            self.activate(item, fresh);
        }
    }

    fn activate(&self, item: ResultItem, fresh: bool) {
        if let ResultItem::Application(app) = &item {
            self.activate_application(app.clone(), fresh);
            return;
        }
        if let ResultItem::File(file) = &item {
            self.open_file(file.clone());
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

    fn activate_application(&self, app: Application, fresh: bool) {
        if fresh {
            self.launch_application(app);
            return;
        }

        // The stream snapshot is intentionally not used for this decision:
        // it can be empty during startup or reconnecting while niri still has
        // a matching window. The service makes one live query off GTK's main
        // thread and launches only after niri confirms no match exists.
        let identities = app.identities().map(str::to_owned).collect::<Vec<_>>();
        let services = self.services.clone();
        let desktop_id = app.desktop_id.clone();
        let instance = self.instance;
        topbar_services::Runtime::handle().spawn(async move {
            let identity_refs = identities.iter().map(String::as_str).collect::<Vec<_>>();
            let result = services
                .compositor
                .handle()
                .focus_application(&identity_refs)
                .await;
            if matches!(result, Ok(true)) {
                services
                    .launcher_usage
                    .record(&desktop_id, chrono::Utc::now().timestamp());
            }
            glib::idle_add_once(move || match result {
                Ok(true) => dismiss_instance(instance),
                Ok(false) => CURRENT.with_borrow(|current| {
                    if let Some(launcher) = current
                        .as_ref()
                        .filter(|launcher| launcher.instance == instance)
                    {
                        launcher.launch_application(app);
                    }
                }),
                Err(error) => status_instance(instance, &error.to_string()),
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
        // The GDK context carries this layer surface's Wayland activation
        // metadata, while GIO still handles Exec expansion, terminal apps and
        // D-Bus activation.
        let context = gtk4::prelude::WidgetExt::display(&self.window).app_launch_context();
        let services = self.services.clone();
        let desktop_id = app.desktop_id.clone();
        let instance = self.instance;
        info.launch_uris_async(&[], Some(&context), gio::Cancellable::NONE, move |result| {
            match result {
                Ok(()) => {
                    services
                        .launcher_usage
                        .record(&desktop_id, chrono::Utc::now().timestamp());
                    dismiss_instance(instance);
                }
                Err(error) => {
                    status_instance(instance, &format!("Could not launch application: {error}"))
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

fn app_menu(anchor: &gtk4::Widget, app: Application) {
    let menu = gtk4::Popover::new();
    let launch = Button::with_label("Launch New");
    launch.add_css_class(classes::DIALOG_BUTTON);
    launch.connect_clicked(move |_| {
        CURRENT.with_borrow(|current| {
            if let Some(launcher) = current.as_ref() {
                launcher.activate(ResultItem::Application(app.clone()), true);
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
                    launcher.activate(ResultItem::File(file.clone()), false);
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

fn unambiguous_application_id(applications: &[Application], app_id: &str) -> Option<String> {
    let matches = applications
        .iter()
        .filter(|application| {
            application
                .identities()
                .any(|identity| normalize_identity(identity) == normalize_identity(app_id))
        })
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].desktop_id.clone())
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

/// Return whether a service update contains a new visible catalog.
fn catalog_revision_changed(last_seen: &Cell<u64>, current: u64) -> bool {
    last_seen.replace(current) != current
}

/// Keep an existing selection when an asynchronous result update reorders rows.
fn stable_index(items: &[ResultItem], selected_id: Option<&str>) -> usize {
    selected_id
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
        windows: (!query.is_empty() && wants(Filter::Windows) && !windows.connected)
            .then(|| "Window search is unavailable while niri reconnects.".into()),
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
    let current = bounds
        .iter()
        .find_map(|(id, bounds)| (*id == selected_id).then_some(*bounds))?;
    let target = spatial_candidate(
        current,
        bounds
            .iter()
            .filter_map(|(id, bounds)| (*id != selected_id).then_some((*id, *bounds))),
        direction,
    )?;
    Some(target.to_string())
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
    fn normalizes_only_explicit_desktop_identity_variants() {
        assert_eq!(
            normalize_identity("org.Example.App.desktop"),
            "org.example.app"
        );
        assert_eq!(normalize_identity("@org.Example.App"), "org.example.app");
    }

    #[test]
    fn result_updates_keep_the_same_stable_selection() {
        let rows = [ResultItem::Wallpaper, ResultItem::Theme];
        assert_eq!(stable_index(&rows, Some("action:theme")), 1);
        assert_eq!(stable_index(&rows, Some("gone")), 0);
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
    fn spatial_navigation_crosses_list_sections_by_aligned_rows() {
        let bounds = |x, y, width| NavigationBounds {
            x,
            y,
            width,
            height: 36.0,
        };
        let rows = [
            ("window", bounds(0.0, 0.0, 300.0)),
            ("file", bounds(0.0, 52.0, 300.0)),
            ("action", bounds(0.0, 104.0, 300.0)),
        ];
        assert_eq!(
            spatial_candidate(
                rows[1].1,
                rows.iter().copied().filter(|(id, _)| *id != "file"),
                Direction::Up
            ),
            Some("window")
        );
        assert_eq!(
            spatial_candidate(
                rows[1].1,
                rows.iter().copied().filter(|(id, _)| *id != "file"),
                Direction::Down
            ),
            Some("action")
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
