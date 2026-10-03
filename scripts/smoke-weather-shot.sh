#!/usr/bin/env sh
# One screenshot of whatever the weather run put on screen, plus the state
# file the panel wrote. Driven by scripts/smoke-weather.sh; not useful alone.
#
# $SMOKE_EXPECT names the layer surface this scenario is about, so the capture
# waits until that surface has actually been drawn rather than for a number of
# seconds — see scripts/smoke-shot.sh for why the distinction has teeth.
set -eu

art="$SMOKE_ARTIFACTS"
. "$(dirname "$0")/smoke-shot.sh"
. "$(dirname "$0")/smoke-pointer.sh"

shot weather "${SMOKE_EXPECT:-}"

if [ "${SMOKE_EXPECT:-}" = topbar-dialog ]; then
  before=$(picker_saved weather)
  picker_click "Search for a city"
  wtype -M ctrl -k a -m ctrl
  key_press BackSpace
  picker_probe "GtkButton location-result" "Search for a city" mscrss
  [ "$(picker_saved weather)" = "$before" ]
  picker_click picker-selector
  picker_probe "GtkButton picker-option" "Temperature unit" fhr
  key_press Escape
  [ "$(picker_saved weather)" = "$before" ]
  picker_click picker-selector
  picker_dump
  picker_read query "Temperature unit" ""
  key_press Down
  type_text fhr
  key_press Down
  key_press Return
  [ "$(picker_saved weather)" = "$before" ]
  picker_click Save
  assert_unmapped topbar-dialog
  python3 - "$XDG_STATE_HOME/topbar/state.json" <<'PY'
import json, pathlib, sys, time
path = pathlib.Path(sys.argv[1])
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    if path.exists() and json.loads(path.read_text()).get("weather", {}).get("unit") == "fahrenheit":
        break
    time.sleep(.1)
else:
    raise SystemExit("filtered unit activation did not save Fahrenheit's original index")
PY
  "$SMOKE_TOPBAR" popover show weather-setup
  assert_mapped topbar-dialog
  picker_click picker-selector
  picker_probe "GtkButton picker-option" "Temperature unit" cls
  key_press Down
  type_text cls
  key_press Down
  key_press Return
  picker_click Save
  assert_unmapped topbar-dialog
  python3 - "$XDG_STATE_HOME/topbar/state.json" <<'PY'
import json, pathlib, sys, time
path = pathlib.Path(sys.argv[1])
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    if path.exists() and json.loads(path.read_text()).get("weather", {}).get("unit") == "celsius":
        break
    time.sleep(.1)
else:
    raise SystemExit("filtered unit activation did not save Celsius's original index")
PY
  "$SMOKE_TOPBAR" popover show weather-setup
  assert_mapped topbar-dialog
  picker_click "Search for a city"
  wtype -M ctrl -k a -m ctrl
  key_press BackSpace
  picker_click GtkExpander
  picker_click "GtkEntry location-coordinate" 1
  wtype -M ctrl -k a -m ctrl
  type_text "12.25"
  key_press Home
  key_press End
  key_press BackSpace
  picker_dump
  picker_read query "Search for a city" ""
  shot weather-picker-editors topbar-dialog
  key_press Escape
fi

# The location the dialog saves lives in the sandboxed state file, and a
# second panel start reading it is what makes "saved" mean something.
if [ -n "${XDG_STATE_HOME:-}" ] && [ -f "$XDG_STATE_HOME/topbar/state.json" ]; then
  cp "$XDG_STATE_HOME/topbar/state.json" "$art/state.json"
fi

niri msg layers >"$art/layers.txt" 2>&1 || true
