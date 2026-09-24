#!/usr/bin/env sh
# Hyprland 0.56.2 Lua-session smoke. Never launch on the production bus or DRM seat.
# Run from the dev shell; all captured windows belong to this nested compositor.
set -eu
for tool in Hyprland hyprctl grim cargo python3 dbus-run-session timeout wlrctl gtk4-demo magick notify-send; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done
: "${XDG_RUNTIME_DIR:?a host Wayland session is required}"
: "${WAYLAND_DISPLAY:?a host Wayland session is required}"
repo=$(pwd)
artifacts=${1:-target/visual-smoke-hyprland}
mkdir -p "$artifacts"
artifacts=$(realpath "$artifacts")
cargo build -p topbar
# Keep the runtime path short: Hyprland appends a long instance signature before
# its Unix socket names, which must fit the kernel's pathname limit.
box=$(mktemp -d /tmp/h.XXXXXX)
trap 'rm -rf "$box"' EXIT INT TERM
host_runtime=$XDG_RUNTIME_DIR
host_display=$WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="$box/run" XDG_CONFIG_HOME="$box/config"
export XDG_STATE_HOME="$box/state" XDG_CACHE_HOME="$box/cache" XDG_DATA_HOME="$box/data"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/hypr" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
case "$host_display" in
  /*) host_socket=$host_display ;;
  *) host_socket="$host_runtime/$host_display" ;;
esac
test -S "$host_socket" || { echo "host Wayland socket is absent" >&2; exit 1; }
ln -s "$host_socket" "$XDG_RUNTIME_DIR/host-wayland"
export WAYLAND_DISPLAY=host-wayland XDG_CURRENT_DESKTOP=Hyprland GDK_BACKEND=wayland
unset NIRI_SOCKET HYPRLAND_INSTANCE_SIGNATURE NOTIFY_SOCKET PULSE_SERVER PULSE_RUNTIME_PATH PIPEWIRE_REMOTE
# An unavailable libseat backend forbids acquiring the live DRM/logind seat;
# Aquamarine then uses its Wayland fallback. No systemd environment import/notify.
export LIBSEAT_BACKEND=topbar-smoke-no-seat HYPRLAND_NO_SD_VARS=1 HYPRLAND_NO_SD_NOTIFY=1
export SMOKE_TOPBAR="$repo/target/debug/topbar" SMOKE_ARTIFACTS="$artifacts"
export SMOKE_ENTRY="$box/session.sh" SMOKE_DRIVER="$repo/scripts/smoke-hyprland.py"
cat >"$SMOKE_ENTRY" <<'SH'
#!/usr/bin/env sh
set -eu
# Even read-only system services see this run's bus, never the real system bus.
export DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS"
status=0
python3 "$SMOKE_DRIVER" >"$SMOKE_ARTIFACTS/driver.log" 2>&1 || status=$?
printf '%s\n' "$status" >"$SMOKE_ARTIFACTS/status"
hyprctl dispatch 'hl.dsp.exit()' >/dev/null 2>&1 || true
exit "$status"
SH
cat >"$XDG_CONFIG_HOME/hypr/hyprland.lua" <<'LUA'
hl.config({input={kb_layout="us,ru"}, decoration={blur={enabled=true,size=6,passes=2}}, animations={enabled=false}})
hl.monitor({output="",mode="preferred",position="auto",scale=1})
for i=1,3 do hl.workspace_rule({workspace=tostring(i),persistent=true}) end
hl.layer_rule({match={namespace="^topbar.*$"},no_anim=true})
hl.on("hyprland.start",function() hl.exec_cmd("sh "..string.format("%q",os.getenv("SMOKE_ENTRY"))) end)
LUA
rm -f "$artifacts/status"
timeout "${TOPBAR_SMOKE_TIMEOUT:-240}s" dbus-run-session --config-file="$repo/scripts/smoke-session.conf" -- \
  Hyprland --config "$XDG_CONFIG_HOME/hypr/hyprland.lua" >"$artifacts/compositor.log" 2>&1
if [ "$(cat "$artifacts/status" 2>/dev/null)" != 0 ]; then
  cat "$artifacts/driver.log" >&2
  echo "Hyprland smoke failed; evidence: $artifacts" >&2
  exit 1
fi
cat "$artifacts/driver.log"
echo "Hyprland smoke evidence: $artifacts"
