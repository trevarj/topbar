//! Behaviour tests for the daemon, driven through its own handle.
//!
//! Every one of these runs [`Notifications::detached`]: the state machine with
//! no bus behind it. That is deliberate and load-bearing — a test that could
//! reach a session bus could take `org.freedesktop.Notifications` away from
//! the desktop the developer is using. The bus half is covered by
//! `tests/notifications_bus.rs`, which stands up a private bus of its own.

use std::path::PathBuf;
use std::time::Duration;

use super::*;

/// How long a test waits for the daemon to catch up before failing.
const PATIENCE: Duration = Duration::from_secs(5);

/// A daemon with a scratch state file, and a subscription to its snapshots.
struct Fixture {
    notifications: Notifications,
    state: watch::Receiver<Arc<NotifState>>,
    path: PathBuf,
}

impl Fixture {
    /// A daemon starting from nothing.
    fn new(label: &str) -> Self {
        Self::restoring(label, PersistedNotifications::default())
    }

    /// A daemon starting from `persisted`.
    fn restoring(label: &str, persisted: PersistedNotifications) -> Self {
        let path = std::env::temp_dir()
            .join(format!("topbar-notif-{}-{label}", std::process::id()))
            .join("state.json");
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));

        let (_, store) = StateStore::open_at(path.clone());
        let notifications = Notifications::detached(persisted, store);
        let state = notifications.state();
        Self {
            notifications,
            state,
            path,
        }
    }

    fn handle(&self) -> &NotificationsHandle {
        self.notifications.handle()
    }

    /// The snapshot as of right now.
    fn now(&self) -> Arc<NotifState> {
        self.state.borrow().clone()
    }

    /// Wait until a snapshot satisfies `predicate`.
    async fn settle(
        &mut self,
        what: &str,
        predicate: impl Fn(&NotifState) -> bool,
    ) -> Arc<NotifState> {
        let wait = async {
            loop {
                let snapshot = self.state.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                self.state.changed().await.expect("the daemon is alive");
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Wait for the state file to satisfy `predicate`, and return it.
    async fn settled_on_disk(
        &self,
        predicate: impl Fn(&PersistedNotifications) -> bool,
    ) -> PersistedNotifications {
        for _ in 0..200 {
            let saved = std::fs::read_to_string(&self.path)
                .ok()
                .and_then(|json| {
                    serde_json::from_str::<crate::state_store::PersistedState>(&json).ok()
                })
                .map(|state| state.notifications)
                .unwrap_or_default();
            if predicate(&saved) {
                return saved;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the state file never reached the expected value");
    }
}

/// A plain notification from `app`, with a banner that will not time out
/// during a test.
fn request(app: &str, summary: &str) -> Request {
    Request {
        app_name: app.to_string(),
        replaces_id: 0,
        summary: summary.to_string(),
        body: String::new(),
        actions: Vec::new(),
        urgency: Urgency::Normal,
        transient: false,
        icon: IconSource::default(),
        expire_timeout: 60_000,
        internal: false,
    }
}

/// The ids on screen, newest first.
fn toast_ids(state: &NotifState) -> Vec<u32> {
    state
        .toasts
        .iter()
        .map(|toast| toast.notification.id)
        .collect()
}

/// The ids in the history, newest first, ignoring the grouping.
fn history_ids(state: &NotifState) -> Vec<u32> {
    state.flat_history().map(|view| view.id).collect()
}

// ---------------------------------------------------------------------------
// Arrival, replacement, routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_notification_becomes_a_banner_and_a_history_entry() {
    let mut fixture = Fixture::new("arrival");
    let id = fixture
        .handle()
        .deliver(request("Fractal", "Ada"))
        .await
        .expect("delivered");
    assert_ne!(id, 0, "0 is reserved by the protocol");

    let state = fixture
        .settle("the notification", |state| !state.toasts.is_empty())
        .await;
    assert_eq!(toast_ids(&state), [id]);
    assert_eq!(history_ids(&state), [id]);
    assert_eq!(state.history.len(), 1);
    assert_eq!(state.history[0].app_name, "Fractal");
    assert_eq!(state.history[0].newest().summary, "Ada");
    assert_eq!(state.unseen_count, 1);
}

#[tokio::test]
async fn replacing_a_notification_updates_it_where_it_stands() {
    let mut fixture = Fixture::new("replaces");

    let first = fixture
        .handle()
        .deliver(request("Fractal", "one"))
        .await
        .expect("first");
    let second = fixture
        .handle()
        .deliver(request("Slack", "two"))
        .await
        .expect("second");
    fixture
        .settle("both", |state| state.history.len() == 2)
        .await;

    let replacement = Request {
        replaces_id: first,
        ..request("Fractal", "one, updated")
    };
    let replaced = fixture
        .handle()
        .deliver(replacement)
        .await
        .expect("replacement");

    assert_eq!(replaced, first, "a replacement keeps the sender's id");
    let state = fixture
        .settle("the update", |state| {
            state
                .flat_history()
                .any(|view| view.summary == "one, updated")
        })
        .await;

    assert_eq!(
        history_ids(&state),
        [second, first],
        "the entry is updated in place, not moved to the front"
    );
    assert_eq!(state.history.len(), 2);
    assert_eq!(
        toast_ids(&state),
        [second, first],
        "and its banner keeps its place in the stack too"
    );
    assert_eq!(state.unseen_count, 2, "a replacement is not a new arrival");
}

#[tokio::test]
async fn replacing_an_id_that_is_gone_makes_a_new_notification() {
    let mut fixture = Fixture::new("replaces-missing");
    let id = fixture
        .handle()
        .deliver(Request {
            replaces_id: 4242,
            ..request("Fractal", "orphan")
        })
        .await
        .expect("delivered");

    assert_ne!(id, 4242);
    let state = fixture
        .settle("the notification", |state| state.history.len() == 1)
        .await;
    assert_eq!(history_ids(&state), [id]);
}

#[tokio::test]
async fn a_transient_notification_is_a_banner_and_nothing_else() {
    let mut fixture = Fixture::new("transient");
    let id = fixture
        .handle()
        .deliver(Request {
            transient: true,
            ..request("mpv", "playing")
        })
        .await
        .expect("delivered");

    let state = fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;
    assert_eq!(toast_ids(&state), [id]);
    assert!(
        state.history.is_empty(),
        "a transient never enters the history"
    );
    assert_eq!(state.unseen_count, 0);
}

// ---------------------------------------------------------------------------
// Do Not Disturb
// ---------------------------------------------------------------------------

#[tokio::test]
async fn do_not_disturb_suppresses_the_banner_but_keeps_the_record() {
    let mut fixture = Fixture::new("dnd");
    fixture.handle().set_dnd(true).await.expect("dnd on");
    fixture.settle("dnd", |state| state.dnd).await;

    let id = fixture
        .handle()
        .deliver(request("Slack", "quiet"))
        .await
        .expect("delivered");
    let state = fixture
        .settle("the history entry", |state| !state.history.is_empty())
        .await;

    assert!(
        state.toasts.is_empty(),
        "no banner while Do Not Disturb is on"
    );
    assert_eq!(history_ids(&state), [id]);
}

#[tokio::test]
async fn a_critical_notification_ignores_do_not_disturb() {
    let mut fixture = Fixture::new("dnd-critical");
    fixture.handle().set_dnd(true).await.expect("dnd on");
    fixture.settle("dnd", |state| state.dnd).await;

    let id = fixture
        .handle()
        .deliver(Request {
            urgency: Urgency::Critical,
            ..request("UPower", "Battery critically low")
        })
        .await
        .expect("delivered");

    let state = fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;
    assert_eq!(toast_ids(&state), [id]);
    assert_eq!(history_ids(&state), [id]);
}

#[tokio::test]
async fn a_silenced_transient_is_discarded_rather_than_stored() {
    let mut fixture = Fixture::new("dnd-transient");
    fixture.handle().set_dnd(true).await.expect("dnd on");
    fixture.settle("dnd", |state| state.dnd).await;

    fixture
        .handle()
        .deliver(Request {
            transient: true,
            ..request("mpv", "playing")
        })
        .await
        .expect("delivered");

    // Give the daemon a moment to do the wrong thing, if it were going to.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = fixture.now();
    assert!(state.toasts.is_empty());
    assert!(
        state.history.is_empty(),
        "it has nowhere to be, so it is gone"
    );
}

#[tokio::test]
async fn the_panels_own_report_is_a_banner_even_under_do_not_disturb() {
    let mut fixture = Fixture::new("report");
    fixture.handle().set_dnd(true).await.expect("dnd on");
    fixture.settle("dnd", |state| state.dnd).await;

    fixture
        .handle()
        .report("Could not reach the compositor".into(), String::new())
        .await
        .expect("reported");

    let state = fixture
        .settle("the report", |state| !state.toasts.is_empty())
        .await;
    assert_eq!(state.toasts[0].notification.app_name, INTERNAL_APP);
    assert_eq!(
        state.toasts[0].notification.summary,
        "Could not reach the compositor"
    );
    assert!(
        state.history.is_empty(),
        "the panel does not fill its own history"
    );
}

// ---------------------------------------------------------------------------
// The banner stack
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_three_banners_are_on_screen_but_everything_is_recorded() {
    let mut fixture = Fixture::new("cap");
    let mut ids = Vec::new();
    for index in 0..5 {
        ids.push(
            fixture
                .handle()
                .deliver(request("Fractal", &format!("message {index}")))
                .await
                .expect("delivered"),
        );
    }

    let state = fixture
        .settle("all five", |state| history_ids(state).len() == 5)
        .await;
    assert_eq!(
        toast_ids(&state),
        [ids[2], ids[1], ids[0]],
        "the first three banners hold their slots; later ones go straight to the history"
    );
    assert_eq!(state.toasts.len(), MAX_TOASTS);
    assert_eq!(state.history[0].count(), 5);
}

#[tokio::test]
async fn a_critical_notification_pushes_the_oldest_ordinary_banner_off() {
    let mut fixture = Fixture::new("cap-critical");
    let mut ids = Vec::new();
    for index in 0..MAX_TOASTS {
        ids.push(
            fixture
                .handle()
                .deliver(request("Fractal", &format!("message {index}")))
                .await
                .expect("delivered"),
        );
    }
    fixture
        .settle("a full stack", |state| state.toasts.len() == MAX_TOASTS)
        .await;

    let critical = fixture
        .handle()
        .deliver(Request {
            urgency: Urgency::Critical,
            ..request("UPower", "Battery critically low")
        })
        .await
        .expect("delivered");

    let state = fixture
        .settle("the critical banner", |state| {
            toast_ids(state).contains(&critical)
        })
        .await;
    assert_eq!(state.toasts.len(), MAX_TOASTS);
    assert_eq!(toast_ids(&state), [critical, ids[2], ids[1]]);
    assert!(
        history_ids(&state).contains(&ids[0]),
        "the banner that made way is still in the history"
    );
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_banner_expires_and_leaves_its_history_entry_behind() {
    let mut fixture = Fixture::new("expiry");
    let id = fixture
        .handle()
        .deliver(Request {
            expire_timeout: 60,
            ..request("Fractal", "brief")
        })
        .await
        .expect("delivered");

    fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;
    let state = fixture
        .settle("the banner to expire", |state| state.toasts.is_empty())
        .await;
    assert_eq!(
        history_ids(&state),
        [id],
        "a banner timing out is not the notification being closed"
    );
}

#[tokio::test]
async fn a_critical_banner_never_expires_on_its_own() {
    let mut fixture = Fixture::new("critical-persists");
    fixture
        .handle()
        .deliver(Request {
            urgency: Urgency::Critical,
            expire_timeout: 40,
            ..request("UPower", "Battery critically low")
        })
        .await
        .expect("delivered");

    fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fixture.now().toasts.len(),
        1,
        "a critical banner waits for the user, whatever timeout it asked for"
    );
}

#[tokio::test]
async fn hovering_a_banner_holds_its_timer() {
    let mut fixture = Fixture::new("pause");
    let id = fixture
        .handle()
        .deliver(Request {
            expire_timeout: 150,
            ..request("Fractal", "hover me")
        })
        .await
        .expect("delivered");

    fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;
    fixture.handle().pause_toast(id).await.expect("paused");
    fixture
        .settle("the pause", |state| {
            state.toasts.iter().all(|toast| toast.paused)
        })
        .await;

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        fixture.now().toasts.len(),
        1,
        "the pointer is on it, so its time does not run"
    );

    fixture.handle().resume_toast(id).await.expect("resumed");
    fixture
        .settle("the banner to expire", |state| state.toasts.is_empty())
        .await;
}

#[tokio::test]
async fn a_replacement_restarts_the_timer() {
    let mut fixture = Fixture::new("replace-timer");
    let id = fixture
        .handle()
        .deliver(Request {
            expire_timeout: 200,
            ..request("Fractal", "first")
        })
        .await
        .expect("delivered");
    fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;

    for round in 0..3 {
        tokio::time::sleep(Duration::from_millis(120)).await;
        fixture
            .handle()
            .deliver(Request {
                replaces_id: id,
                expire_timeout: 200,
                ..request("Fractal", &format!("update {round}"))
            })
            .await
            .expect("replacement");
        assert_eq!(
            fixture.now().toasts.len(),
            1,
            "each replacement gives the banner its full time again"
        );
    }
}

// ---------------------------------------------------------------------------
// Dismissal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dismissing_a_banner_leaves_the_notification_in_the_history() {
    let mut fixture = Fixture::new("dismiss-toast");
    let id = fixture
        .handle()
        .deliver(request("Fractal", "keep me"))
        .await
        .expect("delivered");
    fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;

    fixture.handle().dismiss_toast(id).await.expect("dismissed");
    let state = fixture
        .settle("the dismissal", |state| state.toasts.is_empty())
        .await;
    assert_eq!(history_ids(&state), [id]);
}

#[tokio::test]
async fn dismissing_a_transient_banner_ends_the_notification() {
    let mut fixture = Fixture::new("dismiss-transient");
    let id = fixture
        .handle()
        .deliver(Request {
            transient: true,
            ..request("mpv", "playing")
        })
        .await
        .expect("delivered");
    fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;

    fixture.handle().dismiss_toast(id).await.expect("dismissed");
    let state = fixture
        .settle("the dismissal", |state| state.toasts.is_empty())
        .await;
    assert!(state.history.is_empty());
}

#[tokio::test]
async fn dismissing_a_notification_takes_its_banner_with_it() {
    let mut fixture = Fixture::new("dismiss");
    let id = fixture
        .handle()
        .deliver(request("Fractal", "go away"))
        .await
        .expect("delivered");
    fixture
        .settle("the banner", |state| !state.toasts.is_empty())
        .await;

    fixture
        .handle()
        .dismiss(id, CloseReason::Dismissed)
        .await
        .expect("dismissed");
    let state = fixture
        .settle("the dismissal", |state| state.history.is_empty())
        .await;
    assert!(state.toasts.is_empty());
    assert_eq!(state.unseen_count, 0);
}

#[tokio::test]
async fn clearing_a_group_leaves_the_other_applications_alone() {
    let mut fixture = Fixture::new("clear-group");
    for summary in ["one", "two"] {
        fixture
            .handle()
            .deliver(request("Fractal", summary))
            .await
            .expect("delivered");
    }
    let kept = fixture
        .handle()
        .deliver(request("Slack", "stay"))
        .await
        .expect("delivered");
    let state = fixture
        .settle("both groups", |state| state.history.len() == 2)
        .await;

    let fractal = state
        .history
        .iter()
        .find(|group| group.app_name == "Fractal")
        .expect("a Fractal group")
        .key
        .clone();

    fixture
        .handle()
        .clear_group(fractal)
        .await
        .expect("cleared");
    let state = fixture
        .settle("the clear", |state| state.history.len() == 1)
        .await;
    assert_eq!(history_ids(&state), [kept]);
    assert_eq!(state.history[0].app_name, "Slack");
}

#[tokio::test]
async fn clearing_everything_empties_the_history() {
    let mut fixture = Fixture::new("clear-all");
    for app in ["Fractal", "Slack", "Fractal"] {
        fixture
            .handle()
            .deliver(request(app, "hello"))
            .await
            .expect("delivered");
    }
    fixture
        .settle("the history", |state| state.history.len() == 2)
        .await;

    fixture.handle().clear_all().await.expect("cleared");
    let state = fixture
        .settle("the clear", |state| state.history.is_empty())
        .await;
    assert!(
        state.toasts.is_empty(),
        "the banners go with their notifications"
    );
    assert_eq!(state.unseen_count, 0);
}

#[tokio::test]
async fn an_action_on_a_notification_that_is_gone_is_reported() {
    let fixture = Fixture::new("action-missing");
    let error = fixture
        .handle()
        .invoke_action(404, "default".into(), None)
        .await
        .expect_err("there is no notification 404");
    assert!(matches!(error, SvcError::GoneNotification(404)));
    assert_eq!(
        error.user_message(),
        "That notification is no longer available"
    );
}

#[tokio::test]
async fn invoking_an_action_closes_the_notification_behind_it() {
    let mut fixture = Fixture::new("action");
    let id = fixture
        .handle()
        .deliver(Request {
            actions: vec![Action {
                key: "default".into(),
                label: String::new(),
            }],
            ..request("Fractal", "click me")
        })
        .await
        .expect("delivered");
    fixture
        .settle("the notification", |state| !state.history.is_empty())
        .await;

    fixture
        .handle()
        .invoke_action(id, "default".into(), Some("token-123".into()))
        .await
        .expect("invoked");

    let state = fixture
        .settle("the close", |state| state.history.is_empty())
        .await;
    assert!(state.toasts.is_empty());
}

// ---------------------------------------------------------------------------
// Seen counting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opening_the_panel_marks_the_history_as_seen() {
    let mut fixture = Fixture::new("seen");
    for summary in ["one", "two", "three"] {
        fixture
            .handle()
            .deliver(request("Fractal", summary))
            .await
            .expect("delivered");
    }
    let state = fixture
        .settle("three arrivals", |state| state.unseen_count == 3)
        .await;
    assert_eq!(state.history[0].count(), 3);

    fixture.handle().mark_seen().await.expect("seen");
    fixture
        .settle("the mark", |state| state.unseen_count == 0)
        .await;

    fixture
        .handle()
        .deliver(request("Fractal", "four"))
        .await
        .expect("delivered");
    fixture
        .settle("the next arrival", |state| state.unseen_count == 1)
        .await;
}

// ---------------------------------------------------------------------------
// The history cap and persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_history_is_bounded_and_drops_the_oldest_first() {
    let mut fixture = Fixture::new("bounded");
    let mut ids = Vec::new();
    for index in 0..MAX_HISTORY + 5 {
        ids.push(
            fixture
                .handle()
                .deliver(request("Fractal", &format!("message {index}")))
                .await
                .expect("delivered"),
        );
    }

    let state = fixture
        .settle("the history to settle", |state| {
            history_ids(state).len() == MAX_HISTORY
        })
        .await;

    let kept = history_ids(&state);
    assert_eq!(kept.len(), MAX_HISTORY);
    assert_eq!(kept[0], *ids.last().expect("an id"), "newest first");
    for evicted in &ids[..5] {
        assert!(
            !kept.contains(evicted),
            "{evicted} should have been evicted"
        );
    }
}

#[tokio::test]
async fn the_history_and_do_not_disturb_survive_a_restart() {
    let mut fixture = Fixture::new("persist");
    fixture.handle().set_dnd(true).await.expect("dnd on");
    for summary in ["older", "newer"] {
        fixture
            .handle()
            .deliver(Request {
                icon: IconSource {
                    app_icon: "org.gnome.Fractal".into(),
                    desktop_entry: Some("org.gnome.Fractal".into()),
                    ..IconSource::default()
                },
                ..request("Fractal", summary)
            })
            .await
            .expect("delivered");
    }
    fixture
        .settle("the history", |state| state.history.len() == 1)
        .await;

    let saved = fixture
        .settled_on_disk(|saved| saved.history.len() == 2 && saved.dnd)
        .await;
    assert_eq!(
        saved
            .history
            .iter()
            .map(|entry| entry.summary.as_str())
            .collect::<Vec<_>>(),
        ["newer", "older"],
        "the file is newest first, like the list"
    );
    assert!(saved.next_id > saved.history[0].id);

    // A second daemon restoring the same document sees the same list.
    let mut restarted = Fixture::restoring("persist-restored", saved);
    let state = restarted
        .settle("the restored history", |state| !state.history.is_empty())
        .await;
    assert!(state.dnd, "Do Not Disturb is remembered");
    assert_eq!(state.history.len(), 1, "and the grouping is rebuilt");
    assert_eq!(state.history[0].app_name, "Fractal");
    assert_eq!(state.history[0].count(), 2);
    assert_eq!(state.history[0].newest().summary, "newer");
    assert_eq!(
        state.unseen_count, 0,
        "nothing restored from disk counts as new"
    );
    assert!(state.toasts.is_empty(), "a restart does not replay banners");
}

#[tokio::test]
async fn a_restart_never_reuses_a_live_notification_id() {
    let persisted = PersistedNotifications {
        dnd: false,
        next_id: 1,
        history: vec![PersistedNotification {
            id: 90,
            app_name: "Fractal".into(),
            ..PersistedNotification::default()
        }],
    };
    let mut fixture = Fixture::restoring("id-reuse", persisted);
    let id = fixture
        .handle()
        .deliver(request("Slack", "new"))
        .await
        .expect("delivered");
    assert!(id > 90, "the next id clears everything restored from disk");

    let state = fixture
        .settle("both", |state| state.history.len() == 2)
        .await;
    assert_eq!(history_ids(&state), [id, 90]);
}

#[tokio::test]
async fn transient_notifications_are_never_written_to_disk() {
    let mut fixture = Fixture::new("no-transient-persist");
    fixture
        .handle()
        .deliver(request("Fractal", "recorded"))
        .await
        .expect("delivered");
    fixture
        .handle()
        .deliver(Request {
            transient: true,
            ..request("mpv", "playing")
        })
        .await
        .expect("delivered");
    fixture
        .settle("both", |state| !state.toasts.is_empty())
        .await;

    let saved = fixture
        .settled_on_disk(|saved| saved.history.len() == 1)
        .await;
    assert_eq!(saved.history[0].summary, "recorded");
}

#[tokio::test]
async fn the_daemon_reports_that_it_is_not_serving_when_it_has_no_bus() {
    let fixture = Fixture::new("disabled");
    assert!(
        !fixture.now().enabled,
        "nothing was served, so nothing is enabled"
    );
    // With no bus half at all the startup future resolves rather than hanging,
    // so the panel's failure sink is never left waiting on it.
    fixture
        .notifications
        .startup()
        .await
        .expect("a detached daemon has nothing to report");
}
