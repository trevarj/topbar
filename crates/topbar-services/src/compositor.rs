//! The panel's Niri compositor boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use crate::error::SvcError;
use crate::niri::{Niri, NiriHandle};

/// Niri's stable native workspace identity.
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

/// One live niri window shown in launcher search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    /// Native identity used for exact activation.
    pub id: u64,
    /// Window title.
    pub title: String,
    /// Application identity reported by niri.
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

/// Niri service or a startup selection failure.
#[derive(Clone)]
pub enum Compositor {
    /// Native Niri service, with its replay protocol unchanged.
    Niri(Niri),
    /// Unavailable desktop identity.
    Disconnected(String),
}

/// Cheap action handle to exactly one selected compositor.
#[derive(Clone)]
pub enum CompositorHandle {
    /// Niri actions.
    Niri(NiriHandle),
    /// A selection error, retained for actionable click failures.
    Disconnected(String),
}

/// Resolve the desktop identity before opening its socket.
fn select(configured: &str, desktop: &str, niri: bool) -> Result<(), SvcError> {
    if configured == "niri" {
        return Ok(());
    }
    if configured != "auto" {
        return Err(SvcError::CompositorSelection(format!(
            "unknown backend {configured}"
        )));
    }
    if desktop
        .split(|c: char| c == ':' || c == ';' || c.is_whitespace())
        .any(|token| token.eq_ignore_ascii_case("niri"))
        || (desktop.is_empty() && niri)
    {
        Ok(())
    } else {
        Err(SvcError::CompositorSelection(
            "Niri session not found: set XDG_CURRENT_DESKTOP or advanced.compositor and export NIRI_SOCKET".into(),
        ))
    }
}

impl Compositor {
    pub(crate) fn start(configured: &str) -> Self {
        let niri = std::env::var_os("NIRI_SOCKET")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        match select(configured, &desktop, niri.is_some()) {
            Ok(()) => Self::Niri(Niri::start(niri)),
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
            Self::Disconnected(_) => None,
        }
    }

    /// Actions address this backend only.
    pub fn handle(&self) -> CompositorHandle {
        match self {
            Self::Niri(service) => CompositorHandle::Niri(service.handle().clone()),
            Self::Disconnected(error) => CompositorHandle::Disconnected(error.clone()),
        }
    }

    /// Subscribe without a forwarding task or snapshot copy.
    pub fn workspaces(&self) -> watch::Receiver<Arc<WorkspacesSnapshot>> {
        match self {
            Self::Niri(service) => service.workspaces(),
            Self::Disconnected(_) => watch::channel(Arc::new(WorkspacesSnapshot::default())).1,
        }
    }

    /// Subscribe to the main keyboard's layout.
    pub fn keyboard_layout(&self) -> watch::Receiver<Arc<KeyboardLayoutSnapshot>> {
        match self {
            Self::Niri(service) => service.keyboard_layout(),
            Self::Disconnected(_) => watch::channel(Arc::new(KeyboardLayoutSnapshot::default())).1,
        }
    }

    /// Subscribe to launcher window state.
    pub fn windows(&self) -> watch::Receiver<Arc<WindowsSnapshot>> {
        match self {
            Self::Niri(service) => service.windows(),
            Self::Disconnected(_) => watch::channel(Arc::new(WindowsSnapshot::default())).1,
        }
    }

    /// Interrupt reconnect backoff after resume.
    pub fn health_check(&self) {
        match self {
            Self::Niri(service) => service.health_check(),
            Self::Disconnected(_) => {}
        }
    }
}

impl CompositorHandle {
    /// Activate exactly one captured window ID.
    pub async fn focus_window(&self, id: u64) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_window(id).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }
    /// Focus the captured native workspace ID.
    pub async fn focus_workspace(&self, id: WorkspaceId) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_workspace(id).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Raise the first matching normalized application identity.
    pub async fn focus_app(&self, identities: &[&str]) -> Result<bool, SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_app(identities).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Focus an existing launcher application after querying niri live.
    ///
    /// `Ok(false)` means the compositor answered and has no matching window;
    /// callers may then start a fresh process. Any error must keep that launch
    /// path closed, since the query or focus action could have failed.
    pub async fn focus_application(&self, identities: &[&str]) -> Result<bool, SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_application(identities).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Select the next main-keyboard layout.
    pub async fn switch_layout_next(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.switch_layout_next().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Select the previous main-keyboard layout.
    pub async fn switch_layout_prev(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.switch_layout_prev().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// End this compositor's session, not an enclosing nested session.
    pub async fn quit_compositor(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.quit_compositor().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_requires_a_niri_session_unless_explicit() {
        assert!(select("niri", "", false).is_ok());
        assert!(select("auto", "GNOME:NiRi", false).is_ok());
        assert!(select("auto", "", true).is_ok());
        assert!(select("auto", "other", true).is_err());
        assert!(select("auto", "", false).is_err());
        assert!(select("other", "niri", true).is_err());
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
