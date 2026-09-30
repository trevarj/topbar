#!/usr/bin/env sh
# The media matrix, driven inside the nested niri session.
#
#   nix develop -c env TOPBAR_SMOKE_OPEN=clock TOPBAR_SMOKE_PLAYERS=1 \
#     TOPBAR_SMOKE_TIMEOUT=180 TOPBAR_SMOKE_DRIVER=scripts/smoke-media.sh \
#     TOPBAR_VISUAL_CONFIG=crates/topbar-core/tests/fixtures/live-config.toml \
#     ./scripts/visual-smoke-niri.sh target/visual-smoke/media
#
# It screenshots the control panel with no player, with one, with two (the
# switcher), and after relevance moves the card by itself; then it swaps track
# and cover several hundred times and reports the panel's VmRSS, which is how
# "the media card does not leak" stops being an opinion.
#
# Everything runs against the private bus `visual-smoke-niri.sh` set up. The
# players are `topbar-fake-player`, and the only way to reach into them is
# their own control interface, which no real player has.
set -eu

art="$SMOKE_ARTIFACTS"
player="${SMOKE_FAKE_PLAYER:-}"
if [ -z "$player" ]; then
  echo "smoke-media: set TOPBAR_SMOKE_PLAYERS=1 so the fake player is built" >&2
  exit 1
fi

# How many times the track is swapped, and how many of those swaps also change
# the cover slowly enough to beat the service's 200ms art debounce.
swaps=500
art_swaps=40
# Distinct cover files, so the panel's texture cache is cycled rather than hit.
covers=20

# --- the covers, written from here so the run needs no image tools ----------
base64 -d >"$art/cover-a.png" <<'PNG'
iVBORw0KGgoAAAANSUhEUgAAAGAAAABgCAMAAADVRocKAAAAIGNIUk0AAHomAACAhAAA+gAAAIDo
AAB1MAAA6mAAADqYAAAXcJy6UTwAAAASUExURWeljlaKd////0hyYzdXSyI1LqbjWGYAAAABYktH
RAJmC3xkAAAAB3RJTUUH6ggEDCMRGRrgJQAAACV0RVh0ZGF0ZTpjcmVhdGUAMjAyNi0wOC0wNFQx
MjozNToxNiswMDowMI4foawAAAAldEVYdGRhdGU6bW9kaWZ5ADIwMjYtMDgtMDRUMTI6MzU6MTYr
MDA6MDD/QhkQAAAAKHRFWHRkYXRlOnRpbWVzdGFtcAAyMDI2LTA4LTA0VDEyOjM1OjE2KzAwOjAw
qFc4zwAAAWdJREFUaN7tkEmCQyEIRDXA/a+c/g6ARP1D9ypdtQG14AkpQRAEQdDXKOceswuW5OBb
1Nn9+J6LemahJ+OrP83y6E8hLXqtpL/Kef7+8dk6juOXsAfUOU4AoaMOWa+XAOfav7u15Yl+D3Df
BQAAAG4CSC20ABCZJx/5QwAtADQD/ARqR9oUF0BpQLsVFYN2IkrUqD7uAXQGUE8FTHQGOMpXK4y9
HgIWnkkvAAD4ZsBVATAXTwDMll0BeD/zsj+l/nyEblwC2Np5v8+P9urgEcA9vwjgWT46CsAGPgU4
Z1jOgPJvBcDheT1BHT/6x3iYNK8ritoAbgsAAAAAAIA/BIi0WLINQKrb/PHcMvUlM6h5CxDpHxL3
tbHeh1Tbq8ppDWh/HfzxXGDqS308sTVtALYO0b9/1vulJyPWyHIFYP5ZvcfYBHJhgt4k+OPZkA0Q
tQMIBzPLVvcBN5Ue1AAAAAAAAADAPwS8AZ51XtudxMjvAAAAAElFTkSuQmCC
PNG
base64 -d >"$art/cover-b.png" <<'PNG'
iVBORw0KGgoAAAANSUhEUgAAAGAAAABgCAMAAADVRocKAAAAIGNIUk0AAHomAACAhAAA+gAAAIDo
AAB1MAAA6mAAADqYAAAXcJy6UTwAAAAVUExURcR4sadmlrmhs/7+/oZTekwwRkIqProu5KEAAAAH
dElNRQfqCAQMIxEZGuAlAAAAJXRFWHRkYXRlOmNyZWF0ZQAyMDI2LTA4LTA0VDEyOjM1OjE2KzAw
OjAwjh+hrAAAACV0RVh0ZGF0ZTptb2RpZnkAMjAyNi0wOC0wNFQxMjozNToxNiswMDowMP9CGRAA
AAAodEVYdGRhdGU6dGltZXN0YW1wADIwMjYtMDgtMDRUMTI6MzU6MTcrMDA6MDAOIDN7AAABxUlE
QVRo3u2X63qDMAiGIULu/5LXJiZGKwSsW7dnfD/MQfheSTy0AKFQ6BYhOoP2faxHBBzPOOJbtwra
6WG+zHUbOR7P4lvYMWGc30z88Wsx2JshGo+J3WCMBz3+3O9GBSAAAfhvAEhLVXJkmUObeVO6FQDL
mUy5pqBF0j2AtMiaL9QcoPkbCA1ASFTb/RiXmdYUomfCrl8GUAdl7qyd+teNoKaxX0aARB372hr8
C4FoWxQaBkQ6IJkAiQZPGgBUANiqeW1t/itBECjnbAtUN/oSwFqAWoICsBeglaAA7AVoJcgATwFK
CTLAU4BSggxw+S+LG+BboR8AoBfg2wJ5Ez4HcPqLmxCAXwz4+7fptz/Jt77s+KHnsU2W3gUAV6Pe
NkDt8wHt/x5wv7h+pU/AChugtc8uAG8+tPOBbYG4V1Zb1zeZJR9gUZ5dINFFAbDjdxFfAth3gS4C
rA8b8kWAcZESXwbYFonfAFgI/BZgTuA3AZN9SLP0OUC9l3CabQDIRUwv3wLIOT/eGmeIRAZ/hrzz
ynnXaYCThcJDnJRfANkkbn9rE9KQMskXAVZs5qzmg5hnBUzyPwe4SwEIQAACEIAABMCkL2XUd8Dm
tgXCAAAAAElFTkSuQmCC
PNG

index=0
while [ "$index" -lt "$covers" ]; do
  if [ $((index % 2)) -eq 0 ]; then
    cp "$art/cover-a.png" "$art/cover-$index.png"
  else
    cp "$art/cover-b.png" "$art/cover-$index.png"
  fi
  index=$((index + 1))
done

# --- helpers ----------------------------------------------------------------

# Call a fake player's control interface: control <name> <Method> [args...]
control() {
  name=$1
  method=$2
  shift 2
  gdbus call --session \
    --dest "org.mpris.MediaPlayer2.$name" \
    --object-path /org/mpris/MediaPlayer2 \
    --method "io.github.trevarj.topbar.FakePlayer1.$method" \
    "$@" >/dev/null
}

# The panel's resident set, in kilobytes.
rss() {
  awk '/^VmRSS:/ { print $2 }' "/proc/$SMOKE_PANEL_PID/status"
}

# Screenshots come from the shared helper, which waits on evidence rather than
# on a clock: the popover has to be mapped, something has to be drawn below the
# bar, and two consecutive captures have to agree. This script used to sleep
# three seconds and hope, and the media card is one of the two the comment at
# the top of smoke-shot.sh is about.
. "$(dirname "$0")/smoke-shot.sh"

# Every capture here is of the control panel, which is a popover.
card() {
  shot "$1" topbar-popover
  echo "smoke-media: $1"
}

# --- (c) nothing on the bus -------------------------------------------------
# The panel opened its control panel a second in; with no player there must be
# no media card at all and the column must look exactly like M4's.
card media-0-no-players

# --- (a) one player ---------------------------------------------------------
# CanGoNext is off, so the next button has to be visibly dimmed. The title must
# wrap and stop at three lines without widening the panel beside the larger art.
"$player" --name smokeone --identity "Aurora Player" --desktop-entry org.gnome.Music \
  --title "Windowlicker — A Very Long Live Recording From The Evening Session With An Extended Introduction And A Slow Ending That Must Wrap Across The Media Card Rather Than Widen The Control Panel Beyond Its Normal Size" \
  --artist "Aphex Twin" --album "Windowlicker" \
  --art "file://$art/cover-a.png" --status Playing \
  --length 221000000 --position 72000000 --no-next >/dev/null 2>&1 &
one=$!
card media-1-one-player

# --- (b) two players --------------------------------------------------------
"$player" --name smoketwo --identity "Beacon Player" --title "Avril 14th" \
  --artist "Aphex Twin" --album "Drukqs" --art "file://$art/cover-b.png" \
  --status Paused --length 120000000 >/dev/null 2>&1 &
two=$!
card media-2-two-players

# WordChar must also break a title with no word boundaries, with both switcher
# buttons still visible and the cover retaining its square allocation.
control smokeone SetTrack \
  "WindowlickerAnUnbrokenTitleThatMustWrapEvenWithoutSpacesAndStopAfterThreeLinesRatherThanWideningTheControlPanelOrPushingThePlayerSwitcherOutsideTheCardWindowlickerAnUnbrokenTitleThatMustWrapEvenWithoutSpaces" \
  "Aphex Twin" "file://$art/cover-a.png" 221000000
card media-2-unbroken-title

# The card follows what is playing without anyone clicking anything: the first
# player stops, the second starts, and the switcher's ring moves with it.
control smokeone SetStatus '"Paused"'
control smoketwo SetStatus '"Playing"'
card media-3-relevance-moved

# The other half of the rule — a pin, which outranks all of this — is only
# reachable by clicking a switcher button, and there is no synthetic pointer
# in the dev shell. It is covered by the bus tests instead.

# --- the leak check ---------------------------------------------------------
before=$(rss)
index=0
while [ "$index" -lt "$swaps" ]; do
  control smoketwo SetTrack "Track $index" "Aphex Twin" \
    "file://$art/cover-$((index % covers)).png" 120000000
  index=$((index + 1))
  if [ "$index" -eq 50 ]; then
    sleep 1
    warm=$(rss)
  fi
done
sleep 2
after_swaps=$(rss)

# Slowly enough that the 200ms art debounce lets every cover through, so the
# decode and texture-cache path is exercised too rather than collapsed away.
index=0
while [ "$index" -lt "$art_swaps" ]; do
  control smoketwo SetTrack "Cover $index" "Aphex Twin" \
    "file://$art/cover-$((index % covers)).png" 120000000
  sleep 0.3
  index=$((index + 1))
done
sleep 2
after=$(rss)
card media-4-after-churn

{
  echo "swaps=$swaps art_swaps=$art_swaps"
  echo "rss_before_kb=$before"
  echo "rss_warm_kb=${warm:-$before}"
  echo "rss_after_swaps_kb=$after_swaps"
  echo "rss_after_kb=$after"
} | tee "$art/media-rss.txt"

awk -v warm="${warm:-$before}" -v after="$after" 'BEGIN {
  growth = (after - warm) / warm * 100
  printf "smoke-media: RSS %d kB -> %d kB after warmup (%+.1f%%)\n", warm, after, growth
  if (growth > 10) {
    print "smoke-media: the panel grew more than 10% after warmup" > "/dev/stderr"
    exit 1
  }
}'

# --- a track with no cover --------------------------------------------------
# The placeholder, not the last track's picture: art is cleared on the same
# grace period it is fetched on.
control smoketwo SetTrack "Nothing To Look At" "Nobody" "" 90000000
card media-5-no-cover

# --- the players go away ----------------------------------------------------
kill "$one" "$two" 2>/dev/null || true
card media-6-players-gone
