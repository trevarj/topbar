//! The panel's runtime state file.
//!
//! Anything the panel has to remember across restarts but must never write
//! back into the user's `config.toml` lives in
//! `$XDG_STATE_HOME/topbar/state.json`: the notification history, the Do Not
//! Disturb flag, the weather coordinates, and the crypto widget's entries.
//!
//! One task owns the file. Callers hand it *edits* rather than whole
//! documents, so two services updating different sections cannot clobber each
//! other the way v1's read-modify-write-the-world `save()` could. Writes are
//! debounced and atomic (write a sibling temp file, then rename), so a burst
//! of notifications costs one write and a crash mid-write leaves the previous
//! state intact rather than a truncated file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::crypto::PersistedCrypto;
use crate::network::PersistedNetwork;
use crate::notifications::PersistedNotifications;
use crate::weather::PersistedWeather;

/// How long the writer collects further edits before it touches the disk.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Directory the state file lives in, under `$XDG_STATE_HOME`.
const STATE_DIR: &str = "topbar";
/// The same directory under the project's former name, migrated on first run.
const LEGACY_STATE_DIR: &str = "gnome-topbar";
/// Name of the state file itself.
const STATE_FILE: &str = "state.json";

/// Everything the panel remembers across restarts.
///
/// Every field is `#[serde(default)]`, so a state file written by an older
/// build — or one with a section this build does not know about — still loads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    /// Notification history and the Do Not Disturb flag.
    pub notifications: PersistedNotifications,
    /// The weather location the setup dialog last saved.
    pub weather: PersistedWeather,
    /// The price entries the crypto settings view last saved.
    pub crypto: PersistedCrypto,
    /// The VPN profile that was last actually up.
    pub network: PersistedNetwork,
    /// Only application identities and their launcher use scores.
    pub launcher: PersistedLauncher,
}

/// Successful launcher activations, keyed by desktop entry ID.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedLauncher {
    /// Usage records; queries, paths, and window titles are never persisted.
    pub applications: BTreeMap<String, UsageRecord>,
}

/// One application's usage count and exponentially decaying score.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageRecord {
    /// Successful app launches and unambiguous window activations.
    pub count: u64,
    /// Unix timestamp of the last successful use, in seconds.
    pub last_use: i64,
    /// Score evaluated at `last_use`.
    pub score: f64,
}

/// Thirty days, measured in seconds.
const USAGE_HALF_LIFE: f64 = 30.0 * 24.0 * 60.0 * 60.0;

impl UsageRecord {
    /// Score after decay to `now`, even for records with malformed timestamps.
    pub fn score_at(&self, now: i64) -> f64 {
        let age = now.saturating_sub(self.last_use).max(0) as f64;
        if self.score.is_finite() && self.score >= 0.0 {
            self.score * 2.0_f64.powf(-age / USAGE_HALF_LIFE)
        } else {
            0.0
        }
    }

    /// Count a successful activation at `now`.
    pub fn record(&mut self, now: i64) {
        self.score = self.score_at(now) + 1.0;
        self.count = self.count.saturating_add(1);
        self.last_use = now;
    }
}

impl PersistedLauncher {
    /// Count one successful activation of an installed application.
    pub fn record(&mut self, desktop_id: &str, now: i64) {
        self.applications
            .entry(desktop_id.to_owned())
            .or_default()
            .record(now);
    }

    /// Up to six installed applications, highest decayed score first.
    pub fn frequent<'a>(
        &self,
        installed: impl IntoIterator<Item = &'a str>,
        now: i64,
    ) -> Vec<String> {
        let mut matches: Vec<_> = installed
            .into_iter()
            .filter_map(|id| {
                let usage = self.applications.get(id)?;
                (usage.count > 0).then_some((id, usage))
            })
            .collect();
        matches.sort_by(|(left_id, left), (right_id, right)| {
            right
                .score_at(now)
                .total_cmp(&left.score_at(now))
                .then_with(|| right.last_use.cmp(&left.last_use))
                .then_with(|| left_id.cmp(right_id))
        });
        matches
            .into_iter()
            .take(6)
            .map(|(id, _)| id.to_owned())
            .collect()
    }
}

/// Live launcher usage cache, mirrored into the existing debounced state writer.
#[derive(Clone)]
pub struct LauncherUsage {
    state: Arc<Mutex<PersistedLauncher>>,
    store: StateStore,
}

impl LauncherUsage {
    /// Restore use scores from the panel's state document.
    pub fn new(state: PersistedLauncher, store: StateStore) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            store,
        }
    }

    /// Record a successful activation of an unambiguous desktop ID.
    pub fn record(&self, desktop_id: &str, now: i64) {
        if desktop_id.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.record(desktop_id, now);
        }
        let id = desktop_id.to_owned();
        self.store
            .update(move |state| state.launcher.record(&id, now));
    }

    /// Return frequent IDs among the currently installed applications.
    pub fn frequent<'a>(
        &self,
        installed: impl IntoIterator<Item = &'a str>,
        now: i64,
    ) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.frequent(installed, now))
            .unwrap_or_default()
    }
}

/// One queued change to the state document.
type Edit = Box<dyn FnOnce(&mut PersistedState) + Send>;

/// A handle to the panel's state file.
///
/// Cloning is cheap and every clone writes through the same task, which is
/// what makes "single writer" true no matter how many services hold one.
#[derive(Clone)]
pub struct StateStore {
    edits: mpsc::UnboundedSender<Edit>,
}

impl StateStore {
    /// Read the state file and start its writer task.
    ///
    /// Must be called from inside the service runtime. The read is blocking
    /// and one-shot: it happens during start-up, before GTK exists, and a
    /// missing or corrupt file simply yields defaults.
    pub fn open() -> (PersistedState, Self) {
        let base = state_base();
        migrate_legacy_dir(&base);
        Self::open_at(base.join(STATE_DIR).join(STATE_FILE))
    }

    /// The same, for a caller that chooses the path — tests, chiefly.
    pub fn open_at(path: PathBuf) -> (PersistedState, Self) {
        let state = read(&path);
        let (edits, queue) = mpsc::unbounded_channel();
        tokio::spawn(write_loop(path, state.clone(), queue));
        (state, Self { edits })
    }

    /// Queue a change to the state document.
    ///
    /// Deliberately infallible: nothing the user does should fail because the
    /// panel could not remember something. A broken state file is a log line,
    /// not a toast.
    pub fn update(&self, edit: impl FnOnce(&mut PersistedState) + Send + 'static) {
        if self.edits.send(Box::new(edit)).is_err() {
            warn!("the state writer has stopped; runtime state will not be saved");
        }
    }
}

/// Apply edits as they arrive, writing at most once per [`DEBOUNCE`] window.
async fn write_loop(
    path: PathBuf,
    mut state: PersistedState,
    mut edits: mpsc::UnboundedReceiver<Edit>,
) {
    let mut written = Some(state.clone());

    while let Some(edit) = edits.recv().await {
        edit(&mut state);

        // Absorb everything that arrives while the window is open. A hundred
        // notifications in a second cost one write, not a hundred. The loop
        // ends when the window elapses or the last handle goes away; either
        // way what is in hand is what belongs on disk.
        while let Ok(Some(edit)) = tokio::time::timeout(DEBOUNCE, edits.recv()).await {
            edit(&mut state);
        }

        if written.as_ref() == Some(&state) {
            continue;
        }
        write(&path, &state);
        written = Some(state.clone());
    }
}

/// `$XDG_STATE_HOME`, or its `$HOME` fallback.
fn state_base() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
            home.join(".local").join("state")
        })
}

/// Move a pre-rename state directory to its new name, once.
///
/// A plain rename rather than a copy, and only when there is nothing to
/// clobber: if the user already has a `topbar` directory, the old one is left
/// alone for them to look at and delete.
fn migrate_legacy_dir(base: &Path) {
    let legacy = base.join(LEGACY_STATE_DIR);
    let current = base.join(STATE_DIR);
    if !legacy.is_dir() || current.exists() {
        return;
    }
    match std::fs::rename(&legacy, &current) {
        Ok(()) => info!(
            "moved runtime state from {} to {}",
            legacy.display(),
            current.display()
        ),
        Err(error) => warn!(
            "could not move runtime state from {} to {}: {error}",
            legacy.display(),
            current.display()
        ),
    }
}

/// Load `path`, falling back to defaults for anything unreadable.
fn read(path: &Path) -> PersistedState {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            debug!("no state file at {}; starting fresh", path.display());
            return PersistedState::default();
        }
        Err(error) => {
            warn!("could not read {}: {error}", path.display());
            return PersistedState::default();
        }
    };

    match serde_json::from_str(&contents) {
        Ok(state) => {
            debug!("loaded state from {}", path.display());
            state
        }
        Err(error) => {
            // Keeping the broken file would mean losing the same state on
            // every start; overwriting it silently is the lesser evil, and the
            // next successful write does exactly that.
            warn!(
                "{} is not valid state JSON ({error}); ignoring it",
                path.display()
            );
            PersistedState::default()
        }
    }
}

/// Write `state` to `path` atomically.
///
/// The temp file is a sibling so the rename stays within one filesystem, and
/// it carries the pid so two panels racing each other cannot share one.
fn write(path: &Path, state: &PersistedState) {
    let Some(parent) = path.parent() else {
        warn!("{} has no parent directory", path.display());
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        warn!("could not create {}: {error}", parent.display());
        return;
    }

    let json = match serde_json::to_string_pretty(state) {
        Ok(json) => json,
        Err(error) => {
            warn!("could not serialise runtime state: {error}");
            return;
        }
    };

    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&temp, json) {
        warn!("could not write {}: {error}", temp.display());
        let _ = std::fs::remove_file(&temp);
        return;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        warn!("could not replace {}: {error}", path.display());
        let _ = std::fs::remove_file(&temp);
        return;
    }
    debug!("saved state to {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_usage_decays_and_breaks_ties_by_last_use_then_id() {
        let mut usage = PersistedLauncher::default();
        let now = 1_800_000_000;
        usage.record("old.desktop", now - 30 * 24 * 60 * 60);
        usage.record("old.desktop", now - 30 * 24 * 60 * 60);
        usage.record("new.desktop", now);
        assert_eq!(usage.applications["old.desktop"].count, 2);
        assert!((usage.applications["old.desktop"].score_at(now) - 1.0).abs() < 1e-9);
        assert_eq!(
            usage.frequent(["old.desktop", "new.desktop"], now),
            vec!["new.desktop", "old.desktop"]
        );

        usage.record("b.desktop", now);
        usage.record("a.desktop", now);
        assert_eq!(
            usage.frequent(["b.desktop", "a.desktop"], now),
            vec!["a.desktop", "b.desktop"]
        );
        assert!(usage.frequent(["missing.desktop"], now).is_empty());
    }
    use crate::notifications::PersistedNotification;

    /// A unique state-file path for one test.
    fn temp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("topbar-state-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(STATE_FILE)
    }

    /// Wait for `path` to hold state satisfying `predicate`.
    async fn wait_for(path: &Path, predicate: impl Fn(&PersistedState) -> bool) -> PersistedState {
        for _ in 0..200 {
            let state = read(path);
            if predicate(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "state at {} never reached the expected value",
            path.display()
        );
    }

    fn entry(id: u32, summary: &str) -> PersistedNotification {
        PersistedNotification {
            id,
            summary: summary.to_string(),
            ..PersistedNotification::default()
        }
    }

    #[tokio::test]
    async fn a_missing_file_loads_as_defaults() {
        let path = temp_path("missing");
        let (state, _store) = StateStore::open_at(path.clone());
        assert_eq!(state, PersistedState::default());
        assert!(!path.exists(), "reading must not create the file");
    }

    #[tokio::test]
    async fn state_survives_a_round_trip() {
        let path = temp_path("round-trip");
        let (_, store) = StateStore::open_at(path.clone());

        store.update(|state| {
            state.notifications.dnd = true;
            state.notifications.next_id = 42;
            state.notifications.history = vec![entry(7, "hello"), entry(6, "older")];
        });

        wait_for(&path, |state| state.notifications.next_id == 42).await;

        // A second store reading the same path sees exactly what was written.
        let (reloaded, _store) = StateStore::open_at(path.clone());
        assert!(reloaded.notifications.dnd);
        assert_eq!(reloaded.notifications.history.len(), 2);
        assert_eq!(reloaded.notifications.history[0].summary, "hello");
    }

    #[tokio::test]
    async fn a_burst_of_edits_collapses_into_one_document() {
        let path = temp_path("burst");
        let (_, store) = StateStore::open_at(path.clone());

        for id in 1..=50u32 {
            store.update(move |state| state.notifications.next_id = id);
        }

        let settled = wait_for(&path, |state| state.notifications.next_id == 50).await;
        assert_eq!(settled.notifications.next_id, 50);
    }

    #[tokio::test]
    async fn corrupt_json_loads_as_defaults_rather_than_failing() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{ this is not json").expect("write");

        let (state, store) = StateStore::open_at(path.clone());
        assert_eq!(state, PersistedState::default());

        // And the next write repairs the file rather than leaving it broken.
        store.update(|state| state.notifications.dnd = true);
        let repaired = wait_for(&path, |state| state.notifications.dnd).await;
        assert!(repaired.notifications.dnd);
    }

    #[tokio::test]
    async fn no_temp_files_are_left_behind() {
        let path = temp_path("atomic");
        let (_, store) = StateStore::open_at(path.clone());
        store.update(|state| state.notifications.next_id = 3);
        wait_for(&path, |state| state.notifications.next_id == 3).await;

        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?} behind");
    }

    /// A clean `$XDG_STATE_HOME` stand-in for one migration test.
    fn temp_base(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("topbar-base-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create base");
        base
    }

    #[test]
    fn a_pre_rename_state_directory_is_moved_to_the_new_name() {
        let base = temp_base("migrate");
        let legacy = base.join(LEGACY_STATE_DIR);
        std::fs::create_dir_all(&legacy).expect("create legacy dir");
        std::fs::write(legacy.join(STATE_FILE), "{}").expect("write legacy state");

        migrate_legacy_dir(&base);

        assert!(!legacy.exists(), "the legacy directory must be gone");
        assert!(base.join(STATE_DIR).join(STATE_FILE).exists());
    }

    #[test]
    fn migration_never_clobbers_an_existing_directory() {
        let base = temp_base("migrate-conflict");
        let legacy = base.join(LEGACY_STATE_DIR);
        let current = base.join(STATE_DIR);
        std::fs::create_dir_all(&legacy).expect("create legacy dir");
        std::fs::create_dir_all(&current).expect("create current dir");
        std::fs::write(current.join(STATE_FILE), "{\"kept\":true}").expect("write current state");

        migrate_legacy_dir(&base);

        assert!(legacy.exists(), "the legacy directory is left for the user");
        assert_eq!(
            std::fs::read_to_string(current.join(STATE_FILE)).expect("read current state"),
            "{\"kept\":true}"
        );
    }

    #[test]
    fn migration_is_a_no_op_without_a_legacy_directory() {
        let base = temp_base("migrate-absent");
        migrate_legacy_dir(&base);
        assert!(!base.join(STATE_DIR).exists());
    }
}
