//! Filename discovery and matching for the launcher.
//!
//! The catalog is deliberately memory-only. `fd` keeps its normal ignore-file
//! behaviour, while this service adds the few directories which are never
//! useful launcher results and very often contain secrets or enormous build
//! trees. Paths cross the service boundary as [`PathBuf`] values, retaining
//! their original Unix bytes for activation; the text used for display is
//! derived only when a caller needs it.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::ffi::{OsStr, OsString};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcessCommand;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::error::SvcError;

/// Maximum number of discovered paths retained in memory.
pub const MAX_PATHS: usize = 500_000;
/// Maximum number of original pathname bytes retained in memory.
pub const MAX_PATH_BYTES: usize = 64 * 1024 * 1024;
/// Number of file results a query returns.
pub const RESULT_LIMIT: usize = 30;

/// The time a launcher query waits for another keystroke before it is ranked.
const QUERY_DEBOUNCE: Duration = Duration::from_millis(75);
/// Opening the launcher refreshes a catalog at least this old.
const OPEN_REFRESH_AGE: Duration = Duration::from_secs(2 * 60);
/// A completed catalog is refreshed at this cadence even when the launcher is closed.
const PERIODIC_REFRESH: Duration = Duration::from_secs(10 * 60);
/// The first non-empty partial snapshot after the immediate first result.
///
/// Later snapshots double this threshold. Publishing every fixed-size batch
/// copies the whole growing `Vec` each time and becomes quadratic at the
/// catalog cap; exponential snapshots copy fewer than twice the final entry
/// count while still making a cold launcher useful immediately.
const FIRST_BATCH_AFTER_INITIAL: usize = 128;

/// Directories that must never be catalogued.
///
/// These are `fd --exclude` patterns, kept together so configuration only ever
/// extends one reviewable, test-covered definition. They cover secret stores,
/// caches, browser profiles, Trash, package caches, and generated trees.
pub const BUILTIN_EXCLUSIONS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".password-store",
    ".local/share/keyrings",
    ".config/gnome-keyring",
    ".cache",
    ".local/share/Trash",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".nix-defexpr",
    ".nix-profile",
    ".local/state/nix",
    ".cache/nix",
    ".cache/guix",
    ".guix-profile",
    ".cargo/registry",
    ".cargo/git",
    ".npm",
    ".local/share/pnpm",
    ".cache/pip",
    ".direnv",
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "env",
    ".tox",
    "__pycache__",
];

/// Launcher file-search settings after configuration has been parsed.
///
/// Roots may begin with `~`; it is expanded at this service boundary so UI
/// code never has to reinterpret a path. Exclusions are `fd` glob patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchConfig {
    roots: Vec<PathBuf>,
    exclusions: Vec<String>,
}

impl FileSearchConfig {
    /// Make a configuration from launcher roots and additional `fd` exclusions.
    pub fn new(roots: Vec<PathBuf>, exclusions: Vec<String>) -> Self {
        Self { roots, exclusions }
    }

    /// The configured roots before `~` expansion.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The configured exclusions, in addition to [`BUILTIN_EXCLUSIONS`].
    pub fn exclusions(&self) -> &[String] {
        &self.exclusions
    }

    fn resolved(&self) -> Result<ResolvedConfig, &'static str> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut roots = Vec::with_capacity(self.roots.len());
        for root in &self.roots {
            roots.push(expand_home(root, home.as_deref())?);
        }
        Ok(ResolvedConfig {
            roots,
            exclusions: self.exclusions.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedConfig {
    roots: Vec<PathBuf>,
    exclusions: Vec<String>,
}

/// One file in the catalog.
///
/// `PathBuf` retains the original filesystem bytes on Unix. The display text
/// replaces invalid UTF-8 rather than making a path impossible to select.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileEntry {
    path: PathBuf,
}

impl FileEntry {
    /// Retain an original pathname for search and activation.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The original path, suitable for a GIO open or reveal operation.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Safe, lossy text for a launcher row.
    pub fn display_path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// A concise home-relative display and search path when possible.
    pub fn home_relative_path(&self) -> String {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return self.display_path();
        };
        self.path
            .strip_prefix(&home)
            .map(|relative| format!("~/{}", relative.to_string_lossy()))
            .unwrap_or_else(|_| self.display_path())
    }

    /// Safe, lossy basename text for a launcher row.
    pub fn basename(&self) -> String {
        self.path
            .file_name()
            .unwrap_or_else(|| self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    /// A stable identity which keeps the original Unix pathname bytes.
    pub fn identity(&self) -> Vec<u8> {
        path_bytes(&self.path)
    }
}

/// One ranked file result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatch {
    /// The file to display and activate.
    pub entry: FileEntry,
    /// Higher scores sort before lower scores.
    pub score: i64,
    /// Character offsets matched in the basename, for highlighting.
    pub basename_matches: Vec<usize>,
    /// Character offsets matched in the display path, for highlighting.
    pub path_matches: Vec<usize>,
    /// Filesystem modification time, looked up only for ranked results off GTK.
    pub modified: Option<SystemTime>,
    /// File size in bytes, if the file still exists when the search completes.
    pub size: Option<u64>,
}

/// A deterministic fuzzy match suitable for every launcher search surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Higher scores sort before lower scores.
    pub score: i64,
    /// Character offsets in the candidate, suitable for text highlighting.
    pub positions: Vec<usize>,
}

/// The immutable state published to launcher consumers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSearchState {
    /// The current in-memory catalog. It is never written to disk.
    pub entries: Arc<Vec<FileEntry>>,
    /// Changes only when the visible catalog entries change.
    ///
    /// Consumers use this to avoid re-ranking a query for progress and
    /// warning-only updates while a warm refresh is walking in the background.
    pub catalog_revision: u64,
    /// Whether `fd` workers are currently discovering paths.
    pub discovering: bool,
    /// Whether the catalog stopped because one of its storage bounds was met.
    pub partial: bool,
    /// A section-local message for the Files search section.
    pub warning: Option<String>,
    /// When a catalog last completed successfully.
    pub completed_at: Option<SystemTime>,
    /// The pathname bytes held by [`Self::entries`].
    pub path_bytes: usize,
}

/// The asynchronous file catalog service.
#[derive(Clone)]
pub struct FileSearch {
    state: watch::Receiver<Arc<FileSearchState>>,
    commands: mpsc::Sender<Command>,
}

impl FileSearch {
    /// Start discovery immediately with `fd` from `PATH`.
    pub fn start(config: FileSearchConfig) -> Self {
        Self::with_program(config, PathBuf::from("fd"))
    }

    /// Start discovery with an explicit `fd` executable.
    ///
    /// This is primarily a fixture seam. Production callers use [`Self::start`].
    pub fn with_program(config: FileSearchConfig, program: PathBuf) -> Self {
        let (publisher, state) = watch::channel(Arc::new(FileSearchState::default()));
        let (commands, queue) = mpsc::channel(16);
        tokio::spawn(run(publisher, config, program, queue));
        Self { state, commands }
    }

    /// Subscribe to catalog updates.
    pub fn state(&self) -> watch::Receiver<Arc<FileSearchState>> {
        self.state.clone()
    }

    /// Get the catalog as it stands now.
    pub fn current(&self) -> Arc<FileSearchState> {
        self.state.borrow().clone()
    }

    /// A handle that can be held by the launcher without retaining the service.
    pub fn handle(&self) -> FileSearchHandle {
        FileSearchHandle {
            commands: self.commands.clone(),
        }
    }

    /// Tell discovery that the launcher opened.
    pub async fn opened(&self) -> Result<(), SvcError> {
        self.handle().opened().await
    }

    /// Tell discovery that the machine resumed.
    pub async fn resumed(&self) -> Result<(), SvcError> {
        self.handle().resumed().await
    }

    /// Replace roots and exclusions, cancelling the old discovery first.
    pub async fn configure(&self, config: FileSearchConfig) -> Result<(), SvcError> {
        self.handle().configure(config).await
    }
}

/// Commands the launcher sends to the file-search service.
#[derive(Clone)]
pub struct FileSearchHandle {
    commands: mpsc::Sender<Command>,
}

impl FileSearchHandle {
    /// Refresh on launcher opening when the completed catalog is stale.
    pub async fn opened(&self) -> Result<(), SvcError> {
        self.send(Command::Opened).await
    }

    /// Refresh immediately after resume.
    pub async fn resumed(&self) -> Result<(), SvcError> {
        self.send(Command::Refresh).await
    }

    /// Refresh immediately, irrespective of catalog age.
    pub async fn refresh(&self) -> Result<(), SvcError> {
        self.send(Command::Refresh).await
    }

    /// Replace roots and exclusions, cancelling the current workers first.
    pub async fn configure(&self, config: FileSearchConfig) -> Result<(), SvcError> {
        self.send(Command::Configure(config)).await
    }

    /// Rank a query on the service runtime after the launcher debounce.
    ///
    /// A newer query supersedes an older pending query. Its caller receives an
    /// empty result, which lets UI code ignore stale responses by request id.
    pub async fn search(&self, query: String) -> Result<Vec<FileMatch>, SvcError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Search { query, reply }).await?;
        answer
            .await
            .map_err(|_| SvcError::ServiceStopped("file search"))
    }

    async fn send(&self, command: Command) -> Result<(), SvcError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| SvcError::ServiceStopped("file search"))
    }
}

enum Command {
    Opened,
    Refresh,
    Configure(FileSearchConfig),
    Search {
        query: String,
        reply: oneshot::Sender<Vec<FileMatch>>,
    },
}

struct PendingQuery {
    generation: u64,
    query: String,
    reply: oneshot::Sender<Vec<FileMatch>>,
    due: tokio::time::Instant,
}

struct ActiveQuery {
    generation: u64,
    reply: oneshot::Sender<Vec<FileMatch>>,
}

async fn run(
    publisher: watch::Sender<Arc<FileSearchState>>,
    config: FileSearchConfig,
    program: PathBuf,
    mut commands: mpsc::Receiver<Command>,
) {
    let mut config = config;
    let mut discovery = start_discovery(&publisher, &config, &program).await;
    let mut periodic = tokio::time::interval(PERIODIC_REFRESH);
    // The first discovery has already started above.
    periodic.tick().await;
    let mut pending_query: Option<PendingQuery> = None;
    let mut active_query: Option<ActiveQuery> = None;
    let mut query_generation = 0_u64;
    let (ranked_tx, mut ranked_rx) = mpsc::channel(1);

    loop {
        let query_due = pending_query.as_ref().map(|pending| pending.due);
        let query_timer = async move {
            match query_due {
                Some(due) => tokio::time::sleep_until(due).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(query_timer);

        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Opened) => {
                    if !discovery.as_ref().is_some_and(Discovery::active) && catalog_is_stale(&publisher) {
                        cancel_discovery(&mut discovery).await;
                        discovery = start_discovery(&publisher, &config, &program).await;
                    }
                }
                Some(Command::Refresh) => {
                    cancel_discovery(&mut discovery).await;
                    discovery = start_discovery(&publisher, &config, &program).await;
                }
                Some(Command::Configure(next)) => {
                    if next != config {
                        cancel_discovery(&mut discovery).await;
                        config = next;
                        discovery = start_discovery(&publisher, &config, &program).await;
                    }
                }
                Some(Command::Search { query, reply }) => {
                    if let Some(active) = active_query.take() {
                        let _ = active.reply.send(Vec::new());
                    }
                    query_generation = query_generation.wrapping_add(1);
                    if let Some(previous) = pending_query.replace(PendingQuery {
                        generation: query_generation,
                        query,
                        reply,
                        due: tokio::time::Instant::now() + QUERY_DEBOUNCE,
                    }) {
                        let _ = previous.reply.send(Vec::new());
                    }
                }
                None => break,
            },
            _ = periodic.tick() => {
                if !discovery.as_ref().is_some_and(Discovery::active) {
                    cancel_discovery(&mut discovery).await;
                    discovery = start_discovery(&publisher, &config, &program).await;
                }
            },
            _ = &mut query_timer, if pending_query.is_some() => {
                if let Some(pending) = pending_query.take() {
                    let snapshot = publisher.borrow().clone();
                    let generation = pending.generation;
                    let task = tokio::task::spawn_blocking(move || {
                        let mut matches = rank(snapshot.entries.as_ref(), &pending.query);
                        load_result_metadata(&mut matches);
                        matches
                    });
                    let completed = ranked_tx.clone();
                    tokio::spawn(async move {
                        let matches = task.await.unwrap_or_default();
                        let _ = completed.send((generation, matches)).await;
                    });
                    active_query = Some(ActiveQuery {
                        generation,
                        reply: pending.reply,
                    });
                }
            },
            ranked = ranked_rx.recv() => {
                if let Some((generation, matches)) = ranked
                    && active_query.as_ref().is_some_and(|active| active.generation == generation)
                {
                    let active = active_query.take().expect("current query was active");
                    let _ = active.reply.send(matches);
                }
            },
            event = next_event(&mut discovery) => {
                match event {
                    Some(event) => handle_event(&publisher, &mut discovery, event).await,
                    // A worker task should always report completion. A closed
                    // queue is therefore a failed traversal, and preserving a
                    // previous catalog is safer than repeatedly selecting it.
                    None if discovery.as_ref().is_some_and(Discovery::active) => {
                        if let Some(active) = discovery.as_mut() {
                            active.failed = true;
                            active.remaining = 0;
                        }
                        finish_discovery(&publisher, &mut discovery).await;
                    }
                    None => {}
                }
            },
            _ = publisher.closed() => break,
        }
    }

    cancel_discovery(&mut discovery).await;
    if let Some(pending) = pending_query {
        let _ = pending.reply.send(Vec::new());
    }
    if let Some(active) = active_query {
        let _ = active.reply.send(Vec::new());
    }
}

async fn next_event(discovery: &mut Option<Discovery>) -> Option<WorkerEvent> {
    match discovery {
        Some(discovery) => discovery.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn start_discovery(
    publisher: &watch::Sender<Arc<FileSearchState>>,
    config: &FileSearchConfig,
    program: &Path,
) -> Option<Discovery> {
    let resolved = match config.resolved() {
        Ok(resolved) => resolved,
        Err(reason) => {
            publish(publisher, |state| {
                state.discovering = false;
                state.warning = Some(reason.to_string());
            });
            return None;
        }
    };

    if resolved.roots.is_empty() {
        publish(publisher, |state| {
            state.entries = Arc::new(Vec::new());
            state.path_bytes = 0;
            state.discovering = false;
            state.partial = false;
            state.warning = None;
            state.completed_at = Some(SystemTime::now());
        });
        return None;
    }

    let baseline = publisher.borrow().clone();
    let warm = baseline.completed_at.is_some();
    publish_discovery_start(publisher);

    let (events_tx, events) = mpsc::channel(1_024);
    let (cancel_tx, cancel) = watch::channel(false);
    let root_groups = split_roots(resolved.roots);
    let remaining = root_groups.len();
    let workers = root_groups
        .into_iter()
        .map(|roots| {
            let arguments = fd_arguments(&roots, &resolved.exclusions);
            tokio::spawn(worker(
                program.to_path_buf(),
                arguments,
                events_tx.clone(),
                cancel.clone(),
            ))
        })
        .collect::<Vec<_>>();
    drop(events_tx);

    Some(Discovery {
        events,
        cancel: cancel_tx,
        workers,
        remaining,
        builder: CatalogBuilder::default(),
        baseline,
        warm,
        failed: false,
        partial: false,
        next_publish: FIRST_BATCH_AFTER_INITIAL,
    })
}

/// Partition roots over at most two `fd` processes.
fn split_roots(roots: Vec<PathBuf>) -> Vec<Vec<PathBuf>> {
    let workers = roots.len().min(2);
    if workers == 0 {
        return Vec::new();
    }
    let mut groups = vec![Vec::new(); workers];
    for (index, root) in roots.into_iter().enumerate() {
        groups[index % workers].push(root);
    }
    groups
}

async fn handle_event(
    publisher: &watch::Sender<Arc<FileSearchState>>,
    discovery: &mut Option<Discovery>,
    event: WorkerEvent,
) {
    let finish = {
        let Some(active) = discovery.as_mut() else {
            return;
        };
        match event {
            WorkerEvent::Path(raw) => match active.builder.push(raw) {
                Insert::Added => {
                    let initial = active.builder.entries.len() == 1;
                    if should_publish(active.builder.entries.len(), active.next_publish) {
                        publish_working(publisher, active);
                        if !initial {
                            active.next_publish = next_publish_threshold(active.next_publish);
                        }
                    }
                    false
                }
                Insert::Limit => {
                    active.partial = true;
                    publish_working(publisher, active);
                    true
                }
                Insert::Duplicate => false,
            },
            WorkerEvent::Finished { failed } => {
                active.remaining = active.remaining.saturating_sub(1);
                active.failed |= failed;
                active.remaining == 0
            }
        }
    };
    if finish {
        finish_discovery(publisher, discovery).await;
    }
}

async fn finish_discovery(
    publisher: &watch::Sender<Arc<FileSearchState>>,
    discovery: &mut Option<Discovery>,
) {
    let Some(mut finished) = discovery.take() else {
        return;
    };
    finished.cancel_and_reap().await;

    if finished.failed {
        if finished.warm {
            // A warm refresh never replaces its completed baseline until it
            // succeeds. Keep partial/completed metadata as well: callers can
            // still tell that the retained catalog has limited coverage.
            let baseline = Arc::clone(&finished.baseline);
            let warning = if baseline.partial {
                "File discovery failed; showing the previous partial catalog."
            } else {
                "File discovery failed; showing the previous catalog."
            };
            publish(publisher, move |state| {
                state.entries = Arc::clone(&baseline.entries);
                state.path_bytes = baseline.path_bytes;
                state.partial = baseline.partial;
                state.completed_at = baseline.completed_at;
                state.discovering = false;
                state.warning = Some(warning.to_string());
            });
        } else {
            // Cold discovery has no completed baseline to preserve. Publish
            // the complete discovered tail, including entries after the last
            // geometric batch, but do not present it as a completed catalog.
            // A retry can begin from a prior failed cold tail, though. If the
            // retry fails before discovering anything, that tail is still the
            // only usable catalog and must remain visible.
            if finished.builder.entries.is_empty() && !finished.baseline.entries.is_empty() {
                let baseline = Arc::clone(&finished.baseline);
                let warning = if baseline.partial {
                    "File discovery failed; showing the previous partial catalog."
                } else {
                    "File discovery failed; showing the previous catalog."
                };
                publish(publisher, move |state| {
                    state.entries = Arc::clone(&baseline.entries);
                    state.path_bytes = baseline.path_bytes;
                    state.partial = baseline.partial;
                    state.completed_at = baseline.completed_at;
                    state.discovering = false;
                    state.warning = Some(warning.to_string());
                });
                return;
            }
            let entries = Arc::new(finished.builder.entries);
            let bytes = finished.builder.bytes;
            let partial = finished.partial;
            publish(publisher, move |state| {
                state.entries = entries;
                state.path_bytes = bytes;
                state.discovering = false;
                state.partial = partial;
                state.completed_at = None;
                state.warning = Some(if partial {
                    "File discovery failed after reaching a catalog limit; showing discovered files."
                        .to_string()
                } else {
                    "File discovery failed; showing discovered files.".to_string()
                });
            });
        }
        return;
    }

    // This final replacement is one atomic watch update, including the bounds
    // marker. The incremental snapshots above are only there to make a cold
    // startup searchable before the full traversal completes.
    let entries = Arc::new(finished.builder.entries);
    let bytes = finished.builder.bytes;
    let partial = finished.partial;
    publish(publisher, move |state| {
        state.entries = entries;
        state.path_bytes = bytes;
        state.discovering = false;
        state.partial = partial;
        state.warning = partial.then(|| {
            "File catalog is partial: the 500,000-path or 64 MiB limit was reached.".to_string()
        });
        state.completed_at = Some(SystemTime::now());
    });
}

async fn cancel_discovery(discovery: &mut Option<Discovery>) {
    if let Some(mut active) = discovery.take() {
        active.cancel_and_reap().await;
    }
}

fn publish_working(publisher: &watch::Sender<Arc<FileSearchState>>, discovery: &mut Discovery) {
    if discovery.warm {
        return;
    }
    let entries = Arc::new(discovery.builder.entries.clone());
    let bytes = discovery.builder.bytes;
    let partial = discovery.partial;
    publish(publisher, move |state| {
        state.entries = entries;
        state.path_bytes = bytes;
        state.discovering = true;
        // The first cold batch has replaced any failed tail kept during a
        // retry, so its coverage metadata belongs to this traversal.
        state.partial = partial;
        state.completed_at = None;
    });
}

fn should_publish(entries: usize, next_publish: usize) -> bool {
    entries == 1 || entries >= next_publish
}

fn next_publish_threshold(current: usize) -> usize {
    current.saturating_mul(2)
}

fn catalog_is_stale(publisher: &watch::Sender<Arc<FileSearchState>>) -> bool {
    publisher.borrow().completed_at.is_none_or(|completed| {
        completed
            .elapsed()
            .map_or(true, |age| age >= OPEN_REFRESH_AGE)
    })
}

/// Mark the beginning of a traversal without disturbing a completed catalog.
///
/// A warm refresh keeps its completed baseline until a successful final swap.
/// A cold retry also keeps a failed tail until its first replacement batch, so
/// a transient second failure never turns useful results into an empty list.
fn publish_discovery_start(publisher: &watch::Sender<Arc<FileSearchState>>) {
    publish(publisher, |state| {
        state.discovering = true;
        state.warning = None;
    });
}

fn publish(
    publisher: &watch::Sender<Arc<FileSearchState>>,
    edit: impl FnOnce(&mut FileSearchState),
) {
    publisher.send_if_modified(|current| {
        let mut next = (**current).clone();
        edit(&mut next);
        if next.entries != current.entries {
            next.catalog_revision = current.catalog_revision.wrapping_add(1);
        }
        if **current == next {
            false
        } else {
            *current = Arc::new(next);
            true
        }
    });
}

struct Discovery {
    events: mpsc::Receiver<WorkerEvent>,
    cancel: watch::Sender<bool>,
    workers: Vec<JoinHandle<()>>,
    remaining: usize,
    builder: CatalogBuilder,
    /// The last completed snapshot. Warm failures retain it exactly apart
    /// from progress and warning metadata.
    baseline: Arc<FileSearchState>,
    /// A completed catalog exists and this traversal must stage privately.
    warm: bool,
    failed: bool,
    partial: bool,
    next_publish: usize,
}

impl Discovery {
    fn active(&self) -> bool {
        self.remaining > 0
    }

    async fn cancel_and_reap(&mut self) {
        let _ = self.cancel.send(true);
        for worker in self.workers.drain(..) {
            let _ = worker.await;
        }
    }
}

enum WorkerEvent {
    Path(Vec<u8>),
    Finished { failed: bool },
}

async fn worker(
    program: PathBuf,
    arguments: Vec<OsString>,
    sender: mpsc::Sender<WorkerEvent>,
    mut cancelled: watch::Receiver<bool>,
) {
    let mut child = match ProcessCommand::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            let _ = sender.send(WorkerEvent::Finished { failed: true }).await;
            return;
        }
    };

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stderr_drain = tokio::spawn(drain(stderr));
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    let mut cancelled_now = *cancelled.borrow();
    let mut stream_failed = false;

    while !cancelled_now {
        tokio::select! {
            changed = cancelled.changed() => {
                cancelled_now = changed.is_err() || *cancelled.borrow();
            }
            read = stdout.read(&mut chunk) => match read {
                Ok(0) => break,
                Err(_) => {
                    stream_failed = true;
                    break;
                }
                Ok(read) => {
                    bytes.extend_from_slice(&chunk[..read]);
                    while let Some(end) = bytes.iter().position(|byte| *byte == 0) {
                        let path = bytes.drain(..=end).collect::<Vec<_>>();
                        if path.len() > 1 {
                            tokio::select! {
                                changed = cancelled.changed() => {
                                    cancelled_now = changed.is_err() || *cancelled.borrow();
                                    if cancelled_now { break; }
                                }
                                sent = sender.send(WorkerEvent::Path(path[..path.len() - 1].to_vec())) => {
                                    if sent.is_err() { cancelled_now = true; break; }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if cancelled_now {
        let _ = child.kill().await;
    }
    let status = child.wait().await;
    let _ = stderr_drain.await;
    if !cancelled_now {
        let failed = stream_failed || !matches!(status, Ok(status) if status.success());
        let _ = sender.send(WorkerEvent::Finished { failed }).await;
    }
}

async fn drain(mut reader: impl tokio::io::AsyncRead + Unpin) {
    let mut chunk = [0_u8; 8 * 1024];
    while matches!(reader.read(&mut chunk).await, Ok(read) if read != 0) {}
}

/// Arguments for one `fd` worker. Kept pure so the traversal contract is tested
/// without needing the developer's home directory.
fn fd_arguments(roots: &[PathBuf], additional_exclusions: &[String]) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("--hidden"),
        OsString::from("--type"),
        OsString::from("f"),
        OsString::from("--print0"),
        OsString::from("--glob"),
        OsString::from("*"),
    ];
    for exclusion in BUILTIN_EXCLUSIONS
        .iter()
        .copied()
        .chain(additional_exclusions.iter().map(String::as_str))
    {
        arguments.push(OsString::from("--exclude"));
        arguments.push(OsString::from(exclusion));
    }
    arguments.extend(
        roots
            .iter()
            .map(|root| root.as_os_str())
            .map(OsString::from),
    );
    arguments
}

#[derive(Default)]
struct CatalogBuilder {
    entries: Vec<FileEntry>,
    /// Hash buckets point at retained paths instead of retaining a second copy
    /// of every pathname just to detect overlapping configured roots.
    seen: HashMap<u64, usize>,
    /// Only true hash collisions allocate a second bucket entry.
    collisions: HashMap<u64, Vec<usize>>,
    bytes: usize,
}

enum Insert {
    Added,
    Duplicate,
    Limit,
}

impl CatalogBuilder {
    fn push(&mut self, raw: Vec<u8>) -> Insert {
        let fingerprint = path_fingerprint(&raw);
        if let Some(&first) = self.seen.get(&fingerprint)
            && (path_equals_bytes(self.entries[first].path(), &raw)
                || self.collisions.get(&fingerprint).is_some_and(|indices| {
                    indices
                        .iter()
                        .any(|&index| path_equals_bytes(self.entries[index].path(), &raw))
                }))
        {
            return Insert::Duplicate;
        }
        if self.entries.len() >= MAX_PATHS || self.bytes.saturating_add(raw.len()) > MAX_PATH_BYTES
        {
            return Insert::Limit;
        }
        self.bytes += raw.len();
        let index = self.entries.len();
        match self.seen.entry(fingerprint) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(index);
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                self.collisions
                    .entry(fingerprint)
                    .or_insert_with(|| vec![*entry.get()])
                    .push(index);
            }
        }
        self.entries.push(FileEntry::new(path_from_bytes(raw)));
        Insert::Added
    }
}

fn path_fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Deterministically fuzzy-rank catalog entries, preferring basename matches.
pub fn rank(entries: &[FileEntry], query: &str) -> Vec<FileMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut best = BinaryHeap::with_capacity(RESULT_LIMIT + 1);
    for (index, entry) in entries.iter().enumerate() {
        let Some(candidate) = rank_entry(entry, query, index) else {
            continue;
        };
        if best.len() < RESULT_LIMIT {
            best.push(candidate);
        } else if candidate < *best.peek().expect("a full heap has a worst result") {
            let _ = best.pop();
            best.push(candidate);
        }
    }

    let mut matches = best
        .into_iter()
        .map(|ranked| ranked.into_result(entries))
        .collect::<Vec<_>>();
    matches.sort_by(compare_matches);
    matches
}

/// The catalog only stores paths; stat at most the visible results on the search worker.
fn load_result_metadata(matches: &mut [FileMatch]) {
    for matched in matches {
        if let Ok(metadata) = std::fs::metadata(matched.entry.path()) {
            matched.modified = metadata.modified().ok();
            matched.size = Some(metadata.len());
        }
    }
}

fn rank_entry(entry: &FileEntry, query: &str, index: usize) -> Option<RankedMatch> {
    let basename = entry.basename();
    let display_path = entry.home_relative_path();
    let basename_match = rank_match(query, &basename);
    let path_match = rank_match(query, &display_path);
    let (score, basename_matches, path_matches) = match (basename_match, path_match) {
        (Some(basename_match), Some(path_match)) => (
            basename_match.score + path_match.score + 10_000,
            basename_match.positions,
            path_match.positions,
        ),
        (Some(basename_match), None) => (
            basename_match.score + 10_000,
            basename_match.positions,
            Vec::new(),
        ),
        (None, Some(path_match)) => (path_match.score, Vec::new(), path_match.positions),
        (None, None) => return None,
    };
    Some(RankedMatch {
        index,
        score,
        basename_matches,
        path_matches,
        identity: entry.identity(),
    })
}

fn compare_matches(left: &FileMatch, right: &FileMatch) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.entry.identity().cmp(&right.entry.identity()))
}

#[cfg(test)]
fn compare_ranked(left: &RankedMatch, right: &RankedMatch) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.identity.cmp(&right.identity))
}

/// A heap item ordered with the least desirable result at the top.
struct RankedMatch {
    index: usize,
    score: i64,
    basename_matches: Vec<usize>,
    path_matches: Vec<usize>,
    identity: Vec<u8>,
}

impl RankedMatch {
    fn into_result(self, entries: &[FileEntry]) -> FileMatch {
        FileMatch {
            entry: entries[self.index].clone(),
            score: self.score,
            basename_matches: self.basename_matches,
            path_matches: self.path_matches,
            modified: None,
            size: None,
        }
    }
}

impl PartialEq for RankedMatch {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.identity == other.identity
    }
}

impl Eq for RankedMatch {}

impl PartialOrd for RankedMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` returns its greatest item. Reverse score ordering makes
        // the lowest score the eviction candidate, while identity keeps the
        // existing ascending deterministic tie-break intact.
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.identity.cmp(&other.identity))
    }
}

/// Simple deterministic subsequence matching over Unicode scalar values.
///
/// Adjacency, word starts, and early matches score highest. The same function
/// is intentionally reusable by other launcher surfaces instead of relying on
/// toolkit-specific search behaviour.
pub fn rank_match(query: &str, candidate: &str) -> Option<Match> {
    let candidate = candidate.chars().collect::<Vec<_>>();
    let query = query
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    if query.is_empty() {
        return None;
    }
    let mut matched = Vec::with_capacity(query.len());
    let mut cursor = 0;
    for wanted in query {
        let found = candidate[cursor..]
            .iter()
            .position(|actual| actual.to_lowercase().eq(wanted.to_lowercase()))?;
        let index = cursor + found;
        matched.push(index);
        cursor = index + 1;
    }

    let first = *matched.first().expect("a non-empty query matched");
    let adjacency = matched
        .windows(2)
        .filter(|pair| pair[1] == pair[0] + 1)
        .count() as i64;
    let word_starts = matched
        .iter()
        .filter(|&&index| {
            index == 0
                || matches!(
                    candidate[index.saturating_sub(1)],
                    '/' | '-' | '_' | ' ' | '.'
                )
        })
        .count() as i64;
    let gaps = matched
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0] + 1) as i64)
        .sum::<i64>();
    let score = 1_000 + adjacency * 100 + word_starts * 50
        - first as i64 * 5
        - gaps * 10
        - candidate.len() as i64;
    Some(Match {
        score,
        positions: matched,
    })
}

fn expand_home(path: &Path, home: Option<&Path>) -> Result<PathBuf, &'static str> {
    let mut components = path.components();
    let Some(first) = components.next() else {
        return Ok(path.to_path_buf());
    };
    if first.as_os_str() != OsStr::new("~") {
        return Ok(path.to_path_buf());
    }
    let Some(home) = home else {
        return Err("File discovery cannot expand `~` because HOME is unavailable.");
    };
    Ok(components.fold(home.to_path_buf(), |expanded, component| {
        expanded.join(component)
    }))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn path_equals_bytes(path: &Path, bytes: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes() == bytes
}

#[cfg(not(unix))]
fn path_equals_bytes(path: &Path, bytes: &[u8]) -> bool {
    path.as_os_str().to_string_lossy().as_bytes() == bytes
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_state(
        paths: &[&str],
        partial: bool,
        completed_at: Option<SystemTime>,
        revision: u64,
    ) -> Arc<FileSearchState> {
        let entries = paths
            .iter()
            .map(|path| FileEntry::new(PathBuf::from(path)))
            .collect::<Vec<_>>();
        let path_bytes = entries
            .iter()
            .map(|entry| path_bytes(entry.path()).len())
            .sum();
        Arc::new(FileSearchState {
            entries: Arc::new(entries),
            catalog_revision: revision,
            discovering: false,
            partial,
            warning: None,
            completed_at,
            path_bytes,
        })
    }

    fn staged_discovery(baseline: Arc<FileSearchState>) -> Option<Discovery> {
        let (_events_tx, events) = mpsc::channel(1);
        let (cancel, _cancellation) = watch::channel(false);
        Some(Discovery {
            events,
            cancel,
            workers: Vec::new(),
            remaining: 1,
            builder: CatalogBuilder::default(),
            warm: baseline.completed_at.is_some(),
            baseline,
            failed: false,
            partial: false,
            next_publish: FIRST_BATCH_AFTER_INITIAL,
        })
    }

    #[tokio::test]
    async fn cold_discovery_publishes_geometric_batches_before_completion() {
        let initial = Arc::new(FileSearchState::default());
        let (publisher, state) = watch::channel(initial.clone());
        publish_discovery_start(&publisher);
        let mut discovery = staged_discovery(initial);

        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Path(b"/tmp/first".to_vec()),
        )
        .await;
        let first = state.borrow().clone();
        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.catalog_revision, 1);
        assert!(first.discovering);

        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Path(b"/tmp/tail".to_vec()),
        )
        .await;
        assert_eq!(state.borrow().entries.len(), 1, "second path stays staged");
        for index in 3..=FIRST_BATCH_AFTER_INITIAL {
            handle_event(
                &publisher,
                &mut discovery,
                WorkerEvent::Path(format!("/tmp/{index}").into_bytes()),
            )
            .await;
        }
        assert_eq!(state.borrow().entries.len(), FIRST_BATCH_AFTER_INITIAL);

        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Finished { failed: false },
        )
        .await;
        let completed = state.borrow().clone();
        assert_eq!(completed.entries.len(), FIRST_BATCH_AFTER_INITIAL);
        assert!(!completed.discovering);
        assert!(completed.completed_at.is_some());
    }

    #[tokio::test]
    async fn warm_refresh_swaps_entries_only_after_successful_completion() {
        let completed_at = SystemTime::now();
        let baseline = catalog_state(&["/tmp/old"], false, Some(completed_at), 7);
        let (publisher, state) = watch::channel(Arc::clone(&baseline));
        publish_discovery_start(&publisher);
        let mut discovery = staged_discovery(Arc::clone(&baseline));

        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Path(b"/tmp/new".to_vec()),
        )
        .await;
        let staging = state.borrow().clone();
        assert_eq!(staging.entries[0].path(), Path::new("/tmp/old"));
        assert_eq!(staging.catalog_revision, 7);
        assert!(staging.discovering);

        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Finished { failed: false },
        )
        .await;
        let swapped = state.borrow().clone();
        assert_eq!(swapped.entries[0].path(), Path::new("/tmp/new"));
        assert_eq!(swapped.catalog_revision, 8);
        assert!(!swapped.discovering);
        assert!(swapped.completed_at.is_some());
    }

    #[tokio::test]
    async fn warm_refresh_failure_preserves_the_completed_partial_baseline() {
        let completed_at = SystemTime::now();
        let baseline = catalog_state(&["/tmp/old"], true, Some(completed_at), 7);
        let (publisher, state) = watch::channel(Arc::clone(&baseline));
        publish_discovery_start(&publisher);
        let mut discovery = staged_discovery(Arc::clone(&baseline));

        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Path(b"/tmp/new".to_vec()),
        )
        .await;
        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Finished { failed: true },
        )
        .await;

        let retained = state.borrow().clone();
        assert_eq!(retained.entries[0].path(), Path::new("/tmp/old"));
        assert_eq!(retained.path_bytes, baseline.path_bytes);
        assert_eq!(retained.completed_at, Some(completed_at));
        assert!(retained.partial);
        assert_eq!(retained.catalog_revision, 7);
        assert!(
            retained
                .warning
                .as_deref()
                .is_some_and(|message| message.contains("partial"))
        );
    }

    #[tokio::test]
    async fn cold_failure_publishes_the_full_discovered_tail_without_completion() {
        let initial = Arc::new(FileSearchState::default());
        let (publisher, state) = watch::channel(initial.clone());
        publish_discovery_start(&publisher);
        let mut discovery = staged_discovery(initial);

        for path in [b"/tmp/first".as_slice(), b"/tmp/tail".as_slice()] {
            handle_event(&publisher, &mut discovery, WorkerEvent::Path(path.to_vec())).await;
        }
        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Finished { failed: true },
        )
        .await;

        let failed = state.borrow().clone();
        assert_eq!(failed.entries.len(), 2);
        assert!(failed.completed_at.is_none());
        assert!(!failed.discovering);
        assert!(failed.warning.is_some());
    }

    #[tokio::test]
    async fn cold_retry_failure_keeps_the_previous_discovered_tail() {
        let baseline = catalog_state(&["/tmp/usable"], true, None, 7);
        let (publisher, state) = watch::channel(Arc::clone(&baseline));
        publish_discovery_start(&publisher);
        let staging = state.borrow().clone();
        assert_eq!(staging.entries[0].path(), Path::new("/tmp/usable"));
        assert!(staging.discovering);
        assert_eq!(staging.catalog_revision, 7);

        let mut discovery = staged_discovery(Arc::clone(&baseline));
        handle_event(
            &publisher,
            &mut discovery,
            WorkerEvent::Finished { failed: true },
        )
        .await;

        let retained = state.borrow().clone();
        assert_eq!(retained.entries[0].path(), Path::new("/tmp/usable"));
        assert_eq!(retained.path_bytes, baseline.path_bytes);
        assert!(retained.partial);
        assert!(retained.completed_at.is_none());
        assert_eq!(retained.catalog_revision, 7);
        assert!(
            retained
                .warning
                .as_deref()
                .is_some_and(|message| message.contains("partial"))
        );
    }

    #[test]
    fn builtins_cover_the_documented_secret_cache_and_generated_trees() {
        for required in [
            ".ssh",
            ".gnupg",
            ".password-store",
            ".git",
            ".cache",
            ".direnv",
            "node_modules",
            "target",
            ".venv",
            ".local/share/keyrings",
            ".mozilla",
            ".config/google-chrome",
            ".local/share/Trash",
            ".cargo/registry",
            ".cache/pip",
        ] {
            assert!(BUILTIN_EXCLUSIONS.contains(&required), "missing {required}");
        }
    }

    #[test]
    fn partial_snapshot_copies_grow_geometrically() {
        let mut next = FIRST_BATCH_AFTER_INITIAL;
        let mut published = Vec::new();
        for entries in 1..=300_000 {
            if should_publish(entries, next) {
                published.push(entries);
                if entries != 1 {
                    next = next_publish_threshold(next);
                }
            }
        }
        assert_eq!(&published[..3], &[1, 128, 256]);
        assert!(published.iter().sum::<usize>() < 600_000);
    }

    #[test]
    fn fd_uses_hidden_files_print0_and_never_follows_symlinks() {
        let arguments = fd_arguments(&[PathBuf::from("/home/test")], &["private/**".into()]);
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(arguments.windows(1).any(|arg| arg == ["--hidden"]));
        assert!(arguments.windows(2).any(|arg| arg == ["--type", "f"]));
        assert!(arguments.windows(1).any(|arg| arg == ["--print0"]));
        assert!(!arguments.iter().any(|arg| arg == "--follow" || arg == "-L"));
        assert!(
            arguments
                .windows(2)
                .any(|arg| arg == ["--exclude", "private/**"])
        );
    }

    #[test]
    fn catalog_limits_paths_bytes_and_duplicates() {
        let mut catalog = CatalogBuilder::default();
        assert!(matches!(catalog.push(b"/one".to_vec()), Insert::Added));
        assert!(matches!(catalog.push(b"/one".to_vec()), Insert::Duplicate));
        assert_eq!(catalog.bytes, 4);

        let mut catalog = CatalogBuilder {
            bytes: MAX_PATH_BYTES,
            ..CatalogBuilder::default()
        };
        assert!(matches!(catalog.push(b"/one".to_vec()), Insert::Limit));
    }

    #[test]
    fn unusual_path_bytes_survive_display_conversion() {
        let entry = FileEntry::new(path_from_bytes(b"/tmp/odd-\xff-name".to_vec()));
        assert_eq!(entry.identity(), b"/tmp/odd-\xff-name");
        assert!(entry.display_path().contains('\u{fffd}'));
    }

    #[test]
    fn metadata_enriches_ranked_results_without_losing_removed_files() {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/file_search/mod.rs");
        let missing = source.with_file_name("missing-file_search-fixture.rs");
        let mut matches = rank(
            &[
                FileEntry::new(source.clone()),
                FileEntry::new(missing.clone()),
            ],
            "file_search",
        );
        assert_eq!(matches.len(), 2);
        let identities = matches
            .iter()
            .map(|matched| matched.entry.identity())
            .collect::<Vec<_>>();
        load_result_metadata(&mut matches);
        assert_eq!(
            matches
                .iter()
                .map(|matched| matched.entry.identity())
                .collect::<Vec<_>>(),
            identities,
        );
        let existing = matches
            .iter()
            .find(|matched| matched.entry.path() == source)
            .unwrap();
        assert!(existing.modified.is_some());
        assert_eq!(
            existing.size,
            Some(std::fs::metadata(source).unwrap().len())
        );
        let removed = matches
            .iter()
            .find(|matched| matched.entry.path() == missing)
            .unwrap();
        assert_eq!((removed.modified, removed.size), (None, None));
    }

    #[test]
    fn ranking_prefers_basename_then_is_deterministic() {
        let entries = [
            FileEntry::new(PathBuf::from("/src/alpha-document.txt")),
            FileEntry::new(PathBuf::from("/documents/alpha/notes.txt")),
            FileEntry::new(PathBuf::from("/src/a-long-path/to-document.txt")),
        ];
        let ranked = rank(&entries, "doc");
        assert_eq!(ranked[0].entry.path(), Path::new("/src/alpha-document.txt"));
        assert!(ranked[0].score > ranked[1].score);
        assert_eq!(rank(&entries, "DOC"), ranked);
    }

    #[test]
    fn bounded_ranking_matches_the_unbounded_order_on_a_large_fixture() {
        let entries = (0..200)
            .map(|index| FileEntry::new(PathBuf::from(format!("/tmp/{index:03}-file-name.txt"))))
            .collect::<Vec<_>>();
        let actual = rank(&entries, "file");
        let mut expected = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| rank_entry(entry, "file", index))
            .collect::<Vec<_>>();
        expected.sort_by(compare_ranked);
        expected.truncate(RESULT_LIMIT);
        let expected = expected
            .into_iter()
            .map(|ranked| ranked.into_result(&entries))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    #[ignore = "manual performance fixture"]
    fn one_hundred_thousand_file_fixture_keeps_only_thirty_results() {
        let entries = (0..100_000)
            .map(|index| FileEntry::new(PathBuf::from(format!("/fixture/{index:06}-file.txt"))))
            .collect::<Vec<_>>();
        let started = std::time::Instant::now();
        assert_eq!(rank(&entries, "file").len(), RESULT_LIMIT);
        let elapsed = started.elapsed();
        eprintln!("100,000-file ranking: {elapsed:?}");
        assert!(elapsed.as_millis() < 100, "ranking exceeded 100 ms");
    }

    #[test]
    fn unicode_matching_returns_character_offsets() {
        let entry = FileEntry::new(PathBuf::from("/tmp/Grüße.txt"));
        let ranked = rank(&[entry], "grü");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].basename_matches, vec![0, 1, 2]);
    }

    #[test]
    fn shared_matcher_rewards_word_starts_and_adjacency() {
        let word_start = rank_match("fo", "foo bar").unwrap();
        let scattered = rank_match("fo", "x-far-ooo").unwrap();
        assert!(word_start.score > scattered.score);
        assert_eq!(word_start.positions, vec![0, 1]);
    }

    #[test]
    fn home_roots_expand_only_at_the_first_component() {
        let home = Path::new("/home/test");
        assert_eq!(
            expand_home(Path::new("~/code"), Some(home)).unwrap(),
            PathBuf::from("/home/test/code")
        );
        assert_eq!(
            expand_home(Path::new("/tmp/~/code"), Some(home)).unwrap(),
            PathBuf::from("/tmp/~/code")
        );
        assert!(expand_home(Path::new("~"), None).is_err());
    }

    #[test]
    fn roots_never_need_more_than_two_fd_workers() {
        assert_eq!(split_roots(vec![]).len(), 0);
        assert_eq!(split_roots(vec![PathBuf::from("a")]).len(), 1);
        assert_eq!(
            split_roots(vec![
                PathBuf::from("a"),
                PathBuf::from("b"),
                PathBuf::from("c")
            ])
            .len(),
            2
        );
    }

    #[tokio::test]
    async fn cancelling_a_worker_kills_and_reaps_its_child() {
        let (events, mut received) = mpsc::channel(8);
        let (cancel, cancellation) = watch::channel(false);
        let worker = tokio::spawn(worker(
            PathBuf::from("sh"),
            vec![
                OsString::from("-c"),
                OsString::from("while :; do printf '/tmp/file\\0'; sleep 0.01; done"),
            ],
            events,
            cancellation,
        ));
        let first = tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .expect("worker should emit a path")
            .expect("worker event channel should stay open");
        assert!(matches!(first, WorkerEvent::Path(_)));
        cancel.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("cancelled child should be reaped")
            .expect("worker task should not panic");
    }
}
