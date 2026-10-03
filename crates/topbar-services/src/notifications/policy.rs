//! What happens to a notification, decided without a bus or a widget in sight.
//!
//! Every rule the daemon applies — where a notification goes, how long its
//! banner lives, which banner gets pushed out when the stack is full, how the
//! history is grouped and bounded — is a pure function here. The task in
//! [`super::task`] does nothing but sequence them, which is what makes the
//! behaviour testable without a session bus.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::model::{GroupView, NotificationView, Urgency};

/// How many toasts may be on screen at once.
pub const MAX_TOASTS: usize = 3;

/// How long a toast lives when the sender does not ask for something else.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(4000);

/// How many notifications the history keeps.
pub const MAX_HISTORY: usize = 100;

/// Where an arriving notification goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routing {
    /// Whether it is eligible for a toast. The stack may still be full — see
    /// [`admit`].
    pub toast: bool,
    /// Whether it joins the history.
    pub history: bool,
}

impl Routing {
    /// Whether the notification would vanish without ever being displayed.
    pub fn is_discarded(self) -> bool {
        !self.toast && !self.history
    }
}

/// Decide where a notification goes.
///
/// Three rules, in order of precedence:
///
/// - **Critical always toasts.** Do Not Disturb is a request for quiet, not a
///   request to miss the battery dying.
/// - **Transient never enters the history.** The specification says a
///   transient notification is a banner and nothing more.
/// - **Do Not Disturb suppresses the banner, not the record.** Everything
///   non-critical still lands in the history so the user can catch up.
///
/// `internal` marks the panel's own failure reports, which are transient (they
/// are feedback, not history) and bypass Do Not Disturb (a silenced error
/// report is a bug that looks like nothing happening).
pub fn route(urgency: Urgency, transient: bool, dnd: bool, internal: bool) -> Routing {
    Routing {
        toast: internal || urgency.is_critical() || !dnd,
        history: !transient && !internal,
    }
}

/// How long a toast lives, or `None` when it must be dismissed by hand.
///
/// Critical notifications never expire. For everything else a positive
/// `expire_timeout` is honoured and anything else — the `-1` "server decides"
/// and the `0` "never" — becomes [`DEFAULT_TIMEOUT`]. Honouring `0` literally
/// would leave an ordinary banner pinned to the screen forever, and the panel
/// has a history to fall back on: nothing is lost by letting it go.
pub fn toast_timeout(expire_timeout: i32, urgency: Urgency) -> Option<Duration> {
    if urgency.is_critical() {
        return None;
    }
    match u64::try_from(expire_timeout) {
        Ok(millis) if millis > 0 => Some(Duration::from_millis(millis)),
        _ => Some(DEFAULT_TIMEOUT),
    }
}

/// Whether an arriving toast fits, and what has to go if it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// There is a free slot.
    Room,
    /// The stack is full; drop the toast at this index to make room.
    Replace(usize),
    /// The stack is full and nothing may be pushed out.
    Full,
}

/// Decide whether a toast can be shown, given what is already on screen.
///
/// `current` is the stack, newest first. A critical notification evicts the
/// oldest ordinary banner rather than waiting behind it — "critical always
/// toasts" would otherwise be false the moment three chat messages arrive
/// first. A stack of nothing but critical notifications is left alone: none of
/// them may be silently taken away.
pub fn admit(current: &[Urgency], incoming: Urgency) -> Admission {
    if current.len() < MAX_TOASTS {
        return Admission::Room;
    }
    if !incoming.is_critical() {
        return Admission::Full;
    }
    // Newest first, so the last ordinary banner in the slice is the oldest.
    current
        .iter()
        .rposition(|urgency| !urgency.is_critical())
        .map_or(Admission::Full, Admission::Replace)
}

/// The stable key a notification is grouped under.
///
/// The desktop entry is preferred because it survives an application changing
/// its display name between releases; the name is folded to lower case so
/// `Discord` and `discord` are one group rather than two.
pub fn group_key(app_name: &str, desktop_entry: Option<&str>) -> String {
    desktop_entry
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .unwrap_or(app_name)
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase()
}

/// Group a newest-first history into newest-first groups.
///
/// A group's position is its newest member's position, and members keep the
/// order they came in, so the list reads the same way whether or not any
/// group is expanded.
pub fn group(history: &[(String, Arc<NotificationView>)]) -> Vec<GroupView> {
    let mut groups: Vec<GroupView> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();

    for (key, view) in history {
        match index.get(key.as_str()) {
            Some(&at) => groups[at].notifications.push(Arc::clone(view)),
            None => {
                index.insert(key.as_str(), groups.len());
                groups.push(GroupView {
                    key: key.clone(),
                    app_name: view.app_name.clone(),
                    notifications: vec![Arc::clone(view)],
                });
            }
        }
    }
    groups
}

/// The ids the history cap pushes out, oldest first.
///
/// Returns them rather than doing the removal so the caller can close each one
/// on the bus: an evicted notification is gone, and its sender is entitled to
/// hear about it.
pub fn overflow(history: &[(String, Arc<NotificationView>)]) -> Vec<u32> {
    history
        .iter()
        .skip(MAX_HISTORY)
        .map(|(_, view)| view.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::model::IconSource;

    fn view(id: u32, app: &str) -> Arc<NotificationView> {
        Arc::new(NotificationView {
            id,
            app_name: app.to_string(),
            summary: String::new(),
            body: String::new(),
            actions: Vec::new(),
            urgency: Urgency::Normal,
            icon: IconSource::default(),
            timestamp: i64::from(id),
        })
    }

    fn entry(id: u32, app: &str) -> (String, Arc<NotificationView>) {
        (group_key(app, None), view(id, app))
    }

    #[test]
    fn the_do_not_disturb_truth_table() {
        use Urgency::{Critical, Low, Normal};

        // (urgency, transient, dnd) -> (toast, history)
        let table = [
            ((Normal, false, false), (true, true)),
            ((Normal, false, true), (false, true)),
            ((Normal, true, false), (true, false)),
            ((Normal, true, true), (false, false)),
            ((Low, false, false), (true, true)),
            ((Low, false, true), (false, true)),
            ((Critical, false, false), (true, true)),
            ((Critical, false, true), (true, true)),
            ((Critical, true, false), (true, false)),
            ((Critical, true, true), (true, false)),
        ];

        for ((urgency, transient, dnd), (toast, history)) in table {
            let routing = route(urgency, transient, dnd, false);
            assert_eq!(
                routing,
                Routing { toast, history },
                "{urgency:?} transient={transient} dnd={dnd}"
            );
        }
    }

    #[test]
    fn only_a_silenced_transient_is_discarded_outright() {
        assert!(route(Urgency::Normal, true, true, false).is_discarded());
        assert!(!route(Urgency::Normal, false, true, false).is_discarded());
        assert!(!route(Urgency::Critical, true, true, false).is_discarded());
    }

    #[test]
    fn the_panels_own_reports_are_banners_that_ignore_do_not_disturb() {
        let routing = route(Urgency::Normal, true, true, true);
        assert_eq!(
            routing,
            Routing {
                toast: true,
                history: false
            },
            "a failure the user cannot see is a failure that did not happen"
        );
    }

    #[test]
    fn timeouts_honour_the_sender_and_fall_back_to_four_seconds() {
        assert_eq!(
            toast_timeout(2500, Urgency::Normal),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(toast_timeout(-1, Urgency::Normal), Some(DEFAULT_TIMEOUT));
        assert_eq!(
            toast_timeout(0, Urgency::Normal),
            Some(DEFAULT_TIMEOUT),
            "an ordinary banner that never expires would be stuck on screen"
        );
        assert_eq!(toast_timeout(-99, Urgency::Low), Some(DEFAULT_TIMEOUT));
    }

    #[test]
    fn a_critical_notification_never_expires_whatever_it_asks_for() {
        for timeout in [-1, 0, 1, 60_000] {
            assert_eq!(toast_timeout(timeout, Urgency::Critical), None);
        }
    }

    #[test]
    fn the_stack_takes_three_and_then_overflows() {
        use Urgency::{Critical, Normal};

        assert_eq!(admit(&[], Normal), Admission::Room);
        assert_eq!(admit(&[Normal, Normal], Normal), Admission::Room);
        assert_eq!(
            admit(&[Normal, Normal, Normal], Normal),
            Admission::Full,
            "the fourth ordinary banner waits in the history"
        );
        assert_eq!(
            admit(&[Normal, Normal, Normal], Critical),
            Admission::Replace(2)
        );
        assert_eq!(
            admit(&[Normal, Critical, Normal], Critical),
            Admission::Replace(2),
            "the oldest ordinary banner goes, not the oldest banner"
        );
        assert_eq!(
            admit(&[Critical, Critical, Critical], Critical),
            Admission::Full,
            "nothing critical is taken off screen behind the user's back"
        );
    }

    #[test]
    fn grouping_keys_fold_case_and_prefer_the_desktop_entry() {
        assert_eq!(group_key("Discord", None), "discord");
        assert_eq!(group_key("discord", None), "discord");
        assert_eq!(
            group_key("Telegram Desktop", Some("org.telegram.desktop")),
            "org.telegram.desktop"
        );
        assert_eq!(
            group_key("Telegram Desktop", Some("  ")),
            "telegram desktop",
            "a blank hint is no hint"
        );
        assert_eq!(group_key("Slack", Some("@Slack")), "slack");
    }

    #[test]
    fn groups_keep_the_order_the_notifications_arrived_in() {
        let history = vec![
            entry(5, "Telegram"),
            entry(4, "Fractal"),
            entry(3, "Telegram"),
            entry(2, "Slack"),
            entry(1, "Fractal"),
        ];
        let groups = group(&history);

        let names: Vec<&str> = groups.iter().map(|group| group.app_name.as_str()).collect();
        assert_eq!(
            names,
            ["Telegram", "Fractal", "Slack"],
            "a group sits where its newest member does"
        );

        assert_eq!(groups[0].count(), 2);
        assert_eq!(groups[0].newest().id, 5);
        assert_eq!(
            groups[0]
                .notifications
                .iter()
                .map(|view| view.id)
                .collect::<Vec<_>>(),
            [5, 3],
            "members stay newest first inside the group"
        );
        assert_eq!(groups[2].count(), 1);
    }

    #[test]
    fn an_empty_history_groups_into_nothing() {
        assert!(group(&[]).is_empty());
    }

    #[test]
    fn the_history_cap_pushes_out_the_oldest_first() {
        let history: Vec<_> = (0..MAX_HISTORY + 3)
            .map(|index| {
                // Newest first: the highest id is at the front.
                entry((MAX_HISTORY + 3 - index) as u32, "Telegram")
            })
            .collect();

        assert_eq!(
            overflow(&history),
            [3, 2, 1],
            "the three oldest are evicted, in age order"
        );

        assert!(
            overflow(&history[..MAX_HISTORY]).is_empty(),
            "a history at the cap evicts nothing"
        );
    }
}
