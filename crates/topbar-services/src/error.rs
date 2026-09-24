//! The error every mutating service call returns.
//!
//! Two audiences, one type: [`Display`](std::fmt::Display) carries the detail a
//! log needs, and [`SvcError::user_message`] carries the one short sentence a
//! toast or an inline row can show. Widgets never format the detail themselves.

use std::time::Duration;

/// Something a service could not do.
#[derive(Debug, thiserror::Error)]
pub enum SvcError {
    /// `$NIRI_SOCKET` is not set, so there is no compositor to talk to.
    #[error("NIRI_SOCKET is not set; the panel is not running under niri")]
    NoNiriSocket,

    /// Desktop markers and endpoint environment cannot select one backend.
    #[error("compositor selection failed: {0}")]
    CompositorSelection(String),

    /// The selected Hyprland session did not export its endpoint.
    #[error(
        "Hyprland needs XDG_RUNTIME_DIR and HYPRLAND_INSTANCE_SIGNATURE; restart topbar in the selected session"
    )]
    NoHyprlandSocket,

    /// The socket could not be reached, or the connection broke mid-request.
    #[error("compositor socket I/O failed: {0}")]
    Io(#[source] std::io::Error),

    /// The compositor did not answer in time.
    #[error("compositor did not answer within {0:?}")]
    Timeout(Duration),

    /// The compositor answered, refusing the request.
    #[error("compositor refused the request: {0}")]
    Rejected(String),

    /// The compositor answered with something we cannot read.
    #[error("unreadable reply from compositor: {0}")]
    Protocol(String),

    /// The session bus could not be reached, or refused a request.
    ///
    /// Carries the detail as text rather than a `zbus::Error` because it is
    /// published through a watch channel, and every value in one has to clone.
    #[error("session bus error: {0}")]
    Bus(String),

    /// Another process owns a well-known name the panel needs.
    #[error("{0} is owned by another process")]
    NameTaken(String),

    /// The notification the panel was asked to act on has gone.
    #[error("notification {0} no longer exists")]
    GoneNotification(u32),

    /// There is no media player to act on, or the one named has gone.
    #[error("no media player answered: {0}")]
    NoPlayer(String),

    /// The tray item the panel was asked to act on has left the bus.
    #[error("no tray item answered: {0}")]
    NoTrayItem(String),

    /// The tray item is there, but it publishes no menu to open.
    #[error("tray item {0} has no menu")]
    NoTrayMenu(String),

    /// A request to a web service failed, or it answered with an error.
    ///
    /// Carries the reason as text for the same reason [`SvcError::Bus`] does:
    /// it travels through a channel and everything in one has to clone.
    #[error("web request failed: {0}")]
    Http(String),

    /// A web service answered 429: too many requests, come back later.
    ///
    /// Its own variant rather than an [`SvcError::Http`] because the panel's
    /// answer to it is different — wait longer, keep what is on screen — and
    /// because "could not reach the service" would be a lie about a service
    /// that answered promptly and in full.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// A latitude/longitude pair that is not a point on Earth.
    #[error("coordinates {0} are out of range")]
    Coordinates(String),

    /// No sound server is answering.
    #[error("no sound server is answering")]
    AudioUnavailable,

    /// The sound server has no device of that kind to act on.
    ///
    /// Carries `"output"` or `"input"` so the log line says which, while the
    /// user-facing sentence stays the same either way — a user who asked for
    /// the volume and got nothing does not need the word "sink".
    #[error("the sound server has no usable {0} device")]
    AudioDevice(&'static str),

    /// This machine has no backlight to adjust.
    #[error("no backlight device was found under /sys/class/backlight")]
    NoBacklight,

    /// The idle inhibitor could not be taken or released.
    #[error("logind refused the idle inhibitor: {0}")]
    Inhibitor(String),

    /// The power-profiles daemon refused a profile, or is not there.
    #[error("the power-profiles daemon refused the profile: {0}")]
    PowerProfile(String),

    /// The charge limit could not be read or written.
    #[error("the charge limit could not be set: {0}")]
    Battery(String),

    /// logind refused to shut down, restart or suspend the machine.
    #[error("logind refused the power action: {0}")]
    PowerAction(String),

    /// NetworkManager refused a change, or there was nothing to change.
    ///
    /// Covers joining a network, switching the radio and switching a VPN. The
    /// user-facing sentence is deliberately the same for all three: a person
    /// who pressed a Wi-Fi row and got nothing does not need the word
    /// "activation", and the detail is in the log line beside it.
    #[error("the network could not be changed: {0}")]
    Network(String),

    /// BlueZ refused a change, or there was nothing to change.
    ///
    /// Covers the radio switch, connecting a device and answering a pairing.
    /// One variant for all three for the same reason the network has one: a
    /// person who pressed a device row and got nothing does not need the word
    /// "adapter", and the detail is in the log line beside it.
    #[error("Bluetooth could not be changed: {0}")]
    Bluetooth(String),

    /// A command the user configured could not be started.
    #[error("could not run `{command}`: {reason}")]
    Command {
        /// What was being run.
        command: String,
        /// Why it did not start, or how it ended.
        reason: String,
    },

    /// A service task has stopped, so its commands go nowhere.
    #[error("the {0} service is not running")]
    ServiceStopped(&'static str),
}

impl SvcError {
    /// One short sentence, written for the user rather than the log.
    ///
    /// Deliberately free of detail: the specifics belong in the log line that
    /// accompanies the toast, not in the toast.
    pub fn user_message(&self) -> &'static str {
        match self {
            Self::NoNiriSocket | Self::NoHyprlandSocket | Self::Io(_) | Self::Timeout(_) => {
                "Could not reach the compositor"
            }
            Self::CompositorSelection(_) => "Could not select the compositor",
            Self::Rejected(_) => "The compositor refused the request",
            Self::Protocol(_) => "The compositor sent an unexpected reply",
            Self::Bus(_) => "Could not reach the session bus",
            Self::NameTaken(_) => "Another notification daemon is running",
            Self::GoneNotification(_) => "That notification is no longer available",
            Self::NoPlayer(_) => "No media player is available",
            Self::NoTrayItem(_) => "That tray icon is no longer there",
            Self::NoTrayMenu(_) => "That tray icon has no menu",
            Self::Http(_) => "Could not reach the service",
            Self::RateLimited(_) => "Rate limited, retrying later",
            Self::Coordinates(_) => "Those coordinates are out of range",
            Self::AudioUnavailable => "Could not reach the sound server",
            Self::AudioDevice(_) => "No audio device is available",
            Self::NoBacklight => "This screen has no adjustable backlight",
            Self::Inhibitor(_) => "Could not change the idle inhibitor",
            Self::PowerProfile(_) => "Could not change the power mode",
            Self::Battery(_) => "Could not change the charge limit",
            Self::PowerAction(_) => "The system refused that power action",
            Self::Network(_) => "Could not change the network",
            Self::Bluetooth(_) => "Could not change Bluetooth",
            Self::Command { .. } => "That command could not be run",
            Self::ServiceStopped(_) => "That part of the panel has stopped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_messages_are_short_and_detail_free() {
        let errors = [
            SvcError::NoNiriSocket,
            SvcError::Io(std::io::Error::other("connection refused")),
            SvcError::Timeout(Duration::from_secs(2)),
            SvcError::Rejected("no such workspace".into()),
            SvcError::Protocol("expected Handled".into()),
            SvcError::Bus("connection refused".into()),
            SvcError::NameTaken("org.freedesktop.Notifications".into()),
            SvcError::GoneNotification(7),
            SvcError::NoPlayer("org.mpris.MediaPlayer2.spotify".into()),
            SvcError::NoTrayItem(":1.42/StatusNotifierItem".into()),
            SvcError::NoTrayMenu(":1.42/StatusNotifierItem".into()),
            SvcError::Http("connection timed out".into()),
            SvcError::RateLimited("you have exceeded the rate limit".into()),
            SvcError::Coordinates("100, 0".into()),
            SvcError::AudioUnavailable,
            SvcError::AudioDevice("output"),
            SvcError::NoBacklight,
            SvcError::Inhibitor("Interactive authentication required".into()),
            SvcError::PowerProfile("no power-profiles daemon is running".into()),
            SvcError::Battery("start 90% must be below stop 80%".into()),
            SvcError::PowerAction("Interactive authentication required".into()),
            SvcError::Network("802-11-wireless-security.psk was refused".into()),
            SvcError::Bluetooth("br-connection-canceled".into()),
            SvcError::Command {
                command: "loginctl lock-session".into(),
                reason: "No such file or directory".into(),
            },
            SvcError::ServiceStopped("notifications"),
        ];
        for error in errors {
            let message = error.user_message();
            assert!(message.len() < 60, "{message} is too long for a toast");
            assert!(!message.contains("niri"), "{message} leaks the transport");
            assert!(
                !message.contains("org.freedesktop"),
                "{message} leaks a bus name"
            );
            assert!(!message.is_empty());
        }
    }

    #[test]
    fn display_keeps_the_detail_the_log_needs() {
        let error = SvcError::Rejected("no such workspace".into());
        assert!(error.to_string().contains("no such workspace"));
    }
}
