//! The daemon on a real bus — a **private** one, always.
//!
//! Every test here starts its own `dbus-daemon` (see
//! [`crate::private_bus`]) and points both the panel and the client at it by
//! explicit address, which is what makes it safe to run `cargo test` on a live
//! desktop: the developer's notification daemon is never touched, and the name
//! this code requests is requested on a bus that exists only for the length of
//! one test.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use zbus::zvariant::Value;

use super::*;
use crate::notifications::server::{NOTIFICATIONS_NAME, NOTIFICATIONS_PATH};
use crate::private_bus::{PrivateBus, private_bus};

/// How long a test waits for a signal before failing.
const PATIENCE: Duration = Duration::from_secs(10);

/// The client side of the protocol, exactly as an application sees it.
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Client {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    fn get_server_information(&self) -> zbus::Result<(String, String, String, String)>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn activation_token(&self, id: u32, activation_token: &str) -> zbus::Result<()>;
}

/// A daemon serving on `bus`, with a scratch state file.
async fn serving(bus: &PrivateBus, label: &str) -> Notifications {
    let path = std::env::temp_dir()
        .join(format!("topbar-bus-{}-{label}", std::process::id()))
        .join("state.json");
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));

    let (_, store) = StateStore::open_at(path);
    let notifications = Notifications::start(
        PersistedNotifications::default(),
        store,
        Some(bus.address().to_string()),
    );
    notifications
        .startup()
        .await
        .expect("the panel owns the name on a bus of its own");
    notifications
}

/// Post the simplest possible notification.
async fn post(client: &ClientProxy<'_>, summary: &str) -> u32 {
    client
        .notify("Fractal", 0, "", summary, "", &[], HashMap::new(), 60_000)
        .await
        .expect("Notify is answered")
}

/// Wait for the next signal from `stream`, failing rather than hanging.
async fn next<S: StreamExt + Unpin>(stream: &mut S, what: &str) -> S::Item {
    tokio::time::timeout(PATIENCE, stream.next())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("the {what} stream ended"))
}

#[tokio::test]
async fn the_daemon_introduces_itself_the_way_the_specification_asks() {
    let bus = private_bus!();
    let _daemon = serving(&bus, "server-info").await;
    let client = ClientProxy::new(&bus.connect().await)
        .await
        .expect("the interface is on the bus");

    let (name, vendor, version, spec) = client
        .get_server_information()
        .await
        .expect("GetServerInformation is answered");
    assert_eq!(name, "topbar");
    assert_eq!(vendor, "trevarj");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    assert_eq!(spec, "1.2");

    let capabilities = client
        .get_capabilities()
        .await
        .expect("GetCapabilities is answered");
    assert!(capabilities.contains(&"body".to_string()));
    assert!(capabilities.contains(&"actions".to_string()));
    assert!(capabilities.contains(&"body-markup".to_string()));
    assert!(capabilities.contains(&"persistence".to_string()));
    assert!(!capabilities.contains(&"action-icons".to_string()));
    assert!(
        !capabilities.contains(&"sound".to_string()),
        "no sound-file support"
    );
}

#[tokio::test]
async fn a_notification_posted_over_the_bus_reaches_the_panel() {
    let bus = private_bus!();
    let daemon = serving(&bus, "notify").await;
    let client = ClientProxy::new(&bus.connect().await).await.expect("proxy");

    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(2));
    hints.insert("desktop-entry", Value::from("org.gnome.Fractal"));

    let id = client
        .notify(
            "Fractal",
            0,
            "avatar",
            "Ada",
            "see you at six",
            &["default", "", "reply", "Reply"],
            hints,
            -1,
        )
        .await
        .expect("Notify is answered");
    assert_ne!(id, 0);

    let mut state = daemon.state();
    let settled = tokio::time::timeout(PATIENCE, async {
        loop {
            let snapshot = state.borrow_and_update().clone();
            if !snapshot.history.is_empty() {
                return snapshot;
            }
            state.changed().await.expect("the daemon is alive");
        }
    })
    .await
    .expect("the notification reaches the panel");

    assert!(settled.enabled, "the panel is serving the name");
    let view = settled.history[0].newest();
    assert_eq!(view.id, id);
    assert_eq!(view.app_name, "Fractal");
    assert_eq!(view.summary, "Ada");
    assert_eq!(view.body, "see you at six");
    assert_eq!(view.urgency, Urgency::Critical);
    assert_eq!(view.icon.app_icon, "avatar");
    assert_eq!(
        view.icon.desktop_entry.as_deref(),
        Some("org.gnome.Fractal")
    );
    assert_eq!(view.actions.len(), 2);
    assert_eq!(
        view.default_action().expect("a default action").key,
        "default"
    );
    assert_eq!(settled.history[0].key, "org.gnome.fractal");
}

#[tokio::test]
async fn close_notification_answers_with_the_requested_reason() {
    let bus = private_bus!();
    let _daemon = serving(&bus, "close").await;
    let connection = bus.connect().await;
    let client = ClientProxy::new(&connection).await.expect("proxy");
    let mut closed = client
        .receive_notification_closed()
        .await
        .expect("subscribed");

    let id = post(&client, "withdraw me").await;
    client
        .close_notification(id)
        .await
        .expect("CloseNotification is answered");

    let signal = next(&mut closed, "NotificationClosed").await;
    let args = signal.args().expect("signal arguments");
    assert_eq!(*args.id(), id);
    assert_eq!(*args.reason(), CloseReason::Requested.to_wire(), "reason 3");
}

#[tokio::test]
async fn a_dismissed_notification_is_reported_as_dismissed() {
    let bus = private_bus!();
    let daemon = serving(&bus, "dismiss").await;
    let connection = bus.connect().await;
    let client = ClientProxy::new(&connection).await.expect("proxy");
    let mut closed = client
        .receive_notification_closed()
        .await
        .expect("subscribed");

    let id = post(&client, "dismiss me").await;
    daemon
        .handle()
        .dismiss(id, CloseReason::Dismissed)
        .await
        .expect("dismissed");

    let args = next(&mut closed, "NotificationClosed").await;
    let args = args.args().expect("signal arguments");
    assert_eq!(*args.id(), id);
    assert_eq!(*args.reason(), CloseReason::Dismissed.to_wire(), "reason 2");
}

#[tokio::test]
async fn a_transient_banner_running_out_is_reported_as_expired() {
    let bus = private_bus!();
    let _daemon = serving(&bus, "expired").await;
    let connection = bus.connect().await;
    let client = ClientProxy::new(&connection).await.expect("proxy");
    let mut closed = client
        .receive_notification_closed()
        .await
        .expect("subscribed");

    let mut hints = HashMap::new();
    hints.insert("transient", Value::Bool(true));
    let id = client
        .notify("mpv", 0, "", "playing", "", &[], hints, 60)
        .await
        .expect("Notify is answered");

    let args = next(&mut closed, "NotificationClosed").await;
    let args = args.args().expect("signal arguments");
    assert_eq!(*args.id(), id);
    assert_eq!(
        *args.reason(),
        CloseReason::Expired.to_wire(),
        "a transient banner has nowhere to go, so its timeout closes it"
    );
}

#[tokio::test]
async fn an_action_sends_its_token_before_it_sends_the_action() {
    let bus = private_bus!();
    let daemon = serving(&bus, "action").await;
    let connection = bus.connect().await;
    let client = ClientProxy::new(&connection).await.expect("proxy");

    let mut tokens = client.receive_activation_token().await.expect("subscribed");
    let mut invoked = client.receive_action_invoked().await.expect("subscribed");
    let mut closed = client
        .receive_notification_closed()
        .await
        .expect("subscribed");

    let id = client
        .notify(
            "Fractal",
            0,
            "",
            "Ada",
            "",
            &["default", "", "reply", "Reply"],
            HashMap::new(),
            60_000,
        )
        .await
        .expect("Notify is answered");

    daemon
        .handle()
        .invoke_action(id, "reply".into(), Some("niri-token-1".into()))
        .await
        .expect("the action is invoked");

    let token = next(&mut tokens, "ActivationToken").await;
    let token = token.args().expect("signal arguments");
    assert_eq!(*token.id(), id);
    assert_eq!(token.activation_token(), &"niri-token-1");

    let action = next(&mut invoked, "ActionInvoked").await;
    let action = action.args().expect("signal arguments");
    assert_eq!(*action.id(), id);
    assert_eq!(action.action_key(), &"reply");

    let close = next(&mut closed, "NotificationClosed").await;
    let close = close.args().expect("signal arguments");
    assert_eq!(*close.id(), id);
    assert_eq!(
        *close.reason(),
        CloseReason::Dismissed.to_wire(),
        "acting on a notification is the user disposing of it"
    );
}

#[tokio::test]
async fn replacing_over_the_bus_keeps_the_senders_id() {
    let bus = private_bus!();
    let daemon = serving(&bus, "replaces").await;
    let client = ClientProxy::new(&bus.connect().await).await.expect("proxy");

    let first = post(&client, "first").await;
    let again = client
        .notify(
            "Fractal",
            first,
            "",
            "first, updated",
            "",
            &[],
            HashMap::new(),
            60_000,
        )
        .await
        .expect("Notify is answered");
    assert_eq!(again, first);

    let mut state = daemon.state();
    let settled = tokio::time::timeout(PATIENCE, async {
        loop {
            let snapshot = state.borrow_and_update().clone();
            if snapshot
                .flat_history()
                .any(|view| view.summary == "first, updated")
            {
                return snapshot;
            }
            state.changed().await.expect("the daemon is alive");
        }
    })
    .await
    .expect("the replacement reaches the panel");

    assert_eq!(
        settled.flat_history().count(),
        1,
        "a replacement updates the entry rather than adding one"
    );
}

#[tokio::test]
async fn an_image_hint_survives_the_trip_over_the_bus() {
    let bus = private_bus!();
    let daemon = serving(&bus, "image").await;
    let client = ClientProxy::new(&bus.connect().await).await.expect("proxy");

    let pixels: Vec<u8> = vec![0x7f; 2 * 2 * 4];
    let image = zbus::zvariant::StructureBuilder::new()
        .add_field(2i32)
        .add_field(2i32)
        .add_field(8i32)
        .add_field(true)
        .add_field(8i32)
        .add_field(4i32)
        .add_field(pixels)
        .build()
        .expect("a well-formed image structure");

    let mut hints = HashMap::new();
    hints.insert("image-data", Value::Structure(image));
    client
        .notify("Fractal", 0, "", "Ada", "", &[], hints, 60_000)
        .await
        .expect("Notify is answered");

    let mut state = daemon.state();
    let settled = tokio::time::timeout(PATIENCE, async {
        loop {
            let snapshot = state.borrow_and_update().clone();
            if !snapshot.history.is_empty() {
                return snapshot;
            }
            state.changed().await.expect("the daemon is alive");
        }
    })
    .await
    .expect("the notification reaches the panel");

    let image = settled.history[0]
        .newest()
        .icon
        .image_data
        .clone()
        .expect("the pixels came through");
    assert_eq!((image.width, image.height, image.channels), (2, 2, 4));
    assert_eq!(image.data.len(), 16);
}

#[tokio::test]
async fn a_daemon_that_will_not_step_aside_is_reported_rather_than_fought() {
    let bus = private_bus!();

    // A decoy that takes the name and refuses to give it up: no
    // AllowReplacement, so our ReplaceExisting has nothing to work with.
    let decoy = bus.connect().await;
    let reply = decoy
        .request_name_with_flags(
            NOTIFICATIONS_NAME,
            zbus::fdo::RequestNameFlags::DoNotQueue.into(),
        )
        .await
        .expect("the decoy takes the name");
    assert_eq!(reply, zbus::fdo::RequestNameReply::PrimaryOwner);

    let path = std::env::temp_dir()
        .join(format!("topbar-bus-{}-taken", std::process::id()))
        .join("state.json");
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    let (_, store) = StateStore::open_at(path);
    let notifications = Notifications::start(
        PersistedNotifications::default(),
        store,
        Some(bus.address().to_string()),
    );

    let error = notifications
        .startup()
        .await
        .expect_err("the name is not ours to take");
    assert!(
        matches!(&error, SvcError::NameTaken(name) if name == NOTIFICATIONS_NAME),
        "{error:?}"
    );
    assert_eq!(
        error.user_message(),
        "Another notification daemon is running"
    );

    // The panel keeps running: the history is still there, it just says so.
    let state = notifications.state();
    assert!(!state.borrow().enabled);
}

#[tokio::test]
async fn an_unreachable_bus_is_reported_rather_than_fatal() {
    let path = std::env::temp_dir()
        .join(format!("topbar-bus-{}-nobus", std::process::id()))
        .join("state.json");
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    let (_, store) = StateStore::open_at(path);

    let notifications = Notifications::start(
        PersistedNotifications::default(),
        store,
        Some("unix:path=/nonexistent/topbar-no-such-bus".to_string()),
    );

    let error = notifications
        .startup()
        .await
        .expect_err("there is no bus at that address");
    assert!(matches!(error, SvcError::Bus(_)), "{error:?}");
    assert_eq!(error.user_message(), "Could not reach the session bus");
}

#[tokio::test]
async fn the_interface_is_where_the_specification_says_it_is() {
    let bus = private_bus!();
    let _daemon = serving(&bus, "introspect").await;
    let connection = bus.connect().await;

    let introspection = zbus::fdo::IntrospectableProxy::builder(&connection)
        .destination(NOTIFICATIONS_NAME)
        .expect("destination")
        .path(NOTIFICATIONS_PATH)
        .expect("path")
        .build()
        .await
        .expect("proxy")
        .introspect()
        .await
        .expect("introspection is answered");

    for member in [
        r#"name="Notify""#,
        r#"name="CloseNotification""#,
        r#"name="GetCapabilities""#,
        r#"name="GetServerInformation""#,
        r#"name="NotificationClosed""#,
        r#"name="ActionInvoked""#,
        r#"name="ActivationToken""#,
    ] {
        assert!(
            introspection.contains(member),
            "{member} is missing from the introspection XML"
        );
    }
}
