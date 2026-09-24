//! The panel's compositor boundary. Backends publish directly into the same projections.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use crate::error::SvcError;
use crate::hyprland::{Hyprland, HyprlandHandle};
use crate::niri::{Niri, NiriHandle};

/// Stable native identity, never a dispatcher-relative workspace selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceId {
    /// Niri's unsigned native ID.
    Niri(u64),
    /// Hyprland's signed native ID (named workspaces can be negative).
    Hyprland(i64),
}

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

/// Selected compositor service. Selection failures never silently choose a backend.
#[derive(Clone)]
pub enum Compositor {
    /// Native Niri service, with its replay protocol unchanged.
    Niri(Niri),
    /// Native Hyprland service.
    Hyprland(Hyprland),
    /// Ambiguous or unavailable desktop identity.
    Disconnected(String),
}

/// Cheap action handle to exactly one selected compositor.
#[derive(Clone)]
pub enum CompositorHandle {
    /// Niri actions.
    Niri(NiriHandle),
    /// Hyprland actions.
    Hyprland(HyprlandHandle),
    /// A selection error, retained for actionable click failures.
    Disconnected(String),
}

/// Resolve desktop identity before endpoints; never scan the runtime directory.
fn select<'a>(
    configured: &'a str,
    desktop: &str,
    niri: bool,
    hyprland: bool,
) -> Result<&'a str, SvcError> {
    if matches!(configured, "niri" | "hyprland") {
        return Ok(configured);
    }
    if configured != "auto" {
        return Err(SvcError::CompositorSelection(format!(
            "unknown backend {configured}"
        )));
    }
    let mut markers = (false, false);
    for token in desktop.split(|c: char| c == ':' || c == ';' || c.is_whitespace()) {
        if token.eq_ignore_ascii_case("niri") {
            markers.0 = true;
        }
        if token.eq_ignore_ascii_case("hyprland") {
            markers.1 = true;
        }
    }
    match markers {
        (true, false) => Ok("niri"),
        (false, true) => Ok("hyprland"),
        (true, true) => Err(SvcError::CompositorSelection("XDG_CURRENT_DESKTOP names both niri and Hyprland; set advanced.compositor explicitly".into())),
        (false, false) => match (niri, hyprland) {
            (true, false) => Ok("niri"),
            (false, true) => Ok("hyprland"),
            _ => Err(SvcError::CompositorSelection("no unambiguous compositor: set XDG_CURRENT_DESKTOP or advanced.compositor and export the selected session's endpoint".into())),
        },
    }
}

impl Compositor {
    pub(crate) fn start(configured: &str) -> Self {
        let niri = std::env::var_os("NIRI_SOCKET")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .ok()
            .filter(|s| !s.is_empty());
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        match select(configured, &desktop, niri.is_some(), signature.is_some()) {
            Ok("niri") => Self::Niri(Niri::start(niri)),
            Ok("hyprland") => Self::Hyprland(Hyprland::start(crate::hyprland::socket_dir(
                std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
                signature.as_deref(),
            ))),
            Ok(_) => unreachable!("selection only returns implemented backends"),
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
            Self::Hyprland(_) => Some("hyprland"),
            Self::Disconnected(_) => None,
        }
    }

    /// Actions address this backend only.
    pub fn handle(&self) -> CompositorHandle {
        match self {
            Self::Niri(service) => CompositorHandle::Niri(service.handle().clone()),
            Self::Hyprland(service) => CompositorHandle::Hyprland(service.handle().clone()),
            Self::Disconnected(error) => CompositorHandle::Disconnected(error.clone()),
        }
    }

    /// Subscribe without a forwarding task or snapshot copy.
    pub fn workspaces(&self) -> watch::Receiver<Arc<WorkspacesSnapshot>> {
        match self {
            Self::Niri(service) => service.workspaces(),
            Self::Hyprland(service) => service.workspaces(),
            Self::Disconnected(_) => watch::channel(Arc::new(WorkspacesSnapshot::default())).1,
        }
    }

    /// Subscribe to the main keyboard's layout.
    pub fn keyboard_layout(&self) -> watch::Receiver<Arc<KeyboardLayoutSnapshot>> {
        match self {
            Self::Niri(service) => service.keyboard_layout(),
            Self::Hyprland(service) => service.keyboard_layout(),
            Self::Disconnected(_) => watch::channel(Arc::new(KeyboardLayoutSnapshot::default())).1,
        }
    }

    /// Interrupt reconnect backoff after resume.
    pub fn health_check(&self) {
        match self {
            Self::Niri(service) => service.health_check(),
            Self::Hyprland(service) => service.health_check(),
            Self::Disconnected(_) => {}
        }
    }
}

impl CompositorHandle {
    /// Focus the captured native ID; cross-backend IDs are errors.
    pub async fn focus_workspace(&self, id: WorkspaceId) -> Result<(), SvcError> {
        match (self, id) {
            (Self::Niri(handle), WorkspaceId::Niri(id)) => handle.focus_workspace(id).await,
            (Self::Hyprland(handle), WorkspaceId::Hyprland(id)) => handle.focus_workspace(id).await,
            (Self::Disconnected(error), _) => Err(SvcError::CompositorSelection(error.clone())),
            _ => Err(SvcError::Rejected(
                "workspace ID belongs to another compositor".into(),
            )),
        }
    }

    /// Raise the first matching normalized application identity.
    pub async fn focus_app(&self, identities: &[&str]) -> Result<bool, SvcError> {
        match self {
            Self::Niri(handle) => handle.focus_app(identities).await,
            Self::Hyprland(handle) => handle.focus_app(identities).await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Select the next main-keyboard layout.
    pub async fn switch_layout_next(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.switch_layout_next().await,
            Self::Hyprland(handle) => handle.switch_layout_next().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// Select the previous main-keyboard layout.
    pub async fn switch_layout_prev(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.switch_layout_prev().await,
            Self::Hyprland(handle) => handle.switch_layout_prev().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }

    /// End this compositor's session, not an enclosing nested session.
    pub async fn quit_compositor(&self) -> Result<(), SvcError> {
        match self {
            Self::Niri(handle) => handle.quit_compositor().await,
            Self::Hyprland(handle) => handle.quit_compositor().await,
            Self::Disconnected(error) => Err(SvcError::CompositorSelection(error.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_never_falls_back_from_explicit_or_desktop_identity() {
        assert_eq!(select("niri", "Hyprland", false, true).unwrap(), "niri");
        assert_eq!(select("hyprland", "niri", true, false).unwrap(), "hyprland");
        assert_eq!(
            select("auto", "GNOME:NiRi:niri", false, true).unwrap(),
            "niri"
        );
        assert_eq!(select("auto", "Hyprland", true, false).unwrap(), "hyprland");
        assert!(select("auto", "niri:Hyprland", true, false).is_err());
        assert!(select("auto", "not-niri", false, false).is_err());
        assert!(select("auto", "", true, true).is_err());
        assert_eq!(select("auto", "", false, true).unwrap(), "hyprland");
    }

    #[tokio::test]
    async fn native_id_domains_cannot_cross() {
        let handle = CompositorHandle::Niri(NiriHandle::new(None));
        assert!(matches!(
            handle.focus_workspace(WorkspaceId::Hyprland(-9)).await,
            Err(SvcError::Rejected(_))
        ));
    }
}
