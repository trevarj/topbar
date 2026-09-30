# Architecture

A map of topbar v2 for somebody who has to change it: the shape of the
workspace, the rules that shape enforces, and where a new widget, service or
configuration key goes.

## The three crates

```
┌─────────────────────────────────────────────────────────┐
│ topbar — the GTK application                            │
│   app  bar/  widgets/  surfaces/  anim/  style/         │
│   wayland/  bridge  control  commands  reload           │
│   gtk4, gtk4-layer-shell, gdk4-wayland, wayland-client  │
└───────────────┬─────────────────────────────┬───────────┘
                │ Send + Clone handles,       │
                ▼ Arc<Snapshot> values        │
┌─────────────────────────────────────────┐   │
│ topbar-services — everything outward    │   │
│   runtime  lazy  lifecycle  ipc  proc   │   │
│   audio/ battery/ bluetooth/ network/   │   │
│   niri/ notifications/ tray/ updates/ … │   │
│   tokio, zbus  —  NEVER gtk4            │   │
└───────────────┬─────────────────────────┘   │
                ▼                             ▼
┌─────────────────────────────────────────────────────────┐
│ topbar-core                                             │
│   config  delta  ipc  theme  layout_math  xkb_names     │
│   logging  error                                        │
│   serde, toml, thiserror, tracing  —  no GTK, no tokio  │
└─────────────────────────────────────────────────────────┘
```

The split is the thread-safety mechanism, not a tidiness exercise.

`topbar-core` is dependency-light on purpose — no GTK and no tokio — so both
other crates can depend on it without dragging either stack into the other. It
owns the configuration schema and its v1 compatibility surface, the reload
delta, the CLI/panel wire protocol, theme colour primitives, the pure layout
arithmetic, xkb layout names, and logging setup.

`topbar-services` owns everything that talks outward: D-Bus, niri, PulseAudio,
PipeWire, subprocesses, the network. It has **no** `gtk4` dependency, and that
missing edge in `Cargo.toml` is what makes it impossible for a service task to
touch a widget — there is no discipline to remember, because the type does not
exist in that crate. State reaches `topbar`, the GTK application, only as
`Send + Clone` handles and `Arc<Snapshot>` values. `tokio::sync::watch` is
re-exported so the GTK side can name a subscription without depending on tokio
itself; the less of the async stack it can see, the harder it is to do async
work on the main thread by accident.

## The service pattern

Every service is one owning task. Nothing outside it holds its state, and
nothing outside it can lock anything it holds. **State goes out** through
`tokio::sync::watch<Arc<Snapshot>>` — a snapshot is immutable and cheap to
clone, and `watch` only hands out the latest value, so a slow consumer misses
intermediate states rather than backing the producer up. **Commands go in** over
mpsc, with a oneshot reply attached when the caller needs an answer.

The GTK side reaches both through exactly two functions in
`crates/topbar/src/bridge.rs`.

`bridge::bind_state` subscribes a widget to a snapshot channel. It renders once
immediately, before its first `await`, so a widget has content on the frame it
is first drawn rather than one main-loop turn later. It holds the widget
*weakly* and returns a `BindingGuard` whose `Drop` aborts the subscription, so a
widget's bindings die with the widget instead of rendering into a disposed tree.

`bridge::act` runs a mutating call. It is the single place in the panel where a
`Result<(), SvcError>` may be discarded, and it does not discard it: the
failure is logged in full and shown to the user as the one short sentence the
error carries. `bridge::request` is its sibling for calls that answer with
something the caller needs — the tray has to *have* a menu before it can draw
one. `ActionScope` picks the destination: a banner, or a named inline slot under
the Quick Settings row that caused it, because a toast covering the top of the
screen to say what the row could say in place makes the panel feel like it is
shouting.

This is what makes the re-entrancy class of bug structurally impossible rather
than merely rare. The failure it replaces is familiar: a widget holds
`Rc<RefCell<State>>`, a click handler borrows it mutably, calls something that
emits a signal, and the signal handler borrows the same cell — a
`BorrowMutError` that only reproduces under one particular click order. Here a
widget cannot reach service state except by receiving a snapshot, and cannot
reach a service except by sending a message. The render closure runs on the
main context with an `Arc` nothing else will mutate; the command goes to
another thread and comes back as a separate main-context turn. There is no
synchronous call from a widget into a service, so there is no path by which a
widget's own callback re-enters it.

## Lazy service start

Most of what the panel can do, a given panel is not doing. A bar with no
`crypto` widget was still asking CoinGecko for prices every half hour; a bar
with no `quick_settings` was still polling a battery, a Bluetooth adapter and an
update count for nobody.

`crates/topbar-services/src/lazy.rs` fixes that without making services
optional — that would push an `Option` into every widget, the kind of `Option`
that spreads. Instead **the channels are always built**, so handles and
subscriptions work exactly as before and a dormant service publishes its empty
snapshot for ever, and only the *task* is withheld. `Deferred` holds either a
started task or the closure that would start one.

What asks is `Demand` in `runtime.rs`, read out of widget placement rather than
from a switch of its own: a service exists to feed a surface, so "does anything
draw this" already has an answer in the file. A `clock` with
`control_panel = true` is what asks for the weather and media services, because
the control panel is what draws them. Everything absent from `Demand` is
unconditional, each for a reason: audio, brightness and the inhibitor answer
`topbar volume`/`brightness`/`inhibit` with no bar in sight; niri drives the OSD
and every per-output decision; notifications is a *role* on the session bus
rather than a widget; and the network is what connectivity is projected from,
which weather, crypto and `requires_network` scripts all gate on.

Starting is idempotent and one-way. `Services::start_if_needed` runs on every
reload and starts whatever the new configuration now draws. Nothing ever stops
a service again: a widget taken off the bar is far more likely to come back
than to have cost anything by staying subscribed, and "stopped" has no good
meaning for a pairing agent, a tray host or a state file.

## Lifecycle

A laptop that slept overnight wakes with a panel full of yesterday. Everything
on it is stale in the same instant and for the same reason, so exactly one
thing should notice.

`crates/topbar-services/src/lifecycle.rs` is that thing: a single logind
subscriber holding a **delay inhibitor**, a file descriptor from
`Manager.Inhibit` whose existence blocks the suspend.

```
PrepareForSleep(true)   -> publish Suspending, then release the lock
                           (the machine sleeps here)
PrepareForSleep(false)  -> take the lock again, then publish Resumed
```

The order is the point. The lock is released *after* the state is published, so
consumers react to "we are going down" before the machine goes down; on the way
back it is reversed, so the lock is in hand again before anything else can
suspend. v1 subscribed twice from two places and took no inhibitor at all, so
half of what it did about a suspend was still queued when the CPU stopped. The
inhibitor covers `sleep` only, in `delay` mode — the panel has no business
vetoing a shutdown or delaying a lid switch.

`Services::wake_on_resume` is the fan-out. It watches the resume *counter*
rather than a flag, because two resumes in quick succession must not look like
one, and on each increment calls `Services::wake`: the niri stream's health
check, then the resource sampler discarding its CPU delta (which spans the sleep
and is meaningless), then the headset, the battery, the update count, and last
the two that reach the network. The clock is deliberately not in the list — its
tick is a one-shot timer re-armed from inside its own callback, so a deadline
that passed during the sleep fires immediately on resume.

## Surfaces

Everything on screen is a layer-shell surface. `crates/topbar/src/bar/` owns
the bar, `crates/topbar/src/surfaces/` the free-floating ones.

**The bar**, one per monitor, keyed by **connector name** (`eDP-1`, `DP-2`).
GDK hands out fresh `GdkMonitor` objects across a hotplug, so object identity
says nothing about which physical output a bar belongs to; the connector name is
the only stable key. Monitor changes arrive in bursts, so both list signals feed
one debounced sync, which reads the configuration from a shared cell rather than
a value captured up front — that is how a reload landing mid-debounce rebuilds
with the new numbers. A monitor GDK has announced but the compositor has not
finished configuring is skipped and retried under a timeout: a bar keyed on a
made-up name is worse than a bar built five seconds late.

**The popover host**, one per monitor (`surfaces/layer_popover.rs`). GTK's
`GtkPopover` positions itself against a toplevel, which a layer-shell bar is
not. The host is built the first time a widget asks for one, and every widget's
popover is re-parented into that same host — which is how "exactly one popover
open at a time" is structural rather than a rule every widget must remember.
`surfaces/popovers.rs` keeps the registry: each handle registers under its
widget name and connector, which is what `topbar popover show clock` addresses.
Content is built **once**, on first open, and re-parented afterwards, so a
popover opened a thousand times allocates one widget tree. A second,
transparent layer surface below it catches the dismissing click; it asks for an
exclusive zone of zero, so the compositor's own arithmetic leaves the bar
uncovered and clicking the button that opened a popover toggles it shut.

**Toasts** (`surfaces/toast.rs`): one surface per monitor, and only the one on
the focused output shows anything. It unmaps when empty, so a transparent
window never eats desktop clicks. The expiry timer lives in the notification
*service*, not the widget — hovering a banner is a `pause_toast` call rather
than a local `SourceId`, so a banner replaced over D-Bus mid-hover cannot end
up with two timers.

**The OSD capsule** (`surfaces/osd.rs`) takes no keyboard focus and has an empty
input region, so a press goes through to whatever is underneath. Its timer is a
reset rather than a queue: a second event retargets the fill and restarts the
countdown without re-animating the capsule. **Tooltips**
(`surfaces/tooltip.rs`) share one process-wide window, because GTK's native
tooltips do not position correctly against layer-shell surfaces.

### Section layout and the overflow policy

`CenterPriorityLayout` in `bar/section_layout.rs` lays out the three sections;
the arithmetic is in `topbar-core/src/layout_math.rs`, which is where its tests
are. The center is anchored to the true center of the interior first, what
remains on each side becomes that side's budget, and the sides shrink — never
the center.

A panel has to be able to run out of room: the bar is exactly as wide as the
monitor, and the answer to fourteen tray icons that do not fit is to show as
many as do. But GTK's contract is that a parent never allocates a child less
than its minimum, and it says so loudly (`Gtk-CRITICAL … allocate … with width
N < minimum M`). `SectionClip` is that whole policy in one widget: it reports a
horizontal minimum of **zero**, which makes any width a legal allocation, then
gives its child the child's own natural width anyway, positioned so the end
that matters stays visible — the left section's start, the right section's end,
the center's middle. `overflow: hidden` cuts off the rest at the section
boundary, and every widget inside is allocated exactly what it asked for, so
nothing inside complains either. `SectionClip::content_min_width` exists so the
layout math can still ask what the section really wanted; without it the center
would keep its natural width while the tray was cut in half.

## Blur containment

`crates/topbar/src/wayland/blur.rs` speaks `ext-background-effect-v1`, and is
the most contained module in the panel on purpose. The protocol is a *hint*:
the panel hands the compositor the exact region of a surface that should have
the desktop behind it blurred, and a compositor with no blur configured — or
none that speaks the protocol — ignores it. Every failure path ends in "log
once, carry on without blur", and `TOPBAR_NO_BLUR` switches it off entirely so
"degrades silently" is testable rather than hopeful.

GTK does not expose the protocol, so the panel speaks it over **GDK's own
Wayland connection**, borrowed through `gdk4-wayland`:

```
WaylandDisplay::wl_display() -> Proxy::backend() -> Backend::upgrade()
                             -> wayland_client::Connection::from_backend()
```

Opening a second connection with `Backend::from_foreign_display` would be
easier and is wrong: it allocates its own libwayland event queue, and a
roundtrip on that queue can swallow events off the shared socket GDK expects to
read — in practice a missed layer-shell configure and a bar that maps in the
middle of the screen. Borrowing the connection and creating only a private
*event queue* on it leaves GDK's queue untouched.

One `BlurAttachment` owns one surface's effect object for that surface's
lifetime. It is never cloned and never mutated. The guard connects
`map`/`unmap`/`destroy` and the resize notifications itself, and its `Drop`
removes the region and destroys the protocol object. Consumers make two calls by
hand, because only they know when those moments are: `suspend` at the start of a
fade-out — compositor-side blur is rendered independently of widget opacity, so
a surface that fades to nothing leaves a blurred rectangle hanging over the
desktop for the length of the animation — and `set_scale` from a grow-in
animation. That immutability is exactly why flipping `theme.blur` rebuilds
surfaces: an attachment is made once, against the blur manager as it was at the
time, and there is no supported way to edit one afterwards.

Blur covers the bar, the popover host (and so every popover, menu and dialog on
it), notification backgrounds and the OSD capsule. The notification window
keeps one effect object, but its region is the union of the individual rounded
card backgrounds: transparent gaps, corner cutouts and shadow margins are not
blurred. That attachment samples live card geometry after GTK paints, so
replacement, reorder, removal, height and scale changes do not leave an old
stack-wide region behind. SlideBox translates only at snapshot time; its blur
background is translated by the same reveal and then clipped to the card's
slot, preserving the original rounded corners. A departing card is omitted
when its fade starts, because compositor blur ignores widget opacity. The
geometry source holds the card list weakly, and unmap/drop disconnects the
frame-clock and surface watchers. Other surfaces keep their single rounded
region. Tooltips are left out: small, opaque and short-lived.

## Hot reload

`crates/topbar-core/src/delta.rs` says *what* changed;
`crates/topbar/src/reload.rs` decides what to do about it. Both `topbar reload`
and the file watcher land in `Reloader::apply`, so the two cannot drift.

`ConfigDelta` is **derived**, not hand-maintained. v1 kept a list of key names
and a `match` saying which of them meant "restart the clock"; the list went
stale the moment a key was added, and a changed `clock.format` was ignored
until the next restart because of it. Here every section of `Config` derives
`PartialEq` and the delta is comparisons, so a key added to a section tomorrow
is classified correctly today without anyone remembering to say so.

The full routing is in `docs/configuration.md` and in the header of
`reload.rs`. The rule to keep in mind while changing the code: nothing rebuilds
a bar that does not have to, because a rebuilt bar closes whatever popover was
open and restarts every widget's timers. An invalid configuration changes
**nothing** — the running config stays as it was, one banner names the first
error, the whole list goes to the log, and there is no partial application.

## IPC and CLI resilience

Four files: the protocol in `topbar-core/src/ipc.rs`, the listener in
`topbar-services/src/ipc.rs`, the panel-side handlers in
`topbar/src/control.rs`, and the subcommands in `topbar/src/commands.rs`.

**Single instance is a `flock`** on `$XDG_RUNTIME_DIR/topbar.lock`, taken
before anything else starts. A second panel loses it and says so, instead of
quietly fighting the first for the notification name, the layer surfaces and
the socket. The kernel releases it however the process ends, so a crashed panel
leaves nothing behind. Because the lock is held first, a socket file already
sitting on the path *cannot* belong to a live panel — so it is unlinked and
rebound rather than treated as a conflict. That is the whole answer to v1's
stale-socket problem, and it is an ordering rather than a heuristic.

**The wire is length-prefixed JSON**: u32 little-endian frames on a
`SOCK_STREAM` socket, capped at 1 MiB, which removes v1's 256-byte datagram
truncation. Every connection opens with a `Hello` handshake carrying
`PROTOCOL_VERSION`, so an old CLI against a new panel fails loudly rather than
misbehaving.

The services crate does not answer requests. Almost everything the CLI asks for
is something only the GTK thread can do — raise a capsule, open a popover, hide
a bar — so a decoded request is forwarded with a oneshot to answer on, and the
services crate stays free of any idea of what a widget is. The handshake is the
one exception, being about the protocol rather than the panel. On the panel
side one `glib::spawn_future_local` consumes the stream, so requests are
handled strictly in order and nothing there can be re-entered.

**Media keys act for themselves.** `topbar volume`, `topbar brightness` and
`topbar media` talk to PulseAudio, logind and the session bus **directly, in
the CLI process**, and only afterwards try to raise an OSD. A key bound to
`topbar volume up` therefore works with the panel crashed, with the panel not
started yet, and — because `[audio] allow_overdrive` is read by a path that
tolerates a file the panel would refuse — with the configuration broken. The
OSD frame is best effort throughout: it is sent, its failure is logged at
debug, nothing else. A volume key that changed the volume has succeeded whether
or not anybody drew a picture of it, so it exits zero and says nothing, which
is what a key pressed sixty times an hour should do.

Everything else needs the panel, because it *is* the panel: only the process
holding the layer surfaces can hide a bar, and only the process holding the
inhibitor's file descriptor can let go of it. Those commands print one clear
line when nothing is listening.

## The smoke harness

`scripts/visual-smoke-niri.sh` starts the panel inside a nested niri, drives
it, and screenshots it. Local only — niri has no headless backend, so CI cannot
run it. Each convention below was paid for once.

**Capture on evidence, never on a clock.** `scripts/smoke-shot.sh` is sourced
by every driver. `grim` returns the last frame the nested niri *presented*, and
a winit session in a window nobody is looking at is throttled by the host
compositor, so the frame on disk can be seconds behind the frame the panel drew.
A fixed `sleep` is a coin toss and it has come up tails twice. So `shot` waits
on three things: the named layer surface has to be listed by `niri msg layers`;
the area below the bar has to actually have something drawn in it (the nested
background is one flat colour, so "more than one colour down there" is exactly
"a surface was presented"); and two consecutive captures have to be
byte-identical, which is how an open animation is allowed to finish and how a
dialog still reading "Searching…" never reaches disk. A capture that never
satisfies all three fails loudly rather than leaving a stale frame for somebody
to describe as though it were real.

**A driver can click, and it asks the panel where to click.** niri advertises
`zwlr_virtual_pointer_manager_v1` and `zwp_virtual_keyboard_manager_v1` inside a
nested session too, so `scripts/smoke-pointer.sh` drives a real pointer into the
nested seat — which is the only way the path from "the compositor delivered a
button event" to "a GTK gesture fired" is exercised at all. Three dead controls
shipped behind a green suite before there was any way to press one. Coordinates
are never written down: `topbar popover show surface-dump` makes the panel log a
rectangle per control on every mapped layer surface, in monitor pixels, with the
classes and the text on it, and the drivers read the last block back out of
`panel.log`. A driver holding coordinates measured off a screenshot starts
clicking empty space the first time a padding changes, which looks exactly like
the bug it is hunting. Two things the readers have to respect: a pattern is
matched against `"<GtkType> <classes> <label>"`, so `Reply` also matches the
*banner* whose button says that — name the type when it matters — and a row
scrolled out of a list still has a rectangle, so scroll it into view before
clicking it.

**Every XDG path is sandboxed.** `XDG_STATE_HOME`, `XDG_CACHE_HOME`,
`XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR` are redirected into a temporary
directory for the length of the run, so a run cannot touch the developer's real
state — this bit us once, when the state-directory migration renamed the live
panel's directory. Boxing the runtime directory also puts the run's
single-instance lock and IPC socket where no real panel will trip over them, and
`XDG_CONFIG_HOME` is boxed even though the config is passed explicitly, so the
legacy-path fallback cannot find a real user config and add warning noise to
`panel.log`.

**Everything runs inside `dbus-run-session`.** Not optional: the panel takes
`org.freedesktop.Notifications` with `ReplaceExisting`, and a nested panel on
the developer's real session bus would take the desktop's notifications away
from whatever is actually serving them.

**Fakes go on the private bus, never the real system bus.** NetworkManager,
BlueZ, UPower and power-profiles live on the system bus, which nothing in the
sandbox can box — and which *is* the developer's live network, their headphones
and their charge threshold. So `topbar-fake-nm`, `topbar-fake-bluez` and
`topbar-fake-power` serve stand-ins on the run's private session bus, pointed at
with `TOPBAR_SMOKE_NM_BUS`, `TOPBAR_SMOKE_BLUEZ_BUS` and
`TOPBAR_SMOKE_POWER_BUS`. Those overrides are read in debug builds only, and a
debug build *without* them reads and does nothing else: the network and
Bluetooth services refuse every mutation and register no agent, by construction.
A fake `/sys/class/power_supply` tree lands the charge-limit write path in a
temporary directory, and PulseAudio, where a scenario needs it, is a server of
the run's own with a null sink. logind is deliberately not redirected — the idle
inhibitor keeps talking to the real one.

**Sidecars end themselves.** A private bus lives exactly as long as the run; a
fake parked on `pending()` does not. `topbar-services/src/sidecar.rs` gives them
one `park` function that also selects on `connection.closed()`, so when the bus
goes the fakes go. The scripts carry `pkill` traps as a belt, but a trap only
runs if the shell that set it is still there to run it — an interrupted
afternoon otherwise leaves a pile of live fakes.

**`nix flake check` builds in release, and that matters.** Clippy in release
sees what debug cannot: a field used only under `cfg(debug_assertions)` is a
dead field in a packaged build, and only the release lint says so. The same
profile runs the tests, which is why a passing `cargo test` is not proof CI will
pass. `flake check` also runs the shipped example configuration through the real
binary with `--check-config --strict`, so a key renamed in the schema but not in
`config.toml` fails CI instead of printing a warning on somebody's first start.

## Where do I put a new X

### A new widget

1. `crates/topbar/src/widgets/<name>.rs`, or a directory if it has a popover
   and a model. Keep every `BindingGuard` in the struct, so subscriptions die
   with the widget.
2. `widgets/mod.rs`: a `mod` line, an arm in `mount`, the name in `handles`.
3. `topbar-core/src/config.rs`: a `<Name>Config` with a `Default` impl, a
   `<NAME>_KEYS` array, a `#[serde(skip)]` field on `WidgetsConfig`, an arm in
   `parse_widgets`, a call in `WidgetsConfig::validate` if it needs one, and
   the name in `SUPPORTED_WIDGETS`.
4. `topbar-core/src/delta.rs`: a `check(…)` line in `changed_widgets`, or the
   widget never rebuilds when its section is edited.
5. `config.toml`: the section, commented out unless it is on by default.

Skipping a step fails a test.
`widgets::tests::every_name_the_configuration_accepts_has_a_widget_behind_it`
walks `SUPPORTED_WIDGETS` against `handles`;
`delta::tests::every_built_in_section_is_classified` walks the same list and
demands that editing each section names exactly that widget; and
`example_config_equals_defaults_and_warns_about_nothing` fails if the new
section in `config.toml` is not exactly the defaults.

### A new service

1. `crates/topbar-services/src/<name>/`, in the usual three parts: `model.rs`
   for the published snapshot (pure), `task.rs` for the one owning task,
   `mod.rs` for the handle and the module documentation. Publish through
   `watch::Sender<Arc<Snapshot>>`, take commands over mpsc, and return
   `Result<_, SvcError>` from anything mutating so `bridge::act` can report it.
2. `lib.rs`: a `pub mod` line.
3. `runtime.rs`: a field on `Services` and a line in `start`. If it should only
   run when something draws it, add a field to `Demand`, read it in
   `Demand::of` from widget placement, gate the task with `Deferred`, and add
   an `ensure_started` entry to `Services::start_if_needed`.
4. If a suspend makes its data stale, add a line to `Services::wake`.
5. If it must remember something across restarts, add a section to
   `PersistedState` in `state_store.rs`. Never write to the user's config.

Do not add a `gtk4` import. It will not resolve, and that is the design.

### A new configuration key

1. `topbar-core/src/config.rs`: the field and its doc comment, its entry in the
   section's `*_KEYS` array (a key missing from that array is dropped with an
   "unknown option" warning), its value in the section's `Default` impl, and a
   check in `validate` if the value can be wrong.
2. `config.toml`: the key, with the default value, in the same order.
3. Nothing in `delta.rs` — a key added to an existing section is classified by
   the section comparison the day it is added, which is the point of deriving
   the delta. A new **section** does need a line in `ConfigDelta::between` and
   a case in `the_remaining_sections_each_have_a_flag_of_their_own`;
   `two_different_configurations_never_compare_as_unchanged` is the catch-all
   that fails when one is forgotten.
4. `topbar/src/reload.rs`: only if the key has to be told to a running service
   rather than applied by a rebuild.

Skipping a step fails a test:
`config::tests::example_config_equals_defaults_and_warns_about_nothing`, the
same check through the CLI in the flake's `example-config` derivation,
`the_dumped_config_parses_back_to_the_same_config` if the key does not round
trip, and `tests/live_config_contract.rs` if the change makes a real v1 file
warn.
