//! The notification daemon: `org.freedesktop.Notifications`, served by the panel.
//!
//! ```text
//!   applications ──Notify──▶ server.rs (zbus interface)
//!                               │ Command
//!                               ▼
//!   panel widgets ◀──watch─── task.rs (the only owner of state)
//!                               │ ▲
//!                               │ └── policy.rs: every rule, pure
//!                               ▼
//!                            state_store.rs (history + DND on disk)
//! ```
//!
//! The panel takes the name with `ReplaceExisting`, so starting it takes over
//! from whatever daemon was running. Failing to take it is not fatal: the
//! history still shows what was persisted, the widgets say so, and
//! [`Notifications::startup`] hands the reason to the panel's one failure sink.

mod hints;
mod model;
mod policy;
mod server;
mod task;

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use crate::error::SvcError;
use crate::state_store::StateStore;

pub use model::{
    Action, CloseReason, GroupView, IconSource, ImageData, NotifState, NotificationView,
    PersistedNotification, PersistedNotifications, ToastView, Urgency,
};
pub use policy::{DEFAULT_TIMEOUT, MAX_HISTORY, MAX_TOASTS};

use server::{NOTIFICATIONS_NAME, Ownership};
use task::{Command, Daemon, INTERNAL_APP, Request};

/// How many commands may be in flight before a sender waits.
///
/// Generous: a burst of notifications during a build should never make an
/// application block in `Notify`.
const QUEUE: usize = 64;

/// The notification service.
///
/// Cloning is cheap — a channel sender and a watch subscription — so every bar
/// and widget can hold its own copy.
#[derive(Clone)]
pub struct Notifications {
    handle: NotificationsHandle,
    state: watch::Receiver<Arc<NotifState>>,
    ownership: watch::Receiver<Ownership>,
}

impl Notifications {
    /// Start the daemon, restoring `persisted` from the last run.
    ///
    /// `address` overrides the session bus. Production passes `None`; tests
    /// pass a private bus, which is the only thing that keeps a test run from
    /// stealing the name from the user's live desktop.
    pub(crate) fn start(
        persisted: PersistedNotifications,
        store: StateStore,
        address: Option<String>,
    ) -> Self {
        let (this, commands, ownership) = Self::spawn(persisted, store);
        tokio::spawn(server::serve(commands, ownership, address));
        this
    }

    /// Start the state machine with no bus behind it.
    ///
    /// Every behaviour test uses this. It is the reason a `cargo test` run can
    /// never take `org.freedesktop.Notifications` away from the desktop the
    /// developer is sitting in front of: the code that requests the name is
    /// simply not reachable from here.
    #[cfg(test)]
    pub(crate) fn detached(persisted: PersistedNotifications, store: StateStore) -> Self {
        Self::spawn(persisted, store).0
    }

    /// Start the state task, handing back the ends the bus half needs.
    fn spawn(
        persisted: PersistedNotifications,
        store: StateStore,
    ) -> (Self, mpsc::Sender<Command>, watch::Sender<Ownership>) {
        let (commands, queue) = mpsc::channel(QUEUE);
        let (publisher, state) = watch::channel(Arc::new(NotifState::default()));
        let (owner_tx, ownership) = watch::channel(Ownership::Pending);

        tokio::spawn(Daemon::restore(persisted, store, publisher).run(queue));

        let notifications = Self {
            handle: NotificationsHandle {
                commands: commands.clone(),
            },
            state,
            ownership,
        };
        (notifications, commands, owner_tx)
    }

    /// The handle commands are sent through.
    pub fn handle(&self) -> &NotificationsHandle {
        &self.handle
    }

    /// Subscribe to notification state.
    pub fn state(&self) -> watch::Receiver<Arc<NotifState>> {
        self.state.clone()
    }

    /// Resolve once the daemon knows whether it owns the bus name.
    ///
    /// Written to be handed straight to the panel's single failure sink, so
    /// "another notification daemon is already running" reaches the user the
    /// same way every other failure does.
    pub async fn startup(&self) -> Result<(), SvcError> {
        let mut ownership = self.ownership.clone();
        loop {
            let outcome = ownership.borrow_and_update().clone();
            match outcome {
                Ownership::Pending => {}
                Ownership::Owned => return Ok(()),
                Ownership::Bus(detail) => return Err(SvcError::Bus(detail)),
                Ownership::Taken => {
                    return Err(SvcError::NameTaken(NOTIFICATIONS_NAME.to_string()));
                }
            }
            if ownership.changed().await.is_err() {
                // The service is gone; there is nothing left to report.
                return Ok(());
            }
        }
    }
}

/// Commands the panel sends the notification daemon.
///
/// Every mutating call returns a `Result` so failures are reported rather than
/// dropped in a click handler — see `bridge::act` on the GTK side.
#[derive(Clone)]
pub struct NotificationsHandle {
    commands: mpsc::Sender<Command>,
}

impl NotificationsHandle {
    /// Close a notification and tell its sender why.
    pub async fn dismiss(&self, id: u32, reason: CloseReason) -> Result<(), SvcError> {
        self.send(Command::Close(id, reason)).await
    }

    /// Take a banner off screen, leaving its history entry alone.
    pub async fn dismiss_toast(&self, id: u32) -> Result<(), SvcError> {
        self.send(Command::DismissToast(id, CloseReason::Dismissed))
            .await
    }

    /// Close every notification from one application.
    pub async fn clear_group(&self, key: String) -> Result<(), SvcError> {
        self.send(Command::ClearGroup(key)).await
    }

    /// Close the whole history.
    pub async fn clear_all(&self) -> Result<(), SvcError> {
        self.send(Command::ClearAll).await
    }

    /// Turn Do Not Disturb on or off. Persisted across restarts.
    pub async fn set_dnd(&self, dnd: bool) -> Result<(), SvcError> {
        self.send(Command::SetDnd(dnd)).await
    }

    /// Record that the user has looked at the history.
    pub async fn mark_seen(&self) -> Result<(), SvcError> {
        self.send(Command::MarkSeen).await
    }

    /// Hold a banner's timer while the pointer is over it.
    pub async fn pause_toast(&self, id: u32) -> Result<(), SvcError> {
        self.send(Command::PauseToast(id)).await
    }

    /// Let a paused banner's timer run again.
    pub async fn resume_toast(&self, id: u32) -> Result<(), SvcError> {
        self.send(Command::ResumeToast(id)).await
    }

    /// Invoke an action and close the notification behind it.
    ///
    /// `token` is an xdg-activation token obtained from the compositor, which
    /// is what lets the receiving application raise its own window; without
    /// one the action still fires, it just cannot steal focus.
    pub async fn invoke_action(
        &self,
        id: u32,
        key: String,
        token: Option<String>,
    ) -> Result<(), SvcError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::InvokeAction {
            id,
            key,
            token,
            reply,
        })
        .await?;
        answer
            .await
            .map_err(|_| SvcError::ServiceStopped("notifications"))?
    }

    /// Raise the panel's own notification: a toast, never a history entry.
    ///
    /// This is how `bridge::report` puts a failure on screen. It bypasses Do
    /// Not Disturb, because an error the user cannot see is an error that did
    /// not happen, and it works whether or not the daemon owns the bus name.
    pub async fn report(&self, summary: String, body: String) -> Result<(), SvcError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Notify(
            Box::new(Request {
                app_name: INTERNAL_APP.to_string(),
                replaces_id: 0,
                summary,
                body,
                actions: Vec::new(),
                urgency: Urgency::Normal,
                transient: true,
                icon: IconSource {
                    app_icon: "dialog-warning-symbolic".to_string(),
                    ..IconSource::default()
                },
                expire_timeout: -1,
                internal: true,
            }),
            reply,
        ))
        .await?;
        let _ = answer.await;
        Ok(())
    }

    /// Post a notification as if it had arrived on the bus.
    ///
    /// Only the behaviour tests use this; applications go through the zbus
    /// interface, which builds the very same request.
    #[cfg(test)]
    pub(crate) async fn deliver(&self, request: Request) -> Result<u32, SvcError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Notify(Box::new(request), reply)).await?;
        answer
            .await
            .map_err(|_| SvcError::ServiceStopped("notifications"))
    }

    /// Post a command, or report that the service has stopped.
    async fn send(&self, command: Command) -> Result<(), SvcError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| SvcError::ServiceStopped("notifications"))
    }
}

#[cfg(test)]
mod bus_tests;
#[cfg(test)]
mod tests;
