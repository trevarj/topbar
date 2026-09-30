//! The short-lived structured chooser used by the theme and wallpaper scripts.
//!
//! This module deliberately has no `Services` dependency.  A chooser only
//! needs the user's palette and a layer surface, so it remains usable while
//! the panel is stopped or being restarted.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk4::prelude::*;
use gtk4::{
    Align, Application, Button, Entry, Image, Label, Orientation, Picture, Spinner, Window, gdk,
    gio, glib,
};
use serde::Deserialize;
use topbar_core::config::Config;
use topbar_core::ipc::IpcRequest;
use topbar_core::theme::parse_hex_color;
use topbar_services::ipc::InputLock;
use topbar_services::{Runtime, rank_match};

use crate::anim::{Animation, AnimationParams, Easing};
use crate::cli::ChooseLayout;
use crate::ipc_client;
use crate::style::{self, classes, icons};
use crate::surfaces::modal;
use crate::wayland::blur::BlurAttachment;

/// Refuse a pipe large enough to make an accidental binary input painful.
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;
const LIST_WIDTH: i32 = 680;
const WALLPAPER_WIDTH: i32 = 1040;
const THUMBNAIL_WIDTH: i32 = 176;
/// Every picker keeps room for this many choices before it offers a preview.
const MIN_VISIBLE_ROWS: i32 = 4;
const RESULT_ROW_GAP: i32 = 4;
/// The wallpaper image plus a result button's padding and border.
const WALLPAPER_ROW_CHROME: i32 = 18;
/// Theme rows place their swatch alongside three compact text lines. The
/// 96px budget includes the 48px swatch, three 20px lines, text spacing, and
/// the button's vertical padding and border.
const THEME_ROW_HEIGHT: i32 = 96;
const LIST_ROW_HEIGHT: i32 = 44;
/// Title, optional message, search, actions, padding, and box spacing.
/// This intentionally leaves some headroom for fractional-scale text.
const CHOOSER_CHROME_HEIGHT: i32 = 210;
/// The shared wallpaper title, tabs, presets, status, API query, result filter,
/// actions, error line, padding, and spacing (including hidden Local controls).
const WALLPAPER_CHROME_HEIGHT: i32 = 380;
const PREVIEW_MIN_HEIGHT: i32 = 160;
const PREVIEW_MAX_HEIGHT: i32 = 240;
/// A chooser only needs enough decoded images for its visible rows and
/// selected preview.  Keeping this bounded also makes a large wallpaper set
/// cheap to browse.
const THUMBNAIL_CACHE_LIMIT: usize = 64;
const THUMBNAIL_CACHE_BYTES_LIMIT: usize = 48 * 1024 * 1024;
const THUMBNAIL_MAX_PIXELS: i64 = 4_000_000;
const THUMBNAIL_ROW_INTEREST_LIMIT: usize = 24;
const THUMBNAIL_ACTIVE_LIMIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChooserGeometry {
    scroll_height: i32,
    preview_height: Option<i32>,
}

/// Allocate four complete result rows before reserving space for a preview.
/// On shorter outputs the preview disappears; shrinking the result viewport
/// would hide the choice the user is trying to compare.
fn chooser_geometry(
    layout: ChooseLayout,
    thumbnail_height: i32,
    available_height: i32,
) -> ChooserGeometry {
    let row_height = match layout {
        ChooseLayout::Wallpapers => thumbnail_height + WALLPAPER_ROW_CHROME,
        ChooseLayout::Themes => THEME_ROW_HEIGHT,
        ChooseLayout::List => LIST_ROW_HEIGHT,
    };
    let minimum_scroll_height =
        MIN_VISIBLE_ROWS * row_height + (MIN_VISIBLE_ROWS - 1) * RESULT_ROW_GAP;
    let preferred_scroll_height = match layout {
        ChooseLayout::Wallpapers => 480,
        ChooseLayout::Themes | ChooseLayout::List => 420,
    };
    let content_height = available_height
        .saturating_sub(CHOOSER_CHROME_HEIGHT)
        .max(1);
    let spare_for_preview = content_height.saturating_sub(minimum_scroll_height);
    let preview_height = (layout != ChooseLayout::List && spare_for_preview >= PREVIEW_MIN_HEIGHT)
        .then(|| spare_for_preview.min(PREVIEW_MAX_HEIGHT));
    let scroll_ceiling = content_height.saturating_sub(preview_height.unwrap_or(0));
    let scroll_height = preferred_scroll_height
        .min(scroll_ceiling)
        .max(minimum_scroll_height.min(scroll_ceiling));

    ChooserGeometry {
        scroll_height,
        preview_height,
    }
}

/// Request the dialog's content width, while decoding enough detail for a
/// wide wallpaper to be cropped instead of enlarging a shallow thumbnail.
fn wallpaper_preview_dimensions(dialog_width: i32, available_height: i32) -> (i32, i32, i32) {
    let width = dialog_width.saturating_sub(32).max(1);
    let decode_height = width * 9 / 16;
    (
        width,
        decode_height.min(available_height.max(1)),
        decode_height,
    )
}

/// The metadata that makes a decoded image reusable only while it still names
/// the same file content.  The path is retained for a clear error in the
/// chooser, never logged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ThumbnailKey {
    path: PathBuf,
    identity: FileIdentity,
    modified: ModificationStamp,
    #[cfg(unix)]
    changed: ModificationStamp,
    bytes: u64,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModificationStamp {
    seconds: i64,
    nanoseconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ThumbnailRequest {
    path: PathBuf,
    width: i32,
    height: i32,
}

enum ThumbnailRequestState {
    Queued(ThumbnailJobKind),
    Active,
    Ready(ThumbnailKey),
    Failed(String),
    Revalidating(ThumbnailStableState),
}

#[derive(Clone)]
enum ThumbnailStableState {
    Ready(ThumbnailKey),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThumbnailJobKind {
    Probe,
    Decode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThumbnailJob {
    request: ThumbnailRequest,
    generation: u64,
    kind: ThumbnailJobKind,
}

enum CachedThumbnail {
    Texture(gdk::Texture),
    Failed(String),
}

struct DecodedThumbnail {
    key: ThumbnailKey,
    value: CachedThumbnail,
    bytes: usize,
}

/// Plain pixels can cross the worker boundary; the GTK texture is made later
/// on the main thread.
struct DecodedPixels {
    key: ThumbnailKey,
    rgba: Vec<u8>,
    width: i32,
    height: i32,
}

enum DecodeOutcome {
    Pixels(DecodedPixels),
    Failed {
        key: Option<ThumbnailKey>,
        error: String,
    },
    Stale,
}

enum ThumbnailWorkerOutcome {
    Probe(Result<ThumbnailKey, String>),
    Decode(DecodeOutcome),
}

/// GTK-free request scheduling.  The visible interest set is replaced as the
/// query, selection, or viewport changes.  Late workers carry a generation,
/// so they cannot revive a superseded request.
#[derive(Default)]
struct ThumbnailScheduler {
    requests: HashMap<ThumbnailRequest, ThumbnailRequestState>,
    generations: HashMap<ThumbnailRequest, u64>,
    pending: VecDeque<ThumbnailJob>,
    active: usize,
    next_generation: u64,
}

impl ThumbnailScheduler {
    fn synchronize(&mut self, interests: Vec<ThumbnailRequest>) {
        let mut seen = HashSet::new();
        let interests: Vec<_> = interests
            .into_iter()
            .filter(|request| seen.insert(request.clone()))
            .collect();
        self.requests.retain(|request, _| seen.contains(request));
        self.generations.retain(|request, _| seen.contains(request));
        self.pending.retain(|job| {
            self.generations
                .get(&job.request)
                .is_some_and(|generation| *generation == job.generation)
        });
        for request in interests {
            if self.requests.contains_key(&request) {
                self.revalidate(&request);
                continue;
            }
            self.next_generation = self.next_generation.wrapping_add(1);
            let generation = self.next_generation;
            self.requests.insert(
                request.clone(),
                ThumbnailRequestState::Queued(ThumbnailJobKind::Probe),
            );
            self.generations.insert(request.clone(), generation);
            self.pending.push_back(ThumbnailJob {
                request,
                generation,
                kind: ThumbnailJobKind::Probe,
            });
        }
    }

    fn next_job(&mut self) -> Option<ThumbnailJob> {
        if self.active >= THUMBNAIL_ACTIVE_LIMIT {
            return None;
        }
        while let Some(job) = self.pending.pop_front() {
            let Some(state) = self.requests.get_mut(&job.request) else {
                continue;
            };
            if self.generations.get(&job.request) != Some(&job.generation) {
                continue;
            }
            let revalidating = matches!(
                state,
                ThumbnailRequestState::Revalidating(_) if job.kind == ThumbnailJobKind::Probe
            );
            if !revalidating
                && !matches!(state, ThumbnailRequestState::Queued(kind) if *kind == job.kind)
            {
                continue;
            }
            if !revalidating {
                *state = ThumbnailRequestState::Active;
            }
            self.active += 1;
            return Some(job);
        }
        None
    }

    fn complete(&mut self, job: &ThumbnailJob) -> Option<&mut ThumbnailRequestState> {
        self.active = self.active.saturating_sub(1);
        if self.generations.get(&job.request) == Some(&job.generation) {
            self.requests.get_mut(&job.request)
        } else {
            None
        }
    }

    fn queue(&mut self, job: &ThumbnailJob, kind: ThumbnailJobKind) {
        if self.generations.get(&job.request) != Some(&job.generation) {
            return;
        }
        let Some(state) = self.requests.get_mut(&job.request) else {
            return;
        };
        *state = ThumbnailRequestState::Queued(kind);
        self.pending.push_back(ThumbnailJob {
            request: job.request.clone(),
            generation: job.generation,
            kind,
        });
    }

    /// Rechecking a relevant completed source retains its prior image or
    /// failure while the metadata probe runs.  A second synchronization while
    /// it is pending is a no-op, keeping the queue bounded during scrolling.
    fn revalidate(&mut self, request: &ThumbnailRequest) {
        let Some(state) = self.requests.get_mut(request) else {
            return;
        };
        let stable = match state {
            ThumbnailRequestState::Ready(key) => ThumbnailStableState::Ready(key.clone()),
            ThumbnailRequestState::Failed(error) => ThumbnailStableState::Failed(error.clone()),
            ThumbnailRequestState::Queued(_)
            | ThumbnailRequestState::Active
            | ThumbnailRequestState::Revalidating(_) => return,
        };
        let Some(generation) = self.generations.get(request).copied() else {
            return;
        };
        *state = ThumbnailRequestState::Revalidating(stable);
        self.pending.push_back(ThumbnailJob {
            request: request.clone(),
            generation,
            kind: ThumbnailJobKind::Probe,
        });
    }

    fn clear(&mut self) {
        self.requests.clear();
        self.generations.clear();
        self.pending.clear();
    }

    fn state(&self, request: &ThumbnailRequest) -> Option<&ThumbnailRequestState> {
        self.requests.get(request)
    }
}

/// Metadata-keyed texture and failure cache.  It has no request history and
/// therefore remains bounded even for very large wallpaper directories.
#[derive(Default)]
struct ThumbnailCache {
    entries: VecDeque<DecodedThumbnail>,
    bytes: usize,
    pinned: Option<ThumbnailKey>,
}

impl ThumbnailCache {
    fn get(&self, key: &ThumbnailKey) -> Option<&CachedThumbnail> {
        self.entries
            .iter()
            .find(|entry| entry.key == *key)
            .map(|entry| &entry.value)
    }

    fn insert(&mut self, entry: DecodedThumbnail) {
        self.entries.retain(|cached| cached.key != entry.key);
        self.bytes = self.entries.iter().map(|cached| cached.bytes).sum();
        self.bytes = self.bytes.saturating_add(entry.bytes);
        self.entries.push_front(entry);
        while self.entries.len() > THUMBNAIL_CACHE_LIMIT || self.bytes > THUMBNAIL_CACHE_BYTES_LIMIT
        {
            let Some(index) = self
                .entries
                .iter()
                .rposition(|cached| self.pinned.as_ref() != Some(&cached.key))
            else {
                break;
            };
            let Some(evicted) = self.entries.remove(index) else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted.bytes);
        }
    }

    fn set_pinned(&mut self, key: Option<ThumbnailKey>) {
        self.pinned = key;
    }
}

enum ThumbnailView {
    Loading,
    Ready(gdk::Texture),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewStatus {
    Loading,
    Ready,
    Failed,
}

fn preview_status(view: &ThumbnailView) -> PreviewStatus {
    match view {
        ThumbnailView::Loading => PreviewStatus::Loading,
        ThumbnailView::Ready(_) => PreviewStatus::Ready,
        ThumbnailView::Failed(_) => PreviewStatus::Failed,
    }
}

fn preview_allows_apply(status: PreviewStatus) -> bool {
    status == PreviewStatus::Ready
}

/// An item supplied by a script on standard input.
#[derive(Debug, Clone, Deserialize)]
struct Candidate {
    id: String,
    label: String,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default, alias = "preview_path")]
    preview: Option<PathBuf>,
    #[serde(default)]
    palette: Option<Palette>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WallpaperPreset {
    id: String,
    label: String,
}

#[derive(Deserialize)]
struct WallpaperInput {
    pool: Vec<Candidate>,
    presets: Vec<WallpaperPreset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WallpaperPending {
    Search {
        preset: String,
        query: String,
        generation: u64,
    },
    Save {
        preset: String,
        generation: u64,
    },
}

#[derive(Deserialize)]
struct SavedWallpaper {
    path: PathBuf,
}

fn provider_command(executable: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new(executable)
        .args(args)
        .output()
        .map_err(|error| format!("Could not run Wallhaven provider: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Wallhaven {} failed: {}",
            args[0],
            message.trim().chars().take(300).collect::<String>()
        ));
    }
    if output.stdout.len() > MAX_INPUT_BYTES as usize {
        return Err("Wallhaven provider response is too large".to_string());
    }
    Ok(output.stdout)
}

#[derive(Default)]
struct WallpaperSearch {
    query: String,
    rows: Vec<Candidate>,
    error: Option<String>,
    finished: bool,
}

struct WallpaperState {
    executable: PathBuf,
    presets: Vec<WallpaperPreset>,
    pool: Vec<Candidate>,
    // ponytail: retain only the latest query and results for each fixed preset.
    cached: HashMap<String, WallpaperSearch>,
    active: Option<String>,
    last_preset: Option<usize>,
    pending: Option<WallpaperPending>,
    generation: u64,
}

impl WallpaperState {
    fn switch(&mut self, preset: Option<&str>, rows: &mut Vec<Candidate>) -> bool {
        if self.active.as_deref() == preset
            || matches!(self.pending, Some(WallpaperPending::Save { .. }))
        {
            return false;
        }
        if let Some(old) = self.active.take() {
            self.cached.entry(old).or_default().rows = std::mem::take(rows);
        } else {
            self.pool = std::mem::take(rows);
        }
        if let Some(preset) = preset {
            self.last_preset = self.presets.iter().position(|entry| entry.id == preset);
        }
        self.active = preset.map(str::to_owned);
        *rows = if let Some(preset) = preset {
            std::mem::take(&mut self.cached.entry(preset.to_owned()).or_default().rows)
        } else {
            std::mem::take(&mut self.pool)
        };
        if !matches!(
            (&self.pending, preset),
            (Some(WallpaperPending::Search { preset: searching, .. }), Some(next))
                if searching == next
        ) && preset.is_some()
        {
            self.pending = None;
        }
        self.generation += 1;
        true
    }
    fn blocks_selection(&self) -> bool {
        self.pending.is_some()
            && (self.active.is_some()
                || matches!(self.pending, Some(WallpaperPending::Save { .. })))
    }

    fn display_or_cache(&mut self, preset: &str, rows: Vec<Candidate>) -> Option<Vec<Candidate>> {
        let search = self.cached.entry(preset.to_owned()).or_default();
        search.finished = true;
        search.error = None;
        if self.active.as_deref() == Some(preset) {
            Some(rows)
        } else {
            search.rows = rows;
            None
        }
    }

    fn search_failed(&mut self, preset: &str, error: String) -> Option<String> {
        let search = self.cached.get_mut(preset).expect("submitted search");
        search.finished = true;
        search.error = Some(error.clone());
        (self.active.as_deref() == Some(preset)).then_some(error)
    }

    fn preferred_preset(&self) -> Option<&str> {
        self.active.as_deref().or_else(|| {
            self.last_preset
                .and_then(|index| self.presets.get(index))
                .or_else(|| self.presets.first())
                .map(|entry| entry.id.as_str())
        })
    }

    fn current_search(&self) -> Option<&WallpaperSearch> {
        self.active
            .as_ref()
            .and_then(|preset| self.cached.get(preset))
    }

    fn needs_search(&self) -> bool {
        self.pending.is_none() && self.current_search().is_some_and(|search| !search.finished)
    }

    fn retry_query(&self) -> Option<&str> {
        self.current_search()
            .filter(|search| search.error.is_some())
            .map(|search| search.query.as_str())
    }

    fn searching(&mut self, query: String) -> Option<(String, u64)> {
        let preset = self.active.as_ref()?;
        if matches!(self.pending, Some(WallpaperPending::Save { .. }))
            || matches!(&self.pending, Some(WallpaperPending::Search {
                preset: pending_preset, query: pending_query, ..
            }) if pending_preset == preset && pending_query == &query)
        {
            return None;
        }
        let search = self.cached.entry(preset.clone()).or_default();
        search.query = query.clone();
        search.rows.clear();
        search.error = None;
        search.finished = false;
        self.generation += 1;
        let generation = self.generation;
        self.pending = Some(WallpaperPending::Search {
            preset: preset.clone(),
            query,
            generation,
        });
        Some((preset.clone(), generation))
    }

    fn complete(&mut self, request: &WallpaperPending) -> bool {
        if self.pending.as_ref() != Some(request) {
            return false;
        }
        self.pending = None;
        true
    }
}

/// Theme palette metadata is intentionally permissive: callers may include
/// all of their generated roles while the chooser uses the roles it can draw.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Palette {
    Roles(PaletteRoles),
    Colors(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PaletteRoles {
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    surface: Option<String>,
    #[serde(default)]
    foreground: Option<String>,
    #[serde(default)]
    accent: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

impl Palette {
    fn colors(&self) -> Vec<String> {
        let colors = match self {
            Self::Roles(roles) => [
                roles.background.as_deref(),
                roles.surface.as_deref(),
                roles.foreground.as_deref(),
                roles.accent.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::to_owned)
            .collect(),
            Self::Colors(colors) => colors.clone(),
        };
        colors
            .into_iter()
            .filter(|color| parse_hex_color(color).is_some())
            .collect()
    }

    fn mode(&self) -> Option<&str> {
        match self {
            Self::Roles(roles) => roles.mode.as_deref(),
            Self::Colors(_) => None,
        }
    }
}

/// The final result of the interaction.
#[derive(Debug, Clone)]
enum Outcome {
    Selected(String),
    Saved(PathBuf),
    Cancelled,
    Failed(String),
}

/// Run `topbar choose` without starting the panel services.
pub fn run(
    layout: ChooseLayout,
    title: String,
    message: Option<String>,
    selected: Option<String>,
    wallpaper_provider: Option<PathBuf>,
    config_path: Option<&Path>,
) -> ExitCode {
    if wallpaper_provider.is_some() && layout != ChooseLayout::Wallpapers {
        return fail("--wallpaper-provider requires --layout wallpapers");
    }
    let (candidates, presets) = match read_candidates(wallpaper_provider.is_some()) {
        Ok(input) => input,
        Err(error) => return fail(error),
    };

    let config = match Config::find_and_load(config_path) {
        Ok(load) => load.config,
        Err(error) => return fail(error.to_string()),
    };
    // Standalone processes do not pass through `app::run`, so they must set
    // the shared motion preference before any animation is constructed.
    crate::anim::set_animations_enabled(config.theme.animations);

    // The request is deliberately best effort.  The panel may be stopped,
    // and the input lock below is the authority that prevents overlap.
    let _ = ipc_client::request(&IpcRequest::DismissTransient);
    let input_lock = match Runtime::handle().block_on(InputLock::acquire()) {
        Ok(lock) => lock,
        Err(error) => return fail(format!("could not acquire input: {error}")),
    };

    // This is before GTK starts, matching the daemon's layer-shell path.
    if std::env::var_os("GDK_BACKEND").is_none() {
        // SAFETY: no GTK objects or application threads have been created.
        unsafe { std::env::set_var("GDK_BACKEND", "wayland") };
    }
    let focused_output = modal::focused_output_connector();

    let outcome: Rc<RefCell<Option<Outcome>>> = Rc::new(RefCell::new(None));
    let live: Rc<RefCell<Option<Rc<Chooser>>>> = Rc::new(RefCell::new(None));
    let app = Application::builder()
        .application_id("io.github.trevarj.topbar.chooser")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let provider_mode = wallpaper_provider.is_some();
    app.connect_activate({
        let outcome = outcome.clone();
        let live = live.clone();
        move |app| {
            if live.borrow().is_some() {
                return;
            }
            let Some(display) = gdk::Display::default() else {
                *outcome.borrow_mut() = Some(Outcome::Failed(
                    "no display; is a Wayland compositor running?".to_string(),
                ));
                app.quit();
                return;
            };
            if !gtk4_layer_shell::is_supported() {
                *outcome.borrow_mut() = Some(Outcome::Failed(
                    "the compositor does not support wlr-layer-shell".to_string(),
                ));
                app.quit();
                return;
            }
            if let Some(settings) = gtk4::Settings::default() {
                settings.set_gtk_icon_theme_name(Some(&config.theme.icons.theme));
            }
            style::apply(&config);
            // Unlike the panel, this process does not pass through
            // `app::start`, so initialise the compositor protocol before the
            // backdrop attaches its full-screen blur region.
            crate::wayland::blur::init(&display, config.theme.blur);
            let chooser = Chooser::new(
                app.clone(),
                &display,
                focused_output.as_deref(),
                layout,
                candidates.clone(),
                wallpaper_provider.clone().map(|executable| WallpaperState {
                    executable,
                    presets: presets.clone(),
                    pool: Vec::new(),
                    cached: HashMap::new(),
                    active: None,
                    last_preset: None,
                    pending: None,
                    generation: 0,
                }),
                title.clone(),
                message.clone(),
                selected.clone(),
                outcome.clone(),
            );
            // Register the dialog with GTK before activation returns so the
            // standalone application stays alive while the user chooses.
            app.add_window(&chooser.window);
            chooser.open();
            *live.borrow_mut() = Some(chooser);
        }
    });

    // Hold the advisory lock through the event loop.  Dropping it only after
    // both surfaces close prevents another standalone dialog appearing early.
    let status = app.run_with_args::<&str>(&[]);
    drop(input_lock);
    match outcome.borrow_mut().take() {
        Some(Outcome::Selected(id)) if status == glib::ExitCode::SUCCESS => {
            if provider_mode {
                println!("{}", serde_json::json!({"kind":"pool","id":id}));
            } else {
                println!("{id}");
            }
            ExitCode::SUCCESS
        }
        Some(Outcome::Saved(path)) if status == glib::ExitCode::SUCCESS => {
            println!("{}", serde_json::json!({"kind":"saved","path":path}));
            ExitCode::SUCCESS
        }
        Some(Outcome::Selected(_) | Outcome::Saved(_)) => fail("chooser exited unsuccessfully"),
        Some(Outcome::Cancelled) => ExitCode::from(1),
        Some(Outcome::Failed(error)) => fail(error),
        None if status == glib::ExitCode::SUCCESS => fail("chooser closed without a result"),
        None => fail("chooser could not start"),
    }
}

fn fail(message: impl AsRef<str>) -> ExitCode {
    eprintln!("Error: {}", message.as_ref());
    ExitCode::from(2)
}

/// A short press fade gives standalone dialog buttons the same 120ms feedback
/// as panel controls. `Animation` resolves it synchronously when either
/// topbar or GTK accessibility settings disable motion.
fn install_interaction_feedback(button: &Button) {
    let animation = Animation::new(button);
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(0);
    gesture.connect_pressed({
        let button = button.clone();
        let animation = animation.clone();
        move |_, _, _, _| {
            animation.start(
                AnimationParams::new(120).with_easing(Easing::EaseOutCubic),
                Box::new({
                    let button = button.clone();
                    move |progress| button.set_opacity(1.0 - 0.12 * progress)
                }),
                None,
            );
        }
    });
    gesture.connect_released({
        let button = button.clone();
        move |_, _, _, _| {
            let animation = Animation::new(&button);
            animation.start(
                AnimationParams::new(120).with_easing(Easing::EaseOutCubic),
                Box::new({
                    let button = button.clone();
                    move |progress| button.set_opacity(0.88 + 0.12 * progress)
                }),
                None,
            );
        }
    });
    button.add_controller(gesture);
}

fn read_candidates(provider: bool) -> Result<(Vec<Candidate>, Vec<WallpaperPreset>), String> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read standard input: {error}"))?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(format!("chooser input exceeds {MAX_INPUT_BYTES} bytes"));
    }
    let (candidates, presets) = if provider {
        let input: WallpaperInput = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid wallpaper chooser input: {error}"))?;
        if input.presets.is_empty()
            || input.presets.iter().any(|preset| {
                preset.id.is_empty() || !preset.id.bytes().all(|c| c.is_ascii_lowercase())
            })
            || input
                .presets
                .iter()
                .map(|preset| &preset.id)
                .collect::<HashSet<_>>()
                .len()
                != input.presets.len()
        {
            return Err("invalid wallpaper presets".to_string());
        }
        (input.pool, input.presets)
    } else {
        (
            serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid chooser input: {error}"))?,
            Vec::new(),
        )
    };
    validate_candidates(&candidates)?;
    Ok((candidates, presets))
}

fn validate_candidates(candidates: &[Candidate]) -> Result<(), String> {
    let mut ids = HashSet::with_capacity(candidates.len());
    for candidate in candidates {
        if candidate.id.is_empty() {
            return Err("candidate ID must not be empty".to_string());
        }
        if candidate.id.contains(['\n', '\r', '\0']) {
            return Err(format!(
                "candidate ID contains a line break or NUL: {:?}",
                candidate.id
            ));
        }
        if !ids.insert(&candidate.id) {
            return Err(format!("candidate ID is duplicated: {:?}", candidate.id));
        }
    }
    Ok(())
}

/// Filter with the same Unicode-aware matcher as launcher search.  A chooser
/// deliberately keeps the caller's input order: scripts use that order to
/// communicate a useful default and duplicate labels still need to stay
/// distinct by their stable IDs.
fn matching_indices(candidates: &[Candidate], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..candidates.len()).collect();
    }
    candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            [
                Some(candidate.label.as_str()),
                Some(candidate.id.as_str()),
                candidate.subtitle.as_deref(),
            ]
            .into_iter()
            .flatten()
            .chain(candidate.tags.iter().map(String::as_str))
            .any(|field| rank_match(query, field).is_some())
            .then_some(index)
        })
        .collect()
}

fn random_wallpaper_index(
    candidates: &[Candidate],
    visible: &[usize],
    selected: Option<&str>,
) -> Option<usize> {
    let eligible = visible
        .iter()
        .filter(|&&index| candidates[index].preview.is_some())
        .count();
    if eligible == 0 {
        return None;
    }
    // ponytail: scan twice rather than allocate another list of eligible rows.
    let skip_selected = eligible > 1
        && selected.is_some_and(|id| {
            visible
                .iter()
                .any(|&index| candidates[index].preview.is_some() && candidates[index].id == id)
        });
    let slot = glib::random_int_range(0, (eligible - usize::from(skip_selected)) as i32) as usize;
    visible
        .iter()
        .copied()
        .filter(|&index| {
            candidates[index].preview.is_some()
                && (!skip_selected || Some(candidates[index].id.as_str()) != selected)
        })
        .nth(slot)
}

fn retained_selection(
    candidates: &[Candidate],
    visible: &[usize],
    selected: Option<&str>,
) -> Option<String> {
    selected
        .filter(|id| {
            visible
                .iter()
                .any(|&index| candidates[index].id.as_str() == *id)
        })
        .map(str::to_owned)
        .or_else(|| visible.first().map(|&index| candidates[index].id.clone()))
}

/// Return the adjustment value that makes a row visible without moving an
/// already visible selection.
fn scroll_value_for_bounds(current: f64, page: f64, start: f64, end: f64) -> Option<f64> {
    if start < current {
        Some(start)
    } else if end > current + page {
        Some(end - page)
    } else {
        None
    }
}

fn prune_dead_thumbnail_refs<T: glib::prelude::ObjectType>(
    entries: &mut HashMap<ThumbnailRequest, Vec<glib::WeakRef<T>>>,
    request: &ThumbnailRequest,
) {
    if let Some(refs) = entries.get_mut(request) {
        refs.retain(|widget| widget.upgrade().is_some());
        if refs.is_empty() {
            entries.remove(request);
        }
    }
}

/// Live chooser widgets and their small local state.
struct Chooser {
    app: Application,
    window: Window,
    backdrop: Window,
    // Keeps the compositor effect alive for the full-screen backdrop rather
    // than the small foreground dialog.
    _backdrop_blur: BlurAttachment,
    root: gtk4::Box,
    container_motion: Animation,
    layout: ChooseLayout,
    candidates: RefCell<Vec<Candidate>>,
    wallpaper: RefCell<Option<WallpaperState>>,
    pool_tab: Button,
    wallhaven_tab: Button,
    preset_buttons: gtk4::Box,
    status_row: gtk4::Box,
    spinner: Spinner,
    status: Label,
    retry: Button,
    cancel_button: Button,
    random: Button,
    current: Option<String>,
    selected: RefCell<Option<String>>,
    search: Entry,
    wallhaven_query_row: gtk4::Box,
    wallhaven_query: Entry,
    wallhaven_search: Button,
    filter: Entry,
    scroll: gtk4::ScrolledWindow,
    scroll_motion: Animation,
    scroll_target: Rc<Cell<Option<f64>>>,
    scroll_retry_queued: Cell<bool>,
    results: gtk4::Box,
    preview: gtk4::Box,
    preview_picture: Option<(Picture, Image, Label)>,
    preview_request: RefCell<Option<ThumbnailRequest>>,
    row_widgets: RefCell<Vec<(usize, Button)>>,
    thumbnail_width: i32,
    thumbnail_height: i32,
    preview_width: i32,
    preview_decode_height: i32,
    thumbnail_scheduler: RefCell<ThumbnailScheduler>,
    thumbnail_cache: RefCell<ThumbnailCache>,
    thumbnail_images: RefCell<HashMap<ThumbnailRequest, Vec<glib::WeakRef<Image>>>>,
    thumbnail_errors: RefCell<HashMap<ThumbnailRequest, Vec<glib::WeakRef<Label>>>>,
    apply: Button,
    outcome: Rc<RefCell<Option<Outcome>>>,
}

impl Chooser {
    #[allow(clippy::too_many_arguments)]
    fn new(
        app: Application,
        display: &gdk::Display,
        focused_output: Option<&str>,
        layout: ChooseLayout,
        candidates: Vec<Candidate>,
        wallpaper: Option<WallpaperState>,
        title_text: String,
        message_text: Option<String>,
        current: Option<String>,
        outcome: Rc<RefCell<Option<Outcome>>>,
    ) -> Rc<Self> {
        let monitor = modal::standalone_monitor(display, focused_output);
        // The backdrop must map first so it remains below the input surface.
        let backdrop = modal::backdrop(
            monitor.as_ref(),
            "topbar-chooser-backdrop",
            classes::CHOOSER_BACKDROP,
        );
        let backdrop_blur = modal::attach_backdrop_blur(&backdrop);
        let window = modal::centered_window(monitor.as_ref(), "topbar-chooser");
        window.add_css_class(classes::CHOOSER_WINDOW);

        let root = gtk4::Box::new(Orientation::Vertical, 12);
        root.add_css_class(classes::CHOOSER_DIALOG);
        let container_motion = Animation::new(&root);
        let desired_width = if layout == ChooseLayout::Wallpapers {
            WALLPAPER_WIDTH
        } else {
            LIST_WIDTH
        };
        // A width request is GTK's minimum rather than a maximum.  Cap it to
        // the target monitor here so a picker still fits on a small output.
        let monitor_width = monitor
            .as_ref()
            .map(|monitor| monitor.geometry().width())
            .unwrap_or(desired_width);
        let monitor_height = monitor
            .as_ref()
            .map(|monitor| monitor.geometry().height())
            .unwrap_or(800);
        // A dialog owns the rounded CSS shadow. Keep it inside a transparent
        // window gutter so GTK never clips it into a rectangular halo.
        let horizontal_margin = (monitor_width / 12).clamp(36, 48);
        let vertical_margin = (monitor_height / 12).clamp(36, 48);
        let available_width = monitor_width.saturating_sub(horizontal_margin * 2);
        let available_height = monitor_height.saturating_sub(vertical_margin * 2);
        let chooser_width = desired_width.min(available_width.max(1));
        let thumbnail_width = (chooser_width / 3).clamp(96, THUMBNAIL_WIDTH);
        let thumbnail_height = thumbnail_width * 9 / 16;
        let geometry = chooser_geometry(
            layout,
            thumbnail_height,
            available_height.saturating_sub(if wallpaper.is_some() { 160 } else { 0 }),
        );
        // Allow for the Wallhaven controls even when Local hides them. Keep
        // the outer size fixed, but do not stretch a short chooser to fill the
        // entire output: spare space belongs to the desktop, not empty results.
        let scroll_height = if wallpaper.is_some() {
            geometry.scroll_height.min(
                available_height
                    .saturating_sub(448 + geometry.preview_height.unwrap_or(0))
                    .max(200),
            )
        } else {
            geometry.scroll_height
        };
        root.set_size_request(
            chooser_width,
            if wallpaper.is_some() {
                (scroll_height + geometry.preview_height.unwrap_or(0) + WALLPAPER_CHROME_HEIGHT)
                    .min(available_height.saturating_sub(24))
            } else {
                -1
            },
        );
        root.set_margin_start(horizontal_margin);
        root.set_margin_end(horizontal_margin);
        root.set_margin_top(vertical_margin);
        root.set_margin_bottom(vertical_margin);

        let header = gtk4::Box::new(Orientation::Horizontal, 8);
        let title = Label::new(Some(&title_text));
        title.add_css_class(classes::CHOOSER_TITLE);
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_hexpand(true);
        header.append(&title);
        let random = Button::from_icon_name(icons::WALLPAPER_RANDOM);
        random.add_css_class(classes::DIALOG_BUTTON);
        random.add_css_class(classes::CHOOSER_RANDOM);
        random.set_tooltip_text(Some("Select a random wallpaper"));
        random.update_property(&[gtk4::accessible::Property::Label("Random wallpaper")]);
        random.set_focus_on_click(false);
        random.set_valign(Align::Start);
        if layout == ChooseLayout::Wallpapers {
            header.append(&random);
        }
        install_interaction_feedback(&random);
        root.append(&header);
        if let Some(message_text) = message_text.filter(|text| !text.is_empty()) {
            let message = Label::new(Some(&message_text));
            message.add_css_class(classes::CHOOSER_MESSAGE);
            message.set_xalign(0.0);
            message.set_wrap(true);
            root.append(&message);
        }

        let tabs = gtk4::Box::new(Orientation::Horizontal, 8);
        let pool_tab = Button::with_label("Local");
        let wallhaven_tab = Button::with_label("Wallhaven");
        pool_tab.add_css_class(classes::DIALOG_BUTTON);
        wallhaven_tab.add_css_class(classes::DIALOG_BUTTON);
        tabs.append(&pool_tab);
        tabs.append(&wallhaven_tab);
        tabs.set_visible(wallpaper.is_some());
        root.append(&tabs);
        let preset_buttons = gtk4::Box::new(Orientation::Horizontal, 8);
        preset_buttons.set_visible(false);
        if let Some(wallpaper) = &wallpaper {
            for preset in &wallpaper.presets {
                let button = Button::with_label(&preset.label);
                button.add_css_class(classes::DIALOG_BUTTON);
                button.set_widget_name(&preset.id);
                preset_buttons.append(&button);
            }
        }
        root.append(&preset_buttons);

        let status_row = gtk4::Box::new(Orientation::Horizontal, 8);
        status_row.set_height_request(32);
        status_row.set_visible(false);
        let spinner = Spinner::new();
        status_row.append(&spinner);
        let status = Label::new(None);
        status.set_xalign(0.0);
        status.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        status.set_max_width_chars(32);
        status.set_hexpand(true);
        status.add_css_class(classes::CHOOSER_SUBTITLE);
        status_row.append(&status);
        let retry = Button::with_label("Retry");
        retry.set_visible(false);
        retry.add_css_class(classes::DIALOG_BUTTON);
        status_row.append(&retry);
        root.append(&status_row);

        let search = Entry::new();
        search.add_css_class(classes::CHOOSER_SEARCH);
        search.set_placeholder_text(Some("Search"));
        search.set_primary_icon_name(Some("system-search-symbolic"));
        root.append(&search);

        let wallhaven_query_row = gtk4::Box::new(Orientation::Horizontal, 8);
        wallhaven_query_row.set_visible(false);
        let wallhaven_query = Entry::new();
        wallhaven_query.add_css_class(classes::CHOOSER_SEARCH);
        wallhaven_query.set_placeholder_text(Some("Search Wallhaven (empty uses preset)"));
        wallhaven_query.update_property(&[gtk4::accessible::Property::Label("Search Wallhaven")]);
        wallhaven_query.set_primary_icon_name(Some("system-search-symbolic"));
        wallhaven_query.set_hexpand(true);
        let wallhaven_search = Button::with_label("Search");
        wallhaven_search.add_css_class(classes::DIALOG_BUTTON);
        install_interaction_feedback(&wallhaven_search);
        wallhaven_query_row.append(&wallhaven_query);
        wallhaven_query_row.append(&wallhaven_search);
        root.append(&wallhaven_query_row);
        let filter = Entry::new();
        filter.add_css_class(classes::CHOOSER_SEARCH);
        filter.set_placeholder_text(Some("Filter results"));
        filter.update_property(&[gtk4::accessible::Property::Label("Filter results")]);
        filter.set_visible(false);
        root.append(&filter);

        let results = gtk4::Box::new(Orientation::Vertical, 4);
        results.add_css_class(classes::CHOOSER_RESULTS);
        let scroll = gtk4::ScrolledWindow::new();
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        // The non-overlay bar remains visible whenever results exceed
        // the viewport rather than fading over the last label.
        scroll.set_overlay_scrolling(false);
        scroll.set_min_content_height(scroll_height);
        scroll.set_max_content_height(scroll_height);
        scroll.set_child(Some(&results));
        // Theme rows use GTK focus scrolling; other layouts retain selection scrolling.
        scroll
            .child()
            .and_downcast::<gtk4::Viewport>()
            .expect("GtkBox results are wrapped in a viewport")
            .set_scroll_to_focus(layout == ChooseLayout::Themes);
        scroll.set_vexpand(true);
        root.append(&scroll);
        let scroll_motion = Animation::new(&scroll);

        let preview = gtk4::Box::new(Orientation::Vertical, 6);
        preview.add_css_class(classes::CHOOSER_PREVIEW);
        preview.set_visible(geometry.preview_height.is_some());
        root.append(&preview);
        let (preview_width, preview_height, preview_decode_height) =
            wallpaper_preview_dimensions(chooser_width, geometry.preview_height.unwrap_or(1));
        let preview_picture =
            if layout == ChooseLayout::Wallpapers && geometry.preview_height.is_some() {
                let picture = Picture::new();
                picture.add_css_class(classes::CHOOSER_PREVIEW_IMAGE);
                picture.set_content_fit(gtk4::ContentFit::Cover);
                picture.set_size_request(preview_width, preview_height);
                picture.set_halign(Align::Fill);
                picture.set_valign(Align::Fill);
                picture.set_hexpand(true);
                let placeholder = Image::from_icon_name("image-loading-symbolic");
                placeholder.set_pixel_size(64);
                placeholder.set_halign(Align::Center);
                placeholder.set_valign(Align::Center);
                let frame = gtk4::Overlay::new();
                frame.set_size_request(preview_width, preview_height);
                frame.set_halign(Align::Fill);
                frame.set_hexpand(true);
                let canvas = gtk4::Box::new(Orientation::Vertical, 0);
                canvas.set_size_request(preview_width, preview_height);
                frame.set_child(Some(&canvas));
                frame.add_overlay(&picture);
                frame.add_overlay(&placeholder);
                preview.append(&frame);
                let error = Label::new(None);
                error.add_css_class(classes::CHOOSER_SUBTITLE);
                error.set_max_width_chars(60);
                error.set_xalign(0.0);
                error.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                error.set_height_request(24);
                preview.append(&error);
                Some((picture, placeholder, error))
            } else {
                None
            };

        let actions = gtk4::Box::new(Orientation::Horizontal, 8);
        actions.add_css_class(classes::CHOOSER_ACTIONS);
        actions.set_halign(Align::End);
        let cancel = Button::with_label("Cancel");
        cancel.add_css_class(classes::DIALOG_BUTTON);
        let apply = Button::with_label("Apply");
        apply.add_css_class(classes::DIALOG_BUTTON);
        apply.add_css_class(classes::DIALOG_BUTTON_PRIMARY);
        install_interaction_feedback(&cancel);
        install_interaction_feedback(&apply);
        actions.append(&cancel);
        actions.append(&apply);
        root.append(&actions);
        window.set_child(Some(&root));

        let initial = current
            .as_ref()
            .filter(|id| {
                candidates
                    .iter()
                    .any(|candidate| candidate.id.as_str() == id.as_str())
            })
            .cloned()
            .or_else(|| candidates.first().map(|candidate| candidate.id.clone()));
        apply.set_sensitive(initial.is_some());
        let chooser = Rc::new(Self {
            app,
            window,
            backdrop,
            _backdrop_blur: backdrop_blur,
            root,
            container_motion,
            layout,
            candidates: RefCell::new(candidates),
            wallpaper: RefCell::new(wallpaper),
            pool_tab,
            wallhaven_tab,
            preset_buttons,
            status_row,
            spinner,
            status,
            retry,
            cancel_button: cancel.clone(),
            random,
            current,
            selected: RefCell::new(initial),
            search,
            wallhaven_query_row,
            wallhaven_query,
            wallhaven_search,
            filter,
            scroll,
            scroll_motion,
            scroll_target: Rc::new(Cell::new(None)),
            scroll_retry_queued: Cell::new(false),
            results,
            preview,
            preview_picture,
            preview_request: RefCell::new(None),
            row_widgets: RefCell::new(Vec::new()),
            thumbnail_width,
            thumbnail_height,
            preview_width,
            preview_decode_height,
            thumbnail_scheduler: RefCell::new(ThumbnailScheduler::default()),
            thumbnail_cache: RefCell::new(ThumbnailCache::default()),
            thumbnail_images: RefCell::new(HashMap::new()),
            thumbnail_errors: RefCell::new(HashMap::new()),
            apply,
            outcome,
        });
        chooser.wire(&cancel);
        chooser.wire_wallpaper();
        chooser.cancel_if_monitor_disappears(display, monitor.as_ref());
        chooser.render();
        chooser
    }

    fn activate_wallhaven(self: &Rc<Self>) {
        let preset = self
            .wallpaper
            .borrow()
            .as_ref()
            .and_then(WallpaperState::preferred_preset)
            .map(str::to_owned);
        if let Some(preset) = preset {
            self.switch_wallpaper(Some(&preset));
        }
    }

    fn wire_wallpaper(self: &Rc<Self>) {
        if self.wallpaper.borrow().is_none() {
            return;
        }
        self.pool_tab.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.switch_wallpaper(None);
                }
            }
        });
        self.wallhaven_tab.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.activate_wallhaven();
                }
            }
        });
        let mut child = self.preset_buttons.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            let button = widget.downcast::<Button>().expect("preset button");
            let preset = button.widget_name().to_string();
            button.connect_clicked({
                let weak = Rc::downgrade(self);
                move |_| {
                    if let Some(chooser) = weak.upgrade() {
                        chooser.switch_wallpaper(Some(&preset));
                    }
                }
            });
        }
        self.retry.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    let query = chooser
                        .wallpaper
                        .borrow()
                        .as_ref()
                        .and_then(WallpaperState::retry_query)
                        .map(str::to_owned);
                    if let Some(query) = query {
                        chooser.start_wallpaper_search(query);
                    } else {
                        chooser.accept();
                    }
                }
            }
        });
        self.wallhaven_query.connect_activate({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.submit_wallhaven_query();
                }
            }
        });
        self.wallhaven_search.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.submit_wallhaven_query();
                }
            }
        });
        self.update_wallpaper_controls();
    }

    fn update_wallpaper_controls(self: &Rc<Self>) {
        let wallpaper_state = self.wallpaper.borrow();
        let Some(state) = wallpaper_state.as_ref() else {
            return;
        };
        let active = state.active.as_deref();
        let saving = matches!(state.pending, Some(WallpaperPending::Save { .. }));
        self.pool_tab.set_sensitive(!saving);
        self.wallhaven_tab.set_sensitive(!saving);
        self.cancel_button.set_sensitive(!saving);
        self.search.set_sensitive(!saving);
        self.search.set_visible(active.is_none());
        self.wallhaven_query_row.set_visible(active.is_some());
        self.filter.set_visible(active.is_some());
        self.wallhaven_query.set_sensitive(!saving);
        self.wallhaven_search.set_sensitive(!saving);
        self.filter.set_sensitive(!saving);
        self.retry
            .set_sensitive(!saving && (state.retry_query().is_some() || self.selection_is_ready()));
        self.update_random_sensitivity();
        self.pool_tab
            .remove_css_class(classes::DIALOG_BUTTON_PRIMARY);
        self.wallhaven_tab
            .remove_css_class(classes::DIALOG_BUTTON_PRIMARY);
        if active.is_some() {
            self.wallhaven_tab
                .add_css_class(classes::DIALOG_BUTTON_PRIMARY);
        } else {
            self.pool_tab.add_css_class(classes::DIALOG_BUTTON_PRIMARY);
        }
        self.preset_buttons.set_visible(active.is_some());
        let mut child = self.preset_buttons.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            let button = widget.downcast::<Button>().expect("preset button");
            button.set_sensitive(active.is_some() && !saving);
            button.remove_css_class(classes::DIALOG_BUTTON_PRIMARY);
            if active == Some(button.widget_name().as_str()) {
                button.add_css_class(classes::DIALOG_BUTTON_PRIMARY);
            }
        }
        if state.blocks_selection() {
            self.spinner.set_visible(true);
            self.spinner.start();
            self.status.set_tooltip_text(None);
            self.status.set_label(if saving {
                "Saving selected wallpaper…"
            } else {
                "Searching Wallhaven…"
            });
            self.retry.set_visible(false);
        } else {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }
        self.status_row.set_visible(
            active.is_some() && (state.blocks_selection() || !self.status.label().is_empty()),
        );
        drop(wallpaper_state);
        self.update_apply_sensitivity();
    }

    fn switch_wallpaper(self: &Rc<Self>, preset: Option<&str>) {
        let changed = {
            let mut state = self.wallpaper.borrow_mut();
            let Some(state) = state.as_mut() else { return };
            if preset.is_some_and(|id| !state.presets.iter().any(|item| item.id == id)) {
                return;
            }
            state.switch(preset, &mut self.candidates.borrow_mut())
        };
        if !changed {
            return;
        }
        self.thumbnail_scheduler.borrow_mut().clear();
        self.status.set_label("");
        self.status.set_tooltip_text(None);
        self.retry.set_visible(false);
        let (query, error, needs_search) = {
            let state = self.wallpaper.borrow();
            let state = state.as_ref().expect("wallpaper state");
            let search = state.current_search();
            (
                search
                    .map(|search| search.query.clone())
                    .unwrap_or_default(),
                search.and_then(|search| search.error.clone()),
                state.needs_search(),
            )
        };
        self.wallhaven_query.set_text(&query);
        if let Some(error) = error {
            self.wallpaper_error(error);
        }
        *self.selected.borrow_mut() = None;
        self.ensure_visible_selection();
        self.update_wallpaper_controls();
        if preset.is_some() {
            self.wallhaven_query.grab_focus();
        } else {
            self.search.grab_focus();
        }
        self.render();
        if needs_search {
            self.start_wallpaper_search(query);
        }
    }

    fn on_wallhaven(&self) -> bool {
        self.wallpaper
            .borrow()
            .as_ref()
            .is_some_and(|state| state.active.is_some())
    }

    fn submit_wallhaven_query(self: &Rc<Self>) {
        self.start_wallpaper_search(self.wallhaven_query.text().to_string());
    }

    fn start_wallpaper_search(self: &Rc<Self>, query: String) {
        let (executable, preset, generation) = {
            let mut state = self.wallpaper.borrow_mut();
            let Some(state) = state.as_mut() else { return };
            let Some((preset, generation)) = state.searching(query.clone()) else {
                return;
            };
            (state.executable.clone(), preset, generation)
        };
        self.candidates.borrow_mut().clear();
        *self.selected.borrow_mut() = None;
        self.thumbnail_scheduler.borrow_mut().clear();
        self.retry.set_visible(false);
        self.update_wallpaper_controls();
        self.render();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let request_preset = preset.clone();
            let request_query = query.clone();
            let result = gio::spawn_blocking(move || {
                provider_command(&executable, &["search", &request_preset, &request_query])
            })
            .await
            .map_err(|error| format!("Wallhaven worker failed: {error:?}"))
            .and_then(|result| result)
            .and_then(|bytes| {
                serde_json::from_slice::<Vec<Candidate>>(&bytes)
                    .map_err(|error| format!("Invalid Wallhaven results: {error}"))
            })
            .and_then(|rows| {
                validate_candidates(&rows)?;
                if rows
                    .iter()
                    .any(|row| !row.preview.as_deref().is_some_and(Path::is_absolute))
                {
                    return Err("Wallhaven returned a non-local preview".to_string());
                }
                Ok(rows)
            });
            if let Some(chooser) = weak.upgrade() {
                chooser.wallpaper_search_finished(&preset, &query, generation, result);
            }
        });
    }

    fn wallpaper_search_finished(
        self: &Rc<Self>,
        preset: &str,
        query: &str,
        generation: u64,
        result: Result<Vec<Candidate>, String>,
    ) {
        let mut state = self.wallpaper.borrow_mut();
        if self.outcome.borrow().is_some()
            || !state.as_mut().is_some_and(|state| {
                state.complete(&WallpaperPending::Search {
                    preset: preset.to_string(),
                    query: query.to_string(),
                    generation,
                })
            })
        {
            return;
        }
        let result = match result {
            Ok(rows) => state
                .as_mut()
                .expect("wallpaper state")
                .display_or_cache(preset, rows)
                .map(Ok),
            Err(error) => state
                .as_mut()
                .expect("wallpaper state")
                .search_failed(preset, error)
                .map(Err),
        };
        drop(state);
        if let Some(result) = result {
            match result {
                Ok(rows) => {
                    *self.candidates.borrow_mut() = rows;
                    self.status
                        .set_label(if self.candidates.borrow().is_empty() {
                            "No Wallhaven results"
                        } else {
                            ""
                        });
                    self.status.set_tooltip_text(None);
                    self.retry.set_visible(false);
                    *self.selected.borrow_mut() = None;
                    self.ensure_visible_selection();
                    self.render();
                }
                Err(error) => self.wallpaper_error(error),
            }
        }
        self.update_wallpaper_controls();
    }

    fn wallpaper_error(self: &Rc<Self>, error: String) {
        self.status.set_label(&error);
        self.status.set_tooltip_text(Some(&error));
        self.retry.set_visible(true);
        self.status_row.set_visible(true);
    }

    fn save_wallpaper(self: &Rc<Self>, preset: String, id: String) {
        let (executable, generation) = {
            let mut state = self.wallpaper.borrow_mut();
            let state = state.as_mut().expect("wallpaper state");
            state.generation += 1;
            let generation = state.generation;
            state.pending = Some(WallpaperPending::Save {
                preset: preset.clone(),
                generation,
            });
            (state.executable.clone(), generation)
        };
        self.retry.set_visible(false);
        self.update_wallpaper_controls();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let request_preset = preset.clone();
            let result = gio::spawn_blocking(move || {
                provider_command(&executable, &["save", &request_preset, &id])
            })
            .await
            .map_err(|error| format!("Wallhaven worker failed: {error:?}"))
            .and_then(|result| result)
            .and_then(|bytes| {
                serde_json::from_slice::<SavedWallpaper>(&bytes)
                    .map_err(|error| format!("Invalid Wallhaven save response: {error}"))
            })
            .and_then(|result| {
                if result.path.is_absolute() {
                    Ok(result.path)
                } else {
                    Err("Wallhaven returned a non-local saved path".to_string())
                }
            });
            if let Some(chooser) = weak.upgrade() {
                chooser.wallpaper_save_finished(&preset, generation, result);
            }
        });
    }

    fn wallpaper_save_finished(
        self: &Rc<Self>,
        preset: &str,
        generation: u64,
        result: Result<PathBuf, String>,
    ) {
        let mut state = self.wallpaper.borrow_mut();
        if self.outcome.borrow().is_some()
            || !state.as_mut().is_some_and(|state| {
                state.complete(&WallpaperPending::Save {
                    preset: preset.to_string(),
                    generation,
                })
            })
        {
            return;
        }
        drop(state);
        match result {
            Ok(path) => self.finish(Outcome::Saved(path)),
            Err(error) => {
                self.wallpaper_error(error);
                self.retry.set_sensitive(self.selection_is_ready());
                self.update_wallpaper_controls();
            }
        }
    }

    fn wire(self: &Rc<Self>, cancel: &Button) {
        self.random.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.select_random_wallpaper();
                }
            }
        });
        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
        wheel.connect_scroll({
            let weak = Rc::downgrade(self);
            move |_, _, _| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.scroll_motion.cancel();
                    chooser.scroll_target.set(None);
                }
                glib::Propagation::Proceed
            }
        });
        self.scroll.add_controller(wheel);

        for entry in [&self.search, &self.filter] {
            entry.connect_changed({
                let weak = Rc::downgrade(self);
                move |_| {
                    if let Some(chooser) = weak.upgrade() {
                        chooser.ensure_visible_selection();
                        chooser.render();
                    }
                }
            });
        }
        self.search.connect_activate({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.accept();
                }
            }
        });
        self.apply.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.accept();
                }
            }
        });
        cancel.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.cancel();
                }
            }
        });

        let keys = gtk4::EventControllerKey::new();
        // Capture navigation before the focused search entry handles it.
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let weak = Rc::downgrade(self);
            move |_, key, _, modifiers| {
                let Some(chooser) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if matches!(key, gdk::Key::Tab | gdk::Key::ISO_Left_Tab)
                    && modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                {
                    let on_wallhaven = chooser
                        .wallpaper
                        .borrow()
                        .as_ref()
                        .map(|state| state.active.is_some());
                    match on_wallhaven {
                        Some(true) => chooser.switch_wallpaper(None),
                        Some(false) => chooser.activate_wallhaven(),
                        None => return glib::Propagation::Proceed,
                    }
                    return glib::Propagation::Stop;
                }
                match key {
                    gdk::Key::Escape => chooser.cancel(),
                    gdk::Key::Up => chooser.move_selection(-1),
                    gdk::Key::Down => chooser.move_selection(1),
                    gdk::Key::Home => chooser.select_at(0),
                    gdk::Key::End => chooser.select_at(usize::MAX),
                    gdk::Key::Return | gdk::Key::KP_Enter => {
                        if chooser.random.has_focus() || chooser.wallhaven_search.has_focus() {
                            return glib::Propagation::Proceed;
                        }
                        let query_focused = chooser.wallhaven_query.has_focus()
                            || gtk4::prelude::GtkWindowExt::focus(&chooser.window)
                                .is_some_and(|focus| focus.is_ancestor(&chooser.wallhaven_query));
                        if chooser.on_wallhaven() && query_focused {
                            chooser.submit_wallhaven_query();
                        } else {
                            chooser.accept();
                        }
                    }
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        });
        self.window.add_controller(keys);
        self.window.connect_close_request({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.cancel();
                }
                glib::Propagation::Stop
            }
        });
        let click = gtk4::GestureClick::new();
        click.set_button(0);
        click.connect_released({
            let weak = Rc::downgrade(self);
            move |_, _, _, _| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.cancel();
                }
            }
        });
        self.backdrop.add_controller(click);

        let adjustment = self.scroll.vadjustment();
        adjustment.connect_value_changed({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.refresh_thumbnail_interests();
                }
            }
        });
    }

    /// A layer surface cannot be kept alive usefully after its output went
    /// away.  Closing through the normal cancellation path also releases the
    /// input lock held by `run`.
    fn cancel_if_monitor_disappears(
        self: &Rc<Self>,
        display: &gdk::Display,
        monitor: Option<&gdk::Monitor>,
    ) {
        let Some(monitor) = monitor.cloned() else {
            return;
        };
        let monitors = display.monitors();
        monitors.connect_items_changed({
            let weak = Rc::downgrade(self);
            move |monitors, _, _, _| {
                let still_present = (0..monitors.n_items()).any(|index| {
                    monitors
                        .item(index)
                        .and_downcast::<gdk::Monitor>()
                        .is_some_and(|item| item == monitor)
                });
                if !still_present && let Some(chooser) = weak.upgrade() {
                    chooser.cancel();
                }
            }
        });
    }

    fn open(&self) {
        self.backdrop.set_visible(true);
        self.window.present();
        self.root.set_opacity(0.0);
        let root = self.root.clone();
        self.container_motion.start(
            AnimationParams::new(200).with_easing(Easing::EaseOutCubic),
            Box::new(move |progress| root.set_opacity(progress)),
            None,
        );
        self.search.grab_focus();
    }

    fn query_indices(&self) -> Vec<usize> {
        let query = if self.on_wallhaven() {
            self.filter.text()
        } else {
            self.search.text()
        };
        matching_indices(&self.candidates.borrow(), &query)
    }

    fn update_random_sensitivity(&self) {
        let pending = self
            .wallpaper
            .borrow()
            .as_ref()
            .is_some_and(WallpaperState::blocks_selection);
        let visible = self.query_indices();
        let candidates = self.candidates.borrow();
        self.random.set_sensitive(
            !pending
                && visible
                    .iter()
                    .any(|&index| candidates[index].preview.is_some()),
        );
    }

    fn select_random_wallpaper(self: &Rc<Self>) {
        if !self.random.is_sensitive() {
            return;
        }
        let visible = self.query_indices();
        let index = random_wallpaper_index(
            &self.candidates.borrow(),
            &visible,
            self.selected.borrow().as_deref(),
        );
        if let Some(index) = index {
            self.select_index(index);
        }
    }

    fn ensure_visible_selection(self: &Rc<Self>) {
        let visible = self.query_indices();
        let selected = self.selected.borrow();
        if selected.as_ref().is_some_and(|id| {
            visible
                .iter()
                .any(|&index| self.candidates.borrow()[index].id.as_str() == id.as_str())
        }) {
            return;
        }
        drop(selected);
        *self.selected.borrow_mut() = retained_selection(&self.candidates.borrow(), &visible, None);
        self.update_apply_sensitivity();
    }

    fn move_selection(self: &Rc<Self>, direction: isize) {
        let visible = self.query_indices();
        if visible.is_empty() {
            return;
        }
        let selected = self.selected.borrow();
        let here = selected
            .as_ref()
            .and_then(|id| {
                visible
                    .iter()
                    .position(|&index| self.candidates.borrow()[index].id.as_str() == id.as_str())
            })
            .unwrap_or(0);
        drop(selected);
        let target = if direction < 0 {
            here.saturating_sub(direction.unsigned_abs())
        } else {
            here.saturating_add(direction as usize)
                .min(visible.len() - 1)
        };
        self.select_from_keyboard(visible[target]);
    }

    fn select_at(self: &Rc<Self>, index: usize) {
        let visible = self.query_indices();
        if let Some(&candidate) = visible.get(index.min(visible.len().saturating_sub(1))) {
            self.select_from_keyboard(candidate);
        }
    }

    fn select_from_keyboard(self: &Rc<Self>, index: usize) {
        if self.layout == ChooseLayout::Themes {
            self.scroll_motion.cancel();
            self.scroll_target.set(None);
        }
        if let Some((_, row)) = self
            .row_widgets
            .borrow()
            .iter()
            .find(|(candidate, _)| *candidate == index)
        {
            if self.layout == ChooseLayout::Themes && row.has_focus() {
                // Repeated Home/End must bring back a row scrolled away by the wheel.
                self.scroll
                    .child()
                    .and_downcast::<gtk4::Viewport>()
                    .expect("GtkBox results are wrapped in a viewport")
                    .scroll_to(row, None);
            }
            row.grab_focus();
        }
        self.select_index(index);
    }

    fn select_index(self: &Rc<Self>, index: usize) {
        let previous = self.selected.borrow().clone();
        if let Some(previous) = previous.as_deref() {
            let rows = self.row_widgets.borrow();
            if let Some((_, row)) = rows
                .iter()
                .find(|(candidate, _)| self.candidates.borrow()[*candidate].id == previous)
            {
                row.remove_css_class(classes::CHOOSER_RESULT_SELECTED);
            }
        }
        let id = self.candidates.borrow()[index].id.clone();
        if let Some((_, row)) = self
            .row_widgets
            .borrow()
            .iter()
            .find(|(candidate, _)| *candidate == index)
        {
            row.add_css_class(classes::CHOOSER_RESULT_SELECTED);
        }
        *self.selected.borrow_mut() = Some(id);
        self.update_apply_sensitivity();
        self.render_preview();
        if let Some(request) = previous
            .as_deref()
            .and_then(|id| {
                self.candidates
                    .borrow()
                    .iter()
                    .find(|candidate| candidate.id == id)
                    .and_then(|candidate| candidate.preview.clone())
            })
            .map(|path| {
                self.thumbnail_request(&path, self.preview_width, self.preview_decode_height)
            })
        {
            prune_dead_thumbnail_refs(&mut self.thumbnail_images.borrow_mut(), &request);
            prune_dead_thumbnail_refs(&mut self.thumbnail_errors.borrow_mut(), &request);
        }
        self.refresh_thumbnail_interests();
        if self.layout != ChooseLayout::Themes
            && !self.scroll_selected_into_view()
            && !self.scroll_retry_queued.replace(true)
        {
            let chooser = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(chooser) = chooser.upgrade() {
                    chooser.scroll_retry_queued.set(false);
                    chooser.scroll_selected_into_view();
                }
            });
        }
    }

    /// Returns false only when the selected row has not yet been allocated.
    fn scroll_selected_into_view(&self) -> bool {
        let selected = self.selected.borrow();
        let Some(selected) = selected.as_deref() else {
            return true;
        };
        let rows = self.row_widgets.borrow();
        let Some((_, row)) = rows
            .iter()
            .find(|(index, _)| self.candidates.borrow()[*index].id.as_str() == selected)
        else {
            return true;
        };
        let Some(bounds) = row.compute_bounds(&self.results) else {
            return false;
        };
        if bounds.height() <= 0.0 {
            return false;
        }
        let adjustment = self.scroll.vadjustment();
        let current = adjustment.value();
        let page = adjustment.page_size();
        let start = f64::from(bounds.y());
        let end = start + f64::from(bounds.height());
        let base = self.scroll_target.get().unwrap_or(current);
        let Some(value) = scroll_value_for_bounds(base, page, start, end) else {
            return true;
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
        true
    }

    fn accept(self: &Rc<Self>) {
        if !self.selection_is_ready()
            || self
                .wallpaper
                .borrow()
                .as_ref()
                .is_some_and(WallpaperState::blocks_selection)
        {
            return;
        }
        let Some(id) = self.selected.borrow().clone() else {
            return;
        };
        let preset = self
            .wallpaper
            .borrow()
            .as_ref()
            .and_then(|state| state.active.clone());
        if let Some(preset) = preset {
            self.save_wallpaper(preset, id);
        } else {
            self.finish(Outcome::Selected(id));
        }
    }

    fn cancel(&self) {
        if self
            .wallpaper
            .borrow()
            .as_ref()
            .is_some_and(|state| matches!(state.pending, Some(WallpaperPending::Save { .. })))
        {
            return;
        }
        self.finish(Outcome::Cancelled);
    }

    fn finish(&self, outcome: Outcome) {
        if self.outcome.borrow().is_some() {
            return;
        }
        *self.outcome.borrow_mut() = Some(outcome);
        self.thumbnail_scheduler.borrow_mut().clear();
        self.thumbnail_images.borrow_mut().clear();
        self.thumbnail_errors.borrow_mut().clear();
        self.window.close();
        self.backdrop.close();
        self.app.quit();
    }

    fn render(self: &Rc<Self>) {
        self.scroll_motion.cancel();
        self.scroll_target.set(None);
        self.thumbnail_images.borrow_mut().clear();
        self.thumbnail_errors.borrow_mut().clear();
        self.row_widgets.borrow_mut().clear();
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }
        let visible = self.query_indices();
        if visible.is_empty() {
            let empty = Label::new(Some(
                if self.wallpaper.borrow().as_ref().is_some_and(|state| {
                    state.active.is_some()
                        && matches!(state.pending, Some(WallpaperPending::Search { .. }))
                }) {
                    "Searching Wallhaven…"
                } else {
                    "No matching choices"
                },
            ));
            empty.add_css_class(classes::CHOOSER_EMPTY);
            empty.set_xalign(0.0);
            self.results.append(&empty);
        } else {
            for index in visible {
                let row = self.row(index);
                self.row_widgets.borrow_mut().push((index, row.clone()));
                self.results.append(&row);
            }
        }
        self.render_preview();
        self.update_apply_sensitivity();
        self.refresh_thumbnail_interests();
        if self.layout == ChooseLayout::Wallpapers {
            self.update_random_sensitivity();
        }
    }

    fn row(self: &Rc<Self>, index: usize) -> Button {
        let candidates = self.candidates.borrow();
        let candidate = &candidates[index];
        let row = Button::new();
        row.add_css_class(classes::CHOOSER_RESULT);
        if self.selected.borrow().as_deref() == Some(candidate.id.as_str()) {
            row.add_css_class(classes::CHOOSER_RESULT_SELECTED);
        }
        if self.current.as_deref() == Some(candidate.id.as_str()) {
            row.add_css_class(classes::CHOOSER_RESULT_CURRENT);
        }
        row.set_tooltip_text(Some(&candidate.id));

        // A theme swatch beside its metadata keeps a complete result within
        // the measured four-row viewport. Stacking them would make a single
        // row taller than the geometry used by the scroller.
        let content = gtk4::Box::new(Orientation::Horizontal, 10);
        content.set_halign(Align::Fill);
        let request = candidate
            .preview
            .as_deref()
            .map(|path| self.thumbnail_request(path, self.thumbnail_width, self.thumbnail_height));
        if self.layout == ChooseLayout::Wallpapers {
            content.append(&self.thumbnail_image(request.as_ref()));
        } else if self.layout == ChooseLayout::Themes {
            content.append(&palette_swatch(candidate.palette.as_ref()));
        } else if let Some(icon) = candidate.icon.as_deref() {
            content.append(&candidate_icon(icon));
        }

        let text = gtk4::Box::new(Orientation::Vertical, 2);
        text.set_hexpand(true);
        let label = Label::new(Some(&candidate.label));
        label.add_css_class(classes::CHOOSER_LABEL);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        if self.layout == ChooseLayout::Wallpapers {
            label.set_max_width_chars(48);
        }
        text.append(&label);
        if let Some(subtitle) = candidate.subtitle.as_deref() {
            let subtitle = Label::new(Some(subtitle));
            subtitle.add_css_class(classes::CHOOSER_SUBTITLE);
            subtitle.set_xalign(0.0);
            subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            if self.layout == ChooseLayout::Wallpapers {
                subtitle.set_max_width_chars(48);
            }
            text.append(&subtitle);
        }
        if let Some(mode) = candidate.palette.as_ref().and_then(Palette::mode) {
            let mode = Label::new(Some(mode));
            mode.add_css_class(classes::CHOOSER_MODE);
            mode.set_xalign(0.0);
            text.append(&mode);
        }
        if self.layout == ChooseLayout::Wallpapers {
            let unavailable = Label::new(Some("Preview unavailable"));
            unavailable.add_css_class(classes::CHOOSER_SUBTITLE);
            unavailable.set_xalign(0.0);
            unavailable.set_max_width_chars(48);
            unavailable.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            unavailable.set_visible(false);
            if let Some(request) = request.as_ref() {
                self.thumbnail_errors
                    .borrow_mut()
                    .entry(request.clone())
                    .or_default()
                    .push(unavailable.downgrade());
                if matches!(self.thumbnail_view(request), ThumbnailView::Failed(_)) {
                    unavailable.set_visible(true);
                }
            }
            text.append(&unavailable);
        }
        content.append(&text);
        if self.current.as_deref() == Some(candidate.id.as_str()) {
            let current = Label::new(Some("Current"));
            current.add_css_class(classes::CHOOSER_CURRENT);
            current.set_valign(Align::Center);
            content.append(&current);
        }
        row.set_child(Some(&content));
        row.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(chooser) = weak.upgrade() {
                    chooser.select_index(index);
                }
            }
        });
        row
    }

    fn render_preview(self: &Rc<Self>) {
        if self.layout != ChooseLayout::Wallpapers {
            while let Some(child) = self.preview.first_child() {
                self.preview.remove(&child);
            }
        }
        let selected = self.selected.borrow();
        let candidates = self.candidates.borrow();
        let candidate = selected
            .as_deref()
            .and_then(|id| candidates.iter().find(|candidate| candidate.id == id));
        match self.layout {
            ChooseLayout::Wallpapers => {
                let request = candidate.and_then(|candidate| {
                    candidate.preview.as_deref().map(|path| {
                        self.thumbnail_request(path, self.preview_width, self.preview_decode_height)
                    })
                });
                if let Some((picture, placeholder, error)) = self.preview_picture.as_ref() {
                    if let Some(request) = request.as_ref() {
                        Self::set_preview_view(
                            &self.thumbnail_view(request),
                            picture,
                            placeholder,
                            error,
                        );
                    } else {
                        picture.set_paintable(None::<&gdk::Texture>);
                        placeholder.set_visible(false);
                        error.set_label("");
                    }
                }
                *self.preview_request.borrow_mut() = request;
            }
            ChooseLayout::Themes => {
                if let Some(candidate) = candidate {
                    let sample = gtk4::Box::new(Orientation::Vertical, 4);
                    sample.add_css_class(classes::CHOOSER_THEME_SAMPLE);
                    let heading = Label::new(Some(&candidate.label));
                    heading.set_xalign(0.0);
                    sample.append(&heading);
                    sample.append(&palette_swatch(candidate.palette.as_ref()));
                    self.preview.append(&sample);
                }
            }
            ChooseLayout::List => {}
        }
    }

    /// The wallpaper picker has a stronger activation precondition than a
    /// plain list: applying a known unreadable file is never useful.  A decode
    /// in progress also leaves Apply disabled, which prevents Enter from
    /// racing an image worker.
    fn selection_is_ready(self: &Rc<Self>) -> bool {
        let selected = self.selected.borrow();
        let Some(id) = selected.as_deref() else {
            return false;
        };
        if self.layout != ChooseLayout::Wallpapers {
            return true;
        }
        let candidates = self.candidates.borrow();
        let Some(candidate) = candidates.iter().find(|candidate| candidate.id == id) else {
            return false;
        };
        candidate.preview.as_deref().is_some_and(|path| {
            let request =
                self.thumbnail_request(path, self.preview_width, self.preview_decode_height);
            preview_allows_apply(preview_status(&self.thumbnail_view(&request)))
        })
    }

    fn update_apply_sensitivity(self: &Rc<Self>) {
        let pending = self
            .wallpaper
            .borrow()
            .as_ref()
            .is_some_and(WallpaperState::blocks_selection);
        self.apply
            .set_sensitive(!pending && self.selection_is_ready());
    }

    fn thumbnail_image(self: &Rc<Self>, request: Option<&ThumbnailRequest>) -> Image {
        let image = match request.map(|request| self.thumbnail_view(request)) {
            Some(ThumbnailView::Ready(texture)) => Image::from_paintable(Some(&texture)),
            Some(ThumbnailView::Failed(_)) => Image::from_icon_name("image-missing-symbolic"),
            Some(ThumbnailView::Loading) | None => Image::from_icon_name("image-loading-symbolic"),
        };
        if let Some(request) = request {
            self.thumbnail_images
                .borrow_mut()
                .entry(request.clone())
                .or_default()
                .push(image.downgrade());
            image.set_size_request(request.width, request.height);
            image.set_pixel_size(request.height);
        }
        image.set_halign(Align::Start);
        image.set_valign(Align::Center);
        image
    }

    /// Lookup is intentionally side-effect free: it neither probes files nor
    /// queues a worker.  Interest synchronization below owns those actions.
    fn thumbnail_view(&self, request: &ThumbnailRequest) -> ThumbnailView {
        match self.thumbnail_scheduler.borrow().state(request) {
            Some(ThumbnailRequestState::Ready(key)) => self.cached_thumbnail_view(key),
            Some(ThumbnailRequestState::Failed(error)) => ThumbnailView::Failed(error.clone()),
            Some(ThumbnailRequestState::Revalidating(ThumbnailStableState::Ready(key))) => {
                self.cached_thumbnail_view(key)
            }
            Some(ThumbnailRequestState::Revalidating(ThumbnailStableState::Failed(error))) => {
                ThumbnailView::Failed(error.clone())
            }
            Some(ThumbnailRequestState::Queued(_)) | Some(ThumbnailRequestState::Active) | None => {
                ThumbnailView::Loading
            }
        }
    }

    fn cached_thumbnail_view(&self, key: &ThumbnailKey) -> ThumbnailView {
        match self.thumbnail_cache.borrow().get(key) {
            Some(CachedThumbnail::Texture(texture)) => ThumbnailView::Ready(texture.clone()),
            Some(CachedThumbnail::Failed(error)) => ThumbnailView::Failed(error.clone()),
            None => ThumbnailView::Loading,
        }
    }

    fn thumbnail_request(&self, path: &Path, width: i32, height: i32) -> ThumbnailRequest {
        let (width, height) = bounded_thumbnail_dimensions(width, height);
        ThumbnailRequest {
            path: path.to_path_buf(),
            width,
            height,
        }
    }

    fn refresh_thumbnail_interests(self: &Rc<Self>) {
        if self.layout != ChooseLayout::Wallpapers || self.outcome.borrow().is_some() {
            return;
        }
        let mut interests = Vec::with_capacity(THUMBNAIL_ROW_INTEREST_LIMIT + 1);
        if let Some(id) = self.selected.borrow().as_deref()
            && let Some(path) = self
                .candidates
                .borrow()
                .iter()
                .find(|candidate| candidate.id == id)
                .and_then(|candidate| candidate.preview.clone())
        {
            interests.push(self.thumbnail_request(
                &path,
                self.preview_width,
                self.preview_decode_height,
            ));
        }
        interests.extend(self.viewport_thumbnail_requests());
        self.thumbnail_scheduler.borrow_mut().synchronize(interests);
        self.pin_selected_preview();
        self.start_thumbnail_jobs();
    }

    fn pin_selected_preview(&self) {
        let key = self
            .selected
            .borrow()
            .as_deref()
            .and_then(|id| {
                self.candidates
                    .borrow()
                    .iter()
                    .find(|candidate| candidate.id == id)
                    .and_then(|candidate| candidate.preview.clone())
            })
            .map(|path| {
                self.thumbnail_request(&path, self.preview_width, self.preview_decode_height)
            })
            .and_then(
                |request| match self.thumbnail_scheduler.borrow().state(&request) {
                    Some(ThumbnailRequestState::Ready(key)) => Some(key.clone()),
                    Some(ThumbnailRequestState::Revalidating(ThumbnailStableState::Ready(key))) => {
                        Some(key.clone())
                    }
                    _ => None,
                },
            );
        self.thumbnail_cache.borrow_mut().set_pinned(key);
    }

    fn viewport_thumbnail_requests(&self) -> Vec<ThumbnailRequest> {
        let adjustment = self.scroll.vadjustment();
        let start = adjustment.value();
        let page = adjustment.page_size().max(1.0);
        let end = start + page;
        let overscan_start = (start - page).max(0.0);
        let overscan_end = end + page;
        let rows = self.row_widgets.borrow();
        let mut viewport = Vec::new();
        let mut overscan = Vec::new();
        for (index, row) in rows.iter() {
            let Some(bounds) = row.compute_bounds(&self.results) else {
                continue;
            };
            let row_start = f64::from(bounds.y());
            let row_end = row_start + f64::from(bounds.height());
            let request = self.candidates.borrow()[*index]
                .preview
                .as_deref()
                .map(|path| {
                    self.thumbnail_request(path, self.thumbnail_width, self.thumbnail_height)
                });
            let Some(request) = request else {
                continue;
            };
            if row_end > start && row_start < end {
                viewport.push(request);
            } else if row_end > overscan_start && row_start < overscan_end {
                overscan.push(request);
            }
        }
        // Before allocation GTK reports zero-sized rows.  The same bounded
        // prefix is a stable first-frame fallback until scrolling supplies
        // precise geometry.
        if viewport.is_empty() && overscan.is_empty() {
            overscan.extend(rows.iter().filter_map(|(index, _)| {
                self.candidates.borrow()[*index]
                    .preview
                    .as_deref()
                    .map(|path| {
                        self.thumbnail_request(path, self.thumbnail_width, self.thumbnail_height)
                    })
            }));
        }
        bounded_row_interests(viewport.into_iter().chain(overscan))
    }

    fn start_thumbnail_jobs(self: &Rc<Self>) {
        while let Some(job) = self.thumbnail_scheduler.borrow_mut().next_job() {
            self.decode_thumbnail(job);
        }
    }

    fn decode_thumbnail(self: &Rc<Self>, job: ThumbnailJob) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let task = job.clone();
            let outcome = gio::spawn_blocking(move || match task.kind {
                ThumbnailJobKind::Probe => ThumbnailWorkerOutcome::Probe(probe_thumbnail(
                    &task.request.path,
                    task.request.width,
                    task.request.height,
                )),
                ThumbnailJobKind::Decode => ThumbnailWorkerOutcome::Decode(decode_thumbnail(
                    &task.request.path,
                    task.request.width,
                    task.request.height,
                )),
            })
            .await
            .map_err(|error| format!("thumbnail worker failed: {error:?}"));
            let Some(chooser) = weak.upgrade() else {
                return;
            };
            chooser.thumbnail_finished(job, outcome);
        });
    }

    fn thumbnail_finished(
        self: &Rc<Self>,
        job: ThumbnailJob,
        outcome: Result<ThumbnailWorkerOutcome, String>,
    ) {
        let mut changed = false;
        match (job.kind, outcome) {
            (ThumbnailJobKind::Probe, Ok(ThumbnailWorkerOutcome::Probe(Ok(key)))) => {
                let cached = self
                    .thumbnail_cache
                    .borrow()
                    .get(&key)
                    .map(|cached| match cached {
                        CachedThumbnail::Texture(_) => None,
                        CachedThumbnail::Failed(error) => Some(error.clone()),
                    });
                let mut scheduler = self.thumbnail_scheduler.borrow_mut();
                let cache_miss = if let Some(state) = scheduler.complete(&job) {
                    match cached.as_ref() {
                        Some(Some(error)) => *state = ThumbnailRequestState::Failed(error.clone()),
                        Some(None) => *state = ThumbnailRequestState::Ready(key),
                        None => {}
                    }
                    changed = true;
                    cached.is_none()
                } else {
                    false
                };
                if cache_miss {
                    scheduler.queue(&job, ThumbnailJobKind::Decode);
                }
            }
            (ThumbnailJobKind::Probe, Ok(ThumbnailWorkerOutcome::Probe(Err(error))))
            | (ThumbnailJobKind::Probe, Err(error)) => {
                if let Some(state) = self.thumbnail_scheduler.borrow_mut().complete(&job) {
                    *state = ThumbnailRequestState::Failed(error);
                    changed = true;
                }
            }
            (
                ThumbnailJobKind::Decode,
                Ok(ThumbnailWorkerOutcome::Decode(DecodeOutcome::Pixels(decoded))),
            ) => {
                let texture = gdk::MemoryTexture::new(
                    decoded.width,
                    decoded.height,
                    gdk::MemoryFormat::R8g8b8a8,
                    &glib::Bytes::from_owned(decoded.rgba),
                    decoded.width as usize * 4,
                );
                let key = decoded.key;
                let bytes = decoded.width as usize * decoded.height as usize * 4;
                let mut scheduler = self.thumbnail_scheduler.borrow_mut();
                if let Some(state) = scheduler.complete(&job) {
                    self.thumbnail_cache.borrow_mut().insert(DecodedThumbnail {
                        key: key.clone(),
                        value: CachedThumbnail::Texture(texture.upcast()),
                        bytes,
                    });
                    *state = ThumbnailRequestState::Ready(key);
                    changed = true;
                }
            }
            (
                ThumbnailJobKind::Decode,
                Ok(ThumbnailWorkerOutcome::Decode(DecodeOutcome::Failed { key, error })),
            ) => {
                let mut scheduler = self.thumbnail_scheduler.borrow_mut();
                if let Some(state) = scheduler.complete(&job) {
                    if let Some(key) = key {
                        self.thumbnail_cache.borrow_mut().insert(DecodedThumbnail {
                            key: key.clone(),
                            value: CachedThumbnail::Failed(error.clone()),
                            bytes: 0,
                        });
                        *state = ThumbnailRequestState::Ready(key);
                    } else {
                        *state = ThumbnailRequestState::Failed(error);
                    }
                    changed = true;
                }
            }
            (
                ThumbnailJobKind::Decode,
                Ok(ThumbnailWorkerOutcome::Decode(DecodeOutcome::Stale)),
            ) => {
                let mut scheduler = self.thumbnail_scheduler.borrow_mut();
                if scheduler.complete(&job).is_some() {
                    scheduler.queue(&job, ThumbnailJobKind::Probe);
                }
            }
            (ThumbnailJobKind::Decode, Err(error)) => {
                if let Some(state) = self.thumbnail_scheduler.borrow_mut().complete(&job) {
                    *state = ThumbnailRequestState::Failed(error);
                    changed = true;
                }
            }
            _ => {
                if let Some(state) = self.thumbnail_scheduler.borrow_mut().complete(&job) {
                    *state = ThumbnailRequestState::Failed(
                        "thumbnail worker returned an unexpected result".to_string(),
                    );
                    changed = true;
                }
            }
        }
        if changed {
            self.pin_selected_preview();
            self.update_thumbnail_widgets(&job.request);
            self.update_apply_sensitivity();
        }
        self.start_thumbnail_jobs();
    }

    fn update_thumbnail_widgets(&self, request: &ThumbnailRequest) {
        let view = self.thumbnail_view(request);
        if self.preview_request.borrow().as_ref() == Some(request)
            && let Some((picture, placeholder, error)) = self.preview_picture.as_ref()
        {
            Self::set_preview_view(&view, picture, placeholder, error);
        }
        if let Some(images) = self.thumbnail_images.borrow_mut().get_mut(request) {
            images.retain(|image| {
                let Some(image) = image.upgrade() else {
                    return false;
                };
                match &view {
                    ThumbnailView::Ready(texture) => image.set_paintable(Some(texture)),
                    ThumbnailView::Loading => image.set_icon_name(Some("image-loading-symbolic")),
                    ThumbnailView::Failed(_) => image.set_icon_name(Some("image-missing-symbolic")),
                }
                true
            });
        }
        if let Some(labels) = self.thumbnail_errors.borrow_mut().get_mut(request) {
            labels.retain(|label| {
                let Some(label) = label.upgrade() else {
                    return false;
                };
                match &view {
                    ThumbnailView::Failed(error) => {
                        label.set_label(&format!("Preview unavailable: {error}"));
                        label.set_tooltip_text(Some(error));
                        label.set_visible(true);
                    }
                    ThumbnailView::Loading | ThumbnailView::Ready(_) => {
                        label.set_visible(false);
                        label.set_tooltip_text(None);
                    }
                }
                true
            });
        }
    }

    fn set_preview_view(
        view: &ThumbnailView,
        picture: &Picture,
        placeholder: &Image,
        error: &Label,
    ) {
        match view {
            ThumbnailView::Ready(texture) => {
                picture.set_paintable(Some(texture));
                placeholder.set_visible(false);
                error.set_label("");
            }
            ThumbnailView::Loading => {
                // Keep the last painted frame until the next thumbnail is ready.
                placeholder.set_icon_name(Some("image-loading-symbolic"));
                placeholder.set_visible(true);
                error.set_label("");
            }
            ThumbnailView::Failed(message) => {
                picture.set_paintable(None::<&gdk::Texture>);
                placeholder.set_icon_name(Some("image-missing-symbolic"));
                placeholder.set_visible(true);
                error.set_label(&format!("Preview unavailable: {message}"));
            }
        }
    }
}

fn candidate_icon(icon: &str) -> Image {
    let image = if Path::new(icon).is_absolute() {
        Image::from_file(icon)
    } else {
        Image::from_icon_name(icon)
    };
    image.set_pixel_size(28);
    image
}

fn probe_thumbnail(path: &Path, width: i32, height: i32) -> Result<ThumbnailKey, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("the preview path is not a regular file".to_string());
    }
    Ok(thumbnail_key(path, &metadata, width, height))
}

fn thumbnail_key(path: &Path, metadata: &fs::Metadata, width: i32, height: i32) -> ThumbnailKey {
    ThumbnailKey {
        path: path.to_path_buf(),
        identity: file_identity(metadata),
        modified: modification_stamp(metadata.modified().ok()),
        #[cfg(unix)]
        changed: change_stamp(metadata),
        bytes: metadata.len(),
        width,
        height,
    }
}

#[cfg(unix)]
fn change_stamp(metadata: &fs::Metadata) -> ModificationStamp {
    use std::os::unix::fs::MetadataExt;
    ModificationStamp {
        seconds: metadata.ctime(),
        nanoseconds: metadata.ctime_nsec().max(0) as u32,
    }
}

fn decode_thumbnail(path: &Path, width: i32, height: i32) -> DecodeOutcome {
    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return DecodeOutcome::Failed {
                key: None,
                error: "the preview path is not a regular file".to_string(),
            };
        }
        Err(error) => {
            return DecodeOutcome::Failed {
                key: None,
                error: error.to_string(),
            };
        }
    };
    let key = thumbnail_key(path, &metadata, width, height);
    let pixbuf = match gtk4::gdk_pixbuf::Pixbuf::from_file_at_scale(path, width, height, true) {
        Ok(pixbuf) => pixbuf,
        Err(error) => return decode_failure_if_current(path, &key, error.to_string()),
    };
    let width = pixbuf.width() as usize;
    let height = pixbuf.height() as usize;
    let stride = pixbuf.rowstride() as usize;
    let channels = pixbuf.n_channels() as usize;
    if channels < 3 || (pixbuf.has_alpha() && channels < 4) {
        return decode_failure_if_current(
            path,
            &key,
            "preview image has an unsupported pixel layout".to_string(),
        );
    }
    let source = pixbuf.read_pixel_bytes();
    let source: &[u8] = source.as_ref();
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        for column in 0..width {
            let offset = row * stride + column * channels;
            let Some(pixel) = source.get(offset..offset + channels) else {
                return decode_failure_if_current(
                    path,
                    &key,
                    "preview image has incomplete pixel data".to_string(),
                );
            };
            rgba.extend_from_slice(&pixel[..3]);
            rgba.push(if pixbuf.has_alpha() { pixel[3] } else { 255 });
        }
    }
    let after = source_still_matches(path, &key);
    // The source can be replaced while gdk-pixbuf is decoding it.  The next
    // bounded probe/decode cycle observes the new identity instead of
    // presenting pixels whose cache key no longer describes their source.
    if !after {
        return DecodeOutcome::Stale;
    }
    DecodeOutcome::Pixels(DecodedPixels {
        key,
        rgba,
        width: width as i32,
        height: height as i32,
    })
}

fn decode_failure_if_current(path: &Path, key: &ThumbnailKey, error: String) -> DecodeOutcome {
    if source_still_matches(path, key) {
        DecodeOutcome::Failed {
            key: Some(key.clone()),
            error,
        }
    } else {
        DecodeOutcome::Stale
    }
}

fn source_still_matches(path: &Path, key: &ThumbnailKey) -> bool {
    matches!(
        fs::metadata(path),
        Ok(metadata) if metadata.is_file()
            && thumbnail_key(path, &metadata, key.width, key.height) == *key
    )
}

fn bounded_thumbnail_dimensions(width: i32, height: i32) -> (i32, i32) {
    let width = width.max(1);
    let height = height.max(1);
    let pixels = i64::from(width) * i64::from(height);
    if pixels <= THUMBNAIL_MAX_PIXELS {
        return (width, height);
    }
    let scale = (THUMBNAIL_MAX_PIXELS as f64 / pixels as f64).sqrt();
    (
        (width as f64 * scale).floor().max(1.0) as i32,
        (height as f64 * scale).floor().max(1.0) as i32,
    )
}

fn bounded_row_interests(
    requests: impl IntoIterator<Item = ThumbnailRequest>,
) -> Vec<ThumbnailRequest> {
    requests
        .into_iter()
        .take(THUMBNAIL_ROW_INTEREST_LIMIT)
        .collect()
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        FileIdentity {}
    }
}

fn modification_stamp(time: Option<SystemTime>) -> ModificationStamp {
    let duration = time
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .unwrap_or(Duration::ZERO);
    ModificationStamp {
        seconds: duration.as_secs().min(i64::MAX as u64) as i64,
        nanoseconds: duration.subsec_nanos(),
    }
}

fn palette_swatch(palette: Option<&Palette>) -> gtk4::DrawingArea {
    let colors = palette.map(Palette::colors).unwrap_or_default();
    let swatch = gtk4::DrawingArea::new();
    swatch.add_css_class(classes::CHOOSER_SWATCH);
    swatch.set_content_width(160);
    swatch.set_content_height(48);
    swatch.set_draw_func(move |_, cr, width, height| {
        let colors = if colors.is_empty() {
            vec!["#777777".to_string()]
        } else {
            colors.clone()
        };
        let stripe = f64::from(width) / colors.len() as f64;
        for (index, color) in colors.iter().enumerate() {
            if let Some(color) = parse_hex_color(color) {
                cr.set_source_rgb(
                    f64::from(color.r) / 255.0,
                    f64::from(color.g) / 255.0,
                    f64::from(color.b) / 255.0,
                );
                cr.rectangle(index as f64 * stripe, 0.0, stripe.ceil(), f64::from(height));
                let _ = cr.fill();
            }
        }
    });
    swatch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, label: &str, subtitle: Option<&str>) -> Candidate {
        Candidate {
            id: id.to_string(),
            label: label.to_string(),
            subtitle: subtitle.map(str::to_owned),
            icon: None,
            preview: None,
            palette: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn fuzzy_filter_keeps_script_order() {
        let candidates = vec![
            candidate("blue-moon", "Blue Moon", None),
            candidate("night", "Night", Some("blue palette")),
            candidate("ocean", "Ocean", None),
        ];

        // Both first two match as a fuzzy subsequence.  The chooser uses the
        // shared matcher but does not re-sort script-provided choices.
        assert_eq!(matching_indices(&candidates, "blu"), vec![0, 1]);
    }

    #[test]
    fn fuzzy_filter_matches_tag_names_and_aliases_without_metadata() {
        let mut candidates: Vec<Candidate> = serde_json::from_str(
            r#"[{"id":"one","label":"First"},{"id":"two","label":"Second","tags":[]}]"#,
        )
        .expect("tagless inputs remain valid");
        candidates[0].tags = vec!["Blue Sky".into(), "azure heavens".into()];
        assert_eq!(matching_indices(&candidates, "bsk"), vec![0]);
        assert_eq!(matching_indices(&candidates, "azhv"), vec![0]);
        assert_eq!(
            matching_indices(&candidates, "mountain"),
            Vec::<usize>::new()
        );
        assert_eq!(matching_indices(&candidates, ""), vec![0, 1]);
    }

    #[test]
    fn selection_stays_with_its_stable_id_across_filter_updates() {
        let candidates = vec![
            candidate("same-label-a", "Same", None),
            candidate("same-label-b", "Same", None),
        ];
        let visible = vec![0, 1];
        assert_eq!(
            retained_selection(&candidates, &visible, Some("same-label-b")),
            Some("same-label-b".to_string())
        );
        assert_eq!(
            retained_selection(&candidates, &[0], Some("same-label-b")),
            Some("same-label-a".to_string())
        );
    }

    #[test]
    fn random_wallpaper_only_chooses_visible_previewable_rows() {
        let mut candidates = vec![
            candidate("ocean-one", "Ocean One", None),
            candidate("mountain", "Mountain", None),
            candidate("ocean-no-preview", "Ocean Unavailable", None),
            candidate("ocean-two", "Ocean Two", None),
        ];
        candidates[0].preview = Some(PathBuf::from("/wallpapers/one.png"));
        candidates[1].preview = Some(PathBuf::from("/wallpapers/mountain.png"));
        candidates[3].preview = Some(PathBuf::from("/wallpapers/two.png"));
        let visible = matching_indices(&candidates, "ocean");
        assert_eq!(random_wallpaper_index(&candidates, &[], None), None);
        assert_eq!(
            random_wallpaper_index(
                &candidates,
                &matching_indices(&candidates, "Unavailable"),
                None,
            ),
            None
        );
        assert_eq!(
            random_wallpaper_index(&candidates, &[3], Some("ocean-two")),
            Some(3)
        );
        assert_eq!(
            random_wallpaper_index(&candidates, &visible, Some("ocean-one")),
            Some(3)
        );
        assert_eq!(
            random_wallpaper_index(&candidates, &visible, Some("ocean-two")),
            Some(0)
        );
        assert!(matches!(
            random_wallpaper_index(&candidates, &visible, None),
            Some(0 | 3)
        ));
    }

    #[test]
    fn selected_row_scrolls_using_content_coordinates() {
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

    #[test]
    fn discarded_preview_refs_do_not_accumulate_or_remove_live_rows() {
        let old = request(1);
        let next = request(2);
        let row: glib::Object = glib::Object::new();
        let preview: glib::Object = glib::Object::new();
        let next_preview: glib::Object = glib::Object::new();
        let mut refs = HashMap::from([
            (old.clone(), vec![row.downgrade(), preview.downgrade()]),
            (next.clone(), vec![next_preview.downgrade()]),
        ]);

        drop(preview);
        prune_dead_thumbnail_refs(&mut refs, &old);
        assert_eq!(refs[&old].len(), 1);
        assert!(refs[&old][0].upgrade().is_some());
        drop(row);
        prune_dead_thumbnail_refs(&mut refs, &old);
        assert!(!refs.contains_key(&old));
        assert!(refs[&next][0].upgrade().is_some());
    }

    #[test]
    fn wallpaper_geometry_keeps_four_rows_before_a_preview() {
        let geometry = chooser_geometry(ChooseLayout::Wallpapers, 99, 900);
        assert_eq!(geometry.scroll_height, 480);
        assert_eq!(geometry.preview_height, Some(210));
    }

    #[test]
    fn wallpaper_preview_fills_dialog_content_and_keeps_decode_detail() {
        let (width, height, decode_height) = wallpaper_preview_dimensions(1040, 210);
        assert_eq!(width, 1008);
        assert_eq!(height, 210);
        assert_eq!(decode_height, 567);

        let (width, height, decode_height) = wallpaper_preview_dimensions(680, 240);
        assert_eq!((width, height, decode_height), (648, 240, 364));
    }

    #[test]
    fn compact_wallpaper_geometry_hides_preview_before_rows() {
        let geometry = chooser_geometry(ChooseLayout::Wallpapers, 99, 720);
        assert_eq!(geometry.scroll_height, 480);
        assert_eq!(geometry.preview_height, None);
    }

    #[test]
    fn theme_geometry_reserves_four_complete_rows() {
        let geometry = chooser_geometry(ChooseLayout::Themes, 99, 800);
        assert_eq!(geometry.scroll_height, 396);
        assert_eq!(geometry.preview_height, Some(194));
        assert_eq!(
            geometry.scroll_height,
            MIN_VISIBLE_ROWS * THEME_ROW_HEIGHT + (MIN_VISIBLE_ROWS - 1) * RESULT_ROW_GAP
        );
    }

    #[test]
    fn thumbnail_key_changes_when_source_modification_state_changes() {
        let identity = FileIdentity {
            #[cfg(unix)]
            device: 4,
            #[cfg(unix)]
            inode: 8,
        };
        let base = ThumbnailKey {
            path: PathBuf::from("/wallpapers/one.png"),
            identity,
            modified: ModificationStamp {
                seconds: 10,
                nanoseconds: 1,
            },
            #[cfg(unix)]
            changed: ModificationStamp {
                seconds: 10,
                nanoseconds: 1,
            },
            bytes: 100,
            width: 176,
            height: 99,
        };
        let mut changed = base.clone();
        changed.modified.nanoseconds = 2;
        assert_ne!(base, changed);
        let mut replaced = base.clone();
        replaced.identity = FileIdentity {
            #[cfg(unix)]
            device: 4,
            #[cfg(unix)]
            inode: 9,
        };
        assert_ne!(base, replaced);
        #[cfg(unix)]
        {
            let mut changed_ctime = base.clone();
            changed_ctime.changed.nanoseconds = 2;
            assert_ne!(base, changed_ctime);
        }
    }

    #[test]
    fn wallpaper_activation_requires_a_decoded_preview() {
        assert!(!preview_allows_apply(PreviewStatus::Loading));
        assert!(!preview_allows_apply(PreviewStatus::Failed));
        assert!(preview_allows_apply(PreviewStatus::Ready));
    }

    #[test]
    fn corrupt_wallpaper_reports_decode_failure() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "topbar-corrupt-wallpaper-{}-{unique}.png",
            std::process::id()
        ));
        fs::write(&path, b"this is not a PNG image").expect("write corrupt fixture");
        let result = decode_thumbnail(&path, 176, 99);
        fs::remove_file(&path).expect("remove corrupt fixture");
        assert!(
            matches!(result, DecodeOutcome::Failed { .. }),
            "corrupt image must not become selectable"
        );
    }

    fn request(index: usize) -> ThumbnailRequest {
        ThumbnailRequest {
            path: PathBuf::from(format!("/wallpapers/{index}.png")),
            width: 176,
            height: 99,
        }
    }

    fn key(index: usize) -> ThumbnailKey {
        ThumbnailKey {
            path: PathBuf::from(format!("/wallpapers/{index}.png")),
            identity: FileIdentity {
                #[cfg(unix)]
                device: 1,
                #[cfg(unix)]
                inode: index as u64,
            },
            modified: ModificationStamp {
                seconds: index as i64,
                nanoseconds: 1,
            },
            #[cfg(unix)]
            changed: ModificationStamp {
                seconds: index as i64,
                nanoseconds: 1,
            },
            bytes: 1,
            width: 176,
            height: 99,
        }
    }

    #[test]
    fn scheduler_prioritizes_selected_preview_and_limits_workers() {
        let mut scheduler = ThumbnailScheduler::default();
        let preview = ThumbnailRequest {
            path: PathBuf::from("/wallpapers/selected.png"),
            width: 1000,
            height: 560,
        };
        let mut interests = vec![preview.clone()];
        interests.extend((0..THUMBNAIL_ROW_INTEREST_LIMIT).map(request));
        scheduler.synchronize(interests);

        let first = scheduler.next_job().expect("selected preview job");
        let second = scheduler.next_job().expect("first row job");
        assert_eq!(first.request, preview);
        assert_eq!(second.request, request(0));
        assert!(scheduler.next_job().is_none());
        assert_eq!(scheduler.active, THUMBNAIL_ACTIVE_LIMIT);
    }

    #[test]
    fn superseded_interests_drop_queued_work_and_late_completions() {
        let mut scheduler = ThumbnailScheduler::default();
        let old = request(1);
        scheduler.synchronize(vec![old.clone(), request(2)]);
        let active = scheduler.next_job().expect("old request starts");
        scheduler.synchronize(vec![request(90)]);

        assert!(scheduler.complete(&active).is_none());
        let replacement = scheduler.next_job().expect("new request starts");
        assert_eq!(replacement.request, request(90));
        assert_eq!(scheduler.requests.len(), 1);
    }

    #[test]
    fn large_wallpaper_sets_quiesce_after_the_bounded_visible_interest_set() {
        let mut scheduler = ThumbnailScheduler::default();
        // A 100-entry chooser must never turn completion into a request for
        // every historical row.  It contains only the current 24 row
        // interests, and once each completes there is no decode loop.
        scheduler.synchronize(bounded_row_interests((0..100).map(request)));
        let mut jobs = Vec::new();
        while let Some(job) = scheduler.next_job() {
            jobs.push(job);
            if jobs.len() == THUMBNAIL_ACTIVE_LIMIT {
                for job in jobs.drain(..) {
                    *scheduler.complete(&job).expect("current job") =
                        ThumbnailRequestState::Ready(ThumbnailKey {
                            path: job.request.path.clone(),
                            identity: FileIdentity {
                                #[cfg(unix)]
                                device: 1,
                                #[cfg(unix)]
                                inode: 1,
                            },
                            modified: ModificationStamp {
                                seconds: 1,
                                nanoseconds: 1,
                            },
                            #[cfg(unix)]
                            changed: ModificationStamp {
                                seconds: 1,
                                nanoseconds: 1,
                            },
                            bytes: 1,
                            width: job.request.width,
                            height: job.request.height,
                        });
                }
            }
        }
        for job in jobs {
            *scheduler.complete(&job).expect("current job") =
                ThumbnailRequestState::Failed("x".to_string());
        }
        assert_eq!(scheduler.requests.len(), THUMBNAIL_ROW_INTEREST_LIMIT);
        assert!(scheduler.next_job().is_none());
    }

    #[test]
    fn cache_evicts_historical_failures_after_sixty_four_entries() {
        let mut cache = ThumbnailCache::default();
        for index in 0..(THUMBNAIL_CACHE_LIMIT + 8) {
            cache.insert(DecodedThumbnail {
                key: key(index),
                value: CachedThumbnail::Failed("unreadable".to_string()),
                bytes: 0,
            });
        }
        assert_eq!(cache.entries.len(), THUMBNAIL_CACHE_LIMIT);
        assert!(cache.get(&key(0)).is_none());
        assert!(cache.get(&key(THUMBNAIL_CACHE_LIMIT + 7)).is_some());
    }

    #[test]
    fn selected_preview_stays_cached_past_the_entry_limit() {
        let mut cache = ThumbnailCache::default();
        let selected = key(0);
        cache.set_pinned(Some(selected.clone()));
        for index in 0..(THUMBNAIL_CACHE_LIMIT + 8) {
            cache.insert(DecodedThumbnail {
                key: key(index),
                value: CachedThumbnail::Failed("unreadable".to_string()),
                bytes: 0,
            });
        }
        assert!(cache.get(&selected).is_some());
        assert_eq!(cache.entries.len(), THUMBNAIL_CACHE_LIMIT);
    }

    #[test]
    fn retained_completed_interest_reprobes_once_without_hiding_its_result() {
        let mut scheduler = ThumbnailScheduler::default();
        let request = request(1);
        scheduler.synchronize(vec![request.clone()]);
        let initial = scheduler.next_job().expect("initial probe");
        *scheduler.complete(&initial).expect("current request") =
            ThumbnailRequestState::Ready(key(1));

        scheduler.synchronize(vec![request.clone()]);
        assert!(matches!(
            scheduler.state(&request),
            Some(ThumbnailRequestState::Revalidating(
                ThumbnailStableState::Ready(_)
            ))
        ));
        let recheck = scheduler.next_job().expect("asynchronous recheck");
        assert_eq!(recheck.kind, ThumbnailJobKind::Probe);
        scheduler.synchronize(vec![request.clone()]);
        assert!(
            scheduler.pending.is_empty(),
            "must not enqueue a duplicate probe"
        );
        *scheduler.complete(&recheck).expect("current request") =
            ThumbnailRequestState::Failed("repaired source needs decode".to_string());

        scheduler.synchronize(vec![request.clone()]);
        assert!(matches!(
            scheduler.state(&request),
            Some(ThumbnailRequestState::Revalidating(
                ThumbnailStableState::Failed(_)
            ))
        ));
    }
    #[test]
    fn wallpaper_tabs_defer_search_cache_results_and_ignore_late_searches() {
        let mut state = WallpaperState {
            executable: PathBuf::from("/provider"),
            presets: vec![
                WallpaperPreset {
                    id: "nature".into(),
                    label: "Nature".into(),
                },
                WallpaperPreset {
                    id: "mountains".into(),
                    label: "Mountains".into(),
                },
            ],
            pool: Vec::new(),
            cached: HashMap::new(),
            active: None,
            last_preset: None,
            pending: None,
            generation: 0,
        };
        let mut rows = vec![candidate("pool", "Local", None)];
        assert_eq!(state.searching(String::new()), None, "Pool never searches");
        assert!(state.switch(Some("nature"), &mut rows));
        assert!(rows.is_empty());
        let (_, first) = state
            .searching(String::new())
            .expect("first Wallhaven activation searches");
        assert!(state.blocks_selection());
        assert!(
            state.switch(None, &mut rows),
            "Pool remains accessible during search"
        );
        assert_eq!(rows[0].id, "pool");
        assert!(!state.blocks_selection(), "Pool stays actionable");
        assert!(matches!(
            state.pending,
            Some(WallpaperPending::Search { .. })
        ));
        assert!(state.switch(Some("nature"), &mut rows));
        assert!(rows.is_empty());
        assert!(
            state.blocks_selection(),
            "returning to a pending search shows the spinner"
        );
        assert_eq!(
            state.searching(String::new()),
            None,
            "reentry must not launch a duplicate"
        );
        assert!(state.switch(None, &mut rows));
        assert!(state.complete(&WallpaperPending::Search {
            preset: "nature".into(),
            query: String::new(),
            generation: first
        }));
        assert!(
            state
                .display_or_cache("nature", vec![candidate("9d82vk", "Fetched", None)])
                .is_none()
        );
        assert_eq!(
            rows[0].id, "pool",
            "completed search must not replace Pool rows"
        );
        assert!(!state.blocks_selection());
        assert!(state.switch(Some("nature"), &mut rows));
        assert_eq!(rows[0].id, "9d82vk", "return uses cached results");
        assert!(!state.blocks_selection());
        state.pending = Some(WallpaperPending::Save {
            preset: "nature".into(),
            generation: first,
        });
        assert!(
            !state.switch(None, &mut rows),
            "save disables Pool and Cancel until complete"
        );
        assert_eq!(
            state.searching("+new query".into()),
            None,
            "saving blocks query submission"
        );
        assert!(
            matches!(state.pending, Some(WallpaperPending::Save { .. })),
            "save keeps its spinner while the provider is running"
        );
        assert!(state.complete(&WallpaperPending::Save {
            preset: "nature".into(),
            generation: first
        }));
        assert_eq!(state.active.as_deref(), Some("nature"));
        assert!(state.switch(Some("mountains"), &mut rows));
        assert!(rows.is_empty());
        let (_, superseded) = state.searching(String::new()).expect("new preset searches");
        assert!(state.switch(Some("nature"), &mut rows));
        assert_eq!(rows[0].id, "9d82vk");
        assert!(!state.complete(&WallpaperPending::Search {
            preset: "mountains".into(),
            query: String::new(),
            generation: superseded
        }));
        assert_eq!(
            rows[0].id, "9d82vk",
            "stale search cannot clobber current rows"
        );
        assert!(state.switch(Some("mountains"), &mut rows));
        let (_, current) = state
            .searching(String::new())
            .expect("superseded preset searches on reentry");
        assert!(!state.complete(&WallpaperPending::Search {
            preset: "mountains".into(),
            query: String::new(),
            generation: superseded
        }));
        assert!(
            state.blocks_selection(),
            "stale completion cannot hide the spinner"
        );
        assert!(state.complete(&WallpaperPending::Search {
            preset: "mountains".into(),
            query: String::new(),
            generation: current
        }));
        rows = state
            .display_or_cache("mountains", vec![candidate("mountain", "Mountain", None)])
            .expect("active preset displays results");
        assert!(state.switch(None, &mut rows));
        assert_eq!(state.preferred_preset(), Some("mountains"));
        let preset = state.preferred_preset().unwrap().to_owned();
        assert!(state.switch(Some(&preset), &mut rows));
        assert_eq!(rows[0].id, "mountain", "last preset uses cached results");
        let query_a = "+nature -car @someone type:png".to_string();
        let query_b = "like:9d82vk".to_string();
        let (_, first_query) = state.searching(query_a.clone()).expect("submit query A");
        rows.clear();
        assert!(
            state.blocks_selection(),
            "old results cannot be saved during a new query"
        );
        let (_, latest_query) = state
            .searching(query_b.clone())
            .expect("query B supersedes A");
        assert!(!state.complete(&WallpaperPending::Search {
            preset: "mountains".into(),
            query: query_a.clone(),
            generation: first_query,
        }));
        assert!(
            !state.complete(&WallpaperPending::Search {
                preset: "mountains".into(),
                query: query_a,
                generation: latest_query,
            }),
            "a different query cannot complete the latest request"
        );
        assert!(state.blocks_selection());
        assert!(state.complete(&WallpaperPending::Search {
            preset: "mountains".into(),
            query: query_b.clone(),
            generation: latest_query,
        }));
        rows = state
            .display_or_cache("mountains", vec![candidate("latest", "Latest", None)])
            .expect("latest results display");
        assert!(state.switch(None, &mut rows));
        assert_eq!(rows[0].id, "pool", "queries never replace Local rows");
        assert!(state.switch(Some("mountains"), &mut rows));
        assert_eq!(rows[0].id, "latest");
        assert_eq!(state.current_search().unwrap().query, query_b);
        assert!(
            !state.needs_search(),
            "returning to results does not submit again"
        );

        let failed_query = "+nebula -people".to_string();
        let (_, failed_generation) = state.searching(failed_query.clone()).expect("new query");
        rows.clear();
        assert!(state.switch(None, &mut rows));
        assert!(state.complete(&WallpaperPending::Search {
            preset: "mountains".into(),
            query: failed_query.clone(),
            generation: failed_generation,
        }));
        assert_eq!(state.search_failed("mountains", "HTTP 429".into()), None);
        assert!(state.switch(Some("mountains"), &mut rows));
        assert!(
            rows.is_empty(),
            "failed new query cannot revive old results"
        );
        assert_eq!(state.retry_query(), Some(failed_query.as_str()));
        assert!(
            !state.needs_search(),
            "failed queries wait for explicit retry"
        );
        let retry_query = state.retry_query().unwrap().to_owned();
        let (_, retry_generation) = state
            .searching(retry_query.clone())
            .expect("retry submitted query");
        assert!(state.complete(&WallpaperPending::Search {
            preset: "mountains".into(),
            query: retry_query,
            generation: retry_generation,
        }));
        rows = state
            .display_or_cache("mountains", Vec::new())
            .expect("empty query completes");
        assert_eq!(state.retry_query(), None);
        assert!(state.switch(None, &mut rows));
        assert!(state.switch(Some("mountains"), &mut rows));
        assert!(
            !state.needs_search(),
            "empty results are cached, not repeatedly requested"
        );
        assert!(rows.is_empty());
        assert_eq!(
            state.cached.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from(["nature".to_string(), "mountains".to_string()])
        );
    }
}
