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
Niri session by default, with stand-ins on a private bus, and archive screenshots
under `target/visual-smoke/`. The shared harness also supports Mango through
`TOPBAR_SMOKE_COMPOSITOR=mango`. These runs need a visible parent Wayland desktop;
they are not headless CI checks.

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
The harness requires an existing socket, resolving relative names under the
original `XDG_RUNTIME_DIR` and accepting absolute paths. Only the nested backend
uses that absolute parent socket. Keep its window visible so frames are presented.

After the incremental Cargo builds, compositor descendants receive a private HOME,
XDG directories, runtime and session D-Bus. The real system bus is always blocked;
neither the parent runtime bus nor `systemd/private` is linked into the preview.
Inherited compositor markers and remote audio endpoints are cleared. Use the
existing fake-service overrides for network, Bluetooth and power, and the private
PulseAudio server when audio is needed. Do not operate real device controls for
screenshots; this environment isolation is not a filesystem/device-access sandbox.
The supervisor drains only its own descendants, including Mango's asynchronous
session helpers, before removing the private bus/runtime. Ctrl+C stops that
supervisor without tearing its bus down first.

### Mango desktop preview

Run from the host's Niri terminal, not a busless development sandbox. Build Mango
and a theme bundle in the sibling trev-nix checkout with local dotfiles/topbar
overrides first; the selected bundle must contain both `topbar.toml` and
`mango.conf`. The dev shell supplies Adwaita's store-backed `XCURSOR_PATH`, Kitty,
and the capture/helper dependencies. The preview resolves the owner profile's
existing Brave executable before boxing HOME, and fails clearly if it is absent;
Brave is not a topbar SDK dependency. Mango comes from trev-nix's existing pinned
unstable package, not another input here.

```sh
# From topbar; substitute the actual built absolute bundle path.
TOPBAR_SMOKE_COMPOSITOR=mango \
TOPBAR_SMOKE_MANGO_CONFIG="$HOME/Workspace/dotfiles/mango/.config/mango/config.conf" \
TOPBAR_VISUAL_CONFIG="<absolute-bundle>/topbar.toml" \
TOPBAR_SMOKE_TIMEOUT=3600 \
TOPBAR_SMOKE_POWER=1 \
nix shell --inputs-from ../trev-nix ../trev-nix#mango \
  nixpkgs#slurp nixpkgs#satty nixpkgs#xterm -c \
  nix develop -c ./scripts/playground.sh "target/mango-preview/$(date +%Y%m%d-%H%M%S)"
```

Omit `TOPBAR_SMOKE_MANGO_CONFIG` for the minimal Mango smoke configuration.
When supplied, the shared production config keeps its layout/input/effects/rules,
but startup, physical outputs and theme includes are replaced. The nested backend
is forced to Wayland, outputs use the requested `TOPBAR_SMOKE_SCALE` (default 0.75),
and theme colors come from `mango.conf` beside the original panel source.
Virtual `WL-*` outputs inherit the parent's mode and refresh timing and use
`vrr:1`: wlroots' Wayland backend rejects an explicit adaptive-sync disable or
a fixed refresh rate. Readiness requires a connected backend and focused output.
Production Super becomes Alt; **Alt+Ctrl+Space** cycles layouts and
**Alt+Ctrl+grave** toggles the named Kitty scratchpad. MRU uses **Alt+Ctrl+Tab**
and **Alt+Ctrl+Shift+Tab** for reverse. Screenshots use **Alt+Ctrl+Print** for
region→Satty, **Alt+Ctrl+Shift+Print** for output and **Alt+Ctrl+F12** for the
focused window. These are temporary preview-only aliases: parent Niri grabs
ordinary Alt+Tab and Print/Ctrl+Print/Alt+Print, so those keys do not exercise
nested Mango. Production keys remain unchanged. **Alt+Return** and **Alt+B**
launch a disposable Kitty and a separate Brave profile. Production lock/idle/DPMS, user-manager
imports, media/brightness/DDC/Orca, app profiles and theme helpers never run.
The copied source `mango-screenshot` is on the private PATH without activation.
Existing language-switch and MRU bindings are replaced before preview aliases
are added, so remapping the production modifier cannot create duplicate bindings.

Independent language switching has a stock Mango/wlroots nested-backend limit:
the parent's Wayland keyboard modifier events restore its layout group, including
on focus entry. A Mango IPC layout change can therefore persist while idle and
revert on the next parent event. The real-session US/Russian configuration is
unchanged; this preview cannot certify physical-keyboard layout persistence.
Do not hide the reset with a timer or change the host keyboard layout.

Mango reads the current wallpaper target from the host's
`${XDG_CACHE_HOME:-$HOME/.cache}/wallpaper/current.png` before boxing HOME/cache.
A private `awww-daemon -n normal --no-cache` applies that image before the panel
starts; its runtime/cache stay private and cleanup stops only this run's daemon.
An absent or unreadable image keeps the plain background with a diagnostic:
no wallpaper fetch, fallback image, host daemon or theme helper is involved.
Niri keeps its existing fixture background.

`TOPBAR_VISUAL_CONFIG` is read-only input: only a sanitized private TOML copy
reaches the Mango panel or standalone dialogs. Appearance helpers, every `on_*`
callback, custom shell widgets/placement references and command overrides are
removed. Updates point at an absent private-home flake. Brightness is disabled
(it can write sysfs without D-Bus), and headset queries point at a nonexistent
private executable. Mango requires `TOPBAR_SMOKE_POWER=1`; the existing fake power
service must be ready on the private bus with its disposable sysfs tree before
the panel starts. Battery/power state is synthetic; real brightness/headset
controls are intentionally unavailable. Niri fixture scenarios keep their
explicit fixture commands.

The playground launches three disposable Kitty windows and writes a settled
`playground.png`. Each run records `compositor-environment.txt`,
`mango-environment.txt`, `compositor.json`, `backend-ok`, `preview-panel.toml`,
`preview-mango.conf`, `compositor.log`, `panel.log` and `driver.log`.
Readiness means the selected instance supplied a live socket and
`topbar dump state --json` reported `compositor.backend = "mango"` and
`compositor.connected = true`; a screenshot alone is not readiness.
Completed screenshots and any retained `.mango-screenshot.*` staging directories
are copied from private HOME into the artifact directory's `screenshots/` before
cleanup, including interrupted runs.

Review separate fresh runs in order, with user feedback before the next batch:

1. Layout/input: half-width windows, width presets, Scroller/Fair/Dwindle with
   per-tag retention, focus/exchange/tag transfers, floating/signed-zero resizing,
   scratchpad, US/Russian and gestures.
2. Appearance: gaps/borders/corners, dim/shadow/blur, window/tag/layer motion,
   launcher/popover/chooser/OSD/toasts and unanimated transparent selectors/catchers.
3. Integration: persistent per-output tag pills and nonwrapping scroll, overview
   exits through current-pill/window activation, keyboard next/previous, notification
   focus, standalone dialogs, cursor/native/XWayland apps, all three screenshot
   modes, cancellation/missing-focus/privacy failure and nested-only hold-to-log-out.

Repeat output/activation/modal checks with `WLR_WL_OUTPUTS=2`; actual output
removal differs from a geometry/scale change. In the **host terminal**, record
`systemctl --user show-environment` and `systemctl --user is-active niri.service`
before/after, and compare display/compositor values. Host Niri, panel,
notifications and wallpaper must remain unchanged. Never run the real-session
wrapper or activate the NixOS configuration to obtain a nested preview.

The dependency-free transformation regression can run without a display, bus,
Cargo build or compositor:

```sh
nix develop -c ./scripts/visual-smoke-niri.sh --self-check
```

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
