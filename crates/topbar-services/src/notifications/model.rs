//! What a notification is, on the wire and on screen.
//!
//! Two families of type live here. [`NotificationView`] and friends are the
//! immutable projection the panel draws — cheap to clone, `PartialEq` so the
//! watch channel can skip a publish that changes nothing. [`PersistedNotification`]
//! is the same thing minus the binary image data, which is what survives a
//! restart.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// How loudly a notification asks for attention.
///
/// The wire values come from the freedesktop `urgency` hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    /// Background information. Toasts like a normal notification.
    Low,
    /// The default.
    #[default]
    Normal,
    /// Must be seen: bypasses Do Not Disturb and never auto-expires.
    Critical,
}

impl Urgency {
    /// Read an urgency hint, clamping anything unexpected to `Normal`.
    pub fn from_wire(value: u8) -> Self {
        match value {
            0 => Self::Low,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }

    /// The hint value for this urgency.
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }

    /// Whether this urgency ignores Do Not Disturb and the expiry timer.
    pub fn is_critical(self) -> bool {
        matches!(self, Self::Critical)
    }
}

/// Why a notification was closed.
///
/// The discriminants are the `NotificationClosed` reason codes from the
/// specification; nothing else may be sent on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The notification's own timeout ran out. Only reachable for transient
    /// notifications: everything else outlives its banner in the history.
    Expired = 1,
    /// The user dismissed it — a close button, a group clear, Clear all.
    Dismissed = 2,
    /// The sending application called `CloseNotification`.
    Requested = 3,
    /// Anything else: evicted by the history cap, or discarded before it was
    /// ever displayed.
    Undefined = 4,
}

impl CloseReason {
    /// The reason code as it goes onto the bus.
    pub fn to_wire(self) -> u32 {
        self as u32
    }
}

/// Raw pixels from the `image-data` hint.
///
/// Kept as the sender gave them: the panel turns them into a texture on the
/// GTK side, which is the only place that knows about pixel formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Bytes per row, which may exceed `width * channels`.
    pub rowstride: i32,
    /// Whether the last channel is alpha.
    pub has_alpha: bool,
    /// Bits per sample. Only 8 is supported by the specification.
    pub bits_per_sample: i32,
    /// Samples per pixel: 3 for RGB, 4 for RGBA.
    pub channels: i32,
    /// The pixels themselves.
    pub data: Vec<u8>,
}

impl ImageData {
    /// Whether the buffer is big enough for the geometry it claims.
    ///
    /// A sender that lies here would make the texture read past the end of the
    /// buffer, so an image that fails this check is dropped and the next icon
    /// source is used instead.
    pub fn is_coherent(&self) -> bool {
        if self.width <= 0 || self.height <= 0 || self.bits_per_sample != 8 {
            return false;
        }
        if !(3..=4).contains(&self.channels) {
            return false;
        }
        if self.has_alpha != (self.channels == 4) {
            return false;
        }
        let row = i64::from(self.width) * i64::from(self.channels);
        if i64::from(self.rowstride) < row {
            return false;
        }
        let needed = i64::from(self.rowstride) * i64::from(self.height - 1) + row;
        i64::try_from(self.data.len()).is_ok_and(|len| len >= needed)
    }
}

/// Everywhere an icon for a notification could come from.
///
/// The panel walks these in order; the resolution itself is a GTK concern, so
/// the service only carries the candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IconSource {
    /// Pixels sent inline with the notification, if they were coherent.
    pub image_data: Option<Arc<ImageData>>,
    /// A `file://` URI, an absolute path, or an icon-theme name.
    pub image_path: Option<String>,
    /// The `app_icon` argument: an icon name, path, or empty.
    pub app_icon: String,
    /// The `desktop-entry` hint, e.g. `org.telegram.desktop`.
    pub desktop_entry: Option<String>,
}

/// One action button offered by a notification.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Action {
    /// The key sent back in `ActionInvoked`.
    pub key: String,
    /// What the button says.
    pub label: String,
}

impl Action {
    /// Whether this is the implicit action a click on the body invokes.
    pub fn is_default(&self) -> bool {
        self.key == "default"
    }
}

/// A notification as the panel draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationView {
    /// The id the sender knows it by.
    pub id: u32,
    /// Display name of the sending application.
    pub app_name: String,
    /// The headline.
    pub summary: String,
    /// The body, which may carry Pango markup.
    pub body: String,
    /// Buttons, in the order the sender listed them.
    pub actions: Vec<Action>,
    /// How loudly it asks for attention.
    pub urgency: Urgency,
    /// Where its icon comes from.
    pub icon: IconSource,
    /// When it arrived, in seconds since the Unix epoch.
    pub timestamp: i64,
}

impl NotificationView {
    /// The action a click on the body invokes, if the sender offered one.
    pub fn default_action(&self) -> Option<&Action> {
        self.actions.iter().find(|action| action.is_default())
    }

    /// The actions that get their own button: everything but `default`.
    pub fn buttons(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|action| !action.is_default())
    }

    /// The names the compositor might know the sending application by.
    ///
    /// The `desktop-entry` hint first: it is the application id, which is what
    /// a Wayland window reports as its own. The display name is the fallback,
    /// because plenty of senders give nothing else — and for a good few of them
    /// ("Slack", "Telegram") it *is* the window's app id.
    pub fn app_identities(&self) -> Vec<&str> {
        [self.icon.desktop_entry.as_deref(), Some(&self.app_name)]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
            .collect()
    }
}

/// A notification currently shown as a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastView {
    /// The notification itself.
    pub notification: Arc<NotificationView>,
    /// Whether its timer is held because the pointer is over it.
    pub paused: bool,
}

/// One application's notifications in the history list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupView {
    /// Stable grouping key: the desktop entry when there is one, else the
    /// application name folded to lower case.
    pub key: String,
    /// What the group header says — the newest member's application name.
    pub app_name: String,
    /// Members, newest first. Never empty.
    pub notifications: Vec<Arc<NotificationView>>,
}

impl GroupView {
    /// The newest notification in the group.
    pub fn newest(&self) -> &Arc<NotificationView> {
        self.notifications
            .first()
            .expect("a group is only built around at least one notification")
    }

    /// How many notifications the group holds.
    pub fn count(&self) -> usize {
        self.notifications.len()
    }
}

/// Everything the notification widgets draw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifState {
    /// Whether the panel owns `org.freedesktop.Notifications`.
    ///
    /// False means another daemon is running (or the bus is unreachable): the
    /// history still shows whatever was persisted, but nothing new arrives.
    pub enabled: bool,
    /// Whether Do Not Disturb is on.
    pub dnd: bool,
    /// Toasts on screen, newest first.
    pub toasts: Vec<ToastView>,
    /// History, grouped by application, newest group first.
    pub history: Vec<GroupView>,
    /// History entries that have arrived since the panel was last opened.
    pub unseen_count: usize,
}

impl NotifState {
    /// Whether the history has anything in it.
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Every history notification, newest first, ignoring the grouping.
    pub fn flat_history(&self) -> impl Iterator<Item = &Arc<NotificationView>> {
        self.history
            .iter()
            .flat_map(|group| group.notifications.iter())
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// The notification section of the panel's state file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedNotifications {
    /// Whether Do Not Disturb was on when the panel last ran.
    pub dnd: bool,
    /// The next id to hand out, so a restart cannot reuse a live id.
    pub next_id: u32,
    /// History, newest first, bounded by the daemon's cap.
    pub history: Vec<PersistedNotification>,
}

/// A history entry as it is stored on disk.
///
/// `image-data` is deliberately absent: it is megabytes of binary that would
/// have to be base64'd into the state file, and an avatar is not worth that.
/// Restored entries fall back to the next icon source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedNotification {
    /// The id it had.
    pub id: u32,
    /// Display name of the sending application.
    pub app_name: String,
    /// The `app_icon` argument.
    pub app_icon: String,
    /// The headline.
    pub summary: String,
    /// The body.
    pub body: String,
    /// Its action buttons.
    pub actions: Vec<Action>,
    /// How loudly it asked for attention.
    pub urgency: Urgency,
    /// When it arrived, in seconds since the Unix epoch.
    pub timestamp: i64,
    /// The `desktop-entry` hint.
    pub desktop_entry: Option<String>,
    /// The `image-path` hint.
    pub image_path: Option<String>,
}

impl PersistedNotification {
    /// Store a notification.
    pub fn from_view(view: &NotificationView) -> Self {
        Self {
            id: view.id,
            app_name: view.app_name.clone(),
            app_icon: view.icon.app_icon.clone(),
            summary: view.summary.clone(),
            body: view.body.clone(),
            actions: view.actions.clone(),
            urgency: view.urgency,
            timestamp: view.timestamp,
            desktop_entry: view.icon.desktop_entry.clone(),
            image_path: view.icon.image_path.clone(),
        }
    }

    /// Restore one.
    pub fn into_view(self) -> NotificationView {
        NotificationView {
            id: self.id,
            app_name: self.app_name,
            summary: self.summary,
            body: self.body,
            actions: self.actions,
            urgency: self.urgency,
            icon: IconSource {
                image_data: None,
                image_path: self.image_path,
                app_icon: self.app_icon,
                desktop_entry: self.desktop_entry,
            },
            timestamp: self.timestamp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> NotificationView {
        NotificationView {
            id: 9,
            app_name: "Telegram".into(),
            summary: "Ada".into(),
            body: "see you at six".into(),
            actions: vec![
                Action {
                    key: "default".into(),
                    label: String::new(),
                },
                Action {
                    key: "reply".into(),
                    label: "Reply".into(),
                },
            ],
            urgency: Urgency::Normal,
            icon: IconSource {
                image_data: Some(Arc::new(ImageData {
                    width: 1,
                    height: 1,
                    rowstride: 4,
                    has_alpha: true,
                    bits_per_sample: 8,
                    channels: 4,
                    data: vec![1, 2, 3, 4],
                })),
                image_path: Some("/tmp/avatar.png".into()),
                app_icon: "telegram".into(),
                desktop_entry: Some("org.telegram.desktop".into()),
            },
            timestamp: 1_754_300_000,
        }
    }

    #[test]
    fn urgency_round_trips_through_the_wire_value() {
        for urgency in [Urgency::Low, Urgency::Normal, Urgency::Critical] {
            assert_eq!(Urgency::from_wire(urgency.to_wire()), urgency);
        }
        assert_eq!(
            Urgency::from_wire(7),
            Urgency::Normal,
            "clamped, not dropped"
        );
        assert!(Urgency::Critical.is_critical());
        assert!(!Urgency::Normal.is_critical());
    }

    #[test]
    fn the_desktop_entry_leads_the_names_the_sender_is_known_by() {
        assert_eq!(
            view().app_identities(),
            vec!["org.telegram.desktop", "Telegram"]
        );

        let mut nameless = view();
        nameless.icon.desktop_entry = None;
        assert_eq!(nameless.app_identities(), vec!["Telegram"]);

        let mut blank = view();
        blank.icon.desktop_entry = Some("   ".into());
        blank.app_name = String::new();
        assert!(blank.app_identities().is_empty());
    }

    #[test]
    fn close_reasons_use_the_specified_codes() {
        assert_eq!(CloseReason::Expired.to_wire(), 1);
        assert_eq!(CloseReason::Dismissed.to_wire(), 2);
        assert_eq!(CloseReason::Requested.to_wire(), 3);
        assert_eq!(CloseReason::Undefined.to_wire(), 4);
    }

    #[test]
    fn the_default_action_is_separated_from_the_buttons() {
        let view = view();
        assert_eq!(
            view.default_action().expect("a default action").key,
            "default"
        );
        let buttons: Vec<&str> = view.buttons().map(|action| action.key.as_str()).collect();
        assert_eq!(buttons, ["reply"]);
    }

    #[test]
    fn persistence_drops_the_pixels_and_keeps_everything_else() {
        let original = view();
        let restored = PersistedNotification::from_view(&original).into_view();

        assert_eq!(restored.icon.image_data, None, "pixels are not persisted");
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.summary, original.summary);
        assert_eq!(restored.body, original.body);
        assert_eq!(restored.actions, original.actions);
        assert_eq!(restored.urgency, original.urgency);
        assert_eq!(restored.timestamp, original.timestamp);
        assert_eq!(restored.icon.app_icon, original.icon.app_icon);
        assert_eq!(restored.icon.image_path, original.icon.image_path);
        assert_eq!(restored.icon.desktop_entry, original.icon.desktop_entry);
    }

    #[test]
    fn persisted_history_survives_json() {
        let state = PersistedNotifications {
            dnd: true,
            next_id: 12,
            history: vec![PersistedNotification::from_view(&view())],
        };
        let json = serde_json::to_string(&state).expect("serialise");
        let back: PersistedNotifications = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, state);
    }

    #[test]
    fn a_state_file_missing_every_field_still_loads() {
        let back: PersistedNotifications = serde_json::from_str("{}").expect("deserialise");
        assert_eq!(back, PersistedNotifications::default());
    }

    #[test]
    fn image_geometry_is_checked_against_the_buffer() {
        let sound = ImageData {
            width: 2,
            height: 2,
            rowstride: 8,
            has_alpha: true,
            bits_per_sample: 8,
            channels: 4,
            data: vec![0; 16],
        };
        assert!(sound.is_coherent());

        // Padded rows are legal as long as the buffer covers them.
        let padded = ImageData {
            rowstride: 12,
            data: vec![0; 12 + 8],
            ..sound.clone()
        };
        assert!(padded.is_coherent());

        let truncated = ImageData {
            data: vec![0; 4],
            ..sound.clone()
        };
        assert!(
            !truncated.is_coherent(),
            "a short buffer would be read past"
        );

        let narrow = ImageData {
            rowstride: 4,
            ..sound.clone()
        };
        assert!(!narrow.is_coherent(), "rowstride below one row is nonsense");

        for bad in [
            ImageData {
                width: 0,
                ..sound.clone()
            },
            ImageData {
                height: -1,
                ..sound.clone()
            },
            ImageData {
                bits_per_sample: 16,
                ..sound.clone()
            },
            ImageData {
                channels: 2,
                ..sound.clone()
            },
            ImageData {
                has_alpha: false,
                ..sound.clone()
            },
        ] {
            assert!(!bad.is_coherent(), "{bad:?} should be rejected");
        }
    }
}
