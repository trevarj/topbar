//! Compositor blur behind the panel's surfaces, via `ext-background-effect-v1`.
//!
//! Ported from v1 `services/background_effect.rs`, and shrunk. The protocol is
//! a *hint*: the panel hands the compositor the exact region of each surface
//! that should have the desktop behind it blurred, and a compositor that has no
//! blur configured — or does not speak the protocol at all — simply ignores it.
//! Nothing here is ever load-bearing, which is why every failure path in this
//! module ends in "log once, carry on without blur".
//!
//! # Borrowing GTK's connection
//!
//! GTK does not expose this protocol, so the panel speaks it itself over GDK's
//! *own* Wayland connection, reached through `gdk4-wayland`:
//!
//! ```text
//! WaylandDisplay::wl_display() → Proxy::backend() → Backend::upgrade()
//!                              → wayland_client::Connection::from_backend()
//! ```
//!
//! This is the one fragile point in the panel (the same path
//! [`super::activation`] uses, and the only other place `wayland-client`
//! appears). Opening a second connection with `Backend::from_foreign_display`
//! would be easier, but it allocates its own libwayland event queue, and a
//! roundtrip on that queue can swallow events off the shared socket that GDK
//! expects to read — which in practice means a missed layer-shell configure and
//! a bar that maps in the middle of the screen. Borrowing the connection and
//! creating only a private *event queue* on it keeps GDK's own queue untouched.
//!
//! # The guard
//!
//! Consumers never touch the protocol. They call [`attach`], keep the returned
//! [`BlurAttachment`] alive for as long as the surface exists, and that is all:
//! the guard connects `map`/`unmap`/`destroy` and the surface's resize
//! notifications itself, and its `Drop` removes the region and destroys the
//! protocol object. Two calls need making by hand, because only the caller
//! knows when they happen:
//!
//! - [`BlurAttachment::suspend`] at the **start of a fade-out**. Compositor-side
//!   blur is rendered independently of widget opacity, so a surface that fades
//!   to nothing before unmapping leaves a blurred rectangle hanging over the
//!   desktop for the length of the animation. This is v1's hardest-won caveat.
//! - [`BlurAttachment::set_scale`] from a grow-in animation, so the blurred area
//!   tracks the surface actually being drawn instead of appearing at full size
//!   on the first frame.
//!
//! # Surfaces
//!
//! The bar, the shared popover host (and so every popover, Quick Settings, tray
//! menu and dialog on it), the banner stack and the OSD capsule. Tooltips are
//! deliberately left out: they are small, opaque and short-lived, and v1 came to
//! the same conclusion.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gdk4_wayland::prelude::*;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use tracing::{debug, info, warn};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, Capability, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

/// Environment override that skips blur entirely, whatever the config says.
///
/// The panel then runs exactly as it does on a compositor without the protocol,
/// which is what makes "degrades silently" testable rather than hopeful.
const DISABLE_ENV: &str = "TOPBAR_NO_BLUR";

/// How far the rounded corner rows are pulled in, in logical pixels.
///
/// A region is a set of rectangles, so a rounded corner is a staircase; every
/// panel surface with a radius also has a 1px translucent border, and without
/// this the outermost step shows through it as a bright speck.
const CORNER_INSET: i32 = 1;

/// How often an idle panel empties its event queue. See [`BlurManager::watch`].
const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Frames a surface is given to become measurable before blur gives up on it.
///
/// A surface that has just mapped has no size until the compositor configures
/// it, and no layout until GTK has been round the loop, so the first attempt at
/// a region routinely finds nothing to measure. Two seconds at sixty frames is
/// far longer than either takes and still cannot become a surface that asks for
/// a frame forever.
const READY_FRAMES: u32 = 120;

// ---------------------------------------------------------------------------
// Region geometry — plain arithmetic, unit-tested without a compositor
// ---------------------------------------------------------------------------

/// The rectangles that tile a rounded rectangle, in surface-local pixels.
///
/// `wl_region` is built from rectangles, so the rounded shape is rasterised one
/// scanline per corner row. Nearest-integer rounding of the exact inset keeps
/// the total error and the step between neighbouring rows as small as integer
/// rows allow. Rectangles never overlap, which matters because the compositor
/// unions them: an overlap would be invisible but the area arithmetic the tests
/// rely on would stop meaning anything.
///
/// A non-positive width or height yields no rectangles at all — the protocol
/// rejects those — and a zero radius yields the plain rectangle.
fn rounded_rect(x: i32, y: i32, width: i32, height: i32, radius: i32) -> Vec<(i32, i32, i32, i32)> {
    if width <= 0 || height <= 0 {
        return Vec::new();
    }
    // An oversized radius becomes a pill rather than falling back to a square.
    let radius = radius.min(width / 2).min(height / 2);
    if radius <= 0 {
        return vec![(x, y, width, height)];
    }

    let has_center = height > 2 * radius;
    let mut rects = Vec::with_capacity(usize::from(has_center) + 2 * radius as usize);
    if has_center {
        rects.push((x, y + radius, width, height - 2 * radius));
    }

    let r = f64::from(radius);
    for row in 0..radius {
        let dy = r - 0.5 - f64::from(row);
        let inset = if dy < 0.0 {
            0
        } else {
            (r - (r * r - dy * dy).sqrt()).round() as i32
        };
        let inset = (inset + CORNER_INSET).min((width - 1) / 2);
        let row_width = (width - 2 * inset).max(1);
        rects.push((x + inset, y + row, row_width, 1));
        rects.push((x + inset, y + height - 1 - row, row_width, 1));
    }

    rects
}

/// A region already sent to the compositor.
///
/// Grow-in animations ease into the same integer rectangle for several frames
/// near the end of the run; remembering the last one sent turns those frames
/// into nothing at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegionKey {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
}

// ---------------------------------------------------------------------------
// Protocol plumbing
// ---------------------------------------------------------------------------

/// What the private event queue collects from the compositor.
#[derive(Default)]
struct BlurState {
    /// The bound manager global.
    manager: Option<ExtBackgroundEffectManagerV1>,
    /// Bound for its `create_region`, which is the only way to make a region.
    compositor: Option<WlCompositor>,
    /// Whether the compositor currently offers blur. The protocol allows this
    /// to be revoked at runtime, at which point it stops drawing blur whatever
    /// the client believes.
    capable: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for BlurState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        // `GlobalRemove` is ignored on purpose: neither global is one a
        // compositor withdraws mid-session, and blur has a capability event of
        // its own for saying it is no longer available.
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "ext_background_effect_manager_v1" => {
                debug!("blur: compositor offers ext_background_effect_manager_v1 v{version}");
                state.manager = Some(registry.bind(name, version.min(1), queue, ()));
            }
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(4), queue, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for BlurState {
    fn event(
        state: &mut Self,
        _proxy: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        // Sent on bind, and again whenever the answer changes.
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            let capable = flags
                .into_result()
                .is_ok_and(|flags| flags.contains(Capability::Blur));
            if capable != state.capable {
                debug!("blur: capability is now {capable}");
            }
            state.capable = capable;
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtBackgroundEffectSurfaceV1,
        _event: ext_background_effect_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        // The interface has no events.
    }
}

impl Dispatch<WlCompositor, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: wayland_client::protocol::wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegion,
        _event: wayland_client::protocol::wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// The manager
// ---------------------------------------------------------------------------

thread_local! {
    /// The manager, once initialised. `None` means no blur, for any reason.
    static MANAGER: RefCell<Option<Rc<BlurManager>>> = const { RefCell::new(None) };
    /// Protocol objects created since start-up, and how many are alive.
    ///
    /// A surface that is hidden and shown again is a *new* `wl_surface`, so
    /// creations grow with the number of times a popover was opened; what must
    /// stay flat is the number alive, which is what the soak test watches.
    static EFFECTS: Cell<(u64, i64)> = const { Cell::new((0, 0)) };
}

/// Everything blur needs that is not per-surface.
struct BlurManager {
    state: RefCell<BlurState>,
    queue: RefCell<EventQueue<BlurState>>,
    handle: QueueHandle<BlurState>,
}

impl BlurManager {
    /// Bind the protocol on GDK's connection, or explain why blur is off.
    fn init(display: &gdk::Display) -> Option<Self> {
        let display = display
            .clone()
            .downcast::<gdk4_wayland::WaylandDisplay>()
            .ok()?;
        // Cheap early out that costs no roundtrip on compositors without blur.
        if !display.query_registry("ext_background_effect_manager_v1") {
            info!("blur: the compositor does not offer ext-background-effect-v1; running without");
            return None;
        }

        let connection = connection(&display)?;
        let mut queue: EventQueue<BlurState> = connection.new_event_queue();
        let handle = queue.handle();
        let _registry = connection.display().get_registry(&handle, ());

        let mut state = BlurState::default();
        // Two roundtrips: the first turns registry globals into a bound
        // manager, the second collects the `capabilities` event that binding
        // produced. Blur is not *required* to be capable here — the fd watcher
        // picks up a later change — but the globals are.
        for _ in 0..2 {
            if let Err(error) = queue.roundtrip(&mut state) {
                warn!("blur: could not talk to the compositor ({error}); running without");
                return None;
            }
        }
        if state.manager.is_none() || state.compositor.is_none() {
            info!("blur: the compositor withdrew the blur protocol; running without");
            return None;
        }

        debug!("blur: ready (capable={})", state.capable);
        Some(Self {
            state: RefCell::new(state),
            queue: RefCell::new(queue),
            handle,
        })
    }

    /// Keep this queue drained for as long as the panel runs.
    ///
    /// v1 watched the connection's file descriptor with
    /// `glib::unix_fd_add_local`; glib 0.22 no longer has that function. Losing
    /// it costs nothing, and the replacement is in fact safer: **GDK** is the
    /// one reading the socket, and libwayland sorts everything it reads into
    /// per-queue buckets, so this queue's bucket only has to be emptied — never
    /// read from the socket directly, which is the event-stealing hazard this
    /// module exists to avoid.
    ///
    /// Emptying happens after every region change; the timer is the backstop
    /// for a panel sitting idle, where the only thing that can arrive is a
    /// compositor withdrawing blur or a new global appearing.
    fn watch(&self) {
        glib::timeout_add_local(DRAIN_INTERVAL, move || {
            MANAGER.with(|cell| {
                let manager = cell.borrow().clone();
                let Some(manager) = manager else {
                    return glib::ControlFlow::Break;
                };
                manager.dispatch();
                let (created, alive) = effect_counts();
                debug!("blur: {created} effect object(s) created, {alive} alive");
                glib::ControlFlow::Continue
            })
        });
    }

    /// Dispatch whatever GDK's last read left on this queue. Never blocks.
    fn dispatch(&self) {
        let Ok(mut queue) = self.queue.try_borrow_mut() else {
            // Already dispatching, one frame up the stack.
            return;
        };
        let mut state = self.state.borrow_mut();
        if let Err(error) = queue.dispatch_pending(&mut state) {
            // Blur is cosmetic; a protocol hiccup must not take the panel down.
            warn!("blur: dispatch failed ({error})");
        }
    }

    /// Whether the compositor is offering blur right now.
    fn capable(&self) -> bool {
        self.state.borrow().capable
    }

    /// Ask for the effect object belonging to `surface`.
    ///
    /// Creating a second one for the same surface is a protocol error, which is
    /// why exactly one [`BlurAttachment`] owns each surface's object.
    fn effect(&self, surface: &WlSurface) -> Option<ExtBackgroundEffectSurfaceV1> {
        let state = self.state.borrow();
        if !state.capable {
            return None;
        }
        let effect = state
            .manager
            .as_ref()?
            .get_background_effect(surface, &self.handle, ());
        EFFECTS.with(|cell| {
            let (created, alive) = cell.get();
            cell.set((created + 1, alive + 1));
            debug!("blur: effect object created ({} live)", alive + 1);
        });
        Some(effect)
    }

    /// Build a region covering `rects`.
    fn region(&self, rects: &[(i32, i32, i32, i32)]) -> Option<WlRegion> {
        let region = self
            .state
            .borrow()
            .compositor
            .as_ref()?
            .create_region(&self.handle, ());
        for &(x, y, width, height) in rects {
            region.add(x, y, width, height);
        }
        Some(region)
    }

    /// Push queued requests out to the compositor, and take in what came back.
    fn flush(&self) {
        if let Ok(queue) = self.queue.try_borrow() {
            let _ = queue.flush();
        }
        self.dispatch();
    }
}

/// Borrow GDK's own Wayland connection.
///
/// `WaylandDisplay::connection()` is private, but `wl_display()` returns a
/// proxy created *from* that same connection, and a proxy knows its backend —
/// so rebuilding a `Connection` from the backend yields the connection GDK is
/// already using rather than a second one.
fn connection(display: &gdk4_wayland::WaylandDisplay) -> Option<Connection> {
    let backend = display.wl_display()?.backend().upgrade()?;
    Some(Connection::from_backend(backend))
}

/// Start blur, unless the config or the environment says otherwise.
///
/// Called once, from application start-up. Every later [`attach`] is inert if
/// this did not succeed, so the panel is identical minus the blur.
pub fn init(display: &gdk::Display, enabled: bool) {
    if !enabled {
        debug!("blur: disabled by `theme.blur`");
        return;
    }
    if is_active() {
        return;
    }
    // Empty counts as unset, so a script may pass the variable through
    // unconditionally and decide with its value.
    if std::env::var_os(DISABLE_ENV).is_some_and(|value| !value.is_empty()) {
        info!("blur: {DISABLE_ENV} is set; running degraded, without blur");
        return;
    }

    let Some(manager) = BlurManager::init(display) else {
        return;
    };
    let manager = Rc::new(manager);
    manager.watch();
    MANAGER.with(|cell| *cell.borrow_mut() = Some(manager));
}

/// Turn blur on or off under a running panel, returning whether it is on now.
///
/// The reload path. Switching **on** binds the protocol exactly as start-up
/// does — which is why [`BlurManager`] lives in a cell rather than a
/// `OnceLock` — and switching **off** releases the manager, which also stops
/// its drain timer. Neither touches the regions already on screen: an
/// attachment owns one surface's effect object and is never mutated, so the
/// caller has to rebuild the surfaces for the change to be visible. That is
/// [`crate::reload`]'s job and it is why a `theme.blur` flip rebuilds the bars.
pub fn set_enabled(display: &gdk::Display, enabled: bool) -> bool {
    if enabled == is_active() {
        return enabled;
    }
    if enabled {
        init(display, true);
    } else {
        // Every attachment made from here on is inert. The ones still alive
        // keep their regions until their surfaces go, which they are about to.
        MANAGER.with(|cell| *cell.borrow_mut() = None);
        info!("blur: switched off by a reload");
    }
    is_active()
}

/// Whether blur is running. Consumers do not need to ask; the smoke tests do.
pub fn is_active() -> bool {
    MANAGER.with(|cell| cell.borrow().is_some())
}

/// Protocol objects created since start-up, and how many are alive now.
///
/// The soak test asserts the second number stays flat while the first climbs:
/// hiding a window destroys its `wl_surface`, so a reopened popover is a new
/// surface and needs a new object, but nothing may be left behind.
pub fn effect_counts() -> (u64, i64) {
    EFFECTS.with(Cell::get)
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// The pieces of a Wayland surface a region needs.
struct SurfaceInfo {
    wl_surface: WlSurface,
    id: ObjectId,
    width: i32,
    height: i32,
}

impl SurfaceInfo {
    /// Resolve the surface `widget` is drawn on, if it is mapped.
    fn resolve(widget: &gtk4::Widget) -> Option<Self> {
        let gdk_surface = widget.native()?.surface()?;
        let wayland = gdk_surface
            .downcast::<gdk4_wayland::WaylandSurface>()
            .ok()?;
        let wl_surface = wayland.wl_surface()?;
        Some(Self {
            id: wl_surface.id(),
            wl_surface,
            width: wayland.width(),
            height: wayland.height(),
        })
    }
}

/// The effect object for one surface, plus the last region sent on it.
struct Effect {
    surface: ObjectId,
    object: ExtBackgroundEffectSurfaceV1,
    last: Option<RegionKey>,
}

impl Drop for Effect {
    fn drop(&mut self) {
        self.object.destroy();
        EFFECTS.with(|cell| {
            let (created, alive) = cell.get();
            cell.set((created, alive - 1));
            debug!("blur: effect object destroyed ({} live)", alive - 1);
        });
    }
}

/// The state behind a live attachment.
struct Attached {
    manager: Rc<BlurManager>,
    window: glib::WeakRef<gtk4::Widget>,
    content: glib::WeakRef<gtk4::Widget>,
    radius: Box<dyn Fn() -> i32>,
    effect: RefCell<Option<Effect>>,
    /// How much of the surface is actually being drawn, `0.0..=1.0`.
    scale: Cell<f64>,
    /// Set by [`BlurAttachment::suspend`]; cleared by a resume or the next map.
    suspended: Cell<bool>,
    /// Frames spent waiting for the surface to become measurable, and whether
    /// one of those waits is in flight.
    waited: Cell<u32>,
    waiting: Cell<bool>,
    /// Signal handlers to disconnect when the guard is dropped.
    window_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    surface_handlers: RefCell<Vec<(gdk::Surface, glib::SignalHandlerId)>>,
}

impl Attached {
    /// Send the region for the surface as it is right now.
    fn apply(self: &Rc<Self>) {
        if self.suspended.get() || !self.manager.capable() {
            return;
        }
        let (Some(window), Some(content)) = (self.window.upgrade(), self.content.upgrade()) else {
            return;
        };
        if !window.is_mapped() {
            return;
        }
        let Some(info) = SurfaceInfo::resolve(&window) else {
            return;
        };
        // The resize watcher is what retries everything below: a surface has no
        // real size until the compositor has configured it, and the first map
        // runs well before that.
        self.watch_surface(&window);
        // 1×1 is the placeholder GTK maps with before the first configure, and
        // a tree that was parented this turn has no allocation to measure yet.
        // Neither is an error and neither necessarily produces a resize to be
        // woken by, so the next frame is asked instead.
        if info.width <= 1 || info.height <= 1 {
            self.wait_a_frame(&window);
            return;
        }
        let Some(key) = self.geometry(&window, &content, &info) else {
            self.wait_a_frame(&window);
            return;
        };

        let mut slot = self.effect.borrow_mut();
        // A hidden and re-shown window has a brand new `wl_surface`, and an
        // effect object outlives its surface only as a thing to destroy.
        if slot
            .as_ref()
            .is_some_and(|effect| effect.surface != info.id)
        {
            *slot = None;
        }
        if slot.is_none() {
            let Some(object) = self.manager.effect(&info.wl_surface) else {
                return;
            };
            *slot = Some(Effect {
                surface: info.id,
                object,
                last: None,
            });
        }
        let Some(effect) = slot.as_mut() else {
            return;
        };
        if effect.last == Some(key) {
            return;
        }

        let rects = rounded_rect(key.x, key.y, key.width, key.height, key.radius);
        let Some(region) = self.manager.region(&rects) else {
            return;
        };
        effect.object.set_blur_region(Some(&region));
        region.destroy();
        effect.last = Some(key);
        drop(slot);

        // Both the region and its removal are double-buffered, and the caller
        // may well be an idle or a resize callback with no frame behind it, so
        // the surface is committed here. Only blur state GDK knows nothing
        // about is touched; GTK's next frame simply commits its own on top.
        info.wl_surface.commit();
        self.manager.flush();
    }

    /// Where the blurred rectangle goes, in surface-local pixels.
    fn geometry(
        &self,
        window: &gtk4::Widget,
        content: &gtk4::Widget,
        info: &SurfaceInfo,
    ) -> Option<RegionKey> {
        let bounds = content.compute_bounds(window)?;
        if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
            return None;
        }

        // A surface growing in is clipped to a centred fraction of itself, and
        // the blur has to grow with it or it arrives as a hard-edged rectangle
        // before the surface that should be covering it.
        let scale = self.scale.get().clamp(0.0, 1.0);
        let width = f64::from(bounds.width()) * scale;
        let height = f64::from(bounds.height()) * scale;
        let x = f64::from(bounds.x()) + (f64::from(bounds.width()) - width) / 2.0;
        let y = f64::from(bounds.y()) + (f64::from(bounds.height()) - height) / 2.0;

        // Clamp into the surface: a region reaching outside it is undefined,
        // and a shadow margin can put content bounds a pixel over the edge.
        let left = (x.round() as i32).clamp(0, info.width);
        let top = (y.round() as i32).clamp(0, info.height);
        let right = ((x + width).round() as i32).clamp(left, info.width);
        let bottom = ((y + height).round() as i32).clamp(top, info.height);

        Some(RegionKey {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
            radius: (self.radius)().max(0),
        })
    }

    /// Drop the region, leaving the surface as if blur had never been asked for.
    fn clear(&self) {
        let Some(effect) = self.effect.borrow_mut().take() else {
            return;
        };
        // Removal is double-buffered like everything else, so the surface has
        // to be committed while it is still alive for the compositor to act on
        // it. On the unmap path this is the last moment that is true.
        let surface = self
            .window
            .upgrade()
            .as_ref()
            .and_then(SurfaceInfo::resolve)
            .filter(|info| info.id == effect.surface);
        drop(effect);
        if let Some(info) = surface {
            info.wl_surface.commit();
        }
        self.manager.flush();
    }

    /// Try again on the next frame, up to [`READY_FRAMES`] of them.
    ///
    /// The resize watcher covers a surface that changes size, which is most of
    /// them; this covers the one that does not. A banner is given its size
    /// before it is mapped, so the configure that follows changes nothing and
    /// wakes nobody — and without this the banner would go its whole life
    /// unblurred, waiting for a resize that is never coming.
    fn wait_a_frame(self: &Rc<Self>, window: &gtk4::Widget) {
        if self.waiting.get() || self.waited.get() >= READY_FRAMES {
            return;
        }
        self.waiting.set(true);
        let attached = Rc::downgrade(self);
        window.add_tick_callback(move |_widget, _clock| {
            if let Some(attached) = attached.upgrade() {
                attached.waiting.set(false);
                attached.waited.set(attached.waited.get() + 1);
                attached.apply();
            }
            glib::ControlFlow::Break
        });
    }

    /// Re-apply the region whenever the surface changes size.
    ///
    /// Installed on the `GdkSurface` rather than the window, because that is
    /// what learns about a compositor configure, and installed once.
    fn watch_surface(self: &Rc<Self>, window: &gtk4::Widget) {
        if !self.surface_handlers.borrow().is_empty() {
            return;
        }
        let Some(surface) = window.native().and_then(|native| native.surface()) else {
            return;
        };
        let mut handlers = self.surface_handlers.borrow_mut();
        for property in ["width", "height"] {
            let attached = Rc::downgrade(self);
            let id = surface.connect_notify_local(Some(property), move |_, _| {
                // On an idle, not inline: the size lands before the layout that
                // follows from it, and the region comes from the layout.
                let attached = attached.clone();
                glib::idle_add_local_once(move || {
                    if let Some(attached) = attached.upgrade() {
                        attached.apply();
                    }
                });
            });
            handlers.push((surface.clone(), id));
        }
    }
}

/// A live blur region, removed when this is dropped.
///
/// Held by whatever owns the surface. An inert one — blur switched off, or a
/// compositor that does not offer it — behaves identically and does nothing,
/// so consumers never branch on whether blur is available.
pub struct BlurAttachment {
    inner: Option<Rc<Attached>>,
}

impl BlurAttachment {
    /// A guard that does nothing, for when there is no blur to attach.
    pub const fn inert() -> Self {
        Self { inner: None }
    }

    /// Whether this attachment's compositor currently advertises blur.
    /// Callers that need translucency can keep an opaque fallback otherwise.
    pub fn is_available(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|attached| attached.manager.capable())
    }

    /// Remove the region now, and keep it off until the surface maps again.
    ///
    /// Call this the moment a fade-out *starts*. The compositor blurs what is
    /// behind the surface regardless of how opaque the surface itself is, so a
    /// region left in place during a fade is a rectangle of blurred desktop
    /// sitting on screen with nothing drawn over it.
    pub fn suspend(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.suspended.set(true);
        inner.clear();
    }

    /// Put the region back, if the surface is still on screen.
    ///
    /// Mapping does this by itself; it needs calling by hand only when a
    /// fade-out is reversed before the surface ever unmapped.
    pub fn resume(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.suspended.set(false);
        inner.apply();
    }

    /// Track a grow-in animation, `0.0..=1.0` of the surface's full size.
    ///
    /// Cheap enough for a per-frame call: a scale that rounds to the rectangle
    /// already sent costs nothing at all.
    pub fn set_scale(&self, scale: f64) {
        let Some(inner) = &self.inner else {
            return;
        };
        if (inner.scale.get() - scale).abs() < f64::EPSILON {
            return;
        }
        inner.scale.set(scale);
        inner.apply();
    }
}

impl Drop for BlurAttachment {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        for id in inner.window_handlers.borrow_mut().drain(..) {
            if let Some(window) = inner.window.upgrade() {
                window.disconnect(id);
            }
        }
        for (surface, id) in inner.surface_handlers.borrow_mut().drain(..) {
            surface.disconnect(id);
        }
        inner.clear();
    }
}

/// Ask the compositor to blur what is behind `content` on `window`'s surface.
///
/// `radius` is read on every apply rather than captured, so a corner radius
/// that changes with the configuration is picked up without re-attaching.
///
/// The returned guard wires up the surface's whole lifecycle; keep it for as
/// long as the window lives.
pub fn attach(
    window: &impl IsA<gtk4::Widget>,
    content: &impl IsA<gtk4::Widget>,
    radius: impl Fn() -> i32 + 'static,
) -> BlurAttachment {
    let Some(manager) = MANAGER.with(|cell| cell.borrow().clone()) else {
        return BlurAttachment::inert();
    };

    let window = window.as_ref();
    let window_weak = glib::WeakRef::new();
    window_weak.set(Some(window));
    let content_weak = glib::WeakRef::new();
    content_weak.set(Some(content.as_ref()));

    let attached = Rc::new(Attached {
        manager,
        window: window_weak,
        content: content_weak,
        radius: Box::new(radius),
        effect: RefCell::new(None),
        scale: Cell::new(1.0),
        suspended: Cell::new(false),
        waited: Cell::new(0),
        waiting: Cell::new(false),
        window_handlers: RefCell::new(Vec::new()),
        surface_handlers: RefCell::new(Vec::new()),
    });

    let handlers = vec![
        window.connect_map(handler(&attached, |attached| {
            // A surface that went away and came back starts unsuspended: the
            // fade it was suspended for is long over, and it is owed a fresh
            // budget of frames to become measurable in.
            attached.suspended.set(false);
            attached.waited.set(0);
            attached.apply();
        })),
        // Unmap, then destroy as a safety net. Both run while the `wl_surface`
        // still exists, which is the only time the region can be removed.
        window.connect_unmap(handler(&attached, |attached| attached.clear())),
        window.connect_destroy(handler(&attached, |attached| attached.clear())),
    ];
    *attached.window_handlers.borrow_mut() = handlers;

    // Already on screen: the map that would have applied it has been and gone.
    if window.is_mapped() {
        attached.apply();
    }

    BlurAttachment {
        inner: Some(attached),
    }
}

/// A widget signal handler that holds the attachment weakly.
///
/// The window outlives the guard in the general case, so a strong reference
/// here would keep the region alive after its owner dropped it.
fn handler(
    attached: &Rc<Attached>,
    action: impl Fn(&Rc<Attached>) + 'static,
) -> impl Fn(&gtk4::Widget) + 'static {
    let attached: Weak<Attached> = Rc::downgrade(attached);
    move |_widget| {
        if let Some(attached) = attached.upgrade() {
            action(&attached);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Total area of a set of rectangles.
    fn area(rects: &[(i32, i32, i32, i32)]) -> i64 {
        rects
            .iter()
            .map(|&(_, _, w, h)| i64::from(w) * i64::from(h))
            .sum()
    }

    /// Pixels covered at least once, which differs from [`area`] on overlap.
    fn covered(rects: &[(i32, i32, i32, i32)], width: i32, height: i32) -> usize {
        let mut grid = vec![false; (width * height) as usize];
        for &(x, y, w, h) in rects {
            for py in y..y + h {
                for px in x..x + w {
                    grid[(py * width + px) as usize] = true;
                }
            }
        }
        grid.iter().filter(|&&set| set).count()
    }

    #[test]
    fn a_region_with_no_area_has_no_rectangles() {
        for (w, h) in [(0, 10), (10, 0), (-4, 10), (10, -4)] {
            assert!(rounded_rect(0, 0, w, h, 4).is_empty(), "{w}x{h}");
        }
    }

    #[test]
    fn no_radius_is_one_plain_rectangle() {
        assert_eq!(rounded_rect(10, 20, 100, 50, 0), vec![(10, 20, 100, 50)]);
        assert_eq!(rounded_rect(0, 0, 40, 30, -5), vec![(0, 0, 40, 30)]);
    }

    #[test]
    fn a_radius_too_big_for_the_box_becomes_a_pill() {
        // 20x10 clamps the radius to 5, which leaves no central rectangle:
        // every row is a corner row.
        let rects = rounded_rect(0, 0, 20, 10, 100);
        assert!(!rects.is_empty());
        for &(_, _, _, h) in &rects {
            assert_eq!(h, 1, "a pill is scanlines all the way down");
        }
    }

    #[test]
    fn every_rectangle_stays_inside_the_box() {
        for radius in [1, 5, 10, 20, 50] {
            for (w, h) in [(20, 20), (100, 40), (40, 100), (3, 3), (50, 50)] {
                for &(x, y) in &[(0, 0), (10, 20)] {
                    for &(rx, ry, rw, rh) in &rounded_rect(x, y, w, h, radius) {
                        assert!(rw > 0 && rh > 0, "{rw}x{rh} for {w}x{h} r={radius}");
                        assert!(
                            rx >= x && ry >= y && rx + rw <= x + w && ry + rh <= y + h,
                            "({rx},{ry},{rw},{rh}) escapes {w}x{h} at ({x},{y}) r={radius}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rectangles_never_overlap() {
        for radius in [1, 5, 10, 15] {
            for (w, h) in [(30, 30), (50, 20), (20, 50), (31, 31)] {
                let rects = rounded_rect(0, 0, w, h, radius);
                assert_eq!(
                    area(&rects) as usize,
                    covered(&rects, w, h),
                    "overlap in {w}x{h} r={radius}"
                );
            }
        }
    }

    #[test]
    fn rounding_the_corners_takes_area_off_but_not_much() {
        let rects = rounded_rect(0, 0, 100, 50, 10);
        let box_area = 100 * 50;
        assert!(area(&rects) < box_area);
        assert!(area(&rects) > box_area * 9 / 10);
    }

    #[test]
    fn the_shape_is_symmetric_on_both_axes() {
        let (w, h, r) = (40, 40, 10);
        let rects = rounded_rect(0, 0, w, h, r);
        let mut grid = vec![vec![false; w as usize]; h as usize];
        for &(x, y, rw, rh) in &rects {
            for py in y..y + rh {
                for px in x..x + rw {
                    grid[py as usize][px as usize] = true;
                }
            }
        }
        for y in 0..h as usize {
            assert_eq!(grid[y], grid[h as usize - 1 - y], "row {y} is not mirrored");
            for x in 0..w as usize / 2 {
                assert_eq!(
                    grid[y][x],
                    grid[y][w as usize - 1 - x],
                    "({x},{y}) is not mirrored"
                );
            }
        }
    }

    #[test]
    fn every_row_of_the_shape_is_covered() {
        for radius in [1, 5, 10] {
            let rects = rounded_rect(0, 0, 30, 30, radius);
            let mut rows = [false; 30];
            for &(_, y, _, h) in &rects {
                for row in y..y + h {
                    rows[row as usize] = true;
                }
            }
            assert!(rows.iter().all(|&covered| covered), "r={radius}");
        }
    }

    #[test]
    fn moving_the_box_moves_every_rectangle_with_it() {
        let base = rounded_rect(0, 0, 30, 30, 8);
        let moved = rounded_rect(100, 200, 30, 30, 8);
        assert_eq!(base.len(), moved.len());
        for (base, moved) in base.iter().zip(moved.iter()) {
            assert_eq!((base.0 + 100, base.1 + 200, base.2, base.3), *moved);
        }
    }

    #[test]
    fn the_corner_inset_pulls_the_rounded_rows_in() {
        // The central rectangle spans the full width; every corner row is
        // narrower than it, by at least the inset.
        let rects = rounded_rect(0, 0, 40, 30, 8);
        let (_, _, center_width, center_height) = rects[0];
        assert_eq!(center_width, 40);
        assert!(center_height > 1, "the first rectangle is the central one");
        for &(x, _, w, h) in &rects[1..] {
            assert_eq!(h, 1);
            assert!(x >= CORNER_INSET, "row starts at {x}");
            assert!(w <= 40 - 2 * CORNER_INSET, "row is {w} wide");
        }
    }

    #[test]
    fn an_inert_guard_does_nothing_at_all() {
        // Every consumer holds one of these when blur is off, and calls the
        // same methods on it; none of them may touch a manager that is absent.
        let guard = BlurAttachment::inert();
        assert!(!guard.is_available());
        guard.suspend();
        guard.resume();
        guard.set_scale(0.5);
        drop(guard);
        assert!(!is_active(), "no manager is initialised in unit tests");
    }
}
