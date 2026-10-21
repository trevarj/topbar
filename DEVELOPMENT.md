# Development reference

## Development

Run these commands from the repository root:

```sh
direnv allow                                        # or: nix develop
nix develop -c cargo test --workspace --all-targets # inner loop
nix flake check                                     # build + fmt + clippy + tests
nix build                                           # packaged binary in ./result
nix develop -c ./scripts/smoke-full-bar.sh          # nested niri, one screenshot
```

See [the architecture](docs/architecture.md) for the codebase map and
[AGENTS.md](AGENTS.md) for the full working agreement.

## Visual playground and screenshots

The `scripts/smoke-*.sh` runs each drive one part of the panel inside a nested
niri session with stand-ins on a private bus, and archive their screenshots
under `target/visual-smoke/`. They are local-only: niri has no headless
backend, so CI cannot run them.

For an interactive nested panel, run from the repository root:

```sh
nix develop -c ./scripts/playground.sh
```

The playground opens the clock control center and starts a sample paused media
player. Press Ctrl+C in its terminal to stop it. Its default artifact directory
is `target/visual-smoke/playground/`; pass an artifact directory as the first
argument to choose another location. It contains the harness logs, compositor
state and the player's `cover.png`. The playground is interactive, not an
automatic screenshot driver. The full-bar screenshot scenario above writes to
`target/visual-smoke/full-bar/`; other smoke scenarios use their own directories
under `target/visual-smoke/`.

Start from a real graphical desktop session with a nonempty `WAYLAND_DISPLAY`.
Before capture, require that it resolves to an existing socket: use an absolute
value directly, or resolve a relative value under `XDG_RUNTIME_DIR`. Do not
guess a display name or substitute a headless session. Keep the nested niri
window visible so the parent compositor presents its frames.

The harness boxes XDG paths and uses a private D-Bus session. Sample desktop
services must stay on that private bus: launching a second panel on the real
session bus can replace the desktop's notification service. Use the existing
fake-service overrides for network, Bluetooth and power, and the harness's
private PulseAudio server when those services are needed; do not operate on real
devices for screenshots.

Publication captures additionally block access to the real system bus. Before
starting the harness, set `DBUS_SYSTEM_BUS_ADDRESS` to
`unix:path=$work/no-system-bus`, where `$work` is a temporary directory and the
socket does not exist. A private session bus alone does not isolate logind:
the lifecycle service otherwise connects to the host system bus and acquires a
sleep-delay inhibitor. Explicit fake-service overrides still route sample
services to the private session bus.

See [the smoke harness](docs/architecture.md#the-smoke-harness) for the existing
capture helpers, readiness checks, isolation conventions and frame-settling
requirements.

## Application launching

The application launcher uses GIO to fork `.desktop` `Exec=` commands directly
from the panel started by niri. Apps inherit niri's child environment and keep
running when topbar exits. `DBusActivatable` tiles use their `Exec=` instead of
requesting D-Bus activation; entries without a usable `Exec=` report an error.
GIO handles Terminal entries, startup notification and desktop field codes.
For a `DBusActivatable` entry, GIO's in-memory keyfile constructor cannot
expand `%k` (the desktop-file path); ordinary desktop entries retain `%k`.

## Wallpaper provider protocol

`topbar choose --title "Wallpaper" --layout wallpapers --wallpaper-provider PROGRAM` accepts
`{"pool":[...],"presets":[{"id":"nature","label":"Nature"}]}` on stdin. Local
filters immediately; Wallhaven's query is submitted only with Enter or Search.
`Filter results` fuzzy-matches loaded labels, IDs, subtitles, and optional
`tags: ["tag name", "tag alias"]` without making API requests. Apply saves the
selected wallpaper; Enter in the Wallhaven query never applies it.

The provider protocol is `PROGRAM search PRESET QUERY` (empty QUERY uses the
preset defaults), streaming NDJSON: one compact candidate object per line,
flushed as soon as its thumbnail and tags are ready. Each preview path must be
absolute and local; candidate IDs must be unique across the stream. Search
stdout is limited to 8 MiB total. EOF **and a successful process exit** complete
the search; empty stdout means no results. Rows appear while the spinner stays
active, retaining the current filter and selected ID. Apply is enabled once the
selected preview has decoded, even while search is still streaming; applying
supersedes that search with the selected-image save. A malformed row or provider
failure leaves already validated rows visible with an inline error and Retry;
Retry clears them and resubmits the submitted query. Superseded query/preset
events and events after close are ignored.
The chooser caches only the latest submitted query/results per preset; leaving
a pending search invalidates it, so returning restarts that unfinished search.
`PROGRAM save PRESET ID` still returns `{"path":"/absolute/saved/image"}`.
A failed save's Retry repeats the selected-image save, not an earlier search.
Explicit queries are passed unchanged for the provider to URL-encode, preserving
Wallhaven operators without appending preset terms.
Provider-mode output is `{"kind":"pool","id":"..."}` or
`{"kind":"saved","path":"..."}`; cancellation exits 1.

## Display-dependent checks

The display-dependent GTK regression checks can run on a real display or under
Xvfb (`xvfb-run` must be available; each command gets a fresh GTK process):

```sh
nix develop -c env GDK_BACKEND=x11 xvfb-run -a cargo test -p topbar widgets::clock::tests::calendar_tooltip_tracks_local_date_rollover -- --ignored --exact
nix develop -c env GDK_BACKEND=x11 xvfb-run -a cargo test -p topbar surfaces::tooltip::gtk_tests::text_tooltip_replaces_calendar_content -- --ignored --exact
```
