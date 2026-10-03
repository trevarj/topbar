#!/usr/bin/env sh
# The driver for scripts/smoke-qs-pointer.sh. Run inside the nested session.
#
# Every control in Quick Settings, pressed by a synthetic pointer, at the scale
# a real display runs at. Nothing here uses TOPBAR_SMOKE_OPEN to reach a block
# inside the panel: those dispatch the action a click would have dispatched and
# skip the part that keeps turning out to be broken.
#
# Coordinates are never hardcoded. The panel is asked where its controls are —
# `topbar popover show quick-settings-dump` logs a rectangle per control, in
# monitor pixels, with the text on it — and `locate` reads the last dump out of
# panel.log. A driver holding coordinates measured off a screenshot starts
# clicking empty space the first time a padding changes, which looks exactly
# like the dead-control bugs this run exists to catch.
#
# Nothing in here can act on the machine the run is inside:
#
#   - the power rows go to logind on the *system* bus, which no test can put a
#     stand-in in front of, so `Power` refuses every action in a development
#     build (topbar-services/src/power.rs). The hold below therefore runs to
#     completion on purpose, and what lands on screen is the refusal;
#   - the lock button runs a command, so the run is given a config whose lock
#     command is a path that does not exist. `loginctl lock-session` would lock
#     the developer's screen;
#   - NetworkManager and BlueZ are the run's own fakes, on its private bus.
set -eu

. "$(dirname "$0")/smoke-pointer.sh"
. "$(dirname "$0")/smoke-shot.sh"

scenario="${SMOKE_QS_POINTER_SCENARIO:-controls}"

fail=0
profile_monitor=""
trap 'if [ -n "$profile_monitor" ]; then kill "$profile_monitor" 2>/dev/null || true; wait "$profile_monitor" 2>/dev/null || true; fi' EXIT INT TERM
# Run a check, remember a failure, and never stop the run: a driver that exits
# at the first failure photographs nothing after it, and the screenshot of what
# went wrong is the most useful thing it could have left behind.
check() {
  "$@" || fail=1
}

# Ask the panel where everything is. Run it after anything that moves a
# control; every coordinate below comes out of the answer.
#
# The wait before asking is not politeness. The nested session renders in
# software and is throttled by the host compositor, so a click can take seconds
# to become a laid-out panel — and a dump taken before that is a dump of the
# panel as it was *before* the click, which had the driver clicking one step
# behind itself for a whole run. The wait after is on evidence rather than the
# clock: the panel has to have finished writing a new block.
dump() {
  sleep "${DUMP_SETTLE:-2}"
  before=$(grep -c "qs-dump: end" "$SMOKE_ARTIFACTS/panel.log" 2>/dev/null || true)
  "$SMOKE_TOPBAR" popover show quick-settings-dump >/dev/null 2>&1 || true
  waited=0
  while [ "$waited" -lt 20 ]; do
    if [ "$(grep -c "qs-dump: end" "$SMOKE_ARTIFACTS/panel.log" 2>/dev/null || true)" -gt "$before" ]; then
      return 0
    fi
    sleep 0.5
    waited=$((waited + 1))
  done
  echo "smoke-qs-pointer: the panel never answered a dump" >&2
  return 1
}

# One control's rectangle, as `<x> <y> <w> <h>`, from the last dump.
#
#   rect_of Caffeine            the pill with that word on it
#   rect_of qs-toggle-expand 2  the second chevron
#   rect_of bar-button          the button on the bar
#
# The pattern is matched against "<GtkType> <classes> <label>", so a class, a
# widget type and the text on a control are all usable and any of them matches
# wherever it appears. Fails loudly when there is no such control, which is a
# failure worth having: it means the thing being clicked is not on screen.
rect_of() {
  python3 - "$SMOKE_ARTIFACTS/panel.log" "$1" "${2:-1}" <<'PY'
import re, sys

log, pattern, index = sys.argv[1], sys.argv[2], int(sys.argv[3])
text = re.sub(r"\x1b\[[0-9;]*m", "", open(log, errors="replace").read())

# The last dump only: the panel is dumped again after every step.
start = text.rfind("qs-dump: begin")
if start < 0:
    sys.exit("no dump in the log")
end = text.find("qs-dump: end", start)
block = text[start : end if end > 0 else len(text)]

line_re = re.compile(
    r'qs-dump: (\S+) \[([^\]]*)\] "([^"]*)" (-?\d+) (-?\d+) (\d+) (\d+)'
)
found = []
for line in block.splitlines():
    match = line_re.search(line)
    if not match:
        continue
    kind, classes, label = match.group(1), match.group(2), match.group(3)
    haystack = kind + " " + classes.replace(".", " ") + " " + label
    if pattern not in haystack:
        continue
    rect = match.group(4, 5, 6, 7)
    # A control with no size has not been laid out yet, and its centre is the
    # top-left corner of the screen. Clicking there hits the workspace switcher.
    if int(rect[2]) > 0 and int(rect[3]) > 0:
        found.append(rect)

if len(found) < index:
    sys.exit("%s: wanted #%d, found %d" % (pattern, index, len(found)))
print(*found[index - 1])
PY
}

# Dump until `$1` is somewhere in the panel, or give up loudly.
#
# The panel answers a click by asking a service, and the answer arrives when it
# arrives: a fixed wait is a guess, and a dump taken while the answer is still
# in flight is a dump of the panel as it was before the click. Everything here
# that waits for a *control* rather than for a still frame waits through this.
dump_until() {
  tries=0
  while [ "$tries" -lt 10 ]; do
    dump
    if rect_of "$1" "${2:-1}" >/dev/null 2>&1; then
      return 0
    fi
    tries=$((tries + 1))
  done
  echo "smoke-qs-pointer: $1 #${2:-1} never appeared" >&2
  return 1
}

# The centre of a control, as `<x> <y>`.
centre_of() {
  rect=$(rect_of "$@") || return 1
  # shellcheck disable=SC2086
  set -- $rect
  echo "$(($1 + $3 / 2)) $(($2 + $4 / 2))"
}

# Click a control by name. Located first, so a control that moved is still hit
# and one that vanished is a loud failure rather than a click on the wall.
click_on() {
  where=$(centre_of "$1" "${2:-1}") || {
    echo "smoke-qs-pointer: cannot find $1 #${2:-1}" >&2
    return 1
  }
  echo "smoke-qs-pointer: click $1 #${2:-1} at $where"
  # shellcheck disable=SC2086
  click_at $where
}

# Park the pointer on a control without pressing it, for a hover screenshot.
hover_on() {
  where=$(centre_of "$1" "${2:-1}") || {
    echo "smoke-qs-pointer: cannot find $1 #${2:-1}" >&2
    return 1
  }
  echo "smoke-qs-pointer: hover $1 #${2:-1} at $where"
  # shellcheck disable=SC2086
  pointer_to $where
}

# Scroll over a control, rather than at a coordinate off a ruler.
scroll_on() {
  where=$(centre_of "$1" "${2:-1}") || return 1
  # shellcheck disable=SC2086
  scroll_at $where "$3"
}

# Off the panel entirely, so a screenshot is not taken with the pointer sitting
# on a hover state. The panel is anchored top-right; the far left is desktop.
pointer_park() {
  pointer_to 40 940
}

# Click `<percent>` of the way along a control's track: a GtkScale jumps to
# where it was clicked, which is how a slider is set without dragging one.
click_along() {
  what=$1
  percent=$2
  rect=$(rect_of "$what" "${3:-1}") || return 1
  # shellcheck disable=SC2086
  set -- $rect
  x=$(($1 + $3 * percent / 100))
  y=$(($2 + $4 / 2))
  echo "smoke-qs-pointer: click $what at $percent% of its track ($x $y)"
  click_at "$x" "$y"
}

# The accordion's rule: at most one expandable section is open. Each of them
# contributes a control only it has, so counting those counts open sections.
assert_one_expandable() {
  open=""
  for kind in qs-network-row GtkSwitch qs-vpn-row qs-radio-row qs-limit-button qs-power-row; do
    if rect_of "$kind" 1 >/dev/null 2>&1; then
      open="$open $kind"
    fi
  done
  count=$(echo $open | wc -w)
  if [ "$count" -le 1 ]; then
    echo "smoke-qs-pointer: one expandable open ($open)"
    return 0
  fi
  echo "smoke-qs-pointer: $count expandables are open at once:$open" >&2
  return 1
}

picker_mutations() {
  case "$1" in
    network) destination=org.freedesktop.NetworkManager; object=/io/github/trevarj/topbar/FakeNm1; interface=io.github.trevarj.topbar.FakeNm1 ;;
    bluetooth) destination=org.bluez; object=/io/github/trevarj/topbar/FakeBluez1; interface=io.github.trevarj.topbar.FakeBluez1 ;;
  esac
  gdbus call --session --dest "$destination" --object-path "$object" --method "$interface.Calls" |
    python3 -c 'import re, sys; print(re.findall(r"ActivateConnection|AddAndActivateConnection|DeactivateConnection|Disconnect|Connect |Pair |Trusted|Adapter1[.]Powered", sys.stdin.read()))'
}

echo "=== output: $(pointer_size) ==="
if [ "$(pointer_size)" != "918 988" ]; then
  echo "smoke-qs-pointer: this run wants the nested output at scale 1.0, where a"
  echo "logical pixel is a device pixel and a screenshot coordinate is a pointer"
  echo "coordinate. Set TOPBAR_SMOKE_SCALE=1.0."
  exit 1
fi

# The panel has to be built before it can say where anything is, and the bar
# button is the only control on screen before it is. One dump, one click, and
# from there every coordinate comes out of the panel itself.
#
# The bar is asked more than once: the driver starts two seconds after the
# panel does, and on a cold debug build that is sometimes before the bar has
# been through a size negotiation. A widget that has not been laid out has no
# rectangle, and `rect_of` refuses to hand back a centre of nothing.
open_panel() {
  check dump_until bar-button
  check click_on bar-button
  check assert_mapped topbar-popover "opened by a click on the bar"
  dump
}

case "$scenario" in
  # The header: the battery pill, both charge-limit buttons, the lock button
  # and the power section's four rows.
  header)
    echo "--- open"
    open_panel
    pointer_park
    check shot 01-open topbar-popover

    # One Tab into a panel that has just taken the keyboard. Whatever it lands
    # on has to say so: the panel clears Adwaita's focus ring along with its
    # background images, and for a long time it drew nothing in its place. This
    # is the only step in the matrix that presses a key before it looks, which
    # is the point — a pointer-driven run never raises `:focus-visible` and
    # never draws a ring, and that is correct.
    echo "--- one Tab, and the focus has to be visible"
    key_press Tab
    pointer_park
    check shot 01b-focus-ring

    echo "--- the battery pill opens the health card"
    check hover_on qs-battery-pill
    check shot 02-pill-hover
    check click_on qs-battery-pill
    dump
    pointer_park
    check shot 03-battery-card

    echo "--- both charge-limit buttons"
    check click_on qs-limit-button 2
    dump
    pointer_park
    check shot 04-limit-80
    check click_on qs-limit-button 1
    dump
    pointer_park
    check shot 05-limit-full

    echo "--- the lock button reports inline (its command does not exist)"
    check click_on qs-round-button 1
    sleep 2
    pointer_park
    check shot 06-lock-error

    # The panel is retained, so the caption is the same widget the next time it
    # is opened. Nothing clears it but the same action being tried again, and
    # for a lock command nobody tries twice that is never — this is the shot
    # that says the header comes back without Tuesday's failure on it.
    echo "--- and the caption does not survive the panel"
    click_at 200 940
    check assert_unmapped topbar-popover "click-away dismissed it"
    open_panel
    pointer_park
    check shot 06b-reopened-clean

    echo "--- the power button opens the power section"
    check click_on qs-round-button 2
    dump
    check assert_one_expandable
    pointer_park
    check shot 07-power-section

    echo "--- hover a power row"
    check hover_on qs-power-row 1
    check shot 08-power-hover

    echo "--- press and let go of Shut Down: an early release must cancel"
    # Not a hold. Every wlrctl call is its own Wayland client and the button
    # comes back up when it exits, so this is a press and a release with a
    # third of a second between them — which is exactly the contract worth
    # checking here, because a row that fired on a press would be a row that
    # shut the machine down by being brushed against.
    if where=$(centre_of "Shut Down"); then
      # shellcheck disable=SC2086
      press_at $where
      pointer_release
      sleep 2
      pointer_park
      check shot 09-press-cancelled
    else
      fail=1
    fi

    echo "--- and hold it properly, from the keyboard, until it fires"
    # The keyboard is the one synthetic input that can hold something down:
    # wtype takes press, wait and release in a single invocation. The popover
    # has had the keyboard since the first click landed on it, and focus is on
    # the power button that opened the section, so one Tab reaches the first
    # row inside it.
    key_press Tab
    pointer_park
    check shot 10-focus-on-a-row
    hold_key Return 900
    sleep 2
    pointer_park
    check shot 11-hold-fired
    ;;

  # The sliders block and the two plain pills.
  sliders)
    echo "--- open"
    open_panel
    pointer_park
    check shot 01-open

    echo "--- the output slider jumps to where it is clicked"
    check click_along GtkScale 25 1
    dump
    pointer_park
    check shot 02-volume-quarter
    check click_along GtkScale 80 1
    dump
    pointer_park
    check shot 03-volume-loud

    echo "--- the speaker icon mutes, and unmutes"
    check click_on qs-slider-icon 1
    dump
    pointer_park
    check shot 04-muted
    check click_on qs-slider-icon 1
    dump

    echo "--- the chooser chevron opens the output list"
    check hover_on qs-chooser
    check shot 05-chooser-hover
    check click_on qs-chooser
    dump
    pointer_park
    check shot 06-output-list
    sink_before=$(pactl get-default-sink)
    check picker_probe "GtkButton qs-device-row" "Filter output devices" hdmi
    check test "$(pactl get-default-sink)" = "$sink_before"
    check picker_focus "GtkButton qs-device-row"
    type_text hdmi
    pactl set-sink-volume topbar_smoke +1%
    picker_dump
    check picker_read query "Filter output devices" hdmi
    check picker_read focused "Filter output devices"
    wtype -M ctrl -k a -m ctrl
    key_press BackSpace
    dump
    echo "--- and a row in it picks an output"
    check click_on qs-device-row 1
    dump
    pointer_park
    check shot 07-output-picked
    check click_on qs-chooser
    dump

    echo "--- Caffeine, on and off"
    check click_on Caffeine
    dump
    pointer_park
    check shot 08-caffeine-on
    check click_on Caffeine
    dump
    pointer_park
    check shot 09-caffeine-off
    ;;

  # The Wi-Fi list: rows, the password box, scrolling, and the radio switch.
  wifi)
    echo "--- open"
    open_panel
    pointer_park
    check shot 01-open

    echo "--- the chevron opens the list; the body must not switch the radio"
    check click_on qs-toggle-expand 1
    dump
    pointer_park
    check shot 02-wifi-list
    network_before=$(picker_mutations network)
    check picker_probe "GtkButton qs-network-row" "Filter networks" uad
    check picker_focus "GtkButton qs-network-row"
    type_text uad
    gdbus call --session --dest org.freedesktop.NetworkManager \
      --object-path /io/github/trevarj/topbar/FakeNm1 \
      --method io.github.trevarj.topbar.FakeNm1.SetStrength Usadba 15 >/dev/null
    picker_dump
    check picker_read query "Filter networks" uad
    wtype -M ctrl -k a -m ctrl
    key_press BackSpace
    check test "$(picker_mutations network)" = "$network_before"
    dump

    echo "--- hover a network row, then join the open one"
    check hover_on Airport
    check shot 03-network-hover
    check click_on Airport
    dump
    pointer_park
    check snap 04-network-joining

    echo "--- a secured network asks for a password"
    check click_on Cafe
    sleep 3
    dump
    pointer_park
    check snap 05-password-prompt
    check picker_click "Filter networks"
    type_text uad
    picker_dump
    check test "$(picker_read count qs-password-entry)" -gt 0
    check picker_focus qs-password-entry
    type_text "wrong-key-on-purpose"
    picker_dump
    check picker_read query "Filter networks" uad
    check picker_click "Filter networks"
    wtype -M ctrl -k a -m ctrl
    key_press BackSpace
    dump
    check snap 06-password-typed
    echo "--- and Connect sends it, which the fake refuses"
    # By class and position, not by the word on it. `rect_of` matches a
    # substring of "<type> <classes> <label>", and the Wi-Fi pill's own label
    # is "Usadba · Connected" — which contains "Connect". So `click_on Connect`
    # pressed the *pill*, switched the radio off, took the whole list away with
    # it and then reported every following step as a control that had vanished.
    # The password box has exactly two buttons, in this order.
    check click_on qs-password-button 2
    # NetworkManager answers a refused key by asking for it *again*, and
    # between the two there is a moment with no password box at all — so the
    # box is waited for rather than assumed, and what lands on screen is the
    # retry: the same prompt in red, saying the key was wrong.
    check dump_until qs-password-button 1
    pointer_park
    check snap 07-password-refused
    check click_on qs-password-button 1
    sleep 2
    dump

    echo "--- scrolling: twelve networks is taller than the panel may be"
    check scroll_on qs-network-row 1 5
    check shot 08-scrolled
    check scroll_on qs-network-row 1 -5
    check shot 09-scrolled-back

    echo "--- the pill body switches the radio off, which closes the list"
    check click_on qs-toggle 1
    sleep 3
    dump
    pointer_park
    check shot 10-wifi-off
    check click_on qs-toggle 1
    sleep 3
    dump
    ;;

  # Bluetooth, VPN and Power Mode.
  devices)
    echo "--- open"
    open_panel
    pointer_park
    check shot 01-open

    echo "--- the Bluetooth chevron opens the device list"
    check click_on qs-toggle-expand 2
    dump
    check assert_one_expandable
    pointer_park
    check shot 02-bluetooth-list
    bluetooth_before=$(picker_mutations bluetooth)
    check picker_probe "GtkBox qs-device-row" "Filter Bluetooth devices" mxm
    check picker_focus "GtkBox qs-device-row"
    type_text mxm
    gdbus call --session --dest org.bluez --object-path /io/github/trevarj/topbar/FakeBluez1 \
      --method io.github.trevarj.topbar.FakeBluez1.SetBattery mouse 41 >/dev/null
    picker_dump
    check picker_read query "Filter Bluetooth devices" mxm
    wtype -M ctrl -k a -m ctrl
    key_press BackSpace
    check test "$(picker_mutations bluetooth)" = "$bluetooth_before"
    dump

    echo "--- a device switch connects"
    check click_on GtkSwitch 1
    sleep 2
    dump
    pointer_park
    check snap 03-bluetooth-connecting
    sleep 6
    dump
    pointer_park
    check shot 04-bluetooth-settled

    echo "--- opening the VPN list closes the Bluetooth one"
    check click_on qs-toggle-expand 3
    dump
    check assert_one_expandable
    pointer_park
    check shot 05-vpn-list
    vpn_before=$(picker_mutations network)
    check picker_probe "GtkButton qs-vpn-row" "Filter VPN profiles" wrk
    check test "$(picker_mutations network)" = "$vpn_before"
    dump

    echo "--- a VPN row switches its tunnel"
    check click_on qs-vpn-row 1
    sleep 3
    pointer_park
    check snap 06-vpn-up

    # `vpn_close_on_connect` defaults to on, so the tunnel coming up takes the
    # panel with it — which is the setting working. Everything after this point
    # needs the panel back, and without the reopen the last third of this
    # scenario spent its life clicking at a closed popover and reporting Power
    # Mode as a control that was not there.
    echo "--- the panel closed itself on the connect; open it again"
    check assert_unmapped topbar-popover "vpn_close_on_connect closed it"
    # Guarded: a reopen aimed at a panel that never closed would *close* it,
    # and the rest of the scenario would fail for the opposite reason.
    if ! pointer_mapped topbar-popover; then
      open_panel
    fi

    echo "--- Power Mode expands from its body, not only its chevron"
    check click_on "Power Mode"
    dump
    check assert_one_expandable
    pointer_park
    check shot 07-power-mode
    dbus-monitor --session \
      "type='method_call',interface='org.freedesktop.DBus.Properties',arg0='net.hadess.PowerProfiles'" \
      "type='method_call',interface='org.freedesktop.DBus.Properties',arg0='org.freedesktop.UPower.PowerProfiles'" \
      >"$SMOKE_ARTIFACTS/profile-calls.log" 2>"$SMOKE_ARTIFACTS/profile-monitor.stderr" &
    profile_monitor=$!
    waited=0
    while ! grep -q "member=Get" "$SMOKE_ARTIFACTS/profile-calls.log"; do
      [ "$waited" -lt 100 ] || { echo "private profile monitor never became ready" >&2; exit 1; }
      gdbus call --session --dest net.hadess.PowerProfiles --object-path /net/hadess/PowerProfiles \
        --method org.freedesktop.DBus.Properties.Get net.hadess.PowerProfiles ActiveProfile >/dev/null
      sleep .1
      waited=$((waited + 1))
    done
    writes_before=$(grep -c "member=Set" "$SMOKE_ARTIFACTS/profile-calls.log" || true)
    profile_before=$(gdbus call --session --dest net.hadess.PowerProfiles \
      --object-path /net/hadess/PowerProfiles --method org.freedesktop.DBus.Properties.Get \
      net.hadess.PowerProfiles ActiveProfile)
    check picker_probe "GtkButton qs-radio-row" "Filter power profiles" psv
    profile_after=$(gdbus call --session --dest net.hadess.PowerProfiles \
      --object-path /net/hadess/PowerProfiles --method org.freedesktop.DBus.Properties.Get \
      net.hadess.PowerProfiles ActiveProfile)
    check test "$profile_after" = "$profile_before"
    writes_after=$(grep -c "member=Set" "$SMOKE_ARTIFACTS/profile-calls.log" || true)
    check test "$writes_after" = "$writes_before"
    dump
    echo "--- and each radio row picks a profile"
    check picker_focus "GtkButton qs-radio-row"
    type_text psv
    dump
    check click_on qs-radio-row 1
    dump
    pointer_park
    check shot 08-power-saver
    gdbus call --session --dest net.hadess.PowerProfiles --object-path /net/hadess/PowerProfiles \
      --method org.freedesktop.DBus.Properties.Get net.hadess.PowerProfiles ActiveProfile \
      >"$SMOKE_ARTIFACTS/filtered-profile.txt"
    check grep -q power-saver "$SMOKE_ARTIFACTS/filtered-profile.txt"
    check picker_click "Filter power profiles"
    wtype -M ctrl -k a -m ctrl
    key_press BackSpace
    dump
    check click_on qs-radio-row 3
    dump
    pointer_park
    check shot 09-performance
    ;;
esac

echo "--- dismiss"
click_at 200 940
sleep 2
check assert_unmapped topbar-popover "click-away dismissed it"

if [ "$fail" -eq 0 ]; then
  echo "--- result: PASS"
else
  echo "--- result: FAIL"
fi
exit "$fail"
