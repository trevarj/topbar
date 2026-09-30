#!/usr/bin/env sh
# Run from the repo root: nix develop -c ./scripts/playground.sh [artifact-dir]
# Explore the nested panel interactively; press Ctrl+C in this terminal to stop.
# The harness uses a private D-Bus session, so real desktop players are absent.
set -eu

if [ -z "${SMOKE_PANEL_PID:-}" ] || [ -z "${SMOKE_ARTIFACTS:-}" ]; then
  export TOPBAR_SMOKE_OPEN=clock
  export TOPBAR_SMOKE_PLAYERS=1
  export TOPBAR_SMOKE_TIMEOUT="${TOPBAR_SMOKE_TIMEOUT:-3600}"
  export TOPBAR_VISUAL_CONFIG="${TOPBAR_VISUAL_CONFIG:-crates/topbar-core/tests/fixtures/live-config.toml}"
  export TOPBAR_SMOKE_DRIVER="$0"
  exec ./scripts/visual-smoke-niri.sh "${1:-target/visual-smoke/playground}"
fi

art=$(cd "$SMOKE_ARTIFACTS" && pwd)
magick -size 512x512 'gradient:#355c7d-#c06c84' "$art/cover.png"
"$SMOKE_FAKE_PLAYER" --name playground --identity "Aurora Player" \
  --desktop-entry org.gnome.Music \
  --title "Windowlicker: A Very Long Live Recording From The Evening Session With An Extended Introduction And A Slow Ending That Must Wrap Across The Media Card Rather Than Widen The Control Panel Beyond Its Normal Size" \
  --artist "Aphex Twin" --album "Windowlicker" \
  --status Paused --art "file://$art/cover.png" \
  --length 221000000 --position 72000000 >"$art/player.log" 2>&1 &
player_pid=$!
cleanup() {
  kill "$player_pid" 2>/dev/null || true
  wait "$player_pid" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# The fake player stays alive while you explore; only our own child is cleaned up.
wait "$player_pid"
