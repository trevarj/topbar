#!/usr/bin/env sh
# The notifications module under a real pointer, at the scale a real display
# runs at.
#
#   nix develop -c ./scripts/smoke-notifications.sh
#
# Notifications are the one part of the panel a test can drive exactly the way
# a desktop does: `notify-send` on the run's private bus, where the panel owns
# `org.freedesktop.Notifications` and nothing else can hear it. So this run
# sends real notifications and then *uses* what they produce — every control in
# the history column and on every banner is located from the panel's own dump
# and pressed by a synthetic pointer.
#
# It runs at TOPBAR_SMOKE_SCALE=1.0 on purpose. The other drivers run the
# nested output at 0.75, where three device pixels stand in for four logical
# ones and the resampling averages away precisely the fringing, the half-pixel
# baselines and the icon misalignment a refinement pass exists to find.
#
#   1  history   the column: empty, grouped, expanded, closed, cleared, DND,
#                and the unread dot the bar wears until the column is opened
#   2  banners   arrival, hover-pause, an action pill, close, the stack,
#                critical
#   3  edges     long names, huge bodies, markup, sixty entries, a replacement
#                landing in an open group
#   4  sounds    native choices and private null-sink PCM signal/quiet policy
#   5  sounds-restored  the saved choice and history in a fresh panel process
#
# Screenshots and captured output land in target/visual-smoke/notifications/.
set -eu

artifact_root="${1:-target/visual-smoke/notifications}"
mkdir -p "$artifact_root"
artifact_root=$(cd "$artifact_root" && pwd)

for tool in magick niri grim cargo timeout dbus-run-session wlrctl wtype notify-send python3 paplay parec pactl pulseaudio; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing required tool: $tool" >&2
    exit 1
  fi
done

repo=$(pwd)

# The live config with the crypto script taken out: it names a path in the
# developer's dotfiles that a sandboxed run has no business executing, and a
# widget that fails to start is noise in a log this run reads.
config="$artifact_root/config.toml"
sed -e 's|^exec = .*|exec = "/bin/echo BTC"|' \
  "$repo/crates/topbar-core/tests/fixtures/live-config.toml" >"$config"
grep -q "^control_panel = true" "$config" || {
  echo "smoke-notifications: the clock has no control panel in this config" >&2
  exit 1
}

status=0

run() {
  scenario=$1
  mkdir -p "$artifact_root/$scenario"

  # An icon file on disk for the image-path scenario. Written per run rather
  # than committed: a fixture the run makes itself cannot go stale, and this
  # one is eight pixels of red.
  magick -size 64x64 xc:'#e01b24' "$artifact_root/$scenario/icon.png" 2>/dev/null || true

  state_seed=""
  if [ "$scenario" = sounds-restored ]; then
    state_seed="$artifact_root/sounds/restart-state.json"
    test -s "$state_seed" || {
      echo "smoke-notifications: run sounds before sounds-restored" >&2
      status=1
      return
    }
  fi
  scenario_timeout=400
  case "$scenario" in
    sounds) scenario_timeout=900 ;;
  esac
  echo "smoke-notifications: $scenario"
  RUST_LOG="info,topbar=debug,topbar_services::notifications=debug" \
  SMOKE_NOTIFICATIONS_SCENARIO="$scenario" \
  TOPBAR_SMOKE_STATE="$state_seed" \
  TOPBAR_SMOKE_SCALE=1.0 \
  PULSE_LATENCY_MSEC=30 \
  TOPBAR_SMOKE_PULSE=1 \
  TOPBAR_SMOKE_POWER="--active balanced --percent 62 --state 2 --time-to-empty 8100" \
  TOPBAR_SMOKE_TIMEOUT="${TOPBAR_SMOKE_TIMEOUT:-$scenario_timeout}" \
  TOPBAR_SMOKE_DRIVER="$repo/scripts/smoke-notifications-shot.sh" \
  TOPBAR_VISUAL_CONFIG="$config" \
    "$repo/scripts/visual-smoke-niri.sh" "$artifact_root/$scenario" \
    >"$artifact_root/$scenario.log" 2>&1 ||
    echo "smoke-notifications: $scenario exited non-zero; see $artifact_root/$scenario.log" >&2
  mv "$artifact_root/$scenario/panel.log" "$artifact_root/$scenario-panel.log" 2>/dev/null || true
}

for scenario in ${SMOKE_NOTIFICATIONS_ONLY:-history banners edges sounds sounds-restored}; do
  run "$scenario"
done

echo "--- screenshots ---"
find "$artifact_root" -name '*.png' | sort
echo "--- what the driver saw ---"
for file in "$artifact_root"/*/driver.log; do
  [ -e "$file" ] || continue
  echo "=== $file"
  cat "$file"
  grep -q "result: PASS" "$file" || status=1
done
# The panel logs its level in colour, so the escapes come out before the level
# can be matched on.
echo "--- warnings and errors ---"
sed 's/\x1b\[[0-9;]*m//g' "$artifact_root"/*-panel.log 2>/dev/null |
  grep -E "( WARN | ERROR |Gtk-WARNING|Gtk-CRITICAL)" || echo "none"

exit "$status"
