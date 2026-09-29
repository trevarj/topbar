#!/usr/bin/env sh
# Launcher and standalone-dialog visual smoke matrix.
#
#   nix develop -c ./scripts/smoke-launcher-dialogs.sh
#
# Each palette gets a private nested niri session. Both drivers stop their
# panels before standalone dialogs, proving they work without daemon
# ownership; the light driver also checks the opaque no-blur fallback.
#
# Artifacts land in target/visual-smoke/launcher-dialogs/.  The captured
# pinentry transcripts contain only protocol setup and cancellation replies;
# the driver never types a password.
set -eu

artifact_root="${1:-target/visual-smoke/launcher-dialogs}"
mkdir -p "$artifact_root"
artifact_root=$(cd "$artifact_root" && pwd)
repo=$(pwd)

for tool in magick niri grim cargo timeout dbus-run-session wlrctl wtype gtk4-demo fd python3 gdbus; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing required tool: $tool" >&2
    exit 1
  fi
done

# Keep every launcher input deterministic.  `visual-smoke-niri.sh` copies this
# tree into the run's private XDG data directory before GIO starts discovery.
fixtures=$(mktemp -d "$artifact_root/fixtures.XXXXXX")
data_root="$fixtures/data"
file_root="$fixtures/files"
fake_bin="$fixtures/bin"
shell_program=$(command -v sh)
terminal_log="$fixtures/terminal-launch.log"
dbus_log="$fixtures/dbus-activation.log"
exec_log="$fixtures/exec-launch.log"
persist_pid="$fixtures/persistent.pid"
persist_release="$fixtures/persistent.release"
failing_exec="$fixtures/failing-launch"
catalog_root="$file_root/catalog"
stage_256="$fixtures/emit-256"
stage_512="$fixtures/emit-512"
fd_log="$fixtures/fd-stage.log"
mkdir -p "$data_root/applications" "$file_root" "$catalog_root" "$fake_bin"

cat >"$data_root/applications/io.github.topbar.SmokeEditor.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Smoke Editor
GenericName=Fixture text editor
Comment=Deterministic launcher application fixture
Exec=smoke-exec editor
Icon=accessories-text-editor
Keywords=smoke;editor;write;
StartupWMClass=TopbarSmokeEditor
Terminal=false
DESKTOP

cat >"$data_root/applications/io.github.topbar.SmokeTerminal.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Smoke Terminal
GenericName=Fixture terminal
Comment=Second launcher application fixture
Exec=true
Icon=utilities-terminal
Keywords=smoke;terminal;shell;
# Match the GTK Demo window opened behind the launcher. Enter must launch this
# desktop entry even when niri reports an existing window with its identity.
StartupWMClass=org.gtk.Demo4
Terminal=true
DESKTOP

cat >"$data_root/applications/io.github.topbar.HiddenSmoke.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Hidden Smoke
Comment=This entry must not be visible to launcher discovery
Exec=true
Icon=dialog-information
NoDisplay=true
Terminal=false
DESKTOP

printf '#!%s\nexit 0\n' "$shell_program" >"$failing_exec"
chmod 700 "$failing_exec"
cat >"$data_root/applications/io.github.topbar.FailingSmoke.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Failing Smoke
Comment=An executable failure must remain actionable in the launcher
Exec=$failing_exec
Icon=dialog-error
Terminal=false
DESKTOP

cat >"$data_root/applications/io.github.topbar.SmokeDbus.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Smoke D-Bus
Exec=smoke-exec dbus
Icon=applications-system
DBusActivatable=true
Terminal=false
DESKTOP

cat >"$data_root/applications/io.github.topbar.SmokeUnlaunchable.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Smoke Unlaunchable
Comment=DBusActivatable without Exec must report a local error
Icon=dialog-error
DBusActivatable=true
Terminal=false
DESKTOP

cat >"$data_root/applications/io.github.topbar.SmokePersistent.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Smoke Persistent
Comment=Exec child must live past the panel
Exec=smoke-exec persistent
Icon=applications-system
Terminal=false
DESKTOP

# Enough matching application tiles to force a compact launcher to scroll.
# The compact nested output is intentional: a tall host display hid the bug
# where rows below the viewport could never be reached.
catalog_app=1
while [ "$catalog_app" -le 36 ]; do
  app_number=$(printf '%02d' "$catalog_app")
  cat >"$data_root/applications/io.github.topbar.SmokeCatalog$app_number.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Smoke Catalog $app_number
GenericName=Compact output fixture $app_number
Comment=Launcher scroll accessibility fixture
Exec=true
Icon=applications-system
Keywords=smoke;catalog;scroll;
Terminal=false
DESKTOP
  catalog_app=$((catalog_app + 1))
done

# Keep fake executables on the nested run's PATH and record inherited variables.
printf '#!%s\n' "$shell_program" >"$fake_bin/xdg-terminal-exec"
cat >>"$fake_bin/xdg-terminal-exec" <<'SH'
set -eu
printf 'terminal helper invoked\n' >>"$SMOKE_LAUNCHER_TERMINAL_LOG"
for argument in "$@"; do
  printf 'argument: %s\n' "$argument" >>"$SMOKE_LAUNCHER_TERMINAL_LOG"
done
SH
chmod 700 "$fake_bin/xdg-terminal-exec"

printf '#!%s\n' "$shell_program" >"$fake_bin/smoke-exec"
cat >>"$fake_bin/smoke-exec" <<'SH'
set -eu
printf '%s: %s\n' "$1" "$SMOKE_LAUNCHER_INHERITED" >>"$SMOKE_LAUNCHER_EXEC_LOG"
if [ "$1" = persistent ]; then
  printf '%s\n' "$$" >"$SMOKE_LAUNCHER_PERSIST_PID"
  while [ ! -e "$SMOKE_LAUNCHER_PERSIST_RELEASE" ]; do sleep 0.1; done
fi
SH
chmod 700 "$fake_bin/smoke-exec"

printf 'This document is deliberately discoverable by the launcher.\n' \
  >"$file_root/smoke-document.txt"

# The launcher catalog normally finishes too quickly for a visual smoke run to
# observe it.  This fixture is a real tree and the private `fd` shim below
# reports it in three deterministic batches. The driver releases each next
# batch only after photographing the existing matching result.
catalog_file=1
while [ "$catalog_file" -le 511 ]; do
  filler="$catalog_root/catalog-filler-$(printf '%03d' "$catalog_file").txt"
  printf 'catalog filler %s\n' "$catalog_file" >"$filler"
  catalog_file=$((catalog_file + 1))
done

printf '#!%s\n' "$shell_program" >"$fake_bin/fd"
cat >>"$fake_bin/fd" <<'SH'
set -eu
printf 'fd fixture started\n' >>"$SMOKE_LAUNCHER_FD_LOG"

# `topbar-services` asks fd for NUL-delimited paths.  Honor that wire format
# while staging the catalog's 128/256/512-entry snapshots.  Every emitted
# pathname exists under the smoke-only fixture root.
printf '%s\0' "$SMOKE_LAUNCHER_FILE"
entry=1
while [ "$entry" -le 511 ]; do
  printf '%s/catalog-filler-%03d.txt\0' "$SMOKE_LAUNCHER_CATALOG_DIR" "$entry"
  entry=$((entry + 1))
  case "$entry" in
    128) marker="$SMOKE_LAUNCHER_STAGE_256" ;;
    256) marker="$SMOKE_LAUNCHER_STAGE_512" ;;
    *) marker="" ;;
  esac
  if [ -n "$marker" ]; then
    printf 'fd fixture paused at %s paths\n' "$entry" >>"$SMOKE_LAUNCHER_FD_LOG"
    waited=0
    while [ ! -e "$marker" ]; do
      [ "$waited" -lt 1800 ] || exit 1
      sleep 0.1
      waited=$((waited + 1))
    done
    printf 'fd fixture resumed at %s paths\n' "$entry" >>"$SMOKE_LAUNCHER_FD_LOG"
  fi
done
SH
chmod 700 "$fake_bin/fd"

# A real preview and a file which has an image-looking name but cannot decode.
# The chooser has to show its local failure state for the latter without a
# synchronous decoder stalling keyboard input.
magick -size 640x360 gradient:'#445566-#99aabb' "$fixtures/wallpaper-valid.png"
magick -size 240x480 gradient:'#557799-#aaccdd' "$fixtures/wallpaper-portrait.png"
printf 'not an image\n' >"$fixtures/wallpaper-unreadable.png"

state_file="$fixtures/state.json"
cat >"$state_file" <<'JSON'
{
  "launcher": {
    "applications": {
      "io.github.topbar.SmokeEditor.desktop": {
        "count": 4,
        "last_use": 1700000000,
        "score": 4.0
      }
    }
  }
}
JSON

theme_json="$fixtures/themes.json"
cat >"$theme_json" <<'JSON'
[
  {
    "id": "moonlight",
    "label": "Moonlight",
    "subtitle": "Dark",
    "palette": {
      "mode": "dark",
      "background": "#10131a",
      "surface": "#1d2531",
      "foreground": "#edf3ff",
      "accent": "#70b49b"
    }
  },
  {
    "id": "twilight",
    "label": "Twilight",
    "subtitle": "Violet",
    "palette": {
      "mode": "dark",
      "background": "#48315c",
      "surface": "#654776",
      "foreground": "#f4eafb",
      "accent": "#d49dec"
    }
  },
  {
    "id": "ember",
    "label": "Ember",
    "subtitle": "Copper",
    "palette": {
      "mode": "dark",
      "background": "#67402d",
      "surface": "#84563b",
      "foreground": "#fff0de",
      "accent": "#e4a46d"
    }
  },
  {
    "id": "forest",
    "label": "Forest",
    "subtitle": "Moss",
    "palette": {
      "mode": "dark",
      "background": "#34593d",
      "surface": "#487550",
      "foreground": "#ecf5e9",
      "accent": "#a2d589"
    }
  },
  {
    "id": "lagoon",
    "label": "Lagoon",
    "subtitle": "Teal",
    "palette": {
      "mode": "dark",
      "background": "#286471",
      "surface": "#397d8a",
      "foreground": "#e5f9f6",
      "accent": "#7bcfc7"
    }
  },
  {
    "id": "cobalt",
    "label": "Cobalt",
    "subtitle": "Blue",
    "palette": {
      "mode": "dark",
      "background": "#344b84",
      "surface": "#4965a1",
      "foreground": "#e9efff",
      "accent": "#a3beec"
    }
  },
  {
    "id": "rose",
    "label": "Rose",
    "subtitle": "Berry",
    "palette": {
      "mode": "dark",
      "background": "#743c60",
      "surface": "#92517a",
      "foreground": "#ffe9f1",
      "accent": "#e69abf"
    }
  },
  {
    "id": "dawn",
    "label": "Dawn",
    "subtitle": "Light",
    "palette": {
      "mode": "light",
      "background": "#f5f3ed",
      "surface": "#ffffff",
      "foreground": "#24303b",
      "accent": "#3b7662"
    }
  }
]
JSON

wallpaper_json="$fixtures/wallpapers.json"
cat >"$wallpaper_json" <<JSON
[
  {
    "id": "valid",
    "label": "Golden horizon",
    "subtitle": "A valid fixture preview",
    "preview_path": "$fixtures/wallpaper-valid.png"
  },
  {
    "id": "unreadable",
    "label": "Unreadable image",
    "subtitle": "A decode failure must stay local to this row",
    "preview_path": "$fixtures/wallpaper-unreadable.png"
  },
  {
    "id": "aurora",
    "label": "Aurora coast",
    "subtitle": "Extra fixture row one",
    "preview_path": "$fixtures/wallpaper-portrait.png"
  },
  {
    "id": "forest",
    "label": "Forest study",
    "subtitle": "Extra fixture row two",
    "preview_path": "$fixtures/wallpaper-valid.png"
  },
  {
    "id": "tide",
    "label": "Tide pool",
    "subtitle": "Extra fixture row three",
    "preview_path": "$fixtures/wallpaper-valid.png"
  },
  {
    "id": "clouds",
    "label": "Cloud study",
    "subtitle": "Extra fixture row four",
    "preview_path": "$fixtures/wallpaper-valid.png"
  }
]
JSON

# A gated provider exposes each in-dialog spinner without contacting Wallhaven.
tabbed_json="$fixtures/wallpaper-tabs.json"
cat >"$tabbed_json" <<JSON
{"pool":[{"id":"valid","label":"Golden horizon","preview_path":"$fixtures/wallpaper-valid.png"}],
 "presets":[{"id":"nature","label":"Nature"},{"id":"mountains","label":"Mountains"},
            {"id":"buildings","label":"Buildings"},{"id":"abstract","label":"Abstract"}]}
JSON
tabbed_provider="$fixtures/wallpaper-provider"
tabbed_gate="$fixtures/wallpaper-gate"
mkdir -p "$tabbed_gate"
printf '#!%s\n' "$shell_program" >"$tabbed_provider"
cat >>"$tabbed_provider" <<'SH'
set -eu
case "$1:$2" in
  search:nature)
    printf 'search\n' >"$SMOKE_WALLPAPER_GATE/search.ready"
    while [ ! -f "$SMOKE_WALLPAPER_GATE/search.release" ]; do sleep 0.1; done
    printf '[{"id":"9d82vk","label":"Wallhaven Nature / 9d82vk","preview_path":"%s"}]\n' "$SMOKE_WALLPAPER_IMAGE"
    ;;
  save:nature)
    [ "$3" = 9d82vk ] || exit 2
    printf 'save\n' >"$SMOKE_WALLPAPER_GATE/save.ready"
    while [ ! -f "$SMOKE_WALLPAPER_GATE/save.release" ]; do sleep 0.1; done
    printf '{"path":"%s"}\n' "$SMOKE_WALLPAPER_IMAGE"
    ;;
  *) exit 2 ;;
esac
SH
chmod 700 "$tabbed_provider"

# The blurred dark run keeps motion enabled so the stable-frame captures
# exercise selection animations as well as their settled destinations.
# The launcher matrix defaults the nested output to 1.25 scale, making a
# compact fractional display where rows have to scroll instead of falling below
# the physical host window. Callers may override TOPBAR_SMOKE_SCALE to inspect
# another output shape.
config="$fixtures/launcher-dialogs-dark.toml"
sed -e 's/^exec = .*/exec = "\/bin\/echo BTC"/' \
  crates/topbar-core/tests/fixtures/live-config.toml >"$config"
cat >>"$config" <<CONFIG

[launcher]
file_roots = ["$file_root"]
file_exclusions = []

[appearance]
theme_command = ["/bin/true"]
wallpaper_command = ["/bin/true"]
CONFIG

grep -q '^animations = true$' "$config" || {
  echo "could not enable motion in launcher smoke config" >&2
  exit 1
}
grep -q 'file_roots' "$config" || {
  echo "could not configure launcher fixture root" >&2
  exit 1
}

light_config="$fixtures/launcher-dialogs-light.toml"
sed -e 's/^mode = "dark"$/mode = "light"/' \
    -e 's/^blur = true$/blur = false/' \
    -e 's/^animations = true$/animations = false/' "$config" >"$light_config"
grep -q '^mode = "light"$' "$light_config" || {
  echo "could not create light dialog config" >&2
  exit 1
}
grep -q '^blur = false$' "$light_config" || {
  echo "could not disable blur in light fallback config" >&2
  exit 1
}
grep -q '^animations = false$' "$light_config" || {
  echo "could not disable motion in light fallback config" >&2
  exit 1
}

# `cargo build -p topbar` from the harness builds the sibling dedicated binary
# too.  Naming it explicitly makes the pinentry smoke exercise the packaged
# entrypoint branch rather than the ordinary topbar command parser.
pinentry_bin="$repo/target/debug/topbar-pinentry"

blur_log="topbar::wayland::blur=debug"
if [ -n "${RUST_LOG:-}" ]; then
  blur_log="$RUST_LOG,$blur_log"
fi

run_harness() {
  run_artifact=$1
  run_config=$2
  run_light=$3
  mkdir -p "$run_artifact"
  harness_status=0
  RUST_LOG="$blur_log" \
  TOPBAR_SMOKE_DATA="$data_root" \
  TOPBAR_SMOKE_STATE="$state_file" \
  TOPBAR_SMOKE_PINENTRY=1 \
  SMOKE_PATH="$fake_bin" \
  TOPBAR_SMOKE_SCALE="${TOPBAR_SMOKE_SCALE:-1.25}" \
  TOPBAR_SMOKE_TIMEOUT="${TOPBAR_SMOKE_TIMEOUT:-480}" \
  TOPBAR_SMOKE_DRIVER="$repo/scripts/smoke-launcher-dialogs-shot.sh" \
  TOPBAR_VISUAL_CONFIG="$run_config" \
  SMOKE_LAUNCHER_LIGHT_ONLY="$run_light" \
  SMOKE_LAUNCHER_THEME_JSON="$theme_json" \
  SMOKE_LAUNCHER_WALLPAPER_JSON="$wallpaper_json" \
  SMOKE_LAUNCHER_LIGHT_CONFIG="$light_config" \
  SMOKE_WALLPAPER_TABS_JSON="$tabbed_json" \
  SMOKE_WALLPAPER_PROVIDER="$tabbed_provider" \
  SMOKE_WALLPAPER_GATE="$tabbed_gate" \
  SMOKE_WALLPAPER_IMAGE="$fixtures/wallpaper-valid.png" \
  SMOKE_LAUNCHER_PINENTRY="$pinentry_bin" \
  SMOKE_LAUNCHER_FILE="$file_root/smoke-document.txt" \
  SMOKE_LAUNCHER_CATALOG_DIR="$catalog_root" \
  SMOKE_LAUNCHER_STAGE_256="$stage_256" \
  SMOKE_LAUNCHER_STAGE_512="$stage_512" \
  SMOKE_LAUNCHER_FD_LOG="$fd_log" \
  SMOKE_LAUNCHER_TERMINAL_LOG="$terminal_log" \
  SMOKE_LAUNCHER_EXEC_LOG="$exec_log" \
  SMOKE_LAUNCHER_INHERITED=from-niri-child \
  SMOKE_LAUNCHER_PERSIST_PID="$persist_pid" \
  SMOKE_LAUNCHER_PERSIST_RELEASE="$persist_release" \
  SMOKE_LAUNCHER_FAILING_EXEC="$failing_exec" \
  SMOKE_LAUNCHER_DBUS_LOG="$dbus_log" \
  SMOKE_LAUNCHER_DBUS_HELPER="$repo/scripts/smoke-dbus-application.py" \
    sh "$repo/scripts/visual-smoke-niri.sh" "$run_artifact" \
    >"$run_artifact/run.log" 2>&1 || harness_status=$?

  if [ "$harness_status" -ne 0 ]; then
    echo "smoke-launcher-dialogs: nested harness failed ($harness_status); see $run_artifact/run.log" >&2
    return 1
  fi
  if ! grep -q '^--- result: PASS$' "$run_artifact/driver.log" 2>/dev/null; then
    echo "smoke-launcher-dialogs: driver did not report PASS; see $run_artifact/driver.log" >&2
    return 1
  fi
}

status=0
run_harness "$artifact_root" "$config" 0 || status=1
run_harness "$artifact_root/light" "$light_config" 1 || status=1
if grep -q '^D ' "$artifact_root"/pinentry-*.protocol \
    "$artifact_root"/light/pinentry-*.protocol 2>/dev/null; then
  echo "smoke-launcher-dialogs: a pinentry secret response was captured" >&2
  status=1
fi

echo "--- screenshots ---"
find "$artifact_root" -name '*.png' | sort
echo "--- driver ---"
cat "$artifact_root/driver.log" 2>/dev/null || true
cat "$artifact_root/light/driver.log" 2>/dev/null || true
echo "--- standalone protocol results ---"
for transcript in "$artifact_root"/pinentry-*.protocol \
    "$artifact_root"/light/pinentry-*.protocol; do
  [ -f "$transcript" ] || continue
  echo "=== $transcript ==="
  sed -n '1,80p' "$transcript"
done
echo "--- Exec launches ---"
cat "$artifact_root"/fixtures.*/exec-launch.log 2>/dev/null || true

exit "$status"
