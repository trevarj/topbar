#!/usr/bin/env sh
# The driver for scripts/smoke-notifications.sh. Run inside the nested session.
#
# Every control the notifications module has — the history column in the control
# panel and the banners under the bar — pressed by a synthetic pointer, at the
# scale a real display runs at. Notifications arrive the way a real one does:
# `notify-send` against the panel's own daemon, on the run's private bus.
#
# Coordinates are never hardcoded. The panel is asked where its controls are —
# `topbar popover show surface-dump` logs a rectangle per control on every
# mapped layer surface, in monitor pixels, with the text on it — and `locate`
# reads the last dump out of panel.log.
set -eu

. "$(dirname "$0")/smoke-pointer.sh"
. "$(dirname "$0")/smoke-shot.sh"

scenario="${SMOKE_NOTIFICATIONS_SCENARIO:-history}"
art="$SMOKE_ARTIFACTS"

fail=0
# Run a check, remember a failure, and never stop the run: a driver that exits
# at the first failure photographs nothing after it, and the screenshot of what
# went wrong is the most useful thing it could have left behind.
check() {
  "$@" || fail=1
}

# Ask the panel where everything is. Run it after anything that moves a control.
#
# The wait before asking is not politeness: the nested session renders in
# software and is throttled by the host compositor, so a click can take seconds
# to become a laid-out panel, and a dump taken before that is a dump of the
# panel as it was *before* the click. The wait after is on evidence — the panel
# has to have finished writing a new block.
dump() {
  sleep "${DUMP_SETTLE:-2}"
  before=$(grep -c "ui-dump: end" "$art/panel.log" 2>/dev/null || true)
  "$SMOKE_TOPBAR" popover show surface-dump >/dev/null 2>&1 || true
  waited=0
  while [ "$waited" -lt 20 ]; do
    if [ "$(grep -c "ui-dump: end" "$art/panel.log" 2>/dev/null || true)" -gt "$before" ]; then
      return 0
    fi
    sleep 0.5
    waited=$((waited + 1))
  done
  echo "smoke-notifications: the panel never answered a dump" >&2
  return 1
}

# One control's rectangle, as `<x> <y> <w> <h>`, from the last dump.
#
#   rect_of Telegram              whatever has that word on it
#   rect_of notification-close 2  the second row close button
#
# The pattern is matched against "<GtkType> <classes> <label>", so a class, a
# widget type and the text on a control are all usable. Fails loudly when there
# is no such control, which is a failure worth having: it means the thing being
# clicked is not on screen.
rect_of() {
  python3 - "$art/panel.log" "$1" "${2:-1}" <<'PY'
import re, sys

log, pattern, index = sys.argv[1], sys.argv[2], int(sys.argv[3])
text = re.sub(r"\x1b\[[0-9;]*m", "", open(log, errors="replace").read())

# The last dump only: the panel is dumped again after every step.
start = text.rfind("ui-dump: begin")
if start < 0:
    sys.exit("no dump in the log")
end = text.find("ui-dump: end", start)
block = text[start : end if end > 0 else len(text)]

line_re = re.compile(
    r'ui-dump: (\S+) \[([^\]]*)\] "([^"]*)" (-?\d+) (-?\d+) (\d+) (\d+)'
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

# The mapped banner surface's height in the latest layout dump.
toast_height() {
  python3 - "$art/panel.log" <<'PY'
import re, sys
text = open(sys.argv[1], errors="replace").read()
block = text[text.rfind("ui-dump: begin"):]
match = re.search(r'ui-dump: surface "topbar-toast" -?\d+ -?\d+ \d+ (\d+)', block)
if not match:
    sys.exit("no mapped banner surface in the last dump")
print(match.group(1))
PY
}

# How many controls in the last dump match a pattern.
count_of() {
  tally=0
  while rect_of "$1" $((tally + 1)) >/dev/null 2>&1; do
    tally=$((tally + 1))
  done
  echo "$tally"
}

# Dump until `$1` is somewhere on screen, or give up loudly.
dump_until() {
  tries=0
  while [ "$tries" -lt 10 ]; do
    dump
    if rect_of "$1" "${2:-1}" >/dev/null 2>&1; then
      return 0
    fi
    tries=$((tries + 1))
  done
  echo "smoke-notifications: $1 #${2:-1} never appeared" >&2
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
    echo "smoke-notifications: cannot find $1 #${2:-1}" >&2
    return 1
  }
  echo "smoke-notifications: click $1 #${2:-1} at $where"
  # shellcheck disable=SC2086
  click_at $where
}

# Park the pointer on a control without pressing it, for a hover screenshot.
hover_on() {
  where=$(centre_of "$1" "${2:-1}") || {
    echo "smoke-notifications: cannot find $1 #${2:-1}" >&2
    return 1
  }
  echo "smoke-notifications: hover $1 #${2:-1} at $where"
  # shellcheck disable=SC2086
  pointer_to $where
}

scroll_on() {
  where=$(centre_of "$1" "${2:-1}") || return 1
  # shellcheck disable=SC2086
  scroll_at $where "$3"
}

# Off every surface the panel has, so a screenshot is not taken with the
# pointer sitting on a hover state. The control panel is centred under the
# clock and the banners hang below the bar; the bottom-left corner is desktop.
pointer_park() {
  pointer_to 40 960
}

# Open the control panel by clicking the clock, which is what a user does.
open_panel() {
  check dump_until clock
  check click_on clock
  check assert_mapped topbar-popover "the clock opened the control panel"
  dump
}

close_panel() {
  click_at 200 960
  check assert_unmapped topbar-popover "click-away dismissed it"
}

# One notification, from an application that does not exist, on the private bus.
notify() {
  notify-send "$@" || echo "smoke-notifications: notify-send failed: $*" >&2
}

# The three applications the history scenarios are built from. Telegram gets
# three so there is a group to expand, and the desktop-entry hint is what makes
# two differently-spelled app names one group.
#
# Telegram asks for the icon called `telegram`, which is what the real client
# sends and which no icon theme on this machine has — Adwaita 50 dropped the
# whole family of legacy application names. That is deliberate: one of the three
# groups in every screenshot is there to show what a name the theme has never
# heard of falls back to.
populate() {
  notify -a Telegram -h string:desktop-entry:org.telegram.desktop \
    -i telegram "Ada Lovelace" "see you at six"
  notify -a Telegram -h string:desktop-entry:org.telegram.desktop \
    -i telegram "Ada Lovelace" "actually, make it seven"
  notify -a Telegram -h string:desktop-entry:org.telegram.desktop \
    -i telegram "Grace Hopper" "the patch is on the branch, have a look when you can"
  notify -a Fractal -i mail-unread-symbolic \
    "#topbar" "<b>Ada</b>: the banner lands under the bar now"
  notify -a "Software Updater" -i software-update-available-symbolic \
    "Updates available" "7 packages can be updated"
}

echo "=== output: $(pointer_size) ==="
if [ "$(pointer_size)" != "918 988" ]; then
  echo "smoke-notifications: this run wants the nested output at scale 1.0, where"
  echo "a logical pixel is a device pixel and a screenshot coordinate is a"
  echo "pointer coordinate. Set TOPBAR_SMOKE_SCALE=1.0."
  exit 1
fi

case "$scenario" in
  # The history column: empty, populated, expanded, and emptied again by hand.
  history)
    echo "--- the column with nothing in it"
    open_panel
    pointer_park
    check shot 01-empty topbar-popover
    close_panel

    echo "--- five notifications from three applications"
    populate
    sleep 6

    # The banners expire and the panel is shut: without the dot beside the time
    # there is nothing left on screen saying five things arrived, and the whole
    # history is invisible until somebody happens to click the clock. This is
    # the only step that can ask, because opening the panel is what clears it.
    echo "--- and a dot on the bar saying so"
    check dump
    dots=$(count_of clock-unseen)
    echo "smoke-notifications: unseen dots beside the time: $dots"
    [ "$dots" -eq 1 ] || {
      echo "smoke-notifications: five notifications and no dot beside the time" >&2
      fail=1
    }
    pointer_park
    check shot 01b-unseen

    open_panel
    pointer_park
    check shot 02-history topbar-popover

    echo "--- which opening the panel takes away again"
    check dump
    dots=$(count_of clock-unseen)
    echo "smoke-notifications: unseen dots after the open: $dots"
    [ "$dots" -eq 0 ] || {
      echo "smoke-notifications: the dot outlived the panel being opened" >&2
      fail=1
    }

    # One Tab into a panel that has just taken the keyboard. Whatever it lands
    # on has to say so: the column clears Adwaita's focus ring along with its
    # background images, and until this pass it drew nothing in its place. This
    # is the only step in the run that presses a key before it looks — a
    # pointer-driven panel must never draw a ring.
    echo "--- one Tab, and the focus has to be visible"
    key_press Tab
    pointer_park
    check shot 02b-focus-ring

    echo "--- the group header expands the stack"
    # By type *and* class *and* name. The pattern is matched against
    # "<GtkType> <classes> <label>", and the two single-notification groups
    # above Telegram carry the same class on a header that cannot expand at
    # all — clicking one of those photographs a panel that did nothing and
    # calls it an expansion, which is exactly what the first run of this
    # scenario did.
    rows_collapsed=$(count_of notification-row)
    check click_on "GtkButton notification-group-header Telegram"
    dump
    rows_expanded=$(count_of notification-row)
    echo "smoke-notifications: rows $rows_collapsed -> $rows_expanded"
    [ "$rows_expanded" -gt "$rows_collapsed" ] || {
      echo "smoke-notifications: the group header expanded nothing" >&2
      fail=1
    }
    pointer_park
    check shot 03-expanded

    echo "--- and a notification arriving elsewhere does not shut it"
    # The column throws its cards away and builds new ones whenever the history
    # changes. A group that closed itself because a message landed in another
    # application is a group that shut under the reader's hand.
    notify -a Fractal -i mail-unread-symbolic "#topbar" "one more, while a group is open"
    sleep 5
    dump
    rows_after_arrival=$(count_of notification-row)
    echo "smoke-notifications: rows $rows_expanded -> $rows_after_arrival after an arrival"
    # Not "as many as before": the arrival is Fractal's *second* notification,
    # so that group stops being a group of one — it collapses behind its own
    # preview line and takes its row with it. Telegram's three are the point,
    # and if the rebuild had closed Telegram too there would be one row left in
    # the whole column.
    [ "$rows_after_arrival" -gt "$rows_collapsed" ] || {
      echo "smoke-notifications: the open group collapsed when something arrived" >&2
      fail=1
    }
    pointer_park
    check shot 03b-still-expanded

    echo "--- a row reveals its close button under the pointer"
    check hover_on notification-row 1
    check shot 04-row-hover
    echo "--- and pressing it takes that one notification away"
    rows_before=$(count_of notification-row)
    check click_on notification-close 1
    dump
    rows_after=$(count_of notification-row)
    echo "smoke-notifications: rows $rows_before -> $rows_after"
    [ "$rows_after" -lt "$rows_before" ] || {
      echo "smoke-notifications: the close button removed nothing" >&2
      fail=1
    }
    pointer_park
    check shot 05-row-closed

    echo "--- the trash button clears a whole application"
    groups_before=$(count_of notification-group-header)
    check click_on notification-group-clear 1
    dump
    groups_after=$(count_of notification-group-header)
    echo "smoke-notifications: groups $groups_before -> $groups_after"
    [ "$groups_after" -lt "$groups_before" ] || {
      echo "smoke-notifications: the group clear removed nothing" >&2
      fail=1
    }
    pointer_park
    check shot 06-group-cleared

    echo "--- Do Not Disturb, which the bar has to show as well"
    check click_on GtkSwitch 1
    dump
    pointer_park
    check shot 07-dnd-on
    echo "--- and a notification sent while it is on gets no banner"
    notify -a Telegram "Silenced" "this one waits in the history"
    sleep 5
    if pointer_mapped topbar-toast; then
      echo "smoke-notifications: a banner appeared with Do Not Disturb on" >&2
      fail=1
    else
      echo "smoke-notifications: no banner while Do Not Disturb is on"
    fi
    dump
    pointer_park
    check shot 08-dnd-history
    check click_on GtkSwitch 1
    dump

    echo "--- Clear empties the column, and the header goes with it"
    check click_on notification-clear-all
    dump
    pointer_park
    check shot 09-cleared
    if rect_of notification-clear-all >/dev/null 2>&1; then
      echo "smoke-notifications: Clear is still on screen over an empty column" >&2
      fail=1
    else
      echo "smoke-notifications: Clear went with the last notification"
    fi
    close_panel
    ;;

  # The banners: arrival, hover-pause, actions, close, the stack, critical.
  banners)
    echo "--- one banner, with two actions on it"
    # `-A` implies `--wait`, so this stays alive until the action is clicked and
    # prints the key it was given. That is the assertion: the pill went all the
    # way to `ActionInvoked` on the bus and back to the sender.
    notify-send -a Telegram -h string:desktop-entry:org.telegram.desktop \
      -i telegram -t 60000 \
      -A reply=Reply -A later="Remind me later" \
      "Ada Lovelace" "see you at six" >"$art/action.out" 2>&1 &
    sender=$!
    check shot 01-banner topbar-toast
    dump

    echo "--- hovering it stalls the timer"
    check hover_on "GtkBox vertical toast" 1
    dump
    check shot 02-banner-hover
    grep -a "banner .* paused" "$art/panel.log" >/dev/null 2>&1 ||
      echo "smoke-notifications: the panel never reported a pause" >&2

    echo "--- and the action pill answers a click"
    # By type and class as well as by the word on it: the banner itself is
    # locatable — it has to be, for the hover — and its label list is every
    # label inside it, including the ones on its own buttons. `click_on Reply`
    # pressed the *banner*, which took the "I have read it" path, dismissed the
    # thing the sender was waiting on and reported an action that never fired.
    check click_on "GtkButton toast-action Reply"
    waited=0
    while [ "$waited" -lt 15 ]; do
      grep -q reply "$art/action.out" 2>/dev/null && break
      sleep 1
      waited=$((waited + 1))
    done
    wait "$sender" 2>/dev/null || true
    if grep -q "^reply$" "$art/action.out" 2>/dev/null; then
      echo "smoke-notifications: the sender was told the action was invoked"
    else
      echo "smoke-notifications: no ActionInvoked reached the sender" >&2
      echo "--- what it did say:"
      cat "$art/action.out" 2>/dev/null || true
      fail=1
    fi
    check assert_unmapped topbar-toast "the action closed the banner"

    echo "--- a short-lived banner outlives its timer while the pointer is on it"
    notify -a Fractal -i mail-unread-symbolic -t 3000 \
      "#topbar" "three seconds, and the pointer is about to stop the clock"
    check assert_mapped topbar-toast "the short banner arrived"
    dump
    check hover_on "GtkBox vertical toast" 1
    # Well past its three seconds. A timer that had kept running would have
    # taken the banner away by now.
    sleep 8
    if pointer_mapped topbar-toast; then
      echo "smoke-notifications: the hovered banner outlived its timeout"
    else
      echo "smoke-notifications: the hovered banner expired anyway" >&2
      fail=1
    fi
    check snap 03-banner-paused
    echo "--- and leaves once the pointer does"
    pointer_park
    check assert_unmapped topbar-toast "the resumed timer ran out"

    echo "--- the close button dismisses one by hand"
    notify -a Telegram -i chat-message-new-symbolic -t 60000 \
      "Grace Hopper" "closing this by hand"
    check assert_mapped topbar-toast "the banner to close arrived"
    dump
    check hover_on "GtkBox vertical toast" 1
    check shot 04-close-revealed
    check click_on toast-close
    check assert_unmapped topbar-toast "the close button dismissed it"

    echo "--- dismissing the first of two banners shrinks the mapped surface"
    notify -a Telegram -i chat-message-new-symbolic -t 60000 "Ada Lovelace" "one"
    notify -a Fractal -i mail-unread-symbolic -t 60000 "#topbar" "two"
    check shot 05-two topbar-toast
    check dump
    before=$(toast_height) || { fail=1; before=0; }
    check hover_on "GtkBox vertical toast" 2
    check click_on toast-close 2
    check dump
    after=$(toast_height) || { fail=1; after=0; }
    remaining=$(count_of "GtkBox vertical toast")
    if [ "$remaining" -ne 1 ] || [ "$after" -ge "$before" ]; then
      echo "smoke-notifications: dismissed one of two banners but the surface stayed ${before}px high (${after}px with $remaining left)" >&2
      fail=1
    fi
    pointer_park
    check shot 05-one topbar-toast

    echo "--- three at once, and a critical one that will not go away"
    notify -a Telegram -i chat-message-new-symbolic -t 60000 "Grace Hopper" "three"
    notify -a "Software Updater" -i software-update-available-symbolic -t 60000 \
      "Updates" "four"
    check shot 05-stack topbar-toast
    dump
    stacked=$(count_of "GtkBox vertical toast")
    echo "smoke-notifications: $stacked banners on screen"
    [ "$stacked" = "3" ] || {
      echo "smoke-notifications: expected three banners, found $stacked" >&2
      fail=1
    }

    echo "--- a critical banner evicts the oldest ordinary one"
    notify -a Battery -u critical -i battery-level-10-symbolic "Battery low" "5% remaining"
    sleep 4
    dump
    pointer_park
    check shot 06-critical
    ;;

  # The shapes a real desktop produces that a tidy fixture never does.
  edges)
    echo "--- a long application name, a huge body, three long actions, markup"
    # In the background because `-A` implies `--wait`: a foreground sender
    # blocks the driver and then takes its own notification away with it, which
    # is a photograph of the desktop where the banner should be.
    notify-send -t 120000 -a "Mokrinskiy Corporate Messenger Enterprise Edition" \
      -i mail-unread-symbolic \
      -A read="Mark everything as read" -A later="Remind me tomorrow morning" \
      -A open=Open \
      "A summary far longer than the column it has to fit inside" \
      "The body is longer still: it runs past two lines and has to end in an ellipsis rather than half a letter, which is what a clipped label looks like when nobody checks. It also carries <b>bold</b>, <i>italic</i> and <u>underlined</u> markup, an unclosed <b>tag, an escaped ampersand (Tom & Jerry) and an <img src=x> element no notification has any business sending." \
      >"$art/long-action.out" 2>&1 &
    long_sender=$!
    notify -t 120000 -a Telegram -h string:desktop-entry:org.telegram.desktop \
      -i telegram "Ada" "short one"
    sleep 4
    check shot 01-banner-long topbar-toast
    kill "$long_sender" 2>/dev/null || true

    echo "--- an icon from a file on disk, and one the theme has never heard of"
    notify -t 120000 -a Camera -i "$SMOKE_ARTIFACTS/icon.png" \
      "From a file" "image-path resolution"
    sleep 3
    check shot 02-icon-file topbar-toast
    notify -t 120000 -a Nothing "No icon at all" "the generic glyph is the last resort"
    sleep 3
    check shot 03-icon-fallback topbar-toast

    echo "--- sixty notifications, which is what a day away from the desk looks like"
    index=1
    while [ "$index" -le 60 ]; do
      notify -a "App $((index % 7))" "Notification $index" "body of number $index"
      index=$((index + 1))
    done
    sleep 8
    open_panel
    pointer_park
    check shot 04-long-history topbar-popover

    echo "--- the column scrolls inside itself rather than growing"
    check scroll_on notification-row 1 5
    check shot 05-scrolled
    check scroll_on notification-row 1 -5

    echo "--- a replacement lands inside a group that is open"
    check dump_until notification-group-header
    check click_on notification-group-header 1
    dump
    pointer_park
    check shot 06-expanded-before-replace
    id=$(notify-send -p -a Telegram -h string:desktop-entry:org.telegram.desktop \
      -i telegram "Ada" "the first version" 2>/dev/null || echo "")
    sleep 3
    if [ -n "$id" ]; then
      notify -r "$id" -a Telegram -h string:desktop-entry:org.telegram.desktop \
        -i telegram "Ada" "the replacement, in the same place"
    else
      echo "smoke-notifications: notify-send printed no id to replace" >&2
      fail=1
    fi
    sleep 4
    dump
    pointer_park
    check shot 07-replaced
    close_panel
    ;;
esac

{
  echo "--- what the daemon logged ---"
  grep -aE "notification|banner" "$art/panel.log" | tail -40 || true
} >"$art/notifications.txt" 2>&1
cat "$art/notifications.txt"

if [ "$fail" -eq 0 ]; then
  echo "--- result: PASS"
else
  echo "--- result: FAIL"
fi
exit "$fail"
