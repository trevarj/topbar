//! `org.freedesktop.Notifications`, served with zbus.
//!
//! The interface is a thin shell: it parses arguments into the daemon's own
//! types and forwards them to the state task, which is the only thing that
//! knows what a notification means. `Notify` waits for the task to answer with
//! an id — zbus dispatches each call on its own task, so awaiting here does
//! not hold up any other method.

use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::zvariant::OwnedValue;
use zbus::{Connection, object_server::SignalEmitter};

use futures_util::StreamExt;

use super::hints::Hints;
use super::model::{Action, CloseReason, IconSource};
use super::task::{Command, Request};

/// The well-known name a notification daemon owns.
pub(super) const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
/// The object path it serves.
pub(super) const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// What the panel tells applications about itself.
const SERVER_NAME: &str = "topbar";
/// Who wrote it.
const VENDOR: &str = "trevarj";
/// The specification version implemented.
const SPEC_VERSION: &str = "1.2";

/// What the daemon can do, in the specification's vocabulary.
///
/// Carried over from v1 unchanged. `action-icons` is deliberately absent: the
/// panel draws action labels, not icons, and claiming otherwise makes senders
/// send an icon name where a label belongs.
pub(super) const CAPABILITIES: &[&str] = &[
    "body",
    "body-markup",
    "actions",
    "persistence",
    "icon-static",
];

/// The interface object registered on the bus.
pub(super) struct Server {
    commands: mpsc::Sender<Command>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    /// Post a notification, or replace one already posted.
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let hints = Hints::parse(&hints);
        let request = Request {
            app_name: display_name(app_name),
            replaces_id,
            summary,
            body,
            actions: pair_actions(&actions),
            urgency: hints.urgency,
            transient: hints.transient,
            icon: IconSource {
                image_data: hints.image_data,
                image_path: hints.image_path,
                app_icon,
                desktop_entry: hints.desktop_entry,
            },
            expire_timeout,
            internal: false,
        };

        let (reply, answer) = oneshot::channel();
        if self
            .commands
            .send(Command::Notify(Box::new(request), reply))
            .await
            .is_err()
        {
            warn!("a notification arrived after the service stopped");
            return 0;
        }
        answer.await.unwrap_or(0)
    }

    /// Withdraw a notification the sender posted earlier.
    async fn close_notification(&self, id: u32) {
        let _ = self
            .commands
            .send(Command::Close(id, CloseReason::Requested))
            .await;
    }

    /// What this daemon supports.
    fn get_capabilities(&self) -> Vec<&'static str> {
        CAPABILITIES.to_vec()
    }

    /// Who this daemon is.
    fn get_server_information(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        (SERVER_NAME, VENDOR, env!("CARGO_PKG_VERSION"), SPEC_VERSION)
    }

    /// A notification is gone, and why.
    #[zbus(signal)]
    pub(super) async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    /// The user picked one of a notification's actions.
    #[zbus(signal)]
    pub(super) async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    /// An xdg-activation token, so the sender may raise its own window.
    ///
    /// Sent immediately before `ActionInvoked`, which is the order the
    /// specification requires: a client reads the token, then acts on the
    /// action.
    #[zbus(signal)]
    pub(super) async fn activation_token(
        emitter: &SignalEmitter<'_>,
        id: u32,
        activation_token: &str,
    ) -> zbus::Result<()>;
}

/// Why the daemon is or is not serving notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ownership {
    /// Still trying.
    Pending,
    /// The panel owns the name.
    Owned,
    /// The session bus could not be reached.
    Bus(String),
    /// Another notification daemon owns the name and would not give it up.
    Taken,
}

/// Connect, serve the interface, and hold the name for as long as we can.
///
/// `address` overrides the session bus, which is how the integration tests
/// reach a private bus instead of the user's live one.
pub(super) async fn serve(
    commands: mpsc::Sender<Command>,
    ownership: tokio::sync::watch::Sender<Ownership>,
    address: Option<String>,
) {
    let built = match address {
        Some(address) => zbus::connection::Builder::address(address.as_str()),
        None => zbus::connection::Builder::session(),
    };
    let connection = match connect(built, &commands).await {
        Ok(connection) => connection,
        Err(error) => {
            error!("could not serve notifications: {error}");
            let _ = ownership.send(Ownership::Bus(error.to_string()));
            return;
        }
    };

    // ReplaceExisting takes over from whatever daemon is running — the panel
    // is the session's shell, so it wins. AllowReplacement returns the
    // courtesy, and DoNotQueue means a refusal is reported now rather than
    // silently arriving twenty minutes later.
    let flags = RequestNameFlags::ReplaceExisting
        | RequestNameFlags::AllowReplacement
        | RequestNameFlags::DoNotQueue;

    match connection
        .request_name_with_flags(NOTIFICATIONS_NAME, flags)
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            info!("serving {NOTIFICATIONS_NAME}");
            let _ = commands
                .send(Command::Enabled(Box::new(connection.clone())))
                .await;
            let _ = ownership.send(Ownership::Owned);
        }
        Ok(reply) => {
            warn!("another notification daemon owns {NOTIFICATIONS_NAME} ({reply:?})");
            let _ = ownership.send(Ownership::Taken);
            return;
        }
        Err(zbus::Error::NameTaken) => {
            warn!("another notification daemon owns {NOTIFICATIONS_NAME}");
            let _ = ownership.send(Ownership::Taken);
            return;
        }
        Err(error) => {
            error!("could not request {NOTIFICATIONS_NAME}: {error}");
            let _ = ownership.send(Ownership::Bus(error.to_string()));
            return;
        }
    }

    watch_ownership(connection, commands).await;
}

/// Build the connection and register the interface on it.
async fn connect(
    builder: zbus::Result<zbus::connection::Builder<'_>>,
    commands: &mpsc::Sender<Command>,
) -> zbus::Result<Connection> {
    builder?
        .serve_at(
            NOTIFICATIONS_PATH,
            Server {
                commands: commands.clone(),
            },
        )?
        .build()
        .await
}

/// Follow the name for the rest of the session.
///
/// Losing it is survivable: the panel keeps its history and its widgets, and
/// simply stops receiving new notifications until it gets the name back.
async fn watch_ownership(connection: Connection, commands: mpsc::Sender<Command>) {
    let proxy = match DBusProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(error) => {
            warn!("cannot follow {NOTIFICATIONS_NAME} ownership: {error}");
            return;
        }
    };

    let mut changes = match proxy.receive_name_owner_changed().await {
        Ok(changes) => changes,
        Err(error) => {
            warn!("cannot follow {NOTIFICATIONS_NAME} ownership: {error}");
            return;
        }
    };
    let me = connection.unique_name().map(ToString::to_string);

    while let Some(change) = changes.next().await {
        let Ok(args) = change.args() else {
            continue;
        };
        if args.name().as_str() != NOTIFICATIONS_NAME {
            continue;
        }
        let owner = args.new_owner().as_ref().map(ToString::to_string);
        let mine = owner.is_some() && owner == me;
        debug!("{NOTIFICATIONS_NAME} is now owned by {owner:?}");

        let command = if mine {
            Command::Enabled(Box::new(connection.clone()))
        } else {
            warn!("lost {NOTIFICATIONS_NAME}; notifications are disabled");
            Command::Disabled
        };
        if commands.send(command).await.is_err() {
            break;
        }
    }
}

/// An application with no name of its own still needs a group header.
fn display_name(app_name: String) -> String {
    if app_name.trim().is_empty() {
        "Unknown".to_string()
    } else {
        app_name
    }
}

/// Fold the flat `[key, label, key, label]` array into pairs.
///
/// A trailing key with no label is dropped: a button with no text is not
/// something the user could choose.
pub(super) fn pair_actions(actions: &[String]) -> Vec<Action> {
    actions
        .chunks_exact(2)
        .map(|pair| Action {
            key: pair[0].clone(),
            label: pair[1].clone(),
        })
        .filter(|action| !action.key.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_folded_into_key_label_pairs() {
        let actions = [
            "default".to_string(),
            String::new(),
            "reply".to_string(),
            "Reply".to_string(),
        ];
        assert_eq!(
            pair_actions(&actions),
            vec![
                Action {
                    key: "default".into(),
                    label: String::new()
                },
                Action {
                    key: "reply".into(),
                    label: "Reply".into()
                },
            ]
        );
    }

    #[test]
    fn a_trailing_key_with_no_label_is_dropped() {
        let actions = [
            "reply".to_string(),
            "Reply".to_string(),
            "orphan".to_string(),
        ];
        assert_eq!(pair_actions(&actions).len(), 1);
        assert!(pair_actions(&[]).is_empty());
    }

    #[test]
    fn an_action_with_no_key_is_dropped() {
        let actions = [String::new(), "Nameless".to_string()];
        assert!(pair_actions(&actions).is_empty());
    }

    #[test]
    fn a_nameless_sender_still_gets_a_group_header() {
        assert_eq!(display_name(String::new()), "Unknown");
        assert_eq!(display_name("   ".to_string()), "Unknown");
        assert_eq!(display_name("Fractal".to_string()), "Fractal");
    }

    #[test]
    fn the_advertised_capabilities_are_the_ones_we_implement() {
        // body-markup is advertised because the panel renders a sanitised
        // subset of Pango markup; action-icons is not, because it does not.
        assert!(CAPABILITIES.contains(&"body"));
        assert!(CAPABILITIES.contains(&"body-markup"));
        assert!(CAPABILITIES.contains(&"actions"));
        assert!(CAPABILITIES.contains(&"persistence"));
        assert!(CAPABILITIES.contains(&"icon-static"));
        assert!(!CAPABILITIES.contains(&"action-icons"));
        assert!(!CAPABILITIES.contains(&"sound"));
    }
}
