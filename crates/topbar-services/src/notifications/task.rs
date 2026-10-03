//! The one task that owns notification state.
//!
//! Everything that can change a notification — a `Notify` call, a toast timer
//! running out, a click on a close button, Do Not Disturb being switched on —
//! arrives here as a message and is applied in order by a single task. That is
//! the whole answer to v1's re-entrancy cluster: there is no callback list to
//! be mutated while it is being walked, because there are no callbacks. The
//! task publishes a snapshot and the panel renders it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::object_server::SignalEmitter;

use crate::error::SvcError;
use crate::state_store::StateStore;

use super::model::{
    Action, CloseReason, GroupView, IconSource, NotifState, NotificationView,
    PersistedNotification, PersistedNotifications, ToastView, Urgency,
};
use super::policy::{self, Admission};
use super::server::{NOTIFICATIONS_PATH, Server};

/// What the panel calls itself when it reports its own failures.
pub(super) const INTERNAL_APP: &str = "topbar";

/// A `Notify` call, parsed.
#[derive(Debug)]
pub(crate) struct Request {
    /// Display name of the sender.
    pub app_name: String,
    /// The id to update in place, or 0 for a new notification.
    pub replaces_id: u32,
    /// The headline.
    pub summary: String,
    /// The body.
    pub body: String,
    /// Action key/label pairs, in order.
    pub actions: Vec<Action>,
    /// How loudly it asks for attention.
    pub urgency: Urgency,
    /// Whether it is a banner and nothing more.
    pub transient: bool,
    /// Where its icon comes from.
    pub icon: IconSource,
    /// Milliseconds the banner asked for: positive, 0, or -1.
    pub expire_timeout: i32,
    /// Whether the panel raised this itself rather than an application.
    pub internal: bool,
}

/// Everything that can reach the state task.
#[derive(Debug)]
pub(super) enum Command {
    /// A notification arrived. Answers with the id it was given.
    Notify(Box<Request>, oneshot::Sender<u32>),
    /// Close a notification outright, banner and history alike.
    Close(u32, CloseReason),
    /// Take a banner off screen, leaving its history entry alone.
    DismissToast(u32, CloseReason),
    /// Close every notification from one application.
    ClearGroup(String),
    /// Close the whole history.
    ClearAll,
    /// Turn Do Not Disturb on or off.
    SetDnd(bool),
    /// The history has been looked at.
    MarkSeen,
    /// Hold a banner's timer while the pointer is over it.
    PauseToast(u32),
    /// Let it run again.
    ResumeToast(u32),
    /// Invoke an action and close the notification behind it.
    InvokeAction {
        /// Which notification.
        id: u32,
        /// Which action key.
        key: String,
        /// An xdg-activation token, so the app may raise itself.
        token: Option<String>,
        /// Where the outcome goes.
        reply: oneshot::Sender<Result<(), SvcError>>,
    },
    /// The daemon owns the bus name and is serving on this connection.
    Enabled(Box<Connection>),
    /// The daemon does not own the name.
    Disabled,
}

/// A banner on screen and what is left of its life.
#[derive(Debug)]
struct Toast {
    /// Which notification it shows.
    id: u32,
    /// How long it still has to live. `None` never expires.
    remaining: Option<Duration>,
    /// When it expires. `None` while paused, or when it never expires.
    deadline: Option<Instant>,
}

impl Toast {
    /// Whether the pointer is holding this banner open.
    fn is_paused(&self) -> bool {
        self.remaining.is_some() && self.deadline.is_none()
    }
}

/// A notification the daemon still knows about.
#[derive(Debug)]
struct Record {
    view: Arc<NotificationView>,
    key: String,
    transient: bool,
    internal: bool,
}

/// The daemon's whole state.
pub(super) struct Daemon {
    enabled: bool,
    dnd: bool,
    next_id: u32,
    /// Notifications in the history, newest first.
    history: Vec<u32>,
    /// Banners on screen, newest first.
    toasts: Vec<Toast>,
    /// Every notification in the history or on screen.
    records: HashMap<u32, Record>,
    /// History entries that have arrived since the panel was last opened.
    unseen: usize,
    connection: Option<Connection>,
    store: StateStore,
    publisher: watch::Sender<Arc<NotifState>>,
}

impl Daemon {
    /// Restore the daemon from what the last run left behind.
    pub(super) fn restore(
        persisted: PersistedNotifications,
        store: StateStore,
        publisher: watch::Sender<Arc<NotifState>>,
    ) -> Self {
        let mut records = HashMap::new();
        let mut history = Vec::new();
        let mut highest = 0;

        for entry in persisted.history.into_iter().take(policy::MAX_HISTORY) {
            highest = highest.max(entry.id);
            let view = entry.into_view();
            let key = policy::group_key(&view.app_name, view.icon.desktop_entry.as_deref());
            history.push(view.id);
            records.insert(
                view.id,
                Record {
                    key,
                    view: Arc::new(view),
                    transient: false,
                    internal: false,
                },
            );
        }

        debug!(
            "restored {} notification(s); Do Not Disturb is {}",
            history.len(),
            if persisted.dnd { "on" } else { "off" }
        );

        Self {
            enabled: false,
            dnd: persisted.dnd,
            // Never hand out an id the last run already used: an application
            // that outlived the panel would otherwise replace a stranger's
            // notification with its own.
            next_id: persisted.next_id.max(highest.saturating_add(1)).max(1),
            history,
            toasts: Vec::new(),
            records,
            // Everything on disk was seen before the panel restarted.
            unseen: 0,
            connection: None,
            store,
            publisher,
        }
    }

    /// Apply commands and expire banners until every handle is gone.
    pub(super) async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        self.publish();
        loop {
            let deadline = self.toasts.iter().filter_map(|toast| toast.deadline).min();
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.apply(command).await,
                    None => break,
                },
                () = sleep_until(deadline) => self.expire_due().await,
            }
            self.publish();
        }
        debug!("the notification service is shutting down");
    }

    /// Apply one command.
    async fn apply(&mut self, command: Command) {
        match command {
            Command::Notify(request, reply) => {
                let id = self.notify(*request).await;
                let _ = reply.send(id);
            }
            Command::Close(id, reason) => self.close(id, reason).await,
            Command::DismissToast(id, reason) => self.dismiss_toast(id, reason).await,
            Command::ClearGroup(key) => {
                for id in self.ids_in_group(&key) {
                    self.close(id, CloseReason::Dismissed).await;
                }
            }
            Command::ClearAll => {
                for id in self.history.clone() {
                    self.close(id, CloseReason::Dismissed).await;
                }
            }
            Command::SetDnd(dnd) => {
                if self.dnd != dnd {
                    info!("Do Not Disturb is now {}", if dnd { "on" } else { "off" });
                    self.dnd = dnd;
                    self.persist();
                }
            }
            Command::MarkSeen => self.unseen = 0,
            Command::PauseToast(id) => self.set_paused(id, true),
            Command::ResumeToast(id) => self.set_paused(id, false),
            Command::InvokeAction {
                id,
                key,
                token,
                reply,
            } => {
                let outcome = self.invoke_action(id, &key, token.as_deref()).await;
                let _ = reply.send(outcome);
            }
            Command::Enabled(connection) => {
                self.connection = Some(*connection);
                self.enabled = true;
            }
            Command::Disabled => {
                self.enabled = false;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Notifications in
    // -----------------------------------------------------------------------

    /// Take a notification, returning the id its sender should use.
    async fn notify(&mut self, request: Request) -> u32 {
        let replacing = request.replaces_id != 0 && self.records.contains_key(&request.replaces_id);
        let id = if replacing {
            request.replaces_id
        } else {
            self.take_id()
        };

        let routing = policy::route(
            request.urgency,
            request.transient,
            self.dnd,
            request.internal,
        );
        let timeout = policy::toast_timeout(request.expire_timeout, request.urgency);
        let urgency = request.urgency;
        let key = policy::group_key(&request.app_name, request.icon.desktop_entry.as_deref());

        let record = Record {
            view: Arc::new(NotificationView {
                id,
                app_name: request.app_name,
                summary: request.summary,
                body: request.body,
                actions: request.actions,
                urgency,
                icon: request.icon,
                timestamp: unix_seconds(),
            }),
            key,
            transient: request.transient,
            internal: request.internal,
        };

        debug!(
            "notification {id} from {} ({urgency:?}{}{})",
            record.view.app_name,
            if replacing { ", replacing" } else { "" },
            if record.transient { ", transient" } else { "" },
        );

        self.records.insert(id, record);

        // A replacement that has become ineligible for the history — an app
        // re-sending the same id as transient — must not leave the old entry
        // behind, and one that is still eligible keeps its place in the list.
        let in_history = self.history.contains(&id);
        match (routing.history, in_history) {
            (true, false) => {
                self.history.insert(0, id);
                self.unseen += 1;
            }
            (false, true) => self.history.retain(|entry| *entry != id),
            _ => {}
        }

        if routing.toast {
            self.show_toast(id, urgency, timeout).await;
        } else {
            self.remove_toast(id);
        }

        if routing.is_discarded() {
            // Silenced and transient: it was never shown and never recorded,
            // so its sender is told it is gone rather than left waiting.
            self.close(id, CloseReason::Undefined).await;
            return id;
        }

        self.evict_overflow().await;
        if routing.history {
            self.persist();
        }
        id
    }

    /// The next free notification id. Never 0, which the protocol reserves.
    fn take_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Put a banner on screen, or refresh the one already there.
    async fn show_toast(&mut self, id: u32, urgency: Urgency, timeout: Option<Duration>) {
        // A replacement restarts the timer where it stands rather than
        // jumping to the top: an app updating a progress notification would
        // otherwise make the stack dance.
        if let Some(toast) = self.toasts.iter_mut().find(|toast| toast.id == id) {
            toast.remaining = timeout;
            toast.deadline = timeout.map(|timeout| Instant::now() + timeout);
            return;
        }

        let stack: Vec<(u32, Urgency)> = self
            .toasts
            .iter()
            .map(|toast| {
                let urgency = self
                    .records
                    .get(&toast.id)
                    .map_or(Urgency::Normal, |record| record.view.urgency);
                (toast.id, urgency)
            })
            .collect();
        let urgencies: Vec<Urgency> = stack.iter().map(|(_, urgency)| *urgency).collect();

        match policy::admit(&urgencies, urgency) {
            Admission::Room => {}
            Admission::Replace(index) => {
                let pushed_out = stack[index].0;
                debug!("banner {pushed_out} makes way for critical notification {id}");
                self.dismiss_toast(pushed_out, CloseReason::Undefined).await;
            }
            Admission::Full => {
                debug!("no banner for notification {id}: the stack is full");
                return;
            }
        }

        self.toasts.insert(
            0,
            Toast {
                id,
                remaining: timeout,
                deadline: timeout.map(|timeout| Instant::now() + timeout),
            },
        );
    }

    // -----------------------------------------------------------------------
    // Notifications out
    // -----------------------------------------------------------------------

    /// Close a notification for good and tell its sender why.
    async fn close(&mut self, id: u32, reason: CloseReason) {
        if self.records.remove(&id).is_none() {
            return;
        }
        let was_in_history = self.history.contains(&id);
        self.history.retain(|entry| *entry != id);
        self.remove_toast(id);
        self.unseen = self.unseen.min(self.history.len());

        self.emit_closed(id, reason).await;
        if was_in_history {
            self.persist();
        }
    }

    /// Take a banner off screen. The history entry, if there is one, stays.
    ///
    /// This is what a banner timing out means: GNOME leaves the notification
    /// in the list, and so does the panel. A notification with no history
    /// entry has nowhere else to be, so it is closed instead.
    async fn dismiss_toast(&mut self, id: u32, reason: CloseReason) {
        if !self.remove_toast(id) {
            return;
        }
        if !self.history.contains(&id) {
            self.close(id, reason).await;
        }
    }

    /// Drop `id` from the stack, reporting whether it was there.
    fn remove_toast(&mut self, id: u32) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|toast| toast.id != id);
        self.toasts.len() != before
    }

    /// Retire every banner whose time is up.
    async fn expire_due(&mut self) {
        let now = Instant::now();
        let due: Vec<u32> = self
            .toasts
            .iter()
            .filter(|toast| toast.deadline.is_some_and(|deadline| deadline <= now))
            .map(|toast| toast.id)
            .collect();
        for id in due {
            self.dismiss_toast(id, CloseReason::Expired).await;
        }
    }

    /// Hold or release a banner's timer.
    fn set_paused(&mut self, id: u32, paused: bool) {
        let Some(toast) = self.toasts.iter_mut().find(|toast| toast.id == id) else {
            return;
        };
        match (paused, toast.deadline) {
            // Pausing: bank what is left so resuming does not restart it.
            (true, Some(deadline)) => {
                let left = deadline.saturating_duration_since(Instant::now());
                // Logged because it is otherwise invisible: the pointer is on
                // the banner, nothing on screen changes, and the only evidence
                // that the timer stopped is the banner still being there a
                // minute later. A smoke run reads this line.
                debug!("banner {id} paused with {}ms left", left.as_millis());
                toast.remaining = Some(left);
                toast.deadline = None;
            }
            (false, None) => {
                toast.deadline = toast.remaining.map(|left| {
                    debug!("banner {id} resumed with {}ms left", left.as_millis());
                    Instant::now() + left
                });
            }
            _ => {}
        }
    }

    /// Push the oldest history entries out once the cap is exceeded.
    async fn evict_overflow(&mut self) {
        let flat = self.flat_history();
        for id in policy::overflow(&flat) {
            debug!("notification {id} evicted: the history is full");
            self.close(id, CloseReason::Undefined).await;
        }
    }

    /// Run an action and close the notification it belonged to.
    async fn invoke_action(
        &mut self,
        id: u32,
        key: &str,
        token: Option<&str>,
    ) -> Result<(), SvcError> {
        let Some(record) = self.records.get(&id) else {
            return Err(SvcError::GoneNotification(id));
        };
        if !record.view.actions.iter().any(|action| action.key == key) {
            warn!(
                "notification {id} from {} does not offer the action `{key}`",
                record.view.app_name
            );
        }

        // The token goes first: the specification has the client read it
        // before it acts on ActionInvoked, so sending it afterwards would be
        // a race the application always loses.
        if let Some(emitter) = self.emitter() {
            if let Some(token) = token
                && let Err(error) = Server::activation_token(&emitter, id, token).await
            {
                warn!("could not send the activation token for notification {id}: {error}");
            }
            if let Err(error) = Server::action_invoked(&emitter, id, key).await {
                warn!("could not report the action on notification {id}: {error}");
            }
        }

        self.close(id, CloseReason::Dismissed).await;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Publishing and persistence
    // -----------------------------------------------------------------------

    /// The history as `(group key, notification)` pairs, newest first.
    fn flat_history(&self) -> Vec<(String, Arc<NotificationView>)> {
        self.history
            .iter()
            .filter_map(|id| self.records.get(id))
            .map(|record| (record.key.clone(), Arc::clone(&record.view)))
            .collect()
    }

    /// Every id belonging to one group, newest first.
    fn ids_in_group(&self, key: &str) -> Vec<u32> {
        self.history
            .iter()
            .filter(|id| self.records.get(id).is_some_and(|record| record.key == key))
            .copied()
            .collect()
    }

    /// Publish the snapshot, unless nothing the panel draws has changed.
    fn publish(&self) {
        let toasts: Vec<ToastView> = self
            .toasts
            .iter()
            .filter_map(|toast| {
                self.records.get(&toast.id).map(|record| ToastView {
                    notification: Arc::clone(&record.view),
                    paused: toast.is_paused(),
                })
            })
            .collect();

        let history: Vec<GroupView> = policy::group(&self.flat_history());
        let state = NotifState {
            enabled: self.enabled,
            dnd: self.dnd,
            toasts,
            history,
            unseen_count: self.unseen,
        };

        self.publisher.send_if_modified(|current| {
            if **current == state {
                return false;
            }
            *current = Arc::new(state);
            true
        });
    }

    /// Queue the history and the Do Not Disturb flag for the state file.
    fn persist(&self) {
        let notifications = PersistedNotifications {
            dnd: self.dnd,
            next_id: self.next_id,
            history: self
                .history
                .iter()
                .filter_map(|id| self.records.get(id))
                .filter(|record| !record.internal)
                .map(|record| PersistedNotification::from_view(&record.view))
                .collect(),
        };
        self.store
            .update(move |state| state.notifications = notifications);
    }

    // -----------------------------------------------------------------------
    // Signals
    // -----------------------------------------------------------------------

    /// Tell a sender its notification is gone.
    async fn emit_closed(&self, id: u32, reason: CloseReason) {
        let Some(emitter) = self.emitter() else {
            return;
        };
        if let Err(error) = Server::notification_closed(&emitter, id, reason.to_wire()).await {
            warn!("could not report notification {id} as closed: {error}");
        }
    }

    /// Something to emit signals through, if there is a bus at all.
    ///
    /// Built per emission rather than cached: it is a connection handle and a
    /// path, and signals are rare compared with everything else here.
    fn emitter(&self) -> Option<SignalEmitter<'static>> {
        let connection = self.connection.as_ref()?;
        match SignalEmitter::new(connection, NOTIFICATIONS_PATH) {
            Ok(emitter) => Some(emitter),
            Err(error) => {
                warn!("could not address the notification interface: {error}");
                None
            }
        }
    }
}

/// Sleep until `deadline`, or forever when there is nothing to wait for.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

/// Now, in seconds since the Unix epoch.
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(0))
}
