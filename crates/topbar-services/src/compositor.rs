//! The panel's compositor boundary and backend selection.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;
use topbar_core::config::MangoConfig;

use crate::error::SvcError;
use crate::mango::{Mango, MangoHandle};
use crate::niri::{Niri, NiriHandle};

/// Stable backend-native workspace identity, captured by UI actions.
pub type WorkspaceId = u64;

/// One workspace as the panel draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceView {
    /// Stable identity captured by a click.
    pub id: WorkspaceId,
    /// One-based ordinal on this output.
    pub idx: usize,
    /// Custom display name, if any.
    pub name: Option<String>,
    /// Visible on its output, which need not be focused.
    pub is_active: bool,
    /// Globally focused workspace.
    pub is_focused: bool,
    /// Workspace or one of its windows requests attention.
    pub is_urgent: bool,
    /// At least one window lives here.
    pub has_windows: bool,
    /// Keep an empty configured workspace visible.
    pub is_persistent: bool,
}

/// Last complete workspace projection, retained while disconnected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspacesSnapshot {
    /// Whether this projection is live.
    pub connected: bool,
    /// Connector name to workspaces in display order.
    pub outputs: BTreeMap<String, Vec<WorkspaceView>>,
    /// The globally focused connector.
    pub focused_output: Option<String>,
}

/// One live compositor window shown in launcher search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    /// Native identity used for exact activation.
    pub id: u64,
    /// Window title.
    pub title: String,
    /// Application identity reported by the compositor.
    pub app_id: String,
    /// Current workspace name or index.
    pub workspace: Option<String>,
    /// Output connector, when assigned.
    pub output: Option<String>,
    /// Last focus time, for application window preference.
    pub focused_at_ms: Option<u128>,
}

/// Window projection; disconnected snapshots deliberately hide stale results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsSnapshot {
    /// Whether the stream currently belongs to a live compositor.
    pub connected: bool,
    /// Stable ordering by native window ID.
    pub windows: Vec<WindowView>,
}

impl WorkspacesSnapshot {
    /// The workspaces belonging to one connector.
    pub fn for_output(&self, connector: &str) -> &[WorkspaceView] {
        self.outputs.get(connector).map_or(&[], Vec::as_slice)
    }

    /// All workspaces in connector then display order.
    pub fn all(&self) -> impl Iterator<Item = &WorkspaceView> {
        self.outputs.values().flatten()
    }

    /// Change connectivity without discarding last-good data.
    pub fn with_connected(mut self, connected: bool) -> Self {
        self.connected = connected;
        self
    }
}

/// Current main-keyboard layout and configured number of choices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardLayoutSnapshot {
    /// Whether this projection is live.
    pub connected: bool,
    /// Checked display name of the active layout.
    pub current_name: Option<String>,
    /// Number of configured layouts, not the number of devices.
    pub layout_count: usize,
}

impl KeyboardLayoutSnapshot {
    /// Active layout's display name.
    pub fn current(&self) -> Option<&str> {
        self.current_name.as_deref()
    }

    /// Whether switching can select another layout.
    pub fn is_switchable(&self) -> bool {
        self.layout_count > 1
    }

    /// Change connectivity without discarding last-good data.
    pub fn with_connected(mut self, connected: bool) -> Self {
        self.connected = connected;
        self
    }
}

/// One selected compositor service or a startup selection failure.
#[derive(Clone)]
pub enum Compositor {
    /// Native Niri service, with its replay protocol unchanged.
    Niri(Niri),
    /// Stock Mango IPC service.
    Mango(Mango),
    /// Unavailable desktop identity.
    Disconnected(String),
}

/// Cheap action handle to exactly one selected compositor.
#[derive(Clone)]
pub enum CompositorHandle {
    /// Niri actions.
    Niri(NiriHandle),
    /// Mango actions.
    Mango(MangoHandle),
    /// A selection error, retained for actionable click failures.
    Disconnected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Niri,
    Mango,
}

/// Desktop tokens take precedence over inherited instance markers.
fn select(configured: &str, desktop: &str, niri: bool, mango: bool) -> Result<Backend, SvcError> {
    match configured {
        "niri" => return Ok(Backend::Niri),
        "mango" => return Ok(Backend::Mango),
        "auto" => {}
        _ => {
            return Err(SvcError::CompositorSelection(format!(
                "unknown backend {configured}"
            )));
        }
    }
    let mut niri_desktop = false;
    let mut mango_desktop = false;
    for token in desktop.split(|c: char| c == ':' || c == ';' || c.is_whitespace()) {
        niri_desktop |= token.eq_ignore_ascii_case("niri");
        mango_desktop |= token.eq_ignore_ascii_case("mango");
    }
    match (niri_desktop, mango_desktop) {
        (true, false) => return Ok(Backend::Niri),
        (false, true) => return Ok(Backend::Mango),
        _ => {}
    }
    if desktop.trim().is_empty() {
        match (niri, mango) {
            (true, false) => return Ok(Backend::Niri),
            (false, true) => return Ok(Backend::Mango),
            _ => {}
        }
    }
    Err(SvcError::CompositorSelection(
        "Cannot select a compositor: set advanced.compositor or an unambiguous XDG_CURRENT_DESKTOP and export its instance socket".into(),
    ))
}

fn instance(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Find the focused connector without starting a long-lived compositor service.
///
/// Standalone dialogs use the same desktop-first selection as the panel.
pub async fn focused_output_connector(configured: &str) -> Result<Option<String>, SvcError> {
    let niri = instance("NIRI_SOCKET");
    let mango = instance("MANGO_INSTANCE_SIGNATURE");
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    match select(configured, &desktop, niri.is_some(), mango.is_some())? {
        Backend::Niri => NiriHandle::new(niri).focused_output_connector().await,
        Backend::Mango => crate::mango::focused_output_connector(mango).await,
    }
}

impl Compositor {
    pub(crate) fn start(configured: &str, metadata: MangoConfig) -> Self {
        let niri = instance("NIRI_SOCKET");
        let mango = instance("MANGO_INSTANCE_SIGNATURE");
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        match select(configured, &desktop, niri.is_some(), mango.is_some()) {
            Ok(Backend::Niri) => Self::Niri(Niri::start(niri)),
            Ok(Backend::Mango) => Self::Mango(Mango::start(mango, metadata)),
            Err(error) => {
                tracing::error!("{error}");
                Self::Disconnected(error.to_string())
            }
        }
    }

    /// Backend selected at startup, independent of current connectivity.
    pub fn backend(&self) -> Option<&'static str> {
        match self {
            Self::Niri(_) => Some("niri"),
            Self::Mango(_) => Some("mango"),
            Self::Disconnected(_) => None,
        }
    }

    /// Actions address this backend only.
    pub fn handle(&self) -> CompositorHandle {
        match self {
            Self::Niri(service) => CompositorHandle::Niri(service.handle().clone()),
            Self::Mango(service) => CompositorHandle::Mango(service.handle().clone()),
            Self::Disconnected(error) => CompositorHandle::Disconnected(error.clone()),
        }
    }

    /// Subscribe without a forwarding task or snapshot copy.
    pub fn workspaces(&self) -> watch::Receiver<Arc<WorkspacesSnapshot>> {
        match self {
            Self::Niri(service) => service.workspaces(),
            Self::Mango(service) => service.workspaces(),
            Self::Disconnected(_) => watch::channel(Arc::new(WorkspacesSnapshot::default())).1,
        }
    }

    /// Subscribe to the main keyboard's layout.
    pub fn keyboard_layout(&self) -> watch::Receiver<Arc<KeyboardLayoutSnapshot>> {
        match self {
            Self::Niri(service) => service.keyboard_layout(),
            Self::Mango(service) => service.keyboard_layout(),
            Self::Disconnected(_) => watch::channel(Arc::new(KeyboardLayoutSnapshot::default())).1,
        }
    }

    /// Subscribe to launcher window state.
    pub fn windows(&self) -> watch::Receiver<Arc<WindowsSnapshot>> {
        match self {
            Self::Niri(service) => service.windows(),
            Self::Mango(service) => service.windows(),
            Self::Disconnected(_) => watch::channel(Arc::new(WindowsSnapshot::default())).1,
        }
    }

    /// Interrupt reconnect backoff after resume.
    pub fn health_check(&self) {
        match self {
            Self::Niri(service) => service.health_check(),
            Self::Mango(service) => service.health_check(),
            Self::Disconnected(_) => {}
        }
    }
}

impl CompositorHandle {
    /// Activate exactly one captured window ID.
    pub async fn focus_window(&self, id: u64) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_window(id).await,
            Self::Mango(handle) => handle.focus_window(id).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }
    /// Focus the captured native workspace ID.
    pub async fn focus_workspace(&self, id: WorkspaceId) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_workspace(id).await,
            Self::Mango(handle) => handle.focus_workspace(id).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Raise the first matching normalized application identity.
    pub async fn focus_app(&self, identities: &[&str]) -> Result<bool, SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_app(identities).await,
            Self::Mango(handle) => handle.focus_app(identities).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Focus an existing launcher application after querying the compositor live.
    ///
    /// `Ok(false)` means the compositor answered and has no matching window;
    /// callers may then start a fresh process. Any error must keep that launch
    /// path closed, since the query or focus action could have failed.
    pub async fn focus_application(&self, identities: &[&str]) -> Result<bool, SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_application(identities).await,
            Self::Mango(handle) => handle.focus_application(identities).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Select the next main-keyboard layout.
    pub async fn switch_layout_next(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.switch_layout_next().await,
            Self::Mango(handle) => handle.switch_layout_next().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Select the previous main-keyboard layout.
    pub async fn switch_layout_prev(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.switch_layout_prev().await,
            Self::Mango(handle) => handle.switch_layout_prev().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// End this compositor's session, not an enclosing nested session.
    pub async fn quit_compositor(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.quit_compositor().await,
            Self::Mango(handle) => handle.quit_compositor().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }
}

/// Whether a window's app id and a notification's idea of its sender are the
/// same application.
///
/// Case-insensitive, and a leading `@` — which some senders put on the display
/// name — is not part of the name.
pub(crate) fn same_app(app_id: &str, identity: &str) -> bool {
    fn normalise(value: &str) -> String {
        value.trim().trim_start_matches('@').to_lowercase()
    }

    normalise(app_id) == normalise(identity)
}

/// Normalise only forms a desktop entry can explicitly name. This accepts
/// niri's bare desktop ID beside its `.desktop` form while retaining exact
/// equality for `StartupWMClass` aliases.
pub(crate) fn same_launcher_application(app_id: &str, identity: &str) -> bool {
    fn normalise(value: &str) -> String {
        let value = value.trim().trim_start_matches('@').to_ascii_lowercase();
        value.strip_suffix(".desktop").unwrap_or(&value).to_owned()
    }

    normalise(app_id) == normalise(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_desktop_first_and_rejects_ambiguous_markers() {
        assert_eq!(select("niri", "", false, false).unwrap(), Backend::Niri);
        assert_eq!(select("mango", "", false, false).unwrap(), Backend::Mango);
        assert_eq!(
            select("auto", "GNOME:NiRi", false, true).unwrap(),
            Backend::Niri
        );
        assert_eq!(select("auto", "mango", true, true).unwrap(), Backend::Mango);
        assert_eq!(select("auto", "", true, false).unwrap(), Backend::Niri);
        assert_eq!(select("auto", "", false, true).unwrap(), Backend::Mango);
        assert!(select("auto", "", true, true).is_err());
        assert!(select("auto", "mango:niri", true, true).is_err());
        assert!(select("auto", "other", true, false).is_err());
        assert!(select("auto", "", false, false).is_err());
        assert!(select("other", "niri", true, false).is_err());
    }

    #[tokio::test]
    async fn disconnected_launcher_activation_refuses_to_start_a_duplicate() {
        let handle = CompositorHandle::Disconnected("niri is reconnecting".into());
        let error = handle
            .focus_application(&["org.example.Editor.desktop"])
            .await
            .expect_err("a disconnected compositor cannot confirm no window exists");
        assert!(matches!(error, SvcError::CompositorSelection(_)));
    }
}
