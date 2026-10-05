# Synthetic pointer and keyboard for the nested-niri smoke drivers. Source it,
# do not run it: `. "$(dirname "$0")/smoke-pointer.sh"`.
#
# Until this existed there was no way to *click* anything in a smoke run. Every
# popover was opened through TOPBAR_SMOKE_OPEN or `topbar popover show`, which
# dispatch the same action a click would have dispatched — so the whole path
# from "the compositor delivered a button event" to "a GTK gesture fired" was
# never once exercised, on any surface, in any run. Two dismissal bugs shipped
# to a real desktop behind a green suite because of it: the click-catcher never
# closed a popover, and the toggle chevrons never opened their sections.
#
# niri advertises zwlr_virtual_pointer_manager_v1 (v2) and
# zwp_virtual_keyboard_manager_v1, and a nested session advertises them too, so
# `wlrctl` can drive one from the inside. The events go into the nested seat
# directly; the host compositor is not involved and the nested winit window
# does not need to be focused, or even visible.
#
#   pointer_to <x> <y>          park the pointer at a logical coordinate
#   click_at <x> <y> [button]   move there, then click (default left)
#   press_at <x> <y> [button]   move there, then press
#   pointer_release [button]    let go
#   scroll_at <x> <y> <amount>  move there, then scroll
#   type_text <text>            type it on the virtual keyboard
#   key_press <key>             one key by xkb name, e.g. Escape
#   hold_key <key> <ms>         hold one down for that long, then let go
#
# `press_at` does NOT hold the button down. Every wlrctl call is a Wayland
# client of its own: it connects, creates a virtual pointer, sends its events
# and exits, and the compositor releases whatever that device was holding when
# it goes. So a `press_at` followed by a `pointer_release` is two clicks, not
# one press — the pair is still worth having, because "an early release must
# cancel" is a real contract, but a control that only responds to a *sustained*
# press cannot be driven this way and a run that thinks it is being held is
# being lied to. `hold_key` is the way to hold something: wtype takes a whole
# press-sleep-release sequence in one invocation, so the key stays down for as
# long as it is asked to.
#
# Coordinates are the nested output in *logical* pixels — which is the pixel
# size of the winit window divided by the `output "winit" { scale }` in the
# config visual-smoke-niri.sh writes. `pointer_size` reports them, and
# `grim` captures in device pixels, so a coordinate read off a screenshot has
# to be scaled before it is clicked. `shot_scale` does that.
#
# Every position is absolute, from a known origin: wlrctl can only move the
# pointer *relatively*, so each move parks it at the top-left corner first by
# asking for a move far larger than any screen, which the compositor clamps to
# the corner. That makes a coordinate mean the same thing on every call,
# whatever the previous one did.
#
# Environment: POINTER_SETTLE (seconds to let a move or a click be processed
# before the next command, 0.3).

POINTER_SETTLE="${POINTER_SETTLE:-0.3}"

# Far enough that any compositor clamps it to the corner of the output.
POINTER_FAR=100000

# Whether synthetic input is usable at all.
pointer_available() {
  command -v wlrctl >/dev/null 2>&1
}

# The nested output, in logical pixels: `<width> <height>`.
#
# The JSON carries the logical rectangle the pointer actually moves in, which
# is what a coordinate handed to `click_at` has to be inside.
pointer_size() {
  niri msg --json outputs 2>/dev/null | python3 -c '
import json, sys
outputs = json.load(sys.stdin)
for output in outputs.values():
    logical = output.get("logical")
    if logical:
        print(logical["width"], logical["height"])
        break
'
}

# Device pixels to logical pixels, for a coordinate read off a screenshot.
#
#   shot_scale 800 600   ->  1066 800   at scale 0.75
shot_scale() {
  niri msg --json outputs 2>/dev/null | python3 -c '
import json, sys
outputs = json.load(sys.stdin)
scale = 1.0
for output in outputs.values():
    logical = output.get("logical")
    if logical:
        scale = logical.get("scale", 1.0) or 1.0
        break
print(round(float(sys.argv[1]) / scale), round(float(sys.argv[2]) / scale))
' "$1" "$2"
}

# Park the pointer at the top-left corner of the output.
pointer_home() {
  # ponytail: niri 26.04/Smithay consumes the first motion after an ended popup
  # grab; reset again to reach the origin. Remove the duplicate when upstream fixes it.
  wlrctl pointer move -$POINTER_FAR -$POINTER_FAR || return 1
  wlrctl pointer move -$POINTER_FAR -$POINTER_FAR
}

# Put the pointer at a logical coordinate, from the corner every time.
pointer_to() {
  pointer_home || return 1
  # A move of zero is not worth a round trip, and wlrctl treats it as one.
  if [ "$1" -ne 0 ] || [ "$2" -ne 0 ]; then
    wlrctl pointer move "$1" "$2" || return 1
  fi
  sleep "$POINTER_SETTLE"
}

# Move there and click. The move comes first because a Wayland button event
# carries no coordinates: what is clicked is whatever the last motion entered.
click_at() {
  pointer_to "$1" "$2" || return 1
  wlrctl pointer click "${3:-left}" || return 1
  sleep "$POINTER_SETTLE"
}

# Move there and press. See the note above: the button comes back up when this
# wlrctl exits, so this is the first half of a click and not a hold.
press_at() {
  pointer_to "$1" "$2" || return 1
  wlrctl pointer click "${3:-left}" state:press || return 1
  sleep "$POINTER_SETTLE"
}

# Let go of whatever press_at is holding.
pointer_release() {
  wlrctl pointer click "${1:-left}" state:release || return 1
  sleep "$POINTER_SETTLE"
}

# Move there and scroll. A positive amount scrolls down.
scroll_at() {
  pointer_to "$1" "$2" || return 1
  wlrctl pointer scroll "$3" 0 || return 1
  sleep "$POINTER_SETTLE"
}

# Type on the virtual keyboard, into whatever holds the keyboard focus.
type_text() {
  wtype "$1"
  sleep "$POINTER_SETTLE"
}

# One key by its xkb name: Escape, Return, Tab.
key_press() {
  wtype -k "$1"
  sleep "$POINTER_SETTLE"
}

# Hold a key down for `<ms>` milliseconds, then let it go.
#
# The press, the wait and the release are one wtype invocation, which is what
# makes this a real hold: a virtual keyboard, like a virtual pointer, releases
# everything it was holding the moment the client that made it exits. The
# hold-to-confirm power rows are the only thing in the panel that needs it, and
# they take Enter and space as well as a press.
hold_key() {
  wtype -P "$1" -s "$2" -p "$1"
  sleep "$POINTER_SETTLE"
}

# Whether niri has the named layer surface mapped. Also in smoke-shot.sh; a
# driver that only wants to click does not have to source both.
pointer_mapped() {
  niri msg layers 2>/dev/null | grep -q "\"$1\""
}

# Wait for the named surface to appear, up to POINTER_WAIT seconds.
#
# A popover opened by a click is not on screen the instant the click lands: the
# content is built the first time it is asked for, which on a debug build is
# seconds rather than frames. Sleeping a fixed amount before asserting is the
# same coin toss smoke-shot.sh was written to stop losing.
wait_mapped() {
  waited=0
  while [ "$waited" -lt "${POINTER_WAIT:-20}" ]; do
    pointer_mapped "$1" && return 0
    sleep 1
    waited=$((waited + 1))
  done
  return 1
}

# Fail loudly if the named surface never appears.
assert_mapped() {
  if wait_mapped "$1"; then
    echo "smoke-pointer: $1 is mapped${2:+ ($2)}"
    return 0
  fi
  echo "smoke-pointer: $1 is NOT mapped${2:+ ($2)}" >&2
  return 1
}

# Wait for the named surface to go, up to POINTER_WAIT seconds.
#
# The mirror of `wait_mapped`, and there for the same reason: a popover is not
# off the screen the instant the click that dismissed it lands. It runs a close
# animation, hands the keyboard back and then unmaps, and an assertion made in
# between says it is still there.
wait_unmapped() {
  waited=0
  while [ "$waited" -lt "${POINTER_WAIT:-20}" ]; do
    pointer_mapped "$1" || return 0
    sleep 1
    waited=$((waited + 1))
  done
  return 1
}

# Fail loudly if the named surface does not go away.
assert_unmapped() {
  if wait_unmapped "$1"; then
    echo "smoke-pointer: $1 is gone${2:+ ($2)}"
    return 0
  fi
  echo "smoke-pointer: $1 is still mapped${2:+ ($2)}" >&2
  return 1
}

# Read native widget state after real keyboard input. Never set a query through IPC.
# Standalone choosers publish the same debug dump on key release.
picker_dump() {
  picker_log=${PICKER_LOG:-$SMOKE_ARTIFACTS/panel.log}
  picker_prefix=${PICKER_PREFIX:-ui-dump}
  picker_before=$(grep -c "$picker_prefix: end" "$picker_log" 2>/dev/null || true)
  if [ "$picker_prefix" = chooser-dump ]; then
    wtype -k Shift_L || return 1
  else
    "$SMOKE_TOPBAR" popover show surface-dump >/dev/null 2>&1 || return 1
  fi
  picker_wait=0
  while [ "$picker_wait" -lt 100 ]; do
    picker_after=$(grep -c "$picker_prefix: end" "$picker_log" 2>/dev/null || true)
    [ "${picker_after:-0}" -gt "${picker_before:-0}" ] && return 0
    sleep 0.1 || return 1
    picker_wait=$((picker_wait + 1))
  done
  echo "picker dump never completed: $picker_log" >&2
  return 1
}

# count, focused, centre or query from the last completed live widget dump.
# GtkType class... locators match exact types and unordered CSS tokens; an optional
# " | label" suffix retains label matching. Other locators keep substring matching.
# Parser-only check: `. scripts/smoke-pointer.sh; picker_read selfcheck ''`.
picker_read() {
  python3 - "${PICKER_LOG:-$SMOKE_ARTIFACTS/panel.log}" "${PICKER_PREFIX:-ui-dump}" "$@" <<'PY'
import re, sys
path, prefix, mode, pattern = sys.argv[1:5]

def matches(widget_type, classes, label, pattern):
    selector, separator, label_pattern = pattern.partition(" | ")
    tokens = selector.split()
    if tokens and tokens[0].startswith("Gtk"):
        return (widget_type == tokens[0]
                and set(tokens[1:]).issubset(classes.split("."))
                and (not separator or label_pattern in label))
    return pattern in " ".join((widget_type, classes.replace(".", " "), label))

if mode == "selfcheck":
    assert matches("GtkButton", "text-button.picker-option", "Off", "GtkButton picker-option")
    assert matches("GtkBox", "horizontal.crypto-setting-row.picker-option", "", "GtkBox crypto-setting-row picker-option")
    assert not matches("GtkBox", "picker-option", "", "GtkButton picker-option")
    assert not matches("GtkButton", "not-picker-option", "picker-option", "GtkButton picker-option")
    assert matches("GtkLabel", "chooser-empty", "No matching choices", "GtkLabel chooser-empty | No matching choices")
    assert not matches("GtkLabel", "chooser-empty", "Loading", "GtkLabel chooser-empty | No matching choices")
    assert matches("GtkSearchEntry", "picker-search", "Temperature unit · fhr", "Temperature unit")
    sys.exit(0)
text = re.sub(r"\x1b\[[0-9;]*m", "", open(path, errors="replace").read())
start = text.rfind(prefix + ": begin")
end = text.find(prefix + ": end", start)
assert start >= 0 and end > start, "no completed picker dump"
rows = []
for line in text[start:end].splitlines():
    m = re.search(re.escape(prefix) + r': (\S+) \[([^\]]*)\] "([^"]*)" (-?\d+) (-?\d+) (\d+) (\d+)(.*)', line)
    if m and matches(m[1], m[2], m[3], pattern):
        rows.append(m)
if mode == "count":
    print(len(rows))
elif mode == "focused":
    assert any("focused=true" in row[8] for row in rows), f"no focused {pattern}"
elif mode == "centre":
    index = int(sys.argv[5]) if len(sys.argv) > 5 else 1
    row = rows[index - 1]
    assert int(row[6]) > 0 and int(row[7]) > 0, "picker not laid out"
    print(int(row[4]) + int(row[6]) // 2, int(row[5]) + int(row[7]) // 2)
elif mode == "labels":
    import json
    print(json.dumps([row[3] for row in rows]))
elif mode == "disabled":
    assert any("sensitive=false" in row[8] for row in rows), f"no disabled {pattern}"
elif mode == "enabled":
    assert any("sensitive=true" in row[8] for row in rows), f"no enabled {pattern}"
elif mode == "query":
    expected = sys.argv[5]
    assert any(row[3] == pattern + " · " + expected for row in rows), f"{pattern}: expected query {expected!r}"
PY
}

picker_focus() (
  pattern=$1
  attempts=0
  while [ "$attempts" -lt 60 ]; do
    picker_dump || return 1
    if picker_read focused "$pattern" 2>/dev/null; then return 0; fi
    key_press Tab || return 1
    attempts=$((attempts + 1))
  done
  echo "could not focus picker option: $pattern" >&2
  return 1
)

picker_click() {
  picker_dump || return 1
  picker_xy=$(picker_read centre "$1" "${2:-1}") || return 1
  # shellcheck disable=SC2086
  click_at $picker_xy
}

# Probe focus-independent typing, tail Backspace, no-match recovery and clearing.
# The caller checks its fake-service recorder/state file around this non-activation probe.
# Callers run probes inside `command || ...`, which disables shell errexit even
# in these subshells. Each action/assertion must therefore propagate failure.
picker_probe() (
  row=$1
  search=$2
  query=$3
  expected=${4:-1}
  picker_dump || return 1
  original=$(picker_read count "$row") || return 1
  [ "$original" -gt 0 ] || return 1
  picker_focus "$row" || return 1
  type_text "$query" || return 1
  picker_dump || return 1
  picker_read query "$search" "$query" || return 1
  [ "$(picker_read count "$row")" -eq "$expected" ] || return 1
  wtype -M shift -k Down -m shift || return 1
  picker_dump || return 1
  picker_read focused "$search" || return 1
  picker_read query "$search" "$query" || return 1
  picker_focus "$row" || return 1
  key_press BackSpace || return 1
  shorter=${query%?}
  picker_dump || return 1
  picker_read query "$search" "$shorter" || return 1
  [ "$(picker_read count "$row")" -ge "$expected" ] || return 1
  picker_focus "$row" || return 1
  type_text "zzzzz" || return 1
  picker_dump || return 1
  [ "$(picker_read count "$row")" -eq 0 ] || return 1
  if [ "${PICKER_PREFIX:-ui-dump}" = chooser-dump ]; then
    picker_read disabled Apply || return 1
    [ "$(picker_read count 'GtkLabel chooser-empty | No matching choices')" -eq 1 ] || return 1
  fi
  for ignored in 1 2 3 4 5; do key_press BackSpace || return 1; done
  picker_dump || return 1
  picker_read query "$search" "$shorter" || return 1
  [ "$(picker_read count "$row")" -ge "$expected" ] || return 1
  wtype -M ctrl -k a -m ctrl || return 1
  key_press BackSpace || return 1
  picker_dump || return 1
  picker_read query "$search" "" || return 1
  [ "$(picker_read count "$row")" -eq "$original" ] || return 1
)

picker_saved() {
  python3 - "$XDG_STATE_HOME/topbar/state.json" "$1" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
state = json.loads(path.read_text()) if path.exists() else {}
print(json.dumps(state.get(sys.argv[2]), sort_keys=True))
PY
}

picker_tail_probe() (
  row=$1
  search=$2
  query=$3
  picker_focus "$row" || return 1
  type_text zzz || return 1
  picker_dump || return 1
  picker_read query "$search" "${query}zzz" || return 1
  [ "$(picker_read count "$row")" -eq 0 ] || return 1
  for ignored in 1 2 3; do key_press BackSpace || return 1; done
  picker_focus "$row" || return 1
  key_press BackSpace || return 1
  picker_dump || return 1
  picker_read query "$search" "${query%?}" || return 1
  wtype -M ctrl -k a -m ctrl || return 1
  type_text "$query" || return 1
  picker_dump || return 1
  picker_read query "$search" "$query" || return 1
)
