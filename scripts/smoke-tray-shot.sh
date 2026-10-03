#!/usr/bin/env sh
# One tray scenario, driven inside the nested niri session. Driven by
# scripts/smoke-tray.sh; not useful alone.
#
# $SMOKE_TRAY_SCENARIO names what to set up. Every scenario starts its
# applications with $SMOKE_FAKE_SNI, waits for each of them to say it has taken
# its name, and only then takes a picture — see scripts/smoke-shot.sh for why
# waiting on evidence rather than on the clock has teeth here.
set -eu

art="$SMOKE_ARTIFACTS"
. "$(dirname "$0")/smoke-shot.sh"
. "$(dirname "$0")/smoke-pointer.sh"

scenario="${SMOKE_TRAY_SCENARIO:-basic}"
sni="${SMOKE_FAKE_SNI:-}"
if [ -z "$sni" ] && [ "$scenario" != "empty" ]; then
  echo "smoke-tray: \$SMOKE_FAKE_SNI is not set; TOPBAR_SMOKE_TRAY was off" >&2
  exit 1
fi

pids=""

# Start one fake application and wait until it is really on the bus.
#
# The binary prints its bus name and then the identifier the panel will know it
# by; reading the first line is how "it started" is established rather than
# assumed. A `sleep` here would be the same coin toss the screenshot helper
# exists to avoid.
start() {
  name=$1
  shift
  out="$art/sni-$name.log"
  : >"$out"
  # shellcheck disable=SC2086
  "$sni" --name "$name" "$@" >"$out" 2>&1 &
  pids="$pids $!"

  waited=0
  while [ "$waited" -lt 20 ]; do
    if [ "$(wc -l <"$out")" -ge 2 ]; then
      echo "smoke-tray: $name is on the bus as $(head -1 "$out")"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "smoke-tray: $name never took a name on the bus" >&2
  cat "$out" >&2
  return 1
}

# The well-known name of an application started with `start`.
bus_name() {
  head -1 "$art/sni-$1.log"
}

# Call something on a fake application's control interface.
control() {
  name=$1
  method=$2
  shift 2
  gdbus call --session \
    --dest "$(bus_name "$name")" \
    --object-path /StatusNotifierItem \
    --method "io.github.trevarj.topbar.FakeSni1.$method" \
    "$@" >>"$art/control.log" 2>&1
}

# An icon directory of the kind an application ships with itself, so the
# IconThemePath branch is exercised against a real file rather than asserted.
make_theme_dir() {
  mkdir -p "$art/icons"
  magick -size 22x22 xc:none -fill '#e01b24' \
    -draw 'circle 11,11 11,3' "$art/icons/own-brand.png"
  echo "$art/icons"
}

case "$scenario" in
  # (a) three items with three different kinds of icon.
  basic)
    theme=$(make_theme_dir)
    start themed --title "Themed Item" --icon-name folder-remote-symbolic \
      --tooltip "Themed Item" --tooltip-body "A Freedesktop icon name"
    start branded --title "Own Icons" --icon-name own-brand --theme-path "$theme" \
      --tooltip "Own Icons" --tooltip-body "Loaded from the application's own directory"
    start colourful --title "Colour Pixmap" --pixmap 22x14:ff3584e4 \
      --tooltip "Colour Pixmap" --tooltip-body "A non-square ARGB pixmap"
    start dim --title "Near Black" --pixmap 22x22:ff181818 \
      --tooltip "Near Black" --tooltip-body "Grayscale, and lifted to be legible"
    shot tray
    ;;

  # (b) and (c): the menu, and a submenu of it.
  menu | submenu)
    start menued --title "Menued Item" --icon-name mail-unread-symbolic --default-menu
    if [ "$scenario" = menu ]; then
      dbus-monitor --session "type='method_call',interface='com.canonical.dbusmenu'" \
        >"$art/menu-events.log" 2>"$art/menu-monitor.stderr" &
      pids="$pids $!"
      "$SMOKE_TOPBAR" popover show tray-menu
      waited=0
      while ! grep -q "member=GetLayout" "$art/menu-events.log"; do
        [ "$waited" -lt 100 ] || { echo "private menu monitor never became ready" >&2; exit 1; }
        gdbus call --session --dest "$(bus_name menued)" --object-path /MenuBar \
          --method com.canonical.dbusmenu.GetLayout 0 0 '[]' >/dev/null
        sleep .1
        waited=$((waited + 1))
      done
    fi
    shot tray topbar-popover
    if [ "$scenario" = menu ]; then
      picker_dump
      picker_read disabled "Not Available"
      before=$(grep -c "member=Event" "$art/menu-events.log" || true)
      picker_probe "GtkButton tray-menu-row picker-option" "Filter menu choices" awy
      after=$(grep -c "member=Event" "$art/menu-events.log" || true)
      [ "$before" -eq "$after" ]
      picker_focus "GtkButton tray-menu-row picker-option"
      type_text awy
      picker_click More
      picker_dump
      [ "$(picker_read count picker-search)" -eq 0 ]
      picker_click tray-menu-back
      picker_dump
      picker_read query "Filter menu choices" ""
      picker_focus "GtkButton tray-menu-row picker-option"
      type_text awy
      picker_focus "GtkButton tray-menu-row picker-option"
      key_press Return
      assert_unmapped topbar-popover
      python3 - "$art/menu-events.log" <<'PY'
import pathlib, re, sys, time
path = pathlib.Path(sys.argv[1])
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    if re.search(r'member=Event\s+int32 5\s+string "clicked"', path.read_text()):
        break
    time.sleep(.1)
else:
    raise SystemExit("filtered Away activation did not send original dbusmenu node 5")
PY
      "$SMOKE_TOPBAR" popover show tray-menu
      assert_mapped topbar-popover
      picker_dump
      picker_read query "Filter menu choices" ""
      shot tray-filter-reopened topbar-popover
    fi
    ;;

  # (d) an item flipping to NeedsAttention: before, and after.
  attention)
    start shouty --title "Attention" --icon-name mail-unread-symbolic
    shot tray-before
    control shouty SetStatus "NeedsAttention"
    # Long enough for both pulse cycles to finish, so what is photographed is
    # the steady tint the pulse settles into rather than a frame of the pulse.
    sleep 2
    shot tray-after
    ;;

  # (e) fourteen items: eleven inline plus a chevron, and what is behind it.
  overflow | overflow-open)
    index=1
    while [ "$index" -le 14 ]; do
      start "item$(printf '%02d' "$index")" --title "Item $index" \
        --icon-name application-x-executable
      index=$((index + 1))
    done
    if [ "$scenario" = "overflow-open" ]; then
      shot tray topbar-popover
    else
      shot tray
    fi
    ;;

  # (f) an item leaving, and a burst of re-registrations that must not flicker.
  churn)
    start staying --title "Staying" --icon-name folder-symbolic
    start leaving --title "Leaving" --icon-name user-trash-symbolic
    shot tray-both

    # Five re-registrations in a fifth of a second, the way a chat client
    # reconnecting behaves. Nothing on the bar may move.
    burst=0
    while [ "$burst" -lt 5 ]; do
      control staying Reregister
      burst=$((burst + 1))
    done
    sleep 1
    shot tray-reregistered

    control leaving Quit
    sleep 2
    shot tray-one
    ;;

  # (g) no tray applications at all: the widget must not be on the bar.
  empty)
    shot tray-empty
    ;;

  *)
    echo "smoke-tray: unknown scenario '$scenario'" >&2
    exit 1
    ;;
esac

niri msg layers >"$art/layers.txt" 2>&1 || true

# The applications are killed with the session, but doing it here keeps a
# scenario that failed from leaving them behind on a bus that outlives it.
for pid in $pids; do
  kill "$pid" 2>/dev/null || true
done
