#!/usr/bin/env sh
# One screenshot of whatever the crypto run put on screen, plus the state file
# the panel wrote. Driven by scripts/smoke-crypto.sh; not useful alone.
#
# $SMOKE_EXPECT names the layer surface this scenario is about, so the capture
# waits until that surface has actually been drawn rather than for a number of
# seconds — see scripts/smoke-shot.sh for why the distinction has teeth.
set -eu

art="$SMOKE_ARTIFACTS"
. "$(dirname "$0")/smoke-shot.sh"
. "$(dirname "$0")/smoke-pointer.sh"

shot crypto "${SMOKE_EXPECT:-}"

if [ "${TOPBAR_SMOKE_OPEN:-}" = crypto-settings ]; then
  before=$(picker_saved crypto)
  picker_probe "GtkBox crypto-setting-row picker-option" "Filter assets" mnr
  [ "$(picker_saved crypto)" = "$before" ]
  picker_click "GtkMenuButton picker-selector" 1
  picker_probe "GtkButton picker-option" "Numerator asset" xr
  key_press Escape
  [ "$(picker_saved crypto)" = "$before" ]
  picker_click "GtkMenuButton picker-selector" 1
  picker_dump
  picker_read query "Numerator asset" ""
  key_press Down
  type_text xr
  key_press Down
  key_press Return
  picker_click "GtkMenuButton picker-selector" 2
  picker_probe "GtkButton picker-option" "Denominator asset" eh
  key_press Down
  type_text eh
  key_press Down
  key_press Return
  [ "$(picker_saved crypto)" = "$before" ]
  picker_click Add
  python3 - "$XDG_STATE_HOME/topbar/state.json" <<'PY'
import json, pathlib, sys, time
path = pathlib.Path(sys.argv[1])
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    if path.exists() and "xmr/eth" in json.loads(path.read_text()).get("crypto", {}).get("entries", []):
        break
    time.sleep(.1)
else:
    raise SystemExit("filtered pair activation did not save its original asset identities")
PY
  shot crypto-filtered-pair topbar-popover
fi

# What the settings view saved lives in the sandboxed state file; copying it out
# is how "the entries were persisted" is checked rather than assumed.
if [ -n "${XDG_STATE_HOME:-}" ] && [ -f "$XDG_STATE_HOME/topbar/state.json" ]; then
  cp "$XDG_STATE_HOME/topbar/state.json" "$art/state.json"
fi

niri msg layers >"$art/layers.txt" 2>&1 || true
