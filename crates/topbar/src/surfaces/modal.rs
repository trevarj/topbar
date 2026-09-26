//! Shared layer-shell ownership for panel dialogs and the launcher.

use std::path::PathBuf;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{Window, gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use niri_ipc::{Reply, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;
use topbar_services::Runtime;
use topbar_services::ipc::InputLock;
use tracing::warn;

use crate::style::classes;
use crate::surfaces::popovers;
use crate::wayland::blur::{self, BlurAttachment};

/// Claim keyboard ownership without ever waiting on GTK's main thread.
pub fn claim_input() -> Option<InputLock> {
    match InputLock::try_acquire() {
        Ok(lock) => lock,
        Err(error) => {
            warn!("could not claim modal keyboard input: {error}");
            None
        }
    }
}

/// Whether an ordinary popover may take focus now.
pub fn input_available() -> bool {
    match InputLock::try_acquire() {
        Ok(lock) => lock.is_some(),
        Err(error) => {
            warn!("modal input guard is unavailable: {error}");
            true
        }
    }
}

/// Close ordinary popovers before a modal claims keyboard focus.
pub fn close_popovers() {
    popovers::dispatch(&topbar_core::ipc::PopoverAction::Hide(None), None);
    // A click may still be propagating through the popover's own gesture.
    glib::idle_add_local_once(|| {
        popovers::dispatch(&topbar_core::ipc::PopoverAction::Hide(None), None);
    });
}

/// Find the monitor containing an anchor, falling back to the first output.
pub fn monitor_of(anchor: &impl IsA<gtk4::Widget>) -> Option<gdk::Monitor> {
    let widget = anchor.as_ref();
    let display = widget.display();
    let surface = widget
        .root()
        .and_then(|root| root.downcast::<Window>().ok())
        .and_then(|window| window.surface());
    surface
        .and_then(|surface| display.monitor_at_surface(&surface))
        .or_else(|| display.monitors().item(0).and_downcast::<gdk::Monitor>())
}

/// Ask niri which output is focused before a standalone GTK loop starts.
/// A missing or stalled compositor leaves monitor selection to GDK's fallback.
pub fn focused_output_connector() -> Option<String> {
    let socket = PathBuf::from(std::env::var_os("NIRI_SOCKET")?);
    Runtime::handle().block_on(async {
        timeout(Duration::from_millis(250), async {
            let mut stream = UnixStream::connect(socket).await.ok()?;
            let mut request = serde_json::to_vec(&Request::FocusedOutput).ok()?;
            request.push(b'\n');
            stream.write_all(&request).await.ok()?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.ok()?;
            let reply: Reply = serde_json::from_str(&line).ok()?;
            match reply {
                Ok(Response::FocusedOutput(Some(output))) => Some(output.name),
                _ => None,
            }
        })
        .await
        .ok()
        .flatten()
    })
}

/// Select the focused output by connector, or the first available monitor.
pub fn standalone_monitor(display: &gdk::Display, connector: Option<&str>) -> Option<gdk::Monitor> {
    let monitors = display.monitors();
    let available = (0..monitors.n_items())
        .filter_map(|index| monitors.item(index).and_downcast::<gdk::Monitor>())
        .collect::<Vec<_>>();
    let connectors = available
        .iter()
        .map(|monitor| monitor.connector())
        .collect::<Vec<_>>();
    let index = selected_monitor_index(connector, &connectors)?;
    available.into_iter().nth(index)
}

fn selected_monitor_index(
    focused: Option<&str>,
    connectors: &[Option<glib::GString>],
) -> Option<usize> {
    if connectors.is_empty() {
        return None;
    }
    focused
        .and_then(|focused| {
            connectors
                .iter()
                .position(|connector| connector.as_deref() == Some(focused))
        })
        .or(Some(0))
}

/// Make the centered foreground surface of a modal.
pub fn centered_window(monitor: Option<&gdk::Monitor>, namespace: &str) -> Window {
    let window = Window::builder().decorated(false).resizable(false).build();
    window.init_layer_shell();
    window.set_namespace(Some(namespace));
    window.set_layer(Layer::Overlay);
    if let Some(monitor) = monitor {
        window.set_monitor(Some(monitor));
    }
    window.set_exclusive_zone(0);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    window
}

/// Make the full-monitor surface behind a modal.
pub fn backdrop(monitor: Option<&gdk::Monitor>, namespace: &str, class: &str) -> Window {
    let window = Window::builder().decorated(false).build();
    // GTK windows have their own opaque background unless the shared modal
    // window rule clears it; the child provides the visible scrim.
    window.add_css_class(classes::LOCATION_WINDOW);
    window.init_layer_shell();
    window.set_namespace(Some(namespace));
    window.set_layer(Layer::Overlay);
    if let Some(monitor) = monitor {
        window.set_monitor(Some(monitor));
    }
    window.set_exclusive_zone(-1);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_keyboard_mode(KeyboardMode::None);
    let surface = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    surface.add_css_class(class);
    surface.set_hexpand(true);
    surface.set_vexpand(true);
    window.set_child(Some(&surface));
    window
}

/// Request compositor blur for the entire visible area of a backdrop.
///
/// The shared blur manager turns this into an inert guard when `theme.blur` is
/// disabled, `TOPBAR_NO_BLUR` is set, or the compositor does not advertise the
/// protocol. The backdrop's CSS scrim remains visible in each of those cases.
pub fn attach_backdrop_blur(backdrop: &Window) -> BlurAttachment {
    let Some(surface) = backdrop.child() else {
        return BlurAttachment::inert();
    };
    blur::attach(backdrop, &surface, || 0)
}

#[cfg(test)]
mod tests {
    use super::selected_monitor_index;
    use gtk4::glib::GString;

    #[test]
    fn standalone_monitor_selects_focused_connector() {
        let connectors = [Some(GString::from("eDP-1")), Some(GString::from("DP-2"))];
        assert_eq!(selected_monitor_index(Some("DP-2"), &connectors), Some(1));
    }

    #[test]
    fn standalone_monitor_falls_back_when_focus_is_missing_or_stale() {
        let connectors = [Some(GString::from("eDP-1")), None];
        assert_eq!(selected_monitor_index(None, &connectors), Some(0));
        assert_eq!(selected_monitor_index(Some("DP-2"), &connectors), Some(0));
        assert_eq!(selected_monitor_index(Some("DP-2"), &[]), None);
    }
}
