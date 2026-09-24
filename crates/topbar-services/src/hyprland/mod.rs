//! Hyprland 0.56.2 Lua-session IPC. Never scans for another instance.

mod requests;
mod snapshot;
mod stream;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::SvcError;
use crate::compositor::{KeyboardLayoutSnapshot, WorkspacesSnapshot};

pub use requests::HyprlandHandle;

/// The session's fixed socket directory. A new instance needs a supervised restart.
pub(crate) fn socket_dir(runtime: Option<PathBuf>, signature: Option<&str>) -> Option<PathBuf> {
    let runtime = runtime.filter(|path| path.is_absolute())?;
    let signature = signature.filter(|s| {
        !s.is_empty() && *s != "." && *s != ".." && !s.contains('/') && !s.contains('\0')
    })?;
    Some(runtime.join("hypr").join(signature))
}

/// Native service with one event stream and fresh command connections.
#[derive(Clone)]
pub struct Hyprland {
    handle: HyprlandHandle,
    workspaces: watch::Receiver<Arc<WorkspacesSnapshot>>,
    keyboard_layout: watch::Receiver<Arc<KeyboardLayoutSnapshot>>,
    kicks: mpsc::Sender<()>,
}

impl Hyprland {
    pub(crate) fn start(directory: Option<PathBuf>) -> Self {
        let (workspace_tx, workspaces) = watch::channel(Arc::new(WorkspacesSnapshot::default()));
        let (keyboard_tx, keyboard_layout) =
            watch::channel(Arc::new(KeyboardLayoutSnapshot::default()));
        let (kicks, queue) = mpsc::channel(1);
        let (live, _) = watch::channel(requests::Live::default());
        let handle = HyprlandHandle::new(directory.clone(), live.clone());
        if let Some(directory) = directory {
            tokio::spawn(stream::run(
                directory,
                handle.clone(),
                live,
                workspace_tx,
                keyboard_tx,
                queue,
            ));
        } else {
            tracing::error!("{}", SvcError::NoHyprlandSocket);
        }
        Self {
            handle,
            workspaces,
            keyboard_layout,
            kicks,
        }
    }

    /// Actions targeting this instance only.
    pub fn handle(&self) -> &HyprlandHandle {
        &self.handle
    }

    /// Subscribe to complete workspace projections.
    pub fn workspaces(&self) -> watch::Receiver<Arc<WorkspacesSnapshot>> {
        self.workspaces.clone()
    }

    /// Subscribe to main-keyboard layout projections.
    pub fn keyboard_layout(&self) -> watch::Receiver<Arc<KeyboardLayoutSnapshot>> {
        self.keyboard_layout.clone()
    }

    /// Wake the stream immediately on resume, coalescing duplicate kicks.
    pub fn health_check(&self) {
        let _ = self.kicks.try_send(());
    }
}
