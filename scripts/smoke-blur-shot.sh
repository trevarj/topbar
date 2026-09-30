#!/usr/bin/env sh
# Photographs every surface the panel asks the compositor to blur, over a
# backdrop busy enough for blur to be visible. Driven by scripts/smoke-blur.sh,
# which runs it three times — with blur on, with it switched off in the
# configuration, and with it switched off by the environment — and then
# compares the frames.
#
# The backdrop is the point. Blurring a flat colour produces the same flat
# colour, so a screenshot of a popover over the nested session's plain
# background proves nothing whatever the compositor did. gtk4-demo is
# a wall of text, borders and controls, and a blurred one is unmistakable.
set -eu

art="$SMOKE_ARTIFACTS"
. "$(dirname "$0")/smoke-shot.sh"

# --- the backdrop ----------------------------------------------------------
niri msg action spawn -- gtk4-demo >/dev/null 2>&1 || true
waited=0
while [ "$waited" -lt 15 ]; do
  niri msg windows 2>/dev/null | grep -q "Demo" && break
  sleep 1
  waited=$((waited + 1))
done
if ! niri msg windows 2>/dev/null | grep -q "Demo"; then
  echo "smoke-blur: no backdrop window; the comparison would be meaningless" >&2
  exit 1
fi
# Give it a moment to finish its own first paint before anything is measured.
sleep 2
shot backdrop

# --- the control panel, which is the surface blur is for --------------------
"$SMOKE_TOPBAR" popover show clock >>"$art/ipc.log" 2>&1 || true
shot panel topbar-popover

# The close is where the fade-out caveat lives: the region has to come off
# before the animation that fades the surface starts, or the compositor keeps
# blurring a rectangle nothing is drawn on any more.
"$SMOKE_TOPBAR" popover hide >>"$art/ipc.log" 2>&1 || true
sleep 2

# --- separated banner backgrounds ------------------------------------------
#
# Two minutes, not the four seconds a banner normally gets. `grim` hands back
# the last frame the nested session *presented*, which under the host
# compositor's throttling can be seconds old — and a banner that has already
# left by the time that frame lands is a screenshot of nothing at all, which is
# exactly what the first version of this script produced.
notify-send -t 120000 "Blur" "The banner surface asks for a region of its own." || true
shot toast topbar-toast
# A summary-only card is shorter, so the gap and both cards' inner-facing
# rounded cutouts can be inspected over the same textured backdrop.
notify-send -t 120000 "Blur: a separate background" || true
shot toast-two topbar-toast

# --- the capsule ------------------------------------------------------------
#
# The capsule hides itself 1500ms after the last event, which is shorter than a
# settled capture takes. Keeping the volume moving keeps it on screen: every
# change restarts its timer, and the value it lands on does not matter.
keep_osd_up() {
  while :; do
    "$SMOKE_TOPBAR" volume set 35 >/dev/null 2>&1 || true
    sleep 1
    "$SMOKE_TOPBAR" volume set 45 >/dev/null 2>&1 || true
    sleep 1
  done
}
keep_osd_up &
osd_pump=$!
trap 'kill "$osd_pump" 2>/dev/null || true' EXIT INT TERM

shot osd topbar-osd

kill "$osd_pump" 2>/dev/null || true
wait "$osd_pump" 2>/dev/null || true
osd_pump=""
# Long enough for the capsule to time out and take its region with it.
sleep 3

# --- what the panel said it was doing ---------------------------------------
{
  echo "--- what blur decided at start-up ---"
  grep -a "blur:" "$art/panel.log" || echo "(the panel said nothing about blur)"

  echo "--- is the panel running with blur ---"
  grep -a "topbar is running" "$art/panel.log" || echo "(no start-up line)"

  echo "--- the fade-out ordering ---"
  # Proof for the caveat that cannot be photographed reliably: the region is
  # destroyed, and only then does the 150ms close animation start.
  grep -aE "effect object destroyed|motion: run [0-9]+ started \(1[0-9][0-9]ms\)" \
    "$art/panel.log" | tail -20 || echo "(nothing to order)"

  echo "--- effect objects ---"
  grep -a "effect object" "$art/panel.log" || echo "(none were ever created)"

  echo "--- anything wayland complained about ---"
  grep -aiE "protocol error|wayland.*error|blur.*(failed|error)" "$art/panel.log" ||
    echo "(no wayland errors)"
} >"$art/blur.txt" 2>&1

cat "$art/blur.txt"
