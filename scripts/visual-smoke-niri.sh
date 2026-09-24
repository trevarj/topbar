#!/usr/bin/env sh
# Nested-niri visual smoke test: start the panel inside a headless-ish niri
# session, screenshot it with grim, and archive the PNG.
#
# Local only — niri has no headless backend, so CI cannot run this.
# Run it from the dev shell: nix develop -c ./scripts/visual-smoke-niri.sh
#
# EVERYTHING runs inside `dbus-run-session`, on a private bus that exists only
# for the length of the run. This is not optional: from M4 the panel takes
# `org.freedesktop.Notifications` with ReplaceExisting, and a nested panel on
# the developer's real session bus would take the desktop's notifications away
# from whatever is actually serving them.
#
# Environment:
#   TOPBAR_VISUAL_CONFIG  panel config to run (default ./config.toml)
#   TOPBAR_SMOKE_OPEN     open a widget's popover without a pointer.
#                         There is no synthetic input in the dev shell, so
#                         this is how an *open* popover gets screenshotted
#                         before M8's `topbar popover show` exists. Debug
#                         builds only, which is what this script builds.
#
#                           clock    open it a second in, leave it open
#                           clock:6  six toggles 1.5s apart. An even count
#                                    ends closed (check teardown: `niri msg
#                                    layers` should list only the bar); an
#                                    odd one ends reopened onto retained
#                                    content.
#
#                         It also takes the name of a registered action for a
#                         surface that is not a popover at all:
#
#                           weather-setup  the weather location dialog
#
#   TOPBAR_SMOKE_QUERY    seeds the weather location dialog's search box and
#                         runs the search, so it can be photographed with
#                         results in it. There is no synthetic keyboard here
#                         any more than there is a pointer.
#
#   TOPBAR_SMOKE_DRIVER   a shell script run inside the nested session once
#                         the panel is up, instead of the default "wait,
#                         then take one screenshot". It is given
#                         $SMOKE_ARTIFACTS, $SMOKE_PANEL_PID (for reading
#                         VmRSS out of /proc) and, when it was built,
#                         $SMOKE_FAKE_PLAYER. It may call notify-send,
#                         gdbus, and grim against the private bus, which is
#                         how the notification and media matrices are driven.
#   TOPBAR_SMOKE_PLAYERS  build `topbar-fake-player` and hand the driver its
#                         path in $SMOKE_FAKE_PLAYER. Off by default: it is
#                         a second binary to link and only the media driver
#                         wants it.
#   TOPBAR_SMOKE_TRAY     the same for `topbar-fake-sni`, handed over in
#                         $SMOKE_FAKE_SNI. Only the tray driver wants it.
#   TOPBAR_SMOKE_POWER    build `topbar-fake-power`, start it on the private
#                         session bus, and point the panel's battery and
#                         power-profiles clients at it. The real ones live on
#                         the SYSTEM bus, which nothing here can box and which
#                         a test must never write to: setting the developer's
#                         charge limit or CPU governor to take a screenshot
#                         would be unforgivable. A fake
#                         /sys/class/power_supply tree is created alongside it
#                         and handed to both the panel and the driver, so the
#                         charge-limit write path lands in a temporary
#                         directory. logind is deliberately NOT redirected —
#                         the idle inhibitor keeps talking to the real one, as
#                         it has since M8.
#                         The value is passed to the fake as extra arguments,
#                         so a scenario can choose which bus names it answers
#                         to and what the battery reads.
#   TOPBAR_SMOKE_NM       build `topbar-fake-nm`, start it on the private
#                         session bus, and point the panel's network service at
#                         it with TOPBAR_SMOKE_NM_BUS. The real NetworkManager
#                         is on the SYSTEM bus and *is* the developer's live
#                         connection: a smoke run must never join a network,
#                         switch a radio, ask a card to scan, or register a
#                         secret agent there — a second agent would intercept
#                         the password prompts the session's own panel is
#                         waiting for. The value is passed to the fake as extra
#                         arguments, so a scenario says what is in range and
#                         what is saved. The driver is handed $SMOKE_FAKE_NM.
#   TOPBAR_SMOKE_BLUEZ    the same for `topbar-fake-bluez`, pointed at with
#                         TOPBAR_SMOKE_BLUEZ_BUS. The real BlueZ is on the
#                         SYSTEM bus and *is* the developer's headphones: a
#                         smoke run must never switch that radio off,
#                         disconnect what is playing, or register a pairing
#                         agent there. A debug build with no
#                         TOPBAR_SMOKE_BLUEZ_BUS refuses all three by
#                         construction — see `network::Access`.
#   SMOKE_PATH            a directory prepended to $PATH inside the session,
#                         for the fake package managers the updates scenarios
#                         run. Nothing else on the machine is on that PATH
#                         entry, so `checkupdates` there is the run's own
#                         script and never pacman-contrib's.
#   TOPBAR_SMOKE_OSRELEASE
#                         an /etc/os-release to copy into the sandbox, so a run
#                         can tell the updates service it is on Arch or Debian
#                         without being on either.
#   TOPBAR_SMOKE_STATE    a state.json copied into the sandboxed
#                         $XDG_STATE_HOME/topbar before the panel starts, so a
#                         run can begin from state a previous session
#                         remembered — a saved weather location, say. The copy
#                         lands inside the sandbox and nowhere else.
#   TOPBAR_SMOKE_PULSE    start a PulseAudio of the run's own, with a null
#                         sink, inside the sandbox, and point the panel and the
#                         CLI at it through $PULSE_SERVER. The developer's real
#                         sound server is on the session they are logged into
#                         and must never hear from a test — so this is how the
#                         volume OSD gets driven by real volume changes.
#                         `2` loads a second null sink: Quick Settings hides its
#                         output chooser on a machine with one output, which is
#                         correct and means a run with one sink cannot click it.
#   TOPBAR_SMOKE_SCALE    the nested output's scale (0.75). The default gives
#                         the session more logical room than the host's window
#                         has pixels, which is what stops the bar clipping its
#                         right-hand section — but it also means GTK renders
#                         every 16px icon into 12 device pixels, and the
#                         resampling hides exactly the fringing and misalignment
#                         a refinement pass is looking for. `1.0` is what a
#                         real display does: fewer logical pixels to lay out in,
#                         and every one of them a pixel.
#   TOPBAR_SMOKE_TIMEOUT  seconds before the session is killed (30).
#
# The driver is also given $SMOKE_TOPBAR, the panel binary, so it can run
# `topbar volume set 30` and the rest against the session it is inside.
set -eu

# A nested non-session launch does not replace inherited desktop markers.
export XDG_CURRENT_DESKTOP=niri
unset HYPRLAND_INSTANCE_SIGNATURE

artifact_dir="${1:-target/visual-smoke}"
config="${TOPBAR_VISUAL_CONFIG:-config.toml}"
driver="${TOPBAR_SMOKE_DRIVER:-}"
timeout_s="${TOPBAR_SMOKE_TIMEOUT:-30}"
mkdir -p "$artifact_dir"
rm -f "$artifact_dir/backend-ok"

bus_config=$(pwd)/scripts/smoke-session.conf

# Sandbox every XDG write path. The panel migrates/creates state dirs and will
# grow cache use over time; a smoke run must never touch the developer's real
# ~/.local/state (this bit us once: the state-dir migration renamed the live
# v1 panel's state directory). Config is passed with --config explicitly, but
# XDG_CONFIG_HOME is boxed too so the legacy-path fallback can't find a real
# user config and add warning noise to panel.log.
xdg_box=$(mktemp -d "${TMPDIR:-/tmp}/topbar-smoke-xdg.XXXXXX")
trap 'rm -rf "$xdg_box"' EXIT INT TERM
export XDG_STATE_HOME="$xdg_box/state"
export XDG_CACHE_HOME="$xdg_box/cache"
export XDG_CONFIG_HOME="$xdg_box/config"
mkdir -p "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME/niri"

# The runtime directory is boxed too, so the panel's single-instance lock and
# its IPC socket land inside the run and nowhere near a real panel's. The host
# compositor's socket is linked in first, because the nested niri finds it
# through exactly this variable.
host_runtime="${XDG_RUNTIME_DIR:-}"
export XDG_RUNTIME_DIR="$xdg_box/run"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
if [ -n "$host_runtime" ] && [ -n "${WAYLAND_DISPLAY:-}" ]; then
  for suffix in "" ".lock"; do
    if [ -e "$host_runtime/$WAYLAND_DISPLAY$suffix" ]; then
      ln -sf "$host_runtime/$WAYLAND_DISPLAY$suffix" \
        "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY$suffix"
    fi
  done
fi

# The /etc/os-release the updates service reads, so a run can be on Arch or
# Debian without the machine being either. The panel is pointed at the copy
# with TOPBAR_SMOKE_ROOT; /etc itself is never touched.
if [ -n "${TOPBAR_SMOKE_OSRELEASE:-}" ]; then
  mkdir -p "$xdg_box/root/etc"
  cp "$TOPBAR_SMOKE_OSRELEASE" "$xdg_box/root/etc/os-release"
  TOPBAR_SMOKE_ROOT="$xdg_box/root"
  export TOPBAR_SMOKE_ROOT
fi

if [ -n "${TOPBAR_SMOKE_STATE:-}" ]; then
  mkdir -p "$XDG_STATE_HOME/topbar"
  cp "$TOPBAR_SMOKE_STATE" "$XDG_STATE_HOME/topbar/state.json"
fi

# The nested compositor gets a config of its own, for two reasons. niri's
# "Important Hotkeys" overlay opens on top of everything at startup and sits
# in the middle of every screenshot, which is precisely where the panel's
# popovers are — and a session with one keyboard layout has no layout to
# switch to, so the keyboard_layout widget correctly draws nothing and can
# never be photographed. Two layouts, which is what the developer's own
# session has.
cat >"$XDG_CONFIG_HOME/niri/config.kdl" <<'KDL'
// Written by scripts/visual-smoke-niri.sh for the nested session.
hotkey-overlay {
    skip-at-startup
}

input {
    keyboard {
        xkb {
            layout "us,ru"
        }
    }
}

// The same blur the developer's own session runs, so a panel asking for a
// blurred region here is answered the way it is answered in real use. The
// numbers are copied from their config; what matters is that blur is on at
// all, because a compositor with it off ignores the request entirely and a
// blur smoke test would then be photographing nothing.
blur {
    on
    passes 2
    offset 3.0
    noise 0.02
    saturation 1.5
}

// Top and overlay layers take the real blur rather than the cheap xray
// approximation, which is what makes the desktop behind a popover smear
// instead of merely showing through.
layer-rule {
    match layer="top"
    match layer="overlay"

    background-effect {
        xray false
    }
}
KDL

# The nested window is however big the *host* compositor made it, which is
# narrower than a real screen and narrow enough that the panel clips its
# right-hand section — which looks exactly like a widget that failed to build
# and is not. Scaling the output down gives the nested session more logical
# room inside the same window, without a script reaching out to resize a
# window in the developer's own session.
#
# It costs something, though, and a refinement pass is the one thing that
# notices: at 0.75 every logical pixel is three quarters of a device one, so a
# 16px icon is resampled into 12 and the fringing, the half-pixel baselines and
# the knob edges a real display would show are averaged away. TOPBAR_SMOKE_SCALE
# is how a run asks for the display the panel actually ships to.
cat >>"$XDG_CONFIG_HOME/niri/config.kdl" <<KDL

output "winit" {
    scale ${TOPBAR_SMOKE_SCALE:-0.75}
}
KDL

for tool in niri grim cargo timeout dbus-run-session; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing required tool: $tool" >&2
    exit 1
  fi
done

cargo build -p topbar

player_abs=""
if [ -n "${TOPBAR_SMOKE_PLAYERS:-}" ]; then
  cargo build -p topbar-services --features fake-player --bin topbar-fake-player
  player_abs=$(pwd)/target/debug/topbar-fake-player
fi

sni_abs=""
if [ -n "${TOPBAR_SMOKE_TRAY:-}" ]; then
  cargo build -p topbar-services --features fake-sni --bin topbar-fake-sni
  sni_abs=$(pwd)/target/debug/topbar-fake-sni
fi

power_abs=""
if [ -n "${TOPBAR_SMOKE_POWER:-}" ]; then
  cargo build -p topbar-services --features fake-power --bin topbar-fake-power
  power_abs=$(pwd)/target/debug/topbar-fake-power
fi

nm_abs=""
if [ -n "${TOPBAR_SMOKE_NM:-}" ]; then
  cargo build -p topbar-services --features fake-nm --bin topbar-fake-nm
  nm_abs=$(pwd)/target/debug/topbar-fake-nm
fi

bluez_abs=""
if [ -n "${TOPBAR_SMOKE_BLUEZ:-}" ]; then
  cargo build -p topbar-services --features fake-bluez --bin topbar-fake-bluez
  bluez_abs=$(pwd)/target/debug/topbar-fake-bluez
fi

artifact_dir_abs=$(cd "$artifact_dir" && pwd)
config_abs=$(cd "$(dirname "$config")" && pwd)/$(basename "$config")
binary_abs=$(pwd)/target/debug/topbar
driver_abs=""
if [ -n "$driver" ]; then
  driver_abs=$(cd "$(dirname "$driver")" && pwd)/$(basename "$driver")
fi

# niri detaches the stdio of the processes it spawns, so the panel's own log
# (and any GTK CSS warning) is captured to a file instead of the terminal.
timeout "${timeout_s}s" dbus-run-session --config-file="$bus_config" -- niri -- sh -c '
set -eu
export SMOKE_ARTIFACTS="$3"
export SMOKE_FAKE_PLAYER="$5"
export SMOKE_FAKE_SNI="$6"
export SMOKE_TOPBAR="$1"
export SMOKE_CONFIG="$2"
export SMOKE_FAKE_NM="${10}"
export SMOKE_FAKE_BLUEZ="${12}"

# The fake package managers the updates scenarios run. Prepended, and the
# directory holds nothing else, so a `checkupdates` inside the session is the
# script this run wrote and never the real pacman-contrib one.
if [ -n "${SMOKE_PATH:-}" ]; then
  PATH="$SMOKE_PATH:$PATH"
  export PATH
fi

pulse_pid=""
if [ -n "$7" ]; then
  # A sound server of this run only: its own runtime path inside the sandbox,
  # no default configuration script, one null sink to change the volume of.
  PULSE_RUNTIME_PATH="$XDG_RUNTIME_DIR/pulse"
  export PULSE_RUNTIME_PATH
  mkdir -p "$PULSE_RUNTIME_PATH"
  # A second output only when a run asks for one. Quick Settings hides its
  # output chooser on a machine with a single sink — correctly — so a run that
  # wants to click that chevron has to be a machine with two.
  second_sink=""
  if [ "$7" = "2" ]; then
    second_sink="--load=module-null-sink sink_name=topbar_smoke_hdmi sink_properties=device.description=Smoke_HDMI"
  fi
  # shellcheck disable=SC2086
  pulseaudio --daemonize=no --exit-idle-time=-1 -n \
    --load="module-native-protocol-unix" \
    --load="module-null-sink sink_name=topbar_smoke sink_properties=device.description=Smoke_Output" \
    ${second_sink:+"$second_sink"} \
    --load="module-null-source source_name=topbar_smoke_mic" \
    --log-target=file:"$3/pulse.log" &
  pulse_pid=$!
  export PULSE_SERVER="unix:$PULSE_RUNTIME_PATH/native"
  waited=0
  while [ ! -S "$PULSE_RUNTIME_PATH/native" ] && [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
fi

power_pid=""
if [ -n "$8" ]; then
  # A power-supply tree of this run only. The panel reads its charge limit
  # from here and writes it back here; /sys is never touched.
  SMOKE_POWER_SYSFS="$XDG_RUNTIME_DIR/power_supply"
  export SMOKE_POWER_SYSFS
  mkdir -p "$SMOKE_POWER_SYSFS/BAT0" "$SMOKE_POWER_SYSFS/AC"
  printf "Battery\n" >"$SMOKE_POWER_SYSFS/BAT0/type"
  printf "Discharging\n" >"$SMOKE_POWER_SYSFS/BAT0/status"
  printf "96\n" >"$SMOKE_POWER_SYSFS/BAT0/charge_control_start_threshold"
  printf "100\n" >"$SMOKE_POWER_SYSFS/BAT0/charge_control_end_threshold"
  printf "Mains\n" >"$SMOKE_POWER_SYSFS/AC/type"
  printf "0\n" >"$SMOKE_POWER_SYSFS/AC/online"

  # shellcheck disable=SC2086
  "$8" --sysfs "$SMOKE_POWER_SYSFS" $9 >"$3/fake-power.log" 2>&1 &
  power_pid=$!
  # The panel finds both fakes on the session bus rather than on the system
  # one. Debug builds only; the packaged binary ignores these entirely.
  TOPBAR_SMOKE_POWER_BUS="$DBUS_SESSION_BUS_ADDRESS"
  TOPBAR_SMOKE_POWER_SYSFS="$SMOKE_POWER_SYSFS"
  export TOPBAR_SMOKE_POWER_BUS TOPBAR_SMOKE_POWER_SYSFS
  waited=0
  while ! grep -q "^ready$" "$3/fake-power.log" 2>/dev/null && [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
fi

bluez_pid=""
if [ -n "${12}" ]; then
  # shellcheck disable=SC2086
  "${12}" ${13} >"$3/fake-bluez.log" 2>&1 &
  bluez_pid=$!
  # The panel talks to this one instead of the system bus. Debug builds only;
  # a debug build WITHOUT it registers no pairing agent and refuses every
  # write rather than touching the real adapter.
  TOPBAR_SMOKE_BLUEZ_BUS="$DBUS_SESSION_BUS_ADDRESS"
  export TOPBAR_SMOKE_BLUEZ_BUS
  waited=0
  while ! grep -q "^ready$" "$3/fake-bluez.log" 2>/dev/null && [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
fi

nm_pid=""
if [ -n "${10}" ]; then
  # shellcheck disable=SC2086
  "${10}" ${11} >"$3/fake-nm.log" 2>&1 &
  nm_pid=$!
  # The panel talks to this one instead of the system bus. Debug builds only;
  # the packaged binary ignores the variable entirely, and a debug build
  # *without* it refuses every mutation rather than touching the real network.
  TOPBAR_SMOKE_NM_BUS="$DBUS_SESSION_BUS_ADDRESS"
  export TOPBAR_SMOKE_NM_BUS
  waited=0
  while ! grep -q "^ready$" "$3/fake-nm.log" 2>/dev/null && [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
fi

"$1" --config "$2" -v >"$3/panel.log" 2>&1 &
panel_pid=$!
# The driver reads /proc/$SMOKE_PANEL_PID/status to watch the panel grow.
export SMOKE_PANEL_PID="$panel_pid"
# Backend selection is part of the proof, not something a screenshot can show.
python3 - "$1" "$3/compositor.json" <<"PY"
import json, pathlib, subprocess, sys, time
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    result = subprocess.run([sys.argv[1], "dump", "state", "--json"], capture_output=True, text=True)
    if result.returncode == 0:
        state = json.loads(result.stdout)
        compositor = state["compositor"]
        if compositor["backend"] == "niri" and compositor["connected"]:
            pathlib.Path(sys.argv[2]).write_text(result.stdout)
            break
    time.sleep(0.1)
else:
    raise SystemExit("nested Niri panel never selected a connected niri backend")
PY
touch "$3/backend-ok"
if [ -n "$4" ]; then
  # niri detaches the stdio of what it spawns, so the driver is captured the
  # same way the panel is. A driver that failed silently was how a broken
  # screenshot went unnoticed for a whole run.
  sh "$4" >"$3/driver.log" 2>&1 || echo "smoke driver failed with status $?" >>"$3/driver.log"
else
  grim "$3/topbar.png"
fi
kill "$panel_pid" 2>/dev/null || true
wait "$panel_pid" 2>/dev/null || true
[ -n "$bluez_pid" ] && kill "$bluez_pid" 2>/dev/null
[ -n "$nm_pid" ] && kill "$nm_pid" 2>/dev/null
[ -n "$power_pid" ] && kill "$power_pid" 2>/dev/null
[ -n "$pulse_pid" ] && kill "$pulse_pid" 2>/dev/null
niri msg action quit --skip-confirmation >/dev/null 2>&1 || true
' sh "$binary_abs" "$config_abs" "$artifact_dir_abs" "$driver_abs" "$player_abs" "$sni_abs" \
  "${TOPBAR_SMOKE_PULSE:-}" "$power_abs" "${TOPBAR_SMOKE_POWER:-}" \
  "$nm_abs" "${TOPBAR_SMOKE_NM:-}" "$bluez_abs" "${TOPBAR_SMOKE_BLUEZ:-}"

test -f "$artifact_dir_abs/backend-ok" || {
  echo "nested Niri backend assertion did not complete" >&2
  exit 1
}
echo "--- panel log ---"
cat "$artifact_dir_abs/panel.log" 2>/dev/null || true
echo "-----------------"
ls -1 "$artifact_dir_abs"/*.png 2>/dev/null || {
  echo "no screenshots were taken" >&2
  exit 1
}
