//! Drawing notifications: the pieces the toast surface and the history column
//! both need.
//!
//! The two surfaces show the same notifications in different shapes — a banner
//! is wide and short, a history row is narrow and stacked — but they resolve
//! icons the same way, sanitise bodies the same way, and say "5m ago" the same
//! way, so those three live here rather than in either of them.

pub mod icon;
pub mod markup;

use chrono::{DateTime, Local, TimeZone};
use gtk4::gdk;
use topbar_services::{NotificationView, Services};

use crate::bridge::{self, ActionScope};
use crate::wayland::activation;

/// Pixel size of the icon on a toast.
pub const TOAST_ICON: i32 = 32;
/// Pixel size of the icon on a history group row.
pub const ROW_ICON: i32 = 24;

/// What a click on a notification does, wherever it is drawn.
///
/// A notification is a way back to whatever raised it, so a click takes the
/// user there. The sender's default action gets first refusal — only it knows
/// which conversation or which tab the click meant — and the application's
/// window is raised either way, because a sender that offered no action still
/// has the window the user just asked for. The activation token is what lets
/// the application raise *itself*; `focus_app` is the fallback for the many
/// senders that ignore it, and for the ones that offer no action at all.
///
/// Returns whether a default action was invoked. Invoking one closes the
/// notification behind it, so the caller only has to take it off screen itself
/// when this says `false`.
pub fn activate(
    services: &Services,
    notification: &NotificationView,
    surface: Option<gdk::Surface>,
    scope: ActionScope,
) -> bool {
    let identities: Vec<String> = notification
        .app_identities()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let compositor = services.compositor.handle();

    // The token has to be asked for here: it needs the surface the click landed
    // on, and surfaces belong to the main thread.
    let action = notification.default_action().map(|action| {
        (
            notification.id,
            action.key.clone(),
            activation::token(None, surface.as_ref()),
        )
    });
    let invoked = action.is_some();
    let notifications = services.notifications.handle().clone();

    bridge::act(scope, async move {
        if let Some((id, key, token)) = action {
            notifications.invoke_action(id, key, token).await?;
        }
        // After the action, not before: an application asked to open a
        // conversation should pick which window that is before we raise one.
        let identities: Vec<&str> = identities.iter().map(String::as_str).collect();
        compositor.focus_app(&identities).await?;
        Ok(())
    });

    invoked
}

/// How old a notification reads as, in words.
///
/// Carried over from v1 unchanged, including "Just now" for anything inside a
/// minute: the history refreshes on the clock's minute tick, so a row that
/// said "1m ago" a second after it arrived would be wrong for 59 of the next
/// 60 seconds.
pub fn relative_time(timestamp: i64, now: DateTime<Local>) -> String {
    let seconds = now.timestamp().saturating_sub(timestamp);
    match seconds {
        // A clock that has gone backwards — an NTP correction, a resume — must
        // not produce "-3m ago".
        i64::MIN..60 => "Just now".to_string(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// The exact moment a notification arrived, for its tooltip.
pub fn absolute_time(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|when| when.format("%A, %B %-d at %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32, minute: u32, second: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 8, 4, hour, minute, second)
            .single()
            .expect("unambiguous local time")
    }

    #[test]
    fn anything_inside_a_minute_is_just_now() {
        let now = at(12, 0, 0);
        assert_eq!(relative_time(now.timestamp(), now), "Just now");
        assert_eq!(relative_time(now.timestamp() - 59, now), "Just now");
    }

    #[test]
    fn minutes_hours_and_days() {
        let now = at(12, 0, 0);
        assert_eq!(relative_time(now.timestamp() - 60, now), "1m ago");
        assert_eq!(relative_time(now.timestamp() - 59 * 60, now), "59m ago");
        assert_eq!(relative_time(now.timestamp() - 3_600, now), "1h ago");
        assert_eq!(relative_time(now.timestamp() - 23 * 3_600, now), "23h ago");
        assert_eq!(relative_time(now.timestamp() - 86_400, now), "1d ago");
        assert_eq!(relative_time(now.timestamp() - 9 * 86_400, now), "9d ago");
    }

    #[test]
    fn a_clock_that_jumped_backwards_does_not_produce_negative_ages() {
        let now = at(12, 0, 0);
        assert_eq!(relative_time(now.timestamp() + 3_600, now), "Just now");
    }

    #[test]
    fn the_boundaries_land_on_the_right_side() {
        let now = at(12, 0, 0);
        // Exactly one minute is a minute, not "just now"; the same at an hour.
        assert_eq!(relative_time(now.timestamp() - 60, now), "1m ago");
        assert_eq!(relative_time(now.timestamp() - 3_599, now), "59m ago");
        assert_eq!(relative_time(now.timestamp() - 86_399, now), "23h ago");
    }

    #[test]
    fn the_absolute_time_reads_as_a_date() {
        let when = at(9, 5, 0);
        assert_eq!(
            absolute_time(when.timestamp()),
            "Tuesday, August 4 at 09:05"
        );
    }
}
