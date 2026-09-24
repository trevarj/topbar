# AGENTS.md

Working agreement for agents and humans on topbar v2.

## System

The host is **NixOS 26.05** running **niri**. Everything goes through the flake —
there is no system-wide Rust toolchain and no `cargo` on `$PATH` outside the dev
shell.

- `direnv allow` once in the repo root, or prefix every command with
  `nix develop -c`.
- **Never run `cargo` outside the dev shell.** A bare `cargo build` either fails
  or picks up the wrong toolchain and links against the wrong GTK.
- The shell is **zsh**.
- Do not set `LD_LIBRARY_PATH` or `CC` anywhere. The flake's `buildInputs` and
  `wrapGAppsHook4` already provide every library, icon theme, and GSettings
  schema the panel needs. Overriding them is how the Guix-era build broke.
- Pre-commit hooks (rustfmt, nixfmt, whitespace, convco) install themselves via
  the dev shell's `shellHook`. Clippy and tests are CI's job, not the hook's.

## Design

v2 is a GNOME Shell top bar, not a status-bar toolkit. When a decision is
ambiguous, ask what GNOME Shell does. GNOME Shell is the design inspiration
only; the project is not affiliated with the GNOME Project.

- **The name is `topbar`, always lowercase.** Prose, headings, sentence
  starts, `--help`, log lines, and the notification server's name all use it
  verbatim — never capitalised, not even to open a sentence. `TOPBAR_*`
  environment variables are the sole exception, and Rust types keep Rust's own
  naming conventions.
- **Quiet.** Widgets that have nothing to say are invisible. `system_monitor`
  shows nothing while the machine is healthy; the mic indicator appears only
  while a source is in use.
- **Continuous.** One solid, full-width, opaque panel pinned to the top edge.
  No islands, no floating pills, no per-widget outlines, no bottom position.
- **System-owned.** The panel presents system state, not decoration. Dark and
  light modes each resolve one palette and one generated stylesheet.
- **Mass code reduction is the point.** v1 was 76k lines; v2 targets ~30k. A
  change that adds a subsystem needs to justify itself against that budget.
- **Only the planned surface.** Do not add widgets, config keys, or options that
  are not in the plan. Dropped v1 features stay dropped; their config keys are
  accepted with a specific warning so old files keep loading.
- Motion: 120-150ms for micro-interactions, 150-250ms for containers, never
  over 300ms. `theme.animations = false` means *zero* motion, and
  `gtk-enable-animations` is honored too.
- Icons: Adwaita symbolic names, resolved through one semantic-name module. The
  `os_logo` Nerd Font glyph is the only text-glyph exception.

## Rust

- Stable toolchain, **edition 2024**. No nightly features.
- **Typed boundaries.** Parse protocol data into types once, at the edge. Use
  `niri-ipc`'s types for niri and `#[zbus::proxy]`/`#[zbus::interface]` for
  D-Bus. Hand-walking `serde_json::Value` or `zvariant::Value` in widget code is
  a bug, not a shortcut.
- **`topbar-services` must never depend on `gtk4`.** That dependency edge is the
  thread-safety mechanism: if services cannot name a widget type, they cannot
  touch one. State crosses the boundary as `Send + Clone` handles and
  `Arc<Snapshot>` values.
- **All GTK work happens on the main thread.** Services publish into
  `tokio::sync::watch`; the GTK side subscribes through `bridge::bind_state`,
  which runs the render closure on the main context and aborts when the widget
  drops.
- Blocking I/O never runs on the GTK main thread. No `call_sync`, no blocking
  socket reads, no `std::process::Command` in a click handler.
- Mutating service calls return `Result`; failures are rendered, not discarded.
- **Every behavior change ships with a test.** Protocol parsers get fixtures,
  state machines get table tests, and the config schema gets a case in
  `crates/topbar-core/src/config.rs` or the live-config contract test.

## Verification

- `nix flake check` — the gate. Run it before every push. It covers build, fmt,
  clippy with `-D warnings`, tests, the pre-commit hooks, and a `--strict` run
  of the shipped example configuration.
- `nix build` — run for any change to packaging, dependencies, or the GTK link
  path.
- `nix develop -c cargo test --workspace --all-targets` — the inner loop.
- **Run clippy in release as well as debug.** A field or function used only
  from a `cfg(debug_assertions)` block is dead code in the packaged build and
  only the release lint says so. `nix flake check` compiles the tests in
  release, so it catches this; a green `cargo clippy` alone does not.
- `nix develop -c ./scripts/visual-smoke-niri.sh` — nested niri + `grim` screenshots.
  Local only: niri has no headless backend, so CI cannot run them.
- UI milestones also need a run on the live niri session against
  `~/.config/topbar/config.toml`.

## The smoke harness

`scripts/visual-smoke-niri.sh` runs the panel inside a nested niri session and
`scripts/smoke-<area>.sh` drives one area of it. The rules below were each
learned by getting them wrong.

- **The developer's session is untouchable.** Everything runs under
  `dbus-run-session` on a private bus, because the panel takes
  `org.freedesktop.Notifications` with `ReplaceExisting` and would otherwise
  take the desktop's notifications away from whatever is serving them.
  `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`
  are all boxed inside the run.
- **Never write to the system bus.** NetworkManager, BlueZ, UPower and
  power-profiles are the machine's live network, headphones and battery. Each
  has a `topbar-fake-*` sidecar that serves the same interfaces on the private
  bus, and a debug build with no `TOPBAR_SMOKE_*_BUS` refuses every mutation
  rather than reaching the real one.
- **Capture on evidence, never on a clock.** `scripts/smoke-shot.sh`'s `shot`
  waits for the layer surface to be mapped, for something to be drawn below the
  bar, and for two consecutive captures to be byte-identical. A fixed `sleep`
  before `grim` is a coin toss, and it has come up tails twice.
- **A stub has to be checked, not assumed.** Every scenario verifies its own
  fixtures — the port answers 200, the fake logged `ready`, the config really
  contains the line `sed` was supposed to put there — before it starts, and
  fails loudly rather than photographing a panel with nothing behind it.
- **Reap everything.** Fakes exit when their bus closes and every script traps
  `EXIT INT TERM` with a `pkill` as the belt.
- Inner scripts passed to `sh -c` are single-quoted, so no apostrophes in their
  comments.

### Two things the nested session does that a real one does not

- **Its frame clock runs at about 1Hz.** The host compositor is not presenting
  a window nobody is looking at, so GTK falls back to a throttled tick, and an
  animation started just after one tick waits most of a second for the next.
  The panel logs the wait — `motion: run N got its first frame after 951ms` in
  a debug build — and the pattern is unmistakable: a run on an
  already-presented surface reads 0-4ms, a run on a surface that has just been
  mapped reads hundreds of milliseconds. This is what made a 150ms banner
  slide look as though it started seconds late. Nothing about it is the
  panel's; on a live session the same line reads one frame. **Do not tune an
  animation against a nested timing.**
- **It has exactly one output and will not give it up.** `niri msg output
  winit off` is refused, so monitor teardown and hotplug cannot be driven here
  — `scripts/smoke-hotplug.sh` says so and churns the output's scale instead,
  which exercises the same debounce and the same handler bookkeeping. Taking a
  monitor away belongs on the live-session checklist.

## Commits

- **Conventional Commits**, enforced by the `convco` pre-commit hook.
- Atomic: each commit builds, passes tests, and does one thing. Prefer a series
  of small commits over one large one.
- Subject in the imperative mood, no trailing period. Explain *why* in the body
  when the reason is not obvious from the diff.
- Breaking changes use `!` and a `BREAKING CHANGE:` footer.
