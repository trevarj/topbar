#!/usr/bin/env sh
# The weather matrix, driven inside the nested niri session.
#
# Every run points the panel at scripts/weather-stub.py rather than the real
# Open-Meteo, so the screenshots are reproducible and the run needs no network:
#
#   nix develop -c ./scripts/smoke-weather.sh
#
# It stands the stub up, then runs the nested session once per state, because
# TOPBAR_SMOKE_OPEN can only open one surface per start:
#
#   1  ready        bar label and the weather popover, five days of forecast
#   2  bare         show_description = false: the icon and the temperature and
#                   nothing else, which is a run about the bar by itself
#   3  panel        the control panel, forecast card filled, no media card
#   4  configure    cold start with no location: the "Configure…" label
#   5  setup        the location dialog, with results from a seeded search
#   6  saved        the same config as (4), but with a location in state.json,
#                   which is what a panel started after a Save sees
#   7  unavailable  the stub answering 429, so the empty state is drawn
#
# Screenshots land in target/visual-smoke/weather/<state>/.
set -eu

artifact_root="${1:-target/visual-smoke/weather}"
port="${TOPBAR_SMOKE_STUB_PORT:-18080}"
mkdir -p "$artifact_root"
artifact_root=$(cd "$artifact_root" && pwd)

for tool in python3 magick curl niri grim cargo timeout dbus-run-session; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing required tool: $tool" >&2
    exit 1
  fi
done

repo=$(pwd)
live_config=crates/topbar-core/tests/fixtures/live-config.toml

# The live config has no coordinates in it — the user picked their location in
# v1's dialog — so a second config with some is written for the runs that need
# a forecast on screen. Moscow, which is what the fixtures are for.
located_config="$artifact_root/located-config.toml"
sed 's/^\[widgets.weather\]$/[widgets.weather]\nlatitude = 55.75204\nlongitude = 37.61781/' \
  "$live_config" >"$located_config"
grep -q '^latitude' "$located_config" || {
  echo "could not add coordinates to the config" >&2
  exit 1
}

# And a third with the condition taken off the bar. The reading behind it is
# identical, so the only difference between this run's screenshot and (1)'s bar
# is the words — which is the whole claim `show_description` makes.
bare_config="$artifact_root/bare-config.toml"
sed 's/^\[widgets.weather\]$/[widgets.weather]\nshow_description = false/' \
  "$located_config" >"$bare_config"
grep -q '^show_description = false' "$bare_config" || {
  echo "could not turn the weather description off in the config" >&2
  exit 1
}

stub_pid=""
stop_stub() {
  if [ -n "$stub_pid" ]; then
    kill "$stub_pid" 2>/dev/null || true
    wait "$stub_pid" 2>/dev/null || true
    stub_pid=""
  fi
}
trap stop_stub EXIT INT TERM

# Start the stub answering with `$1`, and refuse to go on unless it is really
# the one answering.
#
# A stub left over from an earlier run holds the port, the new one dies with
# EADDRINUSE, and every scenario is then quietly served by the *old* one — an
# entire matrix of screenshots showing the wrong thing while claiming to show
# the right one. Asking the port what status it gives back is the only way to
# know which process is behind it.
start_stub() {
  want=$1
  stop_stub
  python3 "$repo/scripts/weather-stub.py" --port "$port" --status "$want" \
    >"$artifact_root/stub-$want.log" 2>&1 &
  stub_pid=$!

  waited=0
  while [ "$waited" -lt 10 ]; do
    got=$(curl -s -o /dev/null -w '%{http_code}' \
      "http://127.0.0.1:$port/forecast?probe=1" 2>/dev/null || echo 000)
    if [ "$got" = "$want" ]; then
      echo "smoke-weather: stub answering $want on port $port"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done

  echo "smoke-weather: nothing on port $port answers $want (got '$got')." >&2
  echo "smoke-weather: a stray stub is probably holding it — check" >&2
  echo "  pgrep -af weather-stub.py" >&2
  cat "$artifact_root/stub-$want.log" >&2 2>/dev/null || true
  exit 1
}

export TOPBAR_WEATHER_API="http://127.0.0.1:$port/forecast"
export TOPBAR_GEOCODING_API="http://127.0.0.1:$port/search"

# What the setup dialog writes when a location is saved. Seeding a run with it
# is the same thing as starting the panel again after a Save.
saved_state="$artifact_root/saved-state.json"
cat >"$saved_state" <<'JSON'
{
  "weather": {
    "location": {
      "label": "Moscow — Moscow, Russia",
      "latitude": 55.75222,
      "longitude": 37.61556
    }
  }
}
JSON

# One nested session: run <name> <config> [open] [query] [state]
#
# What `open` asks for decides what the capture has to wait to be *drawn*, not
# merely mapped: a popover for a widget name, the dialog for `weather-setup`,
# and nothing at all for a run that is about the bar by itself.
run() {
  name=$1
  config=$2
  open=${3:-}
  query=${4:-}
  state=${5:-}

  case "$open" in
    "") export SMOKE_EXPECT="" ;;
    *-setup) export SMOKE_EXPECT="topbar-dialog" ;;
    *) export SMOKE_EXPECT="topbar-popover" ;;
  esac

  if [ -n "$state" ]; then
    export TOPBAR_SMOKE_STATE="$state"
  else
    unset TOPBAR_SMOKE_STATE
  fi

  echo "smoke-weather: $name"
  # Unset rather than empty: an empty TOPBAR_SMOKE_OPEN asks the panel to open
  # a widget called "", which it rightly complains about in panel.log.
  if [ -n "$open" ]; then
    export TOPBAR_SMOKE_OPEN="$open"
  else
    unset TOPBAR_SMOKE_OPEN
  fi
  if [ -n "$query" ]; then
    export TOPBAR_SMOKE_QUERY="$query"
  else
    unset TOPBAR_SMOKE_QUERY
  fi

  TOPBAR_SMOKE_TIMEOUT="${TOPBAR_SMOKE_TIMEOUT:-180}" \
  TOPBAR_SMOKE_DRIVER="$repo/scripts/smoke-weather-shot.sh" \
  TOPBAR_VISUAL_CONFIG="$config" \
    "$repo/scripts/visual-smoke-niri.sh" "$artifact_root/$name" >"$artifact_root/$name.log" 2>&1 ||
    echo "smoke-weather: $name exited non-zero; see $artifact_root/$name.log" >&2
  mv "$artifact_root/$name/panel.log" "$artifact_root/$name-panel.log" 2>/dev/null || true
}

start_stub 200
run ready "$located_config" weather
run bare "$bare_config" ""
run panel "$located_config" clock
run configure "$live_config" ""
run setup "$live_config" weather-setup moscow
run saved "$live_config" weather "" "$saved_state"

# The endpoint goes down: with nothing ever fetched the card has nothing to
# keep, so it draws the unavailable state rather than a blank.
start_stub 429
run unavailable "$located_config" clock

stop_stub

echo "--- screenshots ---"
find "$artifact_root" -name '*.png' | sort
echo "--- panel logs ---"
for log in "$artifact_root"/*-panel.log; do
  [ -f "$log" ] || continue
  echo "=== $log ==="
  cat "$log"
done
