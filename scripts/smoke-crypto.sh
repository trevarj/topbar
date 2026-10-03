#!/usr/bin/env sh
# The crypto matrix, driven inside the nested niri session.
#
# Every run points the panel at scripts/crypto-stub.py rather than the real
# CoinGecko, so the screenshots are reproducible, the numbers are the fixture's
# and can be checked against it, and the run needs no network:
#
#   nix develop -c ./scripts/smoke-crypto.sh
#
# It stands the stub up, then runs the nested session once per state, because
# TOPBAR_SMOKE_OPEN can only open one surface per start:
#
#   1  bar        the default entries on the bar: btc, eth, eth/btc
#   2  prices     the popover's price rows, with change chips
#   3  settings   the same popover switched to its settings view
#   4  monero     entries seeded into state.json: btc, xmr, xmr/btc
#   5  apply      the settings view's "switch Monero on" driven from the smoke
#                 hook, which is what proves the write path end to end: the
#                 bar grows a fourth entry and state.json gains the list
#   6  limited    the stub answering 429 from the first request onwards
#
# Screenshots land in target/visual-smoke/crypto/<state>/.
set -eu

artifact_root="${1:-target/visual-smoke/crypto}"
port="${TOPBAR_SMOKE_STUB_PORT:-18081}"
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

# The live config runs the user's own shell script through custom-crypto and
# says nothing about the built-in widget — that is the whole point of the
# compatibility contract. A second config puts the built-in one on the bar
# instead, deriving everything else from the live one so the panel around it is
# the panel they actually run.
crypto_config="$artifact_root/crypto-config.toml"
sed -e 's/^left = \["workspaces", "custom-crypto"\]$/left = ["workspaces", "crypto"]/' \
  -e 's/^center = \["weather", "clock"\]$/center = ["clock"]/' \
  "$live_config" >"$crypto_config"
grep -q '"workspaces", "crypto"' "$crypto_config" || {
  echo "could not put the crypto widget on the bar" >&2
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
  python3 "$repo/scripts/crypto-stub.py" --port "$port" --status "$want" \
    >"$artifact_root/stub-$want.log" 2>&1 &
  stub_pid=$!

  waited=0
  while [ "$waited" -lt 10 ]; do
    got=$(curl -s -o /dev/null -w '%{http_code}' \
      "http://127.0.0.1:$port/api/v3/simple/price?probe=1" 2>/dev/null || echo 000)
    if [ "$got" = "$want" ]; then
      echo "smoke-crypto: stub answering $want on port $port"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done

  echo "smoke-crypto: nothing on port $port answers $want (got '$got')." >&2
  echo "smoke-crypto: a stray stub is probably holding it — check" >&2
  echo "  pgrep -af crypto-stub.py" >&2
  cat "$artifact_root/stub-$want.log" >&2 2>/dev/null || true
  exit 1
}

# A bare host: the service appends CoinGecko's own path to it, which is what
# keeps the stub honest about the route the real client asks for.
export TOPBAR_CRYPTO_API="http://127.0.0.1:$port"

# What the settings view writes when Monero is switched on and a pair added.
# Seeding a run with it is the same thing as starting the panel after that.
monero_state="$artifact_root/monero-state.json"
cat >"$monero_state" <<'JSON'
{
  "crypto": {
    "entries": ["btc", "xmr", "xmr/btc"]
  }
}
JSON

# One nested session: run <name> [open] [state]
run() {
  name=$1
  open=${2:-}
  state=${3:-}

  # What `open` asks for decides what the capture has to wait to be *drawn*:
  # a popover for a widget name, and nothing at all for a run that only
  # changes the bar.
  case "$open" in
    "" | *-apply) export SMOKE_EXPECT="" ;;
    *) export SMOKE_EXPECT="topbar-popover" ;;
  esac

  if [ -n "$state" ]; then
    export TOPBAR_SMOKE_STATE="$state"
  else
    unset TOPBAR_SMOKE_STATE
  fi

  echo "smoke-crypto: $name"
  # Unset rather than empty: an empty TOPBAR_SMOKE_OPEN asks the panel to open
  # a widget called "", which it rightly complains about in panel.log.
  if [ -n "$open" ]; then
    export TOPBAR_SMOKE_OPEN="$open"
  else
    unset TOPBAR_SMOKE_OPEN
  fi

  TOPBAR_SMOKE_TIMEOUT="${TOPBAR_SMOKE_TIMEOUT:-180}" \
  TOPBAR_SMOKE_DRIVER="$repo/scripts/smoke-crypto-shot.sh" \
  TOPBAR_VISUAL_CONFIG="$crypto_config" \
    "$repo/scripts/visual-smoke-niri.sh" "$artifact_root/$name" >"$artifact_root/$name.log" 2>&1 ||
    echo "smoke-crypto: $name exited non-zero; see $artifact_root/$name.log" >&2
  mv "$artifact_root/$name/panel.log" "$artifact_root/$name-panel.log" 2>/dev/null || true
}

start_stub 200
run bar
run prices crypto
run settings crypto-settings
run monero crypto "$monero_state"
run apply crypto-apply

# The endpoint rate limits from the very first request: with nothing ever
# fetched the widget has nothing to keep, so it draws its logos dimmed.
start_stub 429
run limited

stop_stub

echo "--- screenshots ---"
find "$artifact_root" -name '*.png' | sort
echo "--- panel logs ---"
for log in "$artifact_root"/*-panel.log; do
  [ -f "$log" ] || continue
  echo "=== $log ==="
  cat "$log"
done
