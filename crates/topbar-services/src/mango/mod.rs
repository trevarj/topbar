//! Stock Mango 0.18.2 IPC: paired full-snapshot watches and fresh action sockets.

mod requests;
mod snapshot;
mod stream;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use topbar_core::config::MangoConfig;

use crate::compositor::{KeyboardLayoutSnapshot, WindowsSnapshot, WorkspacesSnapshot};
use crate::error::SvcError;
pub use requests::MangoHandle;

/// Mango projections shared by all bars, without any GTK dependency.
#[derive(Clone)]
pub struct Mango {
    handle: MangoHandle,
    workspaces: watch::Receiver<Arc<WorkspacesSnapshot>>,
    keyboard_layout: watch::Receiver<Arc<KeyboardLayoutSnapshot>>,
    windows: watch::Receiver<Arc<WindowsSnapshot>>,
    kicks: tokio::sync::mpsc::Sender<()>,
}

impl Mango {
    pub(crate) fn start(socket: Option<PathBuf>, metadata: MangoConfig) -> Self {
        let config = Arc::new(metadata);
        let registry = Arc::new(Mutex::new(snapshot::Registry::default()));
        let (workspaces_tx, workspaces) = watch::channel(Arc::new(WorkspacesSnapshot::default()));
        let (keyboard_tx, keyboard_layout) = watch::channel(Arc::new(KeyboardLayoutSnapshot {
            layout_count: config.keyboard_layouts.len(),
            ..KeyboardLayoutSnapshot::default()
        }));
        let (windows_tx, windows) = watch::channel(Arc::new(WindowsSnapshot::default()));
        let (kicks, kick_queue) = tokio::sync::mpsc::channel(1);
        if let Some(path) = socket.clone() {
            tokio::spawn(stream::run(
                path,
                config.clone(),
                registry.clone(),
                stream::Publishers {
                    workspaces: workspaces_tx,
                    keyboard_layout: keyboard_tx,
                    windows: windows_tx,
                },
                kick_queue,
            ));
        } else {
            tracing::error!("no Mango instance socket; compositor widgets remain disconnected");
        }
        Self {
            handle: MangoHandle::new(socket, config, registry),
            workspaces,
            keyboard_layout,
            windows,
            kicks,
        }
    }

    /// Reconnect both watches after resume, interrupting any reconnect backoff.
    pub fn health_check(&self) {
        let _ = self.kicks.try_send(());
    }

    /// Actions address only this selected Mango instance.
    pub fn handle(&self) -> &MangoHandle {
        &self.handle
    }

    /// Subscribe to the workspace projection.
    pub fn workspaces(&self) -> watch::Receiver<Arc<WorkspacesSnapshot>> {
        self.workspaces.clone()
    }

    /// Subscribe to the keyboard-language projection.
    pub fn keyboard_layout(&self) -> watch::Receiver<Arc<KeyboardLayoutSnapshot>> {
        self.keyboard_layout.clone()
    }

    /// Subscribe to live launcher window results.
    pub fn windows(&self) -> watch::Receiver<Arc<WindowsSnapshot>> {
        self.windows.clone()
    }
}

pub(crate) async fn focused_output_connector(
    socket: Option<PathBuf>,
) -> Result<Option<String>, SvcError> {
    requests::focused_output_connector(socket).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_instance_is_truthfully_disconnected() {
        let service = Mango::start(None, MangoConfig::default());
        assert!(!service.workspaces().borrow().connected);
        assert!(!service.windows().borrow().connected);
        assert!(!service.keyboard_layout().borrow().connected);
        assert!(service.handle().quit_compositor().await.is_err());
    }
}
