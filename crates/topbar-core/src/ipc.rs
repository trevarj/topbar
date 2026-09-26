//! CLI ↔ panel IPC protocol.
//!
//! Messages travel over a `SOCK_STREAM` unix socket at
//! `$XDG_RUNTIME_DIR/topbar.sock` as u32 length-prefixed (little-endian)
//! JSON frames. Length prefixing removes v1's 256-byte datagram truncation and
//! the versioned handshake lets an old CLI fail loudly against a new panel.

use serde::{Deserialize, Serialize};

/// Wire protocol version. Bump on any incompatible change to the enums below.
pub const PROTOCOL_VERSION: u32 = 2;

/// Socket file name inside `$XDG_RUNTIME_DIR`.
pub const SOCKET_NAME: &str = "topbar.sock";

/// Lock file name inside `$XDG_RUNTIME_DIR` guarding single-instance startup.
pub const LOCK_NAME: &str = "topbar.lock";

/// Largest frame the panel or CLI will accept (1 MiB).
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

/// Length of the u32-LE frame header.
pub const HEADER_LEN: usize = 4;

/// Message the CLI sends to a running panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Protocol handshake; the panel replies with its own version.
    Hello {
        /// Version the caller speaks.
        version: u32,
    },
    /// Show the volume OSD for an externally applied volume change.
    VolumeChanged {
        /// Current volume percentage.
        percent: u32,
        /// Whether the sink is muted.
        muted: bool,
    },
    /// Show the "audio unavailable" OSD.
    VolumeUnavailable,
    /// Show the brightness OSD for an externally applied change.
    BrightnessChanged {
        /// Current brightness percentage.
        percent: u32,
    },
    /// Toggle the idle/sleep inhibitor.
    ToggleInhibitor,
    /// Control bar visibility.
    Bar {
        /// Requested visibility change.
        action: VisibilityAction,
    },
    /// Control a widget popover.
    Popover {
        /// Requested popover change.
        action: PopoverAction,
    },
    /// Control the panel's full-screen launcher.
    Launcher {
        /// Requested visibility change.
        action: VisibilityAction,
    },
    /// Dismiss any panel-owned transient input surface before a standalone dialog opens.
    DismissTransient,
    /// Control media playback through the panel's MPRIS service.
    Media {
        /// Requested playback change.
        action: MediaAction,
    },
    /// Reload configuration and stylesheet.
    Reload,
    /// Dump panel state for debugging.
    Dump {
        /// What to dump.
        target: DumpTarget,
        /// Render it as JSON rather than as the human-readable form.
        json: bool,
    },
}

/// Reply the panel sends back for every [`IpcRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum IpcResponse {
    /// Handshake reply carrying the panel's protocol version.
    Hello {
        /// Version the panel speaks.
        version: u32,
    },
    /// The request was applied; nothing to report.
    Ok,
    /// The request was applied and produced text output.
    Value {
        /// Human-readable payload (already formatted for stdout).
        text: String,
    },
    /// The request failed.
    Error {
        /// Human-readable failure reason.
        message: String,
    },
}

/// Show/hide/toggle triple shared by bar and popover requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisibilityAction {
    /// Make visible.
    Show,
    /// Hide.
    Hide,
    /// Flip current visibility.
    Toggle,
}

/// Popover control, addressed by widget name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopoverAction {
    /// Open the named widget's popover.
    Show(String),
    /// Close the named popover, or the active one when `None`.
    Hide(Option<String>),
    /// Toggle the named widget's popover.
    Toggle(String),
}

/// MPRIS playback control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    /// Toggle play/pause on the most relevant player.
    PlayPause,
    /// Skip to the next track.
    Next,
    /// Go to the previous track.
    Previous,
    /// Stop playback.
    Stop,
    /// Report the current playback status.
    Status,
}

/// What `topbar dump` should print.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DumpTarget {
    /// The compiled-in example configuration.
    DefaultConfig,
    /// The effective configuration the panel is running with.
    Config,
    /// A snapshot of live service state.
    State,
    /// Both of the above: what `topbar dump` with no target prints.
    All,
}

/// Framing errors for the length-prefixed JSON protocol.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The declared frame length exceeds [`MAX_FRAME_LEN`].
    #[error("frame too large: {len} bytes (max {MAX_FRAME_LEN})")]
    TooLarge {
        /// Declared length.
        len: u32,
    },
    /// The buffer ended before a complete frame was available.
    #[error("incomplete frame: need {needed} more bytes")]
    Incomplete {
        /// Number of bytes still missing.
        needed: usize,
    },
    /// The frame payload was not valid JSON for the expected type.
    #[error("malformed frame payload: {0}")]
    Json(#[from] serde_json::Error),
}

/// Serialize `value` into a u32-LE length-prefixed JSON frame.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(value)?;
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge { len });
    }

    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode one frame from the front of `buf`.
///
/// On success returns the decoded value and the number of bytes consumed, so
/// callers can drain a streaming buffer. Returns [`FrameError::Incomplete`]
/// when more bytes are needed — the buffer must be left untouched in that case.
pub fn decode_frame<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> Result<(T, usize), FrameError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameError::Incomplete {
            needed: HEADER_LEN - buf.len(),
        });
    }

    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge { len });
    }

    let total = HEADER_LEN + len as usize;
    if buf.len() < total {
        return Err(FrameError::Incomplete {
            needed: total - buf.len(),
        });
    }

    let value = serde_json::from_slice(&buf[HEADER_LEN..total])?;
    Ok((value, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_request() {
        let request = IpcRequest::Bar {
            action: VisibilityAction::Toggle,
        };
        let frame = encode_frame(&request).unwrap();
        let (decoded, consumed) = decode_frame::<IpcRequest>(&frame).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn round_trips_launcher_and_dismissal() {
        for request in [
            IpcRequest::Launcher {
                action: VisibilityAction::Toggle,
            },
            IpcRequest::DismissTransient,
        ] {
            let frame = encode_frame(&request).unwrap();
            let (decoded, _) = decode_frame::<IpcRequest>(&frame).unwrap();
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn round_trips_a_response() {
        let response = IpcResponse::Value {
            text: "42".to_string(),
        };
        let frame = encode_frame(&response).unwrap();
        let (decoded, _) = decode_frame::<IpcResponse>(&frame).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn decodes_two_frames_from_one_buffer() {
        let mut buf = encode_frame(&IpcRequest::Reload).unwrap();
        buf.extend(encode_frame(&IpcRequest::ToggleInhibitor).unwrap());

        let (first, consumed) = decode_frame::<IpcRequest>(&buf).unwrap();
        assert_eq!(first, IpcRequest::Reload);
        let (second, _) = decode_frame::<IpcRequest>(&buf[consumed..]).unwrap();
        assert_eq!(second, IpcRequest::ToggleInhibitor);
    }

    #[test]
    fn reports_missing_header_and_payload_bytes() {
        let frame = encode_frame(&IpcRequest::Reload).unwrap();

        let err = decode_frame::<IpcRequest>(&frame[..2]).unwrap_err();
        assert!(matches!(err, FrameError::Incomplete { needed: 2 }));

        let err = decode_frame::<IpcRequest>(&frame[..frame.len() - 3]).unwrap_err();
        assert!(matches!(err, FrameError::Incomplete { needed: 3 }));
    }

    #[test]
    fn rejects_oversized_declared_length() {
        let mut frame = (MAX_FRAME_LEN + 1).to_le_bytes().to_vec();
        frame.extend_from_slice(b"{}");
        let err = decode_frame::<IpcRequest>(&frame).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[test]
    fn rejects_malformed_payload() {
        let mut frame = 2u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"[]");
        assert!(matches!(
            decode_frame::<IpcRequest>(&frame).unwrap_err(),
            FrameError::Json(_)
        ));
    }

    #[test]
    fn handshake_carries_the_protocol_version() {
        let frame = encode_frame(&IpcRequest::Hello {
            version: PROTOCOL_VERSION,
        })
        .unwrap();
        let (decoded, _) = decode_frame::<IpcRequest>(&frame).unwrap();
        assert_eq!(
            decoded,
            IpcRequest::Hello {
                version: PROTOCOL_VERSION
            }
        );
    }
}
