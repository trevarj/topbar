//! Installed desktop applications for launcher search and activation.
//!
//! GIO owns desktop-entry visibility and localisation.  The service copies the
//! small, sendable projection needed by GTK into a watch channel; widgets never
//! retain `gio::AppInfo` objects or inspect desktop files themselves.

use std::sync::{Arc, Mutex};
use std::{
    env,
    path::{Path, PathBuf},
};

use gio::glib::translate::ToGlibPtr;
use gio::prelude::*;
use tokio::sync::watch;

use crate::error::SvcError;

/// Standard file-manager reveal interface. Keep D-Bus values at this typed
/// service boundary rather than passing dynamic variants into widget code.
#[zbus::proxy(
    interface = "org.freedesktop.FileManager1",
    default_service = "org.freedesktop.FileManager1",
    default_path = "/org/freedesktop/FileManager1"
)]
trait FileManager {
    #[zbus(name = "ShowItems")]
    fn show_items(&self, uris: Vec<String>, startup_id: &str) -> zbus::Result<()>;
}

/// An installed visible desktop entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Application {
    /// GIO desktop-entry identity, including the `.desktop` suffix when GIO
    /// supplies one. This is the identity persisted in launcher usage.
    pub desktop_id: String,
    /// Localised application name.
    pub name: String,
    /// Optional localised generic name / description.
    pub generic_name: Option<String>,
    /// Executable name, useful when a desktop entry has no generic name.
    pub executable: String,
    /// Desktop-entry keywords used by launcher fuzzy matching.
    pub keywords: Vec<String>,
    /// Explicit window-manager aliases such as `StartupWMClass`.
    pub aliases: Vec<String>,
    /// Icon serialised by GIO, normally an icon-theme name.
    pub icon: Option<String>,
}

impl Application {
    /// Match a launcher query against all metadata GIO exposes portably.
    pub fn match_score(&self, query: &str) -> Option<crate::file_search::Match> {
        let fields = [
            self.name.as_str(),
            self.generic_name.as_deref().unwrap_or_default(),
            self.desktop_id.as_str(),
        ];
        fields
            .into_iter()
            .chain(self.keywords.iter().map(String::as_str))
            .filter_map(|field| crate::file_search::rank_match(query, field))
            .max_by_key(|matched| matched.score)
    }

    /// Forms accepted by niri's application identity for exact matching.
    pub fn identities(&self) -> impl Iterator<Item = &str> {
        let bare = self
            .desktop_id
            .strip_suffix(".desktop")
            .unwrap_or(&self.desktop_id);
        [self.desktop_id.as_str(), bare]
            .into_iter()
            .chain(self.aliases.iter().map(String::as_str))
    }
}

/// Immutable application discovery snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplicationsState {
    /// Entries sorted by display name and desktop ID for a stable grid.
    pub entries: Vec<Application>,
    /// Whether GIO is refreshing desktop-entry metadata.
    pub loading: bool,
    /// The last discovery failure, kept local to the Applications section.
    pub warning: Option<String>,
}

/// GIO desktop-entry discovery and activation boundary.
#[derive(Clone)]
pub struct Applications {
    state: watch::Receiver<Arc<ApplicationsState>>,
}

impl Applications {
    /// Start an initial discovery on the service runtime.
    pub fn start() -> Self {
        let (publisher, state) = watch::channel(Arc::new(ApplicationsState::default()));
        tokio::task::spawn_blocking(move || {
            let context = gio::glib::MainContext::new();
            let last = Arc::new(Mutex::new(ApplicationsState::default()));
            context
                .with_thread_default(|| {
                    refresh(&publisher, &last);
                    let monitor = gio::AppInfoMonitor::get();
                    monitor.connect_changed({
                        let publisher = publisher.clone();
                        let last = Arc::clone(&last);
                        move |_| refresh(&publisher, &last)
                    });
                    gio::glib::MainLoop::new(Some(&context), false).run();
                })
                .expect("dedicated GIO context");
        });
        Self { state }
    }

    /// Subscribe to discovery state.
    pub fn state(&self) -> watch::Receiver<Arc<ApplicationsState>> {
        self.state.clone()
    }

    /// Reveal a file through FileManager1, opening its parent as a fallback.
    pub async fn reveal_path(&self, path: std::path::PathBuf) -> Result<(), SvcError> {
        let uri = gio::File::for_path(&path).uri().to_string();
        if let Ok(connection) = zbus::Connection::session().await
            && let Ok(manager) = FileManagerProxy::new(&connection).await
            && manager.show_items(vec![uri], "").await.is_ok()
        {
            return Ok(());
        }
        tokio::task::spawn_blocking(move || reveal_path(&path))
            .await
            .map_err(|error| SvcError::Rejected(format!("file reveal worker stopped: {error}")))?
    }
}

fn refresh(
    publisher: &watch::Sender<Arc<ApplicationsState>>,
    last: &Arc<Mutex<ApplicationsState>>,
) {
    let previous = last.lock().map(|state| state.clone()).unwrap_or_default();
    let _ = publisher.send(Arc::new(ApplicationsState {
        loading: true,
        ..previous.clone()
    }));
    let next = match discover() {
        Ok(entries) => ApplicationsState {
            entries,
            loading: false,
            warning: None,
        },
        Err(error) => ApplicationsState {
            loading: false,
            warning: Some(error.to_string()),
            ..previous
        },
    };
    if let Ok(mut state) = last.lock() {
        *state = next.clone();
    }
    let _ = publisher.send(Arc::new(next));
}

fn discover() -> Result<Vec<Application>, SvcError> {
    let mut entries = gio::AppInfo::all()
        .into_iter()
        .filter(|entry| entry.should_show())
        .filter_map(|entry| {
            let desktop_id = entry.id()?.to_string();
            let DesktopMetadata {
                generic_name,
                keywords,
                aliases,
            } = desktop_metadata(&desktop_id);
            (!desktop_id.is_empty()).then(|| Application {
                desktop_id,
                name: entry.display_name().to_string(),
                generic_name: generic_name
                    .or_else(|| entry.description().map(|value| value.to_string())),
                executable: desktop_executable(&entry),
                keywords,
                aliases,
                icon: entry
                    .icon()
                    .and_then(|icon| icon.to_string())
                    .map(|icon| icon.to_string()),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.desktop_id.cmp(&right.desktop_id))
    });
    entries.dedup_by(|left, right| left.desktop_id == right.desktop_id);
    Ok(entries)
}

fn desktop_executable(entry: &gio::AppInfo) -> String {
    // GIO permits NULL for D-Bus-activated entries without Exec; the binding's
    // non-optional PathBuf getter panics on that valid absence.
    // SAFETY: entry is live and GIO owns the returned, possibly NULL, string.
    let path = unsafe { gio::ffi::g_app_info_get_executable(entry.to_glib_none().0) };
    if path.is_null() {
        String::new()
    } else {
        // SAFETY: the non-NULL string remains valid while entry is borrowed.
        unsafe { std::ffi::CStr::from_ptr(path) }
            .to_string_lossy()
            .into_owned()
    }
}

#[derive(Default)]
struct DesktopMetadata {
    generic_name: Option<String>,
    keywords: Vec<String>,
    aliases: Vec<String>,
}

/// Read only the identity additions GIO does not project from a desktop entry.
///
/// GIO remains authoritative for visibility, localisation and launching. The
/// entry file supplies `Keywords` and `StartupWMClass`, which are explicitly
/// intended for desktop search and exact window identity matching.
fn desktop_metadata(id: &str) -> DesktopMetadata {
    let mut roots = Vec::new();
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data_home));
    } else if let Some(home) = env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".local/share"));
    }
    roots.extend(
        env::var_os("XDG_DATA_DIRS")
            .map(|dirs| env::split_paths(&dirs).collect())
            .unwrap_or_else(|| {
                vec![
                    PathBuf::from("/usr/local/share"),
                    PathBuf::from("/usr/share"),
                ]
            }),
    );
    desktop_metadata_in_roots(id, &roots)
}

fn desktop_metadata_in_roots(id: &str, roots: &[PathBuf]) -> DesktopMetadata {
    let Some(stem) = id
        .strip_suffix(".desktop")
        .filter(|stem| !stem.is_empty() && !stem.contains('/') && !stem.contains('\\'))
    else {
        return DesktopMetadata::default();
    };
    let Some(path) = roots
        .iter()
        .find_map(|root| desktop_entry_path(&root.join("applications"), stem))
    else {
        return DesktopMetadata::default();
    };
    metadata_from_key_file(&path).unwrap_or_default()
}

/// A desktop ID replaces each directory separator below `applications` with
/// `-`. Probe only existing directories so hyphens in ordinary filenames do
/// not cause a full scan of the data roots.
fn desktop_entry_path(directory: &Path, stem: &str) -> Option<PathBuf> {
    let direct = directory.join(format!("{stem}.desktop"));
    if direct.is_file() {
        return Some(direct);
    }
    for (index, _) in stem.match_indices('-') {
        let (folder, rest) = stem.split_at(index);
        let nested = directory.join(folder);
        if !folder.is_empty()
            && nested.is_dir()
            && let Some(path) = desktop_entry_path(&nested, &rest[1..])
        {
            return Some(path);
        }
    }
    None
}

fn metadata_from_key_file(path: &std::path::Path) -> Option<DesktopMetadata> {
    let key_file = gio::glib::KeyFile::new();
    key_file
        .load_from_file(path, gio::glib::KeyFileFlags::NONE)
        .ok()?;
    let group = "Desktop Entry";
    let generic_name = key_file
        .locale_string(group, "GenericName", None)
        .ok()
        .map(|value| value.to_string());
    let keywords = key_file
        .locale_string_list(group, "Keywords", None)
        .ok()
        .map(|values| {
            values
                .iter()
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let aliases = key_file
        .string(group, "StartupWMClass")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| vec![value.to_string()])
        .unwrap_or_default();
    Some(DesktopMetadata {
        generic_name,
        keywords,
        aliases,
    })
}

fn open_path(path: &std::path::Path) -> Result<(), SvcError> {
    let file = gio::File::for_path(path);
    let uri = file.uri();
    gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
        .map_err(|error| SvcError::Rejected(format!("could not open file: {error}")))
}

fn reveal_path(path: &std::path::Path) -> Result<(), SvcError> {
    let parent = path
        .parent()
        .ok_or_else(|| SvcError::Rejected("file has no parent directory".to_string()))?;
    open_path(parent)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use gio::glib;

    #[test]
    fn application_match_includes_desktop_identity() {
        let app = Application {
            desktop_id: "org.example.Editor.desktop".into(),
            name: "Editor".into(),
            generic_name: None,
            executable: "editor".into(),
            keywords: vec!["write".into()],
            aliases: vec!["EditorWindow".into()],
            icon: None,
        };
        assert!(app.match_score("example").is_some());
        assert_eq!(
            app.identities().collect::<Vec<_>>(),
            [
                "org.example.Editor.desktop",
                "org.example.Editor",
                "EditorWindow"
            ]
        );
    }

    #[test]
    fn dbus_only_desktop_entries_have_no_executable_without_panicking() {
        for (exec, expected) in [("", ""), ("Exec=echo hello\n", "echo")] {
            let key_file = glib::KeyFile::new();
            key_file
                .load_from_data(
                    &format!(
                        "[Desktop Entry]\nType=Application\nName=Example\nDBusActivatable=true\n{exec}"
                    ),
                    glib::KeyFileFlags::NONE,
                )
                .expect("desktop entry");
            let entry = gio_unix::DesktopAppInfo::from_keyfile(&key_file)
                .expect("valid desktop entry")
                .upcast::<gio::AppInfo>();
            assert_eq!(desktop_executable(&entry), expected);
        }
    }

    #[test]
    fn key_file_prefers_localized_generic_name_and_keywords() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "topbar-app-{}-{}.desktop",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, "[Desktop Entry]\nGenericName=Editor\nGenericName[en_US]=Localized editor\nKeywords=write;plain;\nKeywords[en_US]=localized;words;\nStartupWMClass=EditorWindow\n").unwrap();
        let metadata = metadata_from_key_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // GLib chooses the active locale. The base values are valid fallback
        // evidence on test hosts without en_US installed.
        assert!(
            metadata.generic_name.as_deref() == Some("Editor")
                || metadata.generic_name.as_deref() == Some("Localized editor")
        );
        assert!(!metadata.keywords.is_empty());
        assert_eq!(metadata.aliases, ["EditorWindow"]);
    }

    #[test]
    fn subdirectory_desktop_id_reads_keywords_and_window_alias() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "topbar-desktop-id-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let nested = root.join("applications/kde");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("foo.desktop"),
            "[Desktop Entry]\nGenericName=Nested editor\nKeywords=nested;write;\nStartupWMClass=NestedEditor\n",
        )
        .unwrap();
        let lower_root = root.with_extension("lower");
        let lower_nested = lower_root.join("applications/kde");
        std::fs::create_dir_all(&lower_nested).unwrap();
        std::fs::write(
            lower_nested.join("foo.desktop"),
            "[Desktop Entry]\nKeywords=lower;\nStartupWMClass=LowerEditor\n",
        )
        .unwrap();
        let metadata =
            desktop_metadata_in_roots("kde-foo.desktop", &[root.clone(), lower_root.clone()]);
        let unrelated =
            desktop_metadata_in_roots("kde-foo-extra.desktop", std::slice::from_ref(&root));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(lower_root);
        assert_eq!(metadata.generic_name.as_deref(), Some("Nested editor"));
        assert_eq!(metadata.keywords, ["nested", "write"]);
        assert_eq!(metadata.aliases, ["NestedEditor"]);
        assert!(unrelated.keywords.is_empty());
    }
}
