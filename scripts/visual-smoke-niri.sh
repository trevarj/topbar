#!/usr/bin/env sh
# Nested Wayland visual smoke test (Niri by default, Mango on request).
# Local only: run from a visible parent Wayland session in the dev shell.
#
# EVERYTHING runs inside `dbus-run-session`, on a private bus that exists only
# for the length of the run. This is not optional: from M4 the panel takes
# `org.freedesktop.Notifications` with ReplaceExisting, and a nested panel on
# the developer's real session bus would take the desktop's notifications away
# from whatever is actually serving them.
#
# Environment:
#   TOPBAR_SMOKE_COMPOSITOR niri|mango (niri)
#   TOPBAR_SMOKE_MANGO_CONFIG optional production Mango config to preview safely
#   TOPBAR_VISUAL_CONFIG  read-only panel config source (default ./config.toml)
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
#                         directory. The real system bus is blocked, including
#                         logind; no preview acquires a host sleep inhibitor.
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
#   TOPBAR_SMOKE_PINENTRY build the dedicated `topbar-pinentry` binary. The
#                         normal panel smoke only needs `topbar`; the
#                         standalone pinentry scenario opts in explicitly.
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
#   TOPBAR_SMOKE_DATA     a directory copied into the sandboxed
#                         $XDG_DATA_HOME before the panel starts. Launcher
#                         scenarios use this for deterministic desktop-entry
#                         fixtures; the real per-user application directory is
#                         never read or modified by a smoke run.
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

# Keep transformation and its regression check together; no TOML dependency.
prepare_mango() {
  python3 - "$@" <<'PREPARE'
import copy, json, pathlib, shlex, sys, tomllib

def value(item):
    if isinstance(item, dict):
        return "{ " + ", ".join(json.dumps(k) + " = " + value(v) for k, v in item.items()) + " }"
    if isinstance(item, list):
        return "[" + ", ".join(map(value, item)) + "]"
    if isinstance(item, bool):
        return str(item).lower()
    if isinstance(item, (str, int, float)):
        return json.dumps(item, ensure_ascii=False, allow_nan=False)
    raise ValueError("unsupported TOML value: " + repr(item))

def sanitize(config, home, headset):
    config = copy.deepcopy(config)
    config.pop("appearance", None)
    def clean(table):
        if isinstance(table, dict):
            for key in list(table):
                if key.startswith("on_") or key in {"exec", "command", "update_count_command"} or key.startswith("custom-"):
                    del table[key]
                else:
                    clean(table[key])
        elif isinstance(table, list):
            for item in table:
                clean(item)
    clean(config)
    widgets = config.setdefault("widgets", {})
    for placement in ("left", "center", "right"):
        if placement in widgets:
            widgets[placement] = [name for name in widgets[placement] if not name.startswith("custom-")]
    widgets.setdefault("quick_settings", {})["brightness"] = False
    widgets.setdefault("headset", {})["command"] = str(headset)
    config.setdefault("updates", {})["flake"] = str(home / "absent-updates-flake")
    return config

def preview_config(source, theme, scale):
    lines = []
    for line in source.splitlines():
        key, sep, body = line.partition("=")
        key = key.strip()
        if not sep or key.startswith("#"):
            lines.append(line)
            continue
        if key in {"exec", "exec_once", "source", "monitor_rule", "env", "bindl"}:
            continue
        if key == "var" and body.strip().startswith("mainmod,"):
            lines.append("var=mainmod,ALT")
            continue
        if key in {"bind", "bindr", "axisbind", "mousebind"}:
            fields = [field.strip() for field in body.split(",", 3)]
            if len(fields) < 3:
                raise ValueError("invalid binding: " + line)
            modifiers, button, action = fields[:3]
            # These actions receive explicit preview bindings below.
            if action in {"switcher", "switch_keyboard_layout"}:
                continue
            if button.lower() in {"space", "grave"} and "ALT" in modifiers.upper():
                continue
            if action.startswith("spawn"):
                command = fields[3] if len(fields) == 4 else ""
                args = shlex.split(command)
                if args in [["topbar", "launcher"], ["topbar", "launcher", "toggle"], ["topbar", "popover", "show", "quick_settings"]]:
                    pass
                else:
                    continue
            elif action in {"toggle_named_scratchpad", "sleep_monitor", "wakeup_monitor", "switch_vt"}:
                continue
        lines.append(line)
    # Production shortcuts remain otherwise untouched; these replace only
    # host-facing launches and parent-grabbed modifier/capture shortcuts.
    # Nested Wayland requires adaptive sync and inherits its mode from the parent.
    lines.extend([
        "var=mainmod,ALT",
        "monitor_rule=name:^WL-[0-9]+$,vrr:1,scale:" + str(scale),
        "bind=ALT,Return,spawn,mango-preview-terminal",
        "bind=ALT,b,spawn,mango-preview-browser",
        "bind=ALT,space,switch_keyboard_layout",
        "bind=ALT+CTRL,space,switch_layout",
        "bind=ALT+CTRL,grave,toggle_named_scratchpad,none,^kitty-scratch$,mango-preview-terminal -T kitty-scratch",
        "bind=ALT+CTRL,Tab,switcher,all_next",
        "bind=ALT+CTRL+SHIFT,Tab,switcher,all_prev",
        "bind=ALT+CTRL,Print,spawn,mango-screenshot region",
        "bind=ALT+CTRL+SHIFT,Print,spawn,mango-screenshot output",
        "bind=ALT+CTRL,F12,spawn,mango-screenshot window",
    ])
    if theme is not None:
        lines.append("source=" + str(theme))
    return "\n".join(lines) + "\n"

if sys.argv[1:] == ["--self-check"]:
    original = {
        "appearance": {"theme_command": ["/host/theme"]},
        "theme": {"accent": "#abcdef"},
        "widgets": {"left": ["workspaces", "custom-host"], "custom-host": {"exec": "/host/mutate"},
                    "clock": {"on_click": "/host/callback"}, "headset": {"command": "/host/headset"}},
        "updates": {"update_count_command": "/host/update"},
        "advanced": {"mango": {"persistent_tags": [1, 2, 3], "keyboard_layouts": ["English (US)", "Russian"]}},
    }
    safe = sanitize(original, pathlib.Path("/private/home"), pathlib.Path("/private/no-headset"))
    encoded = "\n".join(json.dumps(k) + " = " + value(v) for k, v in safe.items())
    assert tomllib.loads(encoded) == safe
    assert original["appearance"]["theme_command"] == ["/host/theme"]
    assert "/host/" not in encoded and safe["widgets"]["left"] == ["workspaces"]
    assert not safe["widgets"]["quick_settings"]["brightness"]
    assert safe["advanced"] == original["advanced"]
    fixture = """var=mainmod,SUPER
exec_once=svc session-start
monitor_rule=name:^DP-5$,scale:1.5
source=/host/theme
bindl=NONE,XF86AudioRaiseVolume,spawn,wpctl set-volume @DEFAULT_SINK@ 5%+
bind=$mainmod,p,spawn,theme-switch
bind=$mainmod+ALT,l,spawn_shell,pidof hyprlock || hyprlock --config /host/lock
bind=$mainmod,e,spawn,topbar launcher toggle
bind=$mainmod+SHIFT,p,spawn,wlopm --off '*'
bind=$mainmod,Print,spawn,mango-screenshot region
bind=NONE,Print,spawn,mango-screenshot region
bind=CTRL,Print,spawn,mango-screenshot output
bind=ALT,Print,spawn,mango-screenshot window
bind=$mainmod,h,focusdir,left
bind=ALT,Tab,switcher,all_next
bind=$mainmod,Tab,switcher,all_next
bind=$mainmod+SHIFT,Tab,switcher,all_prev
bind=$mainmod+ALT,space,switch_layout
"""
    preview = preview_config(fixture, pathlib.Path("/bundle/mango.conf"), 1)
    assert "svc" not in preview and "DP-5" not in preview and "/host" not in preview
    assert "theme-switch" not in preview and "wpctl" not in preview
    assert "bind=$mainmod,h,focusdir,left" in preview and "mango-screenshot region" in preview
    assert "bind=ALT+CTRL,space,switch_layout" in preview
    assert "bind=$mainmod+ALT,space" not in preview
    assert "hyprlock" not in preview and "wlopm" not in preview
    assert "topbar launcher toggle" in preview and "toggle_named_scratchpad,none,^kitty-scratch$" in preview
    assert "bind=ALT,Tab," not in preview and "bind=$mainmod,Tab," not in preview
    assert "bind=$mainmod+SHIFT,Tab," not in preview
    assert "bind=NONE,Print," not in preview and "bind=CTRL,Print," not in preview and "bind=ALT,Print," not in preview
    assert preview.count(",switcher,all_next") == 1 and preview.count(",switcher,all_prev") == 1
    assert "bind=ALT+CTRL,Tab,switcher,all_next" in preview
    assert "bind=ALT+CTRL+SHIFT,Tab,switcher,all_prev" in preview
    for modifiers, button, mode in [("ALT+CTRL", "Print", "region"), ("ALT+CTRL+SHIFT", "Print", "output"), ("ALT+CTRL", "F12", "window")]:
        assert f"bind={modifiers},{button},spawn,mango-screenshot {mode}" in preview
        assert preview.count("mango-screenshot " + mode) == 1
    print("preview config isolation self-check passed")
    raise SystemExit

panel_source, panel_target, mango_source, mango_target, private_home, scale = sys.argv[1:]
home = pathlib.Path(private_home)
source_path = pathlib.Path(panel_source)
safe = sanitize(tomllib.loads(source_path.read_text()), home, home.parent / "no-headset")
pathlib.Path(panel_target).write_text("\n".join(json.dumps(k) + " = " + value(v) for k, v in safe.items()) + "\n")
theme = source_path.parent / "mango.conf"
if mango_source and not theme.is_file():
    raise SystemExit("TOPBAR_VISUAL_CONFIG must have its generated mango.conf beside it")
if mango_source:
    source = pathlib.Path(mango_source).read_text()
else:
    source = """xkb_rules_layout=us,ru
xkb_rules_options=compose:ralt,ctrl:nocaps
cursor_theme=Adwaita
cursor_size=24
tag_num=9
tag_rule=id:1,layout_name:scroller,no_hide:1
tag_rule=id:2,layout_name:scroller,no_hide:1
tag_rule=id:3,layout_name:scroller,no_hide:1
tag_rule=id:4,layout_name:scroller
tag_rule=id:5,layout_name:scroller
tag_rule=id:6,layout_name:scroller
tag_rule=id:7,layout_name:scroller
tag_rule=id:8,layout_name:scroller
tag_rule=id:9,layout_name:scroller
circle_layout=scroller,fair,dwindle
scroller_default_proportion=0.5
scroller_proportion_preset=0.33333,0.5,0.66667
scratchpad_width_ratio=0.8
scratchpad_height_ratio=0.9
window_rule=title:^kitty-scratch$,is_named_scratchpad:1,width:0.8,height:0.9
bind=ALT,q,killclient
bind=ALT,o,toggleoverview
"""
    safe.setdefault("advanced", {}).setdefault("mango", {
        "persistent_tags": [1, 2, 3], "keyboard_layouts": ["English (US)", "Russian"],
    })
    pathlib.Path(panel_target).write_text("\n".join(json.dumps(k) + " = " + value(v) for k, v in safe.items()) + "\n")
pathlib.Path(mango_target).write_text(preview_config(source, theme if theme.is_file() else None, float(scale)))
PREPARE
}

if [ "${1:-}" = --self-check ]; then
  prepare_mango --self-check
  exit
fi

compositor="${TOPBAR_SMOKE_COMPOSITOR:-niri}"
case "$compositor" in
  niri|mango) ;;
  *) echo "TOPBAR_SMOKE_COMPOSITOR must be niri or mango" >&2; exit 1 ;;
esac
export TOPBAR_SMOKE_COMPOSITOR="$compositor"
if [ "$compositor" = mango ] && [ "${TOPBAR_SMOKE_POWER:-}" != 1 ]; then
  echo "Mango preview requires TOPBAR_SMOKE_POWER=1 (private power bus and sysfs)" >&2
  exit 1
fi

# Resolve before boxing runtime. Only the nested backend gets this socket;
# descendants receive the nested compositor's own WAYLAND_DISPLAY.
case "${WAYLAND_DISPLAY:-}" in
  /*) parent_wayland="$WAYLAND_DISPLAY" ;;
  "") echo "a real parent WAYLAND_DISPLAY is required" >&2; exit 1 ;;
  *) parent_wayland="${XDG_RUNTIME_DIR:?parent XDG_RUNTIME_DIR is required}/$WAYLAND_DISPLAY" ;;
esac
if [ ! -S "$parent_wayland" ]; then
  echo "parent Wayland display is not a socket: $parent_wayland" >&2
  exit 1
fi
parent_wayland=$(realpath "$parent_wayland")
export WAYLAND_DISPLAY="$parent_wayland"
unset WAYLAND_SOCKET DISPLAY XAUTHORITY NIRI_SOCKET MANGO_INSTANCE_SIGNATURE
unset SWAYSOCK HYPRLAND_INSTANCE_SIGNATURE
unset PULSE_SERVER PULSE_REMOTE PULSE_RUNTIME_PATH PULSE_CLIENTCONFIG PULSE_COOKIE
unset PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR
unset DBUS_STARTER_ADDRESS DBUS_STARTER_BUS_TYPE TOPBAR_SMOKE_POWER_BUS TOPBAR_SMOKE_POWER_SYSFS
unset TOPBAR_SMOKE_NM_BUS TOPBAR_SMOKE_BLUEZ_BUS
export XDG_CURRENT_DESKTOP="$compositor" XDG_SESSION_DESKTOP="$compositor" XDG_SESSION_TYPE=wayland
export WLR_BACKENDS=wayland
export XCURSOR_THEME=Adwaita XCURSOR_SIZE=24
artifact_dir="${1:-target/visual-smoke}"
config="${TOPBAR_VISUAL_CONFIG:-config.toml}"
driver="${TOPBAR_SMOKE_DRIVER:-}"
timeout_s="${TOPBAR_SMOKE_TIMEOUT:-30}"
mkdir -p "$artifact_dir"
rm -f "$artifact_dir/backend-ok" "$artifact_dir/driver-failed"

bus_config=$(pwd)/scripts/smoke-session.conf

# Capture only the selected image before boxing the host's cache and HOME.
SMOKE_WALLPAPER_SOURCE=""
if [ "$compositor" = mango ]; then
  wallpaper_pointer="${XDG_CACHE_HOME:-$HOME/.cache}/wallpaper/current.png"
  if wallpaper_source=$(realpath "$wallpaper_pointer" 2>/dev/null) &&
      [ -f "$wallpaper_source" ] && [ -r "$wallpaper_source" ]; then
    SMOKE_WALLPAPER_SOURCE="$wallpaper_source"
  else
    echo "Mango preview: no readable current wallpaper at $wallpaper_pointer; keeping plain background" >&2
  fi
fi
export SMOKE_WALLPAPER_SOURCE

# Sandbox every XDG write path. The panel migrates/creates state dirs and will
# grow cache use over time; a smoke run must never touch the developer's real
# ~/.local/state (this bit us once: the state-dir migration renamed the live
# v1 panel's state directory). Config is passed with --config explicitly, but
# XDG_CONFIG_HOME is boxed too so the legacy-path fallback can't find a real
# user config and add warning noise to panel.log.
xdg_box=$(mktemp -d "${TMPDIR:-/tmp}/topbar-smoke-xdg.XXXXXX")
# The supervisor drains descendants while the private bus and runtime still
# exist. Signals reach it, not dbus-run-session, so bus teardown happens last.
session_pid=""
cleanup_outer() {
  if [ -n "$session_pid" ]; then
    if [ -f "$xdg_box/supervisor.pid" ]; then
      read -r supervisor_pid <"$xdg_box/supervisor.pid"
      kill -TERM "$supervisor_pid" 2>/dev/null || true
    else
      kill -TERM "$session_pid" 2>/dev/null || true
    fi
    wait "$session_pid" 2>/dev/null || true
  fi
  if [ -d "$xdg_box/home/Pictures/Screenshots" ]; then
    mkdir -p "$artifact_dir/screenshots"
    cp -a "$xdg_box/home/Pictures/Screenshots"/. "$artifact_dir/screenshots"/
  fi
  rm -rf "$xdg_box"
}
trap cleanup_outer EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export XDG_STATE_HOME="$xdg_box/state"
export XDG_CACHE_HOME="$xdg_box/cache"
export XDG_CONFIG_HOME="$xdg_box/config"
export XDG_DATA_HOME="$xdg_box/data"
mkdir -p "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME/niri" "$XDG_CONFIG_HOME/mango" "$XDG_DATA_HOME"

# GIO discovers desktop entries through XDG_DATA_HOME.  Copy an explicit
# fixture tree when a launcher scenario supplies one, so it cannot pick up a
# developer's personal entries or write beside them.
if [ -n "${TOPBAR_SMOKE_DATA:-}" ]; then
  if [ ! -d "$TOPBAR_SMOKE_DATA" ]; then
    echo "TOPBAR_SMOKE_DATA is not a directory: $TOPBAR_SMOKE_DATA" >&2
    exit 1
  fi
  cp -a "$TOPBAR_SMOKE_DATA"/. "$XDG_DATA_HOME"/
fi

# Never link the parent runtime directory or either of its manager sockets.
export XDG_RUNTIME_DIR="$xdg_box/run"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/no-system-bus"
export PULSE_SERVER="unix:$XDG_RUNTIME_DIR/no-pulse-server"
export PIPEWIRE_REMOTE="$XDG_RUNTIME_DIR/no-pipewire-server"
export PIPEWIRE_RUNTIME_DIR="$XDG_RUNTIME_DIR"

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

// Mirror the deployed backdrop rule so visual captures exercise the same
// full-output blur as the live launcher and chooser.
layer-rule {
    match namespace=r#"^topbar-(launcher|chooser)-backdrop$"#

    background-effect {
        xray false
        blur true
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

for tool in "$compositor" grim cargo dbus-run-session python3 setsid realpath; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing required tool: $tool" >&2
    exit 1
  fi
done
if [ "$compositor" = mango ]; then
  for tool in mmsg kitty brave jq wl-copy slurp satty; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "missing required Mango preview tool: $tool" >&2
      exit 1
    fi
  done
  if [ -n "$SMOKE_WALLPAPER_SOURCE" ]; then
    for tool in awww awww-daemon; do
      if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required Mango wallpaper tool: $tool" >&2
        exit 1
      fi
    done
  fi
  : "${XCURSOR_PATH:?export the built Adwaita share/icons path (the dev shell does this)}"
fi

cargo build -p topbar

if [ -n "${TOPBAR_SMOKE_PINENTRY:-}" ]; then
  cargo build -p topbar --bin topbar-pinentry
fi

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
if [ "$compositor" = mango ]; then
  mango_source="${TOPBAR_SMOKE_MANGO_CONFIG:-}"
  if [ -n "$mango_source" ]; then
    mango_source=$(realpath "$mango_source")
    dotfiles_root=$(dirname "$(dirname "$(dirname "$(dirname "$mango_source")")")")
  else
    dotfiles_root="$(pwd)/../dotfiles"
  fi
  helper="$dotfiles_root/bin/.local/bin/mango-screenshot"
  if [ ! -x "$helper" ]; then
    echo "missing executable source screenshot helper: $helper" >&2
    exit 1
  fi
  mkdir -p "$xdg_box/bin"
  cp "$helper" "$xdg_box/bin/mango-screenshot"
  ln -s "$binary_abs" "$xdg_box/bin/topbar"
  export SMOKE_KITTY="$(command -v kitty)" SMOKE_BRAVE="$(command -v brave)"
  cat >"$xdg_box/bin/mango-preview-terminal" <<'TERMINAL'
#!/usr/bin/env sh
exec "$SMOKE_KITTY" --config NONE "$@"
TERMINAL
  cat >"$xdg_box/bin/mango-preview-browser" <<'BROWSER'
#!/usr/bin/env sh
exec "$SMOKE_BRAVE" --user-data-dir="$HOME/.cache/brave-preview" --no-first-run --disable-sync "$@"
BROWSER
  chmod +x "$xdg_box/bin/mango-preview-terminal" "$xdg_box/bin/mango-preview-browser"
  export PATH="$xdg_box/bin:$PATH"
  prepare_mango "$config_abs" "$xdg_box/panel.toml" "$mango_source" \
    "$XDG_CONFIG_HOME/mango/config.conf" "$xdg_box/home" "${TOPBAR_SMOKE_SCALE:-0.75}"
  config_abs="$xdg_box/panel.toml"
  mkdir -p "$XDG_CONFIG_HOME/topbar"
  cp "$config_abs" "$XDG_CONFIG_HOME/topbar/config.toml"
  cp "$config_abs" "$artifact_dir_abs/preview-panel.toml"
  cp "$XDG_CONFIG_HOME/mango/config.conf" "$artifact_dir_abs/preview-mango.conf"
  mango -c "$XDG_CONFIG_HOME/mango/config.conf" -p >"$artifact_dir_abs/mango-config.log" 2>&1
fi

# Build first, then give every compositor descendant a disposable home.
export HOME="$xdg_box/home"
export ZDOTDIR="$HOME"
mkdir -p "$HOME"
if [ "$compositor" = mango ]; then
  "$binary_abs" --config "$config_abs" --check-config --strict
fi

cat >"$xdg_box/inner.sh" <<'INNER'
#!/usr/bin/env sh
set -eu
export SMOKE_ARTIFACTS="$3"
export SMOKE_FAKE_PLAYER="$5"
export SMOKE_FAKE_SNI="$6"
export SMOKE_TOPBAR="$1"
export SMOKE_CONFIG="$2"
printf "COMPOSITOR=%s\nXDG_CURRENT_DESKTOP=%s\nNIRI_SOCKET=%s\nMANGO_INSTANCE_SIGNATURE=%s\nWAYLAND_DISPLAY=%s\n" \
  "$TOPBAR_SMOKE_COMPOSITOR" "$XDG_CURRENT_DESKTOP" "${NIRI_SOCKET:-}" \
  "${MANGO_INSTANCE_SIGNATURE:-}" "${WAYLAND_DISPLAY:-}" >"$3/compositor-environment.txt"
cp "$3/compositor-environment.txt" "$3/$TOPBAR_SMOKE_COMPOSITOR-environment.txt"
case "$TOPBAR_SMOKE_COMPOSITOR" in
  niri) instance="${NIRI_SOCKET:-}" ;;
  mango) instance="${MANGO_INSTANCE_SIGNATURE:-}" ;;
esac
if [ -z "$instance" ] || [ ! -S "$instance" ]; then
  echo "nested $TOPBAR_SMOKE_COMPOSITOR did not provide a live IPC socket" >&2
  exit 1
fi
export SMOKE_FAKE_NM="${10}"
export SMOKE_FAKE_BLUEZ="${12}"

# The fake package managers the updates scenarios run. Prepended, and the
# directory holds nothing else, so a `checkupdates` inside the session is the
# script this run wrote and never the real pacman-contrib one.
if [ -n "${SMOKE_PATH:-}" ]; then
  PATH="$SMOKE_PATH:$PATH"
  export PATH
fi

awww_pid=""
cleanup() {
  for child in "${panel_pid:-}" "$awww_pid" "${bluez_pid:-}" "${nm_pid:-}" "${power_pid:-}" "${pulse_pid:-}"; do
    if [ -n "$child" ]; then
      kill "$child" 2>/dev/null || true
      wait "$child" 2>/dev/null || true
    fi
  done
  case "$TOPBAR_SMOKE_COMPOSITOR" in
    niri) niri msg action quit --skip-confirmation >/dev/null 2>&1 || true ;;
    mango) mmsg dispatch quit >/dev/null 2>&1 || true ;;
  esac
}
trap cleanup EXIT
trap "exit 130" INT
trap "exit 143" TERM

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
  test -S "$PULSE_RUNTIME_PATH/native" || {
    echo "private PulseAudio socket never became ready" >&2
    exit 1
  }
  pactl info >/dev/null
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

  # `1` enables default fixtures; scenario flag strings retain their argv.
  power_args=$9
  if [ "$power_args" = 1 ]; then power_args=""; fi
  # shellcheck disable=SC2086
  "$8" --sysfs "$SMOKE_POWER_SYSFS" $power_args >"$3/fake-power.log" 2>&1 &
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
  grep -q "^ready$" "$3/fake-power.log" || {
    echo "private fake power service never became ready" >&2
    exit 1
  }
fi
if [ "$TOPBAR_SMOKE_COMPOSITOR" = mango ]; then
  test -n "${TOPBAR_SMOKE_POWER_BUS:-}"
  test "$TOPBAR_SMOKE_POWER_SYSFS" = "$XDG_RUNTIME_DIR/power_supply"
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

if [ "$TOPBAR_SMOKE_COMPOSITOR" = mango ] && [ -n "$SMOKE_WALLPAPER_SOURCE" ]; then
  # Native clients inherit only this preview's runtime, cache and HOME.
  awww-daemon -n normal --no-cache >"$3/awww.log" 2>&1 &
  awww_pid=$!
  waited=0
  while ! awww query -n normal >/dev/null 2>&1 && [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
  awww query -n normal >/dev/null 2>&1 || {
    echo "private Mango wallpaper daemon never became ready; see $3/awww.log" >&2
    exit 1
  }
  awww img -n normal --resize crop --transition-type none "$SMOKE_WALLPAPER_SOURCE"
fi

"$1" --config "$2" -v >"$3/panel.log" 2>&1 &
panel_pid=$!
# The driver reads /proc/$SMOKE_PANEL_PID/status to watch the panel grow.
export SMOKE_PANEL_PID="$panel_pid"
# Backend selection is part of the proof, not something a screenshot can show.
python3 - "$1" "$3/compositor.json" <<"PY"
import json, os, pathlib, subprocess, sys, time
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    result = subprocess.run([sys.argv[1], "dump", "state", "--json"], capture_output=True, text=True)
    if result.returncode == 0:
        state = json.loads(result.stdout)
        compositor = state["compositor"]
        if (compositor["backend"] == os.environ["TOPBAR_SMOKE_COMPOSITOR"]
                and compositor["connected"] and compositor["focused_output"]):
            pathlib.Path(sys.argv[2]).write_text(result.stdout)
            break
    time.sleep(0.1)
else:
    raise SystemExit("nested " + os.environ["TOPBAR_SMOKE_COMPOSITOR"] + " preview never became connected with a focused output")
PY
touch "$3/backend-ok"
if [ -n "$4" ]; then
  # Both compositors detach child stdio; preserve driver failures explicitly.
  sh "$4" >"$3/driver.log" 2>&1 || {
    status=$?
    echo "smoke driver failed with status $status" >>"$3/driver.log"
    touch "$3/driver-failed"
    exit "$status"
  }
else
  grim "$3/topbar.png"
fi
cleanup
trap - EXIT
INNER

# Mango's session imports/cleanup are asynchronous, including in nested mode.
# Linux subreaping lets this one supervisor drain only this run's descendants
# before dbus-run-session removes the bus; setsid shields that bus from Ctrl+C.
cat >"$xdg_box/supervisor.py" <<'SUPERVISOR'
import ctypes, os, pathlib, shlex, signal, subprocess, sys, time

libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
    raise OSError(ctypes.get_errno(), "could not enable preview child subreaping")
kind, executable, config, runner, seconds, pidfile, *args = sys.argv[1:]
pathlib.Path(pidfile).write_text(str(os.getpid()) + "\n")
stopping = False
def stop(signum, frame):
    global stopping
    stopping = True
signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
seconds = float(seconds)
if seconds <= 0:
    raise SystemExit("TOPBAR_SMOKE_TIMEOUT must be positive")
command = [executable, "--", "sh", runner, *args] if kind == "niri" else [
    executable, "-d", "-c", config, "-s", shlex.join(["sh", runner, *args]),
]
process = subprocess.Popen(command)
deadline = time.monotonic() + seconds
timed_out = False
try:
    while process.poll() is None:
        if stopping or time.monotonic() >= deadline:
            timed_out = not stopping
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            break
        time.sleep(0.1)
finally:
    # All PIDs read here are this supervisor's children, including adopted
    # setsid descendants. Never match process names or touch the host session.
    children_file = pathlib.Path(f"/proc/{os.getpid()}/task/{os.getpid()}/children")
    deadline = time.monotonic() + 10
    while True:
        try:
            while os.waitpid(-1, os.WNOHANG)[0]:
                pass
        except ChildProcessError:
            break
        sig = signal.SIGTERM if time.monotonic() < deadline else signal.SIGKILL
        for child in children_file.read_text().split():
            try:
                os.kill(int(child), sig)
            except ProcessLookupError:
                pass
        time.sleep(0.05)
raise SystemExit(124 if timed_out else 0 if stopping else process.returncode)
SUPERVISOR

setsid --wait dbus-run-session --config-file="$bus_config" -- \
  python3 "$xdg_box/supervisor.py" "$compositor" "$(command -v "$compositor")" \
  "$XDG_CONFIG_HOME/mango/config.conf" "$xdg_box/inner.sh" "$timeout_s" "$xdg_box/supervisor.pid" \
  "$binary_abs" "$config_abs" "$artifact_dir_abs" "$driver_abs" "$player_abs" "$sni_abs" \
  "${TOPBAR_SMOKE_PULSE:-}" "$power_abs" "${TOPBAR_SMOKE_POWER:-}" \
  "$nm_abs" "${TOPBAR_SMOKE_NM:-}" "$bluez_abs" "${TOPBAR_SMOKE_BLUEZ:-}" \
  >"$artifact_dir_abs/compositor.log" 2>&1 &
session_pid=$!
status=0
wait "$session_pid" || status=$?
session_pid=""
if [ "$status" -ne 0 ]; then
  echo "nested $compositor exited with status $status; see $artifact_dir_abs/compositor.log" >&2
  exit "$status"
fi
test ! -f "$artifact_dir_abs/driver-failed"

test -f "$artifact_dir_abs/backend-ok" || {
  echo "nested $compositor backend assertion did not complete" >&2
  exit 1
}
echo "--- panel log ---"
cat "$artifact_dir_abs/panel.log" 2>/dev/null || true
echo "-----------------"
ls -1 "$artifact_dir_abs"/*.png 2>/dev/null || {
  echo "no screenshots were taken" >&2
  exit 1
}
