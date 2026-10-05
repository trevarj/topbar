#!/usr/bin/env sh
# Driver for smoke-launcher-dialogs.sh.  It starts in the panel-owned
# launcher, then deliberately stops the panel before it presents standalone
# chooser and pinentry surfaces on the same private Wayland session.
set -eu

. "$(dirname "$0")/smoke-pointer.sh"
. "$(dirname "$0")/smoke-shot.sh"

art="$SMOKE_ARTIFACTS"
theme_json="$SMOKE_LAUNCHER_THEME_JSON"
wallpaper_json="$SMOKE_LAUNCHER_WALLPAPER_JSON"
light_config="$SMOKE_LAUNCHER_LIGHT_CONFIG"
pinentry="$SMOKE_LAUNCHER_PINENTRY"
fixture_file="$SMOKE_LAUNCHER_FILE"
fd_log="$SMOKE_LAUNCHER_FD_LOG"
failing_exec="$SMOKE_LAUNCHER_FAILING_EXEC"
stage_256="$SMOKE_LAUNCHER_STAGE_256"
stage_512="$SMOKE_LAUNCHER_STAGE_512"
terminal_log="$SMOKE_LAUNCHER_TERMINAL_LOG"
dbus_log="$SMOKE_LAUNCHER_DBUS_LOG"
exec_log="$SMOKE_LAUNCHER_EXEC_LOG"
persist_pid="$SMOKE_LAUNCHER_PERSIST_PID"
persist_release="$SMOKE_LAUNCHER_PERSIST_RELEASE"

chooser_pid=""
queued_chooser_pid=""
pinentry_pid=""
persistent_child=""
dbus_pid=""
cleanup() {
  [ -z "$chooser_pid" ] || kill "$chooser_pid" 2>/dev/null || true
  [ -z "$queued_chooser_pid" ] || kill "$queued_chooser_pid" 2>/dev/null || true
  [ -z "$pinentry_pid" ] || kill "$pinentry_pid" 2>/dev/null || true
  [ -z "$persistent_child" ] || kill "$persistent_child" 2>/dev/null || true
  [ -z "$dbus_pid" ] || kill "$dbus_pid" 2>/dev/null || true
  touch "$persist_release"
}
trap cleanup EXIT INT TERM

fail=0
check() {
  "$@" || fail=1
}

# The dark half of this matrix owns its palette through SMOKE_CONFIG.  Reading
# the fixture value keeps the image checks tied to the rendered selection
# colour instead of an incidental RGB value in a captured screenshot.
launcher_accent() {
  awk '
    /^\[theme\]$/ { in_theme = 1; next }
    /^\[/ { in_theme = 0 }
    in_theme && /^accent[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*/, "", value)
      gsub(/[[:space:]"]/, "", value)
      print value
      exit
    }
  ' "$SMOKE_CONFIG"
}

accent=$(launcher_accent)
if [ -z "$accent" ]; then
  echo "launcher smoke config has no theme accent" >&2
  exit 1
fi

# A selected launcher row has one long accent-coloured horizontal edge and a
# nearby vertical edge.  ImageMagick exposes the edges as connected components
# once everything except the configured accent has been removed.  Returning
# their geometry lets callers test one concrete row instead of a broad crop.
selected_card_outline() {
  frame=$1
  minimum_width=$2
  minimum_y=$3
  minimum_vertical=$4
  maximum_bottom=$5
  # The one-pixel ring is alpha-blended against the blurred desktop, so its
  # presented pixels can be appreciably dimmer than the configured accent.
  magick "$frame" -alpha off -fuzz 18% -fill black +opaque "$accent" -threshold 1% \
    -define connected-components:verbose=true -connected-components 4 null: 2>&1 |
    awk -v minimum_width="$minimum_width" -v minimum_y="$minimum_y" \
      -v minimum_vertical="$minimum_vertical" -v maximum_bottom="$maximum_bottom" '
      /^[[:space:]]*[0-9]+: [0-9]+x[0-9]+\+[0-9]+\+[0-9]+ / {
        box = $2
        split(box, position, "+")
        split(position[1], dimensions, "x")
        width = dimensions[1] + 0
        height = dimensions[2] + 0
        x = position[2] + 0
        y = position[3] + 0
        if (width >= minimum_width && height <= 2 && y >= minimum_y) {
          horizontal_x[++horizontal_count] = x
          horizontal_y[horizontal_count] = y
          horizontal_width[horizontal_count] = width
        } else if (width <= 2 && height >= minimum_vertical && y >= minimum_y) {
          vertical_x[++vertical_count] = x
          vertical_y[vertical_count] = y
          vertical_height[vertical_count] = height
        }
      }
      END {
        for (horizontal = 1; horizontal <= horizontal_count; horizontal++) {
          for (vertical = 1; vertical <= vertical_count; vertical++) {
            if (vertical_x[vertical] <= horizontal_x[horizontal] &&
                horizontal_x[horizontal] - vertical_x[vertical] <= 20 &&
                vertical_y[vertical] >= horizontal_y[horizontal] &&
                vertical_y[vertical] - horizontal_y[horizontal] <= 40 &&
                vertical_y[vertical] + vertical_height[vertical] <= maximum_bottom) {
              printf "%d %d %d %d\n", horizontal_x[horizontal], horizontal_y[horizontal], \
                horizontal_width[horizontal], vertical_y[vertical] + vertical_height[vertical]
              exit 0
            }
          }
        }
        exit 1
      }
    '
}

# File results span the dialog, so their rounded vertical edges do not always
# survive the compositor's antialiasing as one component.  Their paired long
# horizontal edges do.  This companion finder is deliberately limited to the
# wide file card and returns the same x, top, width, bottom contract.
wide_card_outline() {
  frame=$1
  minimum_width=$2
  minimum_y=$3
  minimum_height=$4
  maximum_height=$5
  maximum_bottom=$6
  magick "$frame" -alpha off -fuzz 8% -fill black +opaque "$accent" -threshold 1% \
    -define connected-components:verbose=true -connected-components 4 null: 2>&1 |
    awk -v minimum_width="$minimum_width" -v minimum_y="$minimum_y" \
      -v minimum_height="$minimum_height" -v maximum_height="$maximum_height" \
      -v maximum_bottom="$maximum_bottom" '
      /^[[:space:]]*[0-9]+: [0-9]+x[0-9]+\+[0-9]+\+[0-9]+ / {
        box = $2
        split(box, position, "+")
        split(position[1], dimensions, "x")
        width = dimensions[1] + 0
        height = dimensions[2] + 0
        x = position[2] + 0
        y = position[3] + 0
        if (width >= minimum_width && height <= 2 && y >= minimum_y) {
          horizontal_x[++horizontal_count] = x
          horizontal_y[horizontal_count] = y
          horizontal_width[horizontal_count] = width
        }
      }
      END {
        for (top = 1; top <= horizontal_count; top++) {
          for (bottom = 1; bottom <= horizontal_count; bottom++) {
            card_height = horizontal_y[bottom] - horizontal_y[top]
            if (card_height >= minimum_height && card_height <= maximum_height &&
                horizontal_y[bottom] <= maximum_bottom &&
                horizontal_x[bottom] - horizontal_x[top] <= 20 &&
                horizontal_x[top] - horizontal_x[bottom] <= 20 &&
                horizontal_width[bottom] - horizontal_width[top] <= 20 &&
                horizontal_width[top] - horizontal_width[bottom] <= 20) {
              printf "%d %d %d %d\n", horizontal_x[top], horizontal_y[top], \
                horizontal_width[top], horizontal_y[bottom]
              exit 0
            }
          }
        }
        exit 1
      }
    '
}

backdrop_y() {
  pointer_geometry=$(pointer_size) || return 1
  IFS=' ' read -r pointer_width pointer_height <<EOF
$pointer_geometry
EOF
  case "$pointer_width:$pointer_height" in
    *[!0-9:]* | :* | *:) return 1 ;;
  esac
  [ "$pointer_width" -gt 0 ] && [ "$pointer_height" -gt 8 ] || return 1
  printf '%s\n' "$((pointer_height - 8))"
}

wait_for_file_catalog() {
  waited=0
  while [ "$waited" -lt 30 ]; do
    "$SMOKE_TOPBAR" dump state --json >"$art/files-state.json" 2>/dev/null || true
    if python3 - "$art/files-state.json" <<'PY'
import json
import sys

try:
    files = json.load(open(sys.argv[1], encoding="utf-8"))["files"]
except (OSError, ValueError, KeyError):
    raise SystemExit(1)
raise SystemExit(0 if not files["discovering"] and files["paths"] > 0 else 1)
PY
    then
      echo "launcher file catalog completed after ${waited}s"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "launcher file catalog never completed" >&2
  return 1
}

# Wait for a staged, still-running catalog. The launcher smoke's private fd
# fixture publishes at 128 and 256 paths so this can exercise a visible file
# result while the service keeps notifying its GTK subscriber.
wait_for_file_catalog_progress() {
  expected=$1
  waited=0
  while [ "$waited" -lt 30 ]; do
    "$SMOKE_TOPBAR" dump state --json >"$art/files-state.json" 2>/dev/null || true
    if python3 - "$art/files-state.json" "$expected" <<'PY'
import json
import sys

try:
    files = json.load(open(sys.argv[1], encoding="utf-8"))["files"]
    expected = int(sys.argv[2])
except (OSError, ValueError, KeyError):
    raise SystemExit(1)
raise SystemExit(0 if files["discovering"] and files["paths"] >= expected else 1)
PY
    then
      echo "launcher file catalog reached ${expected} paths while indexing after ${waited}s"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "launcher file catalog never reached ${expected} paths while indexing" >&2
  return 1
}

blur_effect_creations() {
  grep -aFc 'blur: effect object created' "$art/panel.log" 2>/dev/null || true
}

wait_for_launcher_blur() {
  before=$1
  # The fallback is valid on compositors without the protocol. On the nested
  # niri that advertises blur, a second effect object can only be the launcher's
  # full-screen backdrop: the centered foreground deliberately has none.
  if ! grep -aFq 'blur: ready (capable=true)' "$art/panel.log" 2>/dev/null; then
    echo "launcher blur: nested compositor has no usable blur capability; request check skipped" \
      >"$art/launcher-blur.txt"
    return 0
  fi
  waited=0
  while [ "$waited" -lt 15 ]; do
    after=$(blur_effect_creations)
    if [ "$after" -gt "$before" ]; then
      printf 'launcher blur: effect creation count %s -> %s\n' "$before" "$after" \
        >"$art/launcher-blur.txt"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "launcher blur: backdrop did not create a compositor effect" >&2
  return 1
}

assert_launcher_backdrop_layers() {
  niri msg layers >"$art/launcher-backdrop-layers.txt" 2>&1 || return 1
  foregrounds=$(grep -o '"topbar-launcher"' "$art/launcher-backdrop-layers.txt" | wc -l | tr -d ' ')
  backdrops=$(grep -o '"topbar-launcher-backdrop"' "$art/launcher-backdrop-layers.txt" | wc -l | tr -d ' ')
  if [ "$foregrounds" -ne 1 ] || [ "$backdrops" -ne 1 ]; then
    echo "launcher backdrop: expected one foreground and one full-screen backdrop, saw ${foregrounds} and ${backdrops}" >&2
    return 1
  fi
  echo "launcher backdrop: foreground and full-screen backdrop are mapped"
}

# Four Down presses move through four grid rows, selecting Smoke Catalog 13 in
# this deterministic fixture.  It has to scroll into view.  Require the
# selected card's actual outline to move below the first selection while its
# long vertical edge remains inside the viewport; image difference alone could
# also be caused by an unrelated redraw.
assert_launcher_results_scrolled() {
  before=$art/launcher-compact-before.png
  after=$art/launcher-compact-after.png
  [ -f "$before" ] && [ -f "$after" ] || return 1
  image_geometry=$(magick "$before" -format '%w %h' info:)
  IFS=' ' read -r image_width image_height <<EOF
$image_geometry
EOF
  case "$image_width:$image_height" in
    *[!0-9:]* | :* | *:) return 1 ;;
  esac
  minimum_width=$((image_width / 7))
  minimum_y=$((image_height / 4))
  minimum_vertical=$((image_height / 24))
  maximum_bottom=$((image_height * 9 / 10))
  if ! initial_outline=$(selected_card_outline "$before" "$minimum_width" "$minimum_y" \
    "$minimum_vertical" "$maximum_bottom"); then
    echo "launcher compact output: initial selected result card is not visible" >&2
    return 1
  fi
  if ! deep_outline=$(selected_card_outline "$after" "$minimum_width" "$minimum_y" \
    "$minimum_vertical" "$maximum_bottom"); then
    echo "launcher compact output: selected lower result card is clipped or absent" >&2
    return 1
  fi
  IFS=' ' read -r initial_x initial_top initial_width initial_bottom <<EOF
$initial_outline
EOF
  IFS=' ' read -r deep_x deep_top deep_width deep_bottom <<EOF
$deep_outline
EOF
  if [ "$deep_top" -lt $((image_height / 2)) ] || \
    [ "$deep_top" -le $((initial_top + image_height / 16)) ]; then
    echo "launcher compact output: fourth grid row did not scroll below the initial result" >&2
    return 1
  fi
  printf 'launcher compact selected card initial=%sx%s+%s+%s deep=%sx%s+%s+%s\n' \
    "$initial_width" "$((initial_bottom - initial_top))" "$initial_x" "$initial_top" \
    "$deep_width" "$((deep_bottom - deep_top))" "$deep_x" "$deep_top" \
    >"$art/launcher-compact-scroll.txt"
}

# The first catalog tile supplies both the pixel-to-logical hover target and
# the reference ring. Its neighbour in the next row starts just below it.
catalog_first_outline() (
  frame=$1
  image_geometry=$(magick "$frame" -format '%w %h' info:) || exit 1
  IFS=' ' read -r image_width image_height <<EOF
$image_geometry
EOF
  selected_card_outline "$frame" "$((image_width / 7))" "$((image_height / 4))" \
    "$((image_height / 24))" "$((image_height * 9 / 10))"
)

catalog_hover_point() (
  first=$(catalog_first_outline "$art/launcher-catalog-before.png") || exit 1
  IFS=' ' read -r first_x first_top first_width first_bottom <<EOF
$first
EOF
  shot_scale "$((first_x + first_width / 2))" \
    "$((first_bottom + (first_bottom - first_top) / 2 + first_width / 24))"
)

assert_catalog_hover_states() (
  first=$(catalog_first_outline "$art/launcher-catalog-before.png") || exit 1
  IFS=' ' read -r first_x first_top first_width first_bottom <<EOF
$first
EOF
  image_height=$(magick "$art/launcher-catalog-before.png" -format '%h' info:) || exit 1
  for state in hover leave leave-settled; do
    frame="$art/launcher-catalog-$state.png"
    outline=$(selected_card_outline "$frame" "$((first_width * 3 / 4))" \
      "$first_top" "$((image_height / 24))" "$((first_bottom + 1))") || {
      echo "catalog hover: keyboard-selected first tile lost its ring in $state" >&2
      exit 1
    }
    IFS=' ' read -r x top width bottom <<EOF
$outline
EOF
    if [ "$x" -ne "$first_x" ] || [ "$top" -ne "$first_top" ] || \
      [ "$width" -ne "$first_width" ] || [ "$bottom" -ne "$first_bottom" ]; then
      echo "catalog hover: keyboard-selected tile moved or lost its ring in $state" >&2
      exit 1
    fi
    # Restrict the second ring to the tile directly below the first. The
    # search-entry accent and selected first tile lie above this y range.
    neighbor=$(selected_card_outline "$frame" "$((first_width * 3 / 4))" \
      "$((first_bottom + 1))" "$((image_height / 24))" \
      "$((first_bottom * 2 - first_top + first_width / 12))") || neighbor=
    if [ "$state" = hover ]; then
      [ -n "$neighbor" ] || {
        echo "catalog hover: unselected tile did not gain an accent ring" >&2
        exit 1
      }
      IFS=' ' read -r neighbor_x neighbor_top neighbor_width neighbor_bottom <<EOF
$neighbor
EOF
      if [ "$neighbor_x" -lt "$((first_x - first_width / 12))" ] || \
        [ "$neighbor_x" -gt "$((first_x + first_width / 12))" ] || \
        [ "$neighbor_top" -le "$first_bottom" ] || \
        [ "$neighbor_width" -lt "$((first_width * 3 / 4))" ]; then
        echo "catalog hover: accent ring was not on the neighbouring catalog tile" >&2
        exit 1
      fi
    elif [ -n "$neighbor" ]; then
      echo "catalog hover: unselected tile's ring reappeared after pointer leave ($state)" >&2
      exit 1
    fi
  done
)

assert_launcher_wheel_and_reverse() (
  before=$(catalog_first_outline "$art/launcher-compact-after.png") || exit 1
  wheel=$(catalog_first_outline "$art/launcher-compact-wheel.png") || {
    echo "launcher wheel: selected tile disappeared after native wheel input" >&2
    exit 1
  }
  reverse=$(catalog_first_outline "$art/launcher-compact-reverse.png") || {
    echo "launcher wheel: reverse Up selection is clipped or absent" >&2
    exit 1
  }
  IFS=' ' read -r before_x before_y before_width before_bottom <<EOF
$before
EOF
  IFS=' ' read -r wheel_x wheel_y wheel_width wheel_bottom <<EOF
$wheel
EOF
  IFS=' ' read -r reverse_x reverse_y reverse_width reverse_bottom <<EOF
$reverse
EOF
  if [ "$wheel_x" -ne "$before_x" ] || [ "$wheel_y" -ge "$before_y" ] || \
    [ "$reverse_x" -ne "$before_x" ] || [ "$reverse_y" -ge "$wheel_y" ]; then
    echo "launcher wheel: wheel did not scroll content up or Up did not select a visible prior row" >&2
    exit 1
  fi
)

# Measure the dialog against the flat backdrop along a row above the results,
# then follow its padding at the left edge. Preview pixels never enter the scan.
wallpaper_dialog_bounds() {
  python3 - "$1" <<'PY'
import subprocess
import sys

frame = sys.argv[1]
width, height = map(int, subprocess.check_output(
    ["magick", frame, "-format", "%w %h", "info:"], text=True).split())
pixels = subprocess.check_output(
    ["magick", frame, "-alpha", "off", "-depth", "8", "rgb:-"])
def colour(x, y):
    offset = (y * width + x) * 3
    return pixels[offset:offset + 3]

background = colour(0, height // 2)
def dialog(pixel):
    return sum(abs(a - b) for a, b in zip(pixel, background)) > 5

row = height // 6
left = next((x for x in range(width // 2)
             if all(dialog(colour(j, row)) for j in range(x, x + 8))), None)
right = next((x for x in range(width - 1, width // 2, -1)
              if all(dialog(colour(j, row)) for j in range(x - 7, x + 1))), None)
if left is None or right is None:
    raise SystemExit("wallpaper chooser: horizontal dialog bounds were not found")
gutter = left + 8
top = next((y for y in range(height // 20, height // 2)
            if all(dialog(colour(gutter, j)) for j in range(y, y + 8))), None)
bottom = next((y for y in range(height - 1, height // 2, -1)
               if all(dialog(colour(gutter, j)) for j in range(y - 7, y + 1))), None)
if top is None or bottom is None:
    raise SystemExit("wallpaper chooser: vertical dialog bounds were not found")
if left <= 0 or right >= width - 1 or top <= 0 or bottom >= height - 1:
    raise SystemExit("wallpaper chooser: dialog clipped by output")
print(left, top, right - left + 1, bottom - top + 1)
PY
}

# A settled capture can still show the frame before a key press on nested niri.
# Wait for the selected card itself to move before treating its shot as evidence.
wait_for_wallpaper_selection() {
  name=$1
  minimum=$2
  maximum=$3
  attempts=0
  while [ "$attempts" -lt 6 ]; do
    shot "$name" topbar-chooser >"$art/$name.wait.log" || return 1
    frame="$art/$name.png"
    image_geometry=$(magick "$frame" -format '%w %h' info:) || return 1
    IFS=' ' read -r image_width image_height <<EOF
$image_geometry
EOF
    if outline=$(selected_card_outline "$frame" "$((image_width / 7))" \
      "$((image_height / 6))" "$((image_height / 30))" "$((image_height * 9 / 10))"); then
      IFS=' ' read -r row_x row_top row_width row_bottom <<EOF
$outline
EOF
      if [ "$row_top" -ge "$minimum" ] && [ "$row_top" -le "$maximum" ]; then
        printf '%s %s\n' "$row_top" "$row_bottom"
        return 0
      fi
    fi
    attempts=$((attempts + 1))
  done
  echo "wallpaper chooser: selection did not move into $name after $attempts captures" >&2
  return 1
}

assert_wallpaper_dialog_stable() {
  baseline=$(wallpaper_dialog_bounds "$art/chooser-wallpapers-preview-valid.png") || return 1
  for state in failed portrait returned; do
    measured=$(wallpaper_dialog_bounds "$art/chooser-wallpapers-preview-$state.png") || return 1
    if [ "$measured" != "$baseline" ]; then
      echo "wallpaper chooser resized on $state: $baseline -> $measured" >&2
      return 1
    fi
  done
  printf 'wallpaper dialog stayed at %s across loaded, failed, portrait, and returned previews\n' \
    "$baseline" >"$art/chooser-wallpapers-bounds.txt"
}

wallpaper_preview_crop() {
  bounds=$(wallpaper_dialog_bounds "$1") || return 1
  IFS=' ' read -r dialog_x dialog_y dialog_width dialog_height <<EOF
$bounds
EOF
  crop_width=$((dialog_width * 3 / 5))
  crop_height=$((dialog_height / 4))
  crop_x=$((dialog_x + (dialog_width - crop_width) / 2))
  crop_y=$((dialog_y + dialog_height - dialog_height / 12 - crop_height))
  printf '%sx%s+%s+%s\n' "$crop_width" "$crop_height" "$crop_x" "$crop_y"
}

assert_wallpaper_preview_changes() {
  # Compare only the image area; a selected-card border is not proof of a swap.
  crop=$(wallpaper_preview_crop "$art/chooser-wallpapers-preview-valid.png") || return 1
  difference=$(magick "$art/chooser-wallpapers-preview-valid.png" \
    "$art/chooser-wallpapers-preview-portrait.png" -compose Difference -composite \
    -crop "$crop" +repage -colorspace Gray -format '%[fx:mean]' info:) || return 1
  if ! awk -v difference="$difference" 'BEGIN { exit !(difference > 0.015) }'; then
    echo "wallpaper chooser: portrait image did not replace the loaded preview" >&2
    return 1
  fi
}

wait_for_wallpaper_preview() {
  attempts=0
  while [ "$attempts" -lt 6 ]; do
    if assert_wallpaper_preview_changes 2>/dev/null; then
      return 0
    fi
    shot chooser-wallpapers-preview-portrait topbar-chooser \
      >"$art/chooser-wallpapers-preview-portrait.wait.log" || return 1
    attempts=$((attempts + 1))
  done
  echo "wallpaper chooser: portrait preview did not replace the wide image" >&2
  return 1
}
# This fixture uses blue gradients for both decoded images. The failed
# preview is a neutral missing-image icon; count colour only inside the
# preview area, not the changing selected card in the results above it.
wallpaper_preview_colour_fraction() {
  crop=$(wallpaper_preview_crop "$1") || return 1
  python3 - "$1" "$crop" <<'PY'
import subprocess
import sys

pixels = subprocess.check_output(
    ["magick", sys.argv[1], "-crop", sys.argv[2], "+repage",
     "-alpha", "off", "-depth", "8", "rgb:-"])
blue = sum(b > r + 24 and b > g + 12 and b > 95
           for r, g, b in zip(pixels[::3], pixels[1::3], pixels[2::3]))
print(blue / (len(pixels) // 3))
PY
}

wait_for_wallpaper_preview_colour() {
  name=$1
  kind=$2
  attempts=0
  while [ "$attempts" -lt 6 ]; do
    fraction=$(wallpaper_preview_colour_fraction "$art/$name.png") || return 1
    if [ "$kind" = ready ]; then
      awk -v n="$fraction" 'BEGIN { exit !(n > 0.1) }' && return 0
    else
      awk -v n="$fraction" 'BEGIN { exit !(n < 0.02) }' && return 0
    fi
    shot "$name" topbar-chooser >"$art/$name.wait.log" || return 1
    attempts=$((attempts + 1))
  done
  echo "wallpaper chooser: $kind preview did not appear in $name" >&2
  return 1
}
wallpaper_selected_outline() (
  frame=$1
  image_geometry=$(magick "$frame" -format '%w %h' info:) || exit 1
  IFS=' ' read -r image_width image_height <<EOF
$image_geometry
EOF
  selected_card_outline "$frame" "$((image_width / 7))" \
    "$((image_height / 3))" "$((image_height / 24))" \
    "$((image_height * 9 / 10))"
)

assert_wallpaper_wheel_and_reverse() (
  wallpaper_selected_outline "$art/chooser-wallpapers-scrolled.png" >/dev/null || {
    echo "wallpaper chooser: sixth selected row was clipped after five Down keys" >&2
    exit 1
  }
  wallpaper_selected_outline "$art/chooser-wallpapers-reverse.png" >/dev/null || {
    echo "wallpaper chooser: selected row was clipped after wheel and Up" >&2
    exit 1
  }
  difference=$(magick "$art/chooser-wallpapers-scrolled.png" \
    "$art/chooser-wallpapers-wheel.png" -compose Difference -composite \
    -gravity center -crop '50%x40%+0+0' +repage \
    -colorspace Gray -format '%[fx:mean]' info:) || exit 1
  if ! awk -v difference="$difference" 'BEGIN { exit !(difference > 0.002) }'; then
    echo "wallpaper chooser: native wheel did not change the results viewport" >&2
    exit 1
  fi
)

# The first theme swatch is opaque and fixed in the results viewport. Its
# background stripe must change after navigating past the four visible rows;
# a moved selection ring alone does not prove the list actually scrolled.
assert_theme_results_scrolled() (
  before="$art/chooser-themes-dark.png"
  after="$art/chooser-themes-scrolled.png"
  image_geometry=$(magick "$before" -format '%w %h' info:) || exit 1
  IFS=' ' read -r image_width image_height <<EOF
$image_geometry
EOF
  minimum_width=$((image_width / 7))
  minimum_y=$((image_height / 4))
  minimum_vertical=$((image_height / 32))
  maximum_bottom=$((image_height * 9 / 10))
  first=$(selected_card_outline "$before" "$minimum_width" "$minimum_y" \
    "$minimum_vertical" "$maximum_bottom") || {
    echo "theme chooser: initial selected row is absent" >&2
    exit 1
  }
  deep=$(selected_card_outline "$after" "$minimum_width" "$minimum_y" \
    "$minimum_vertical" "$maximum_bottom") || {
    echo "theme chooser: eighth selected row is clipped or absent" >&2
    exit 1
  }
  IFS=' ' read -r first_x first_top first_width first_bottom <<EOF
$first
EOF
  IFS=' ' read -r deep_x deep_top deep_width deep_bottom <<EOF
$deep
EOF
  row_height=$((first_bottom - first_top))
  if [ "$deep_x" -ne "$first_x" ] || \
    [ "$deep_top" -le $((first_top + row_height * 2)) ] || \
    [ "$((deep_bottom - deep_top))" -lt "$((row_height - 4))" ]; then
    echo "theme chooser: eighth row did not scroll fully into the viewport" >&2
    exit 1
  fi
  stripe_x=$((first_x + 24))
  stripe_y=$((first_top + row_height / 2 - 6))
  difference=$(magick \
    \( "$before" -crop "12x12+${stripe_x}+${stripe_y}" +repage \) \
    \( "$after" -crop "12x12+${stripe_x}+${stripe_y}" +repage \) \
    -compose Difference -composite -colorspace Gray -format '%[fx:mean]' info:) || exit 1
  if ! awk -v difference="$difference" 'BEGIN { exit !(difference > 0.04) }'; then
    echo "theme chooser: results swatch stayed in place after seven Down keys" >&2
    exit 1
  fi
  printf 'theme selected row %s -> %s; first-row swatch difference %s\n' \
    "$first" "$deep" "$difference" >"$art/chooser-themes-scroll.txt"
)

# The exact smoke query has exactly one matching file.  Its selected result
# takes nearly the whole dialog width, so outline geometry pins this check to
# that file row instead of a generic, colourful result-area crop.
file_result_crop_geometry() {
  frame=$1
  image_geometry=$(magick "$frame" -format '%w %h' info:) || return 1
  IFS=' ' read -r image_width image_height <<EOF
$image_geometry
EOF
  case "$image_width:$image_height" in
    *[!0-9:]* | :* | *:) return 1 ;;
  esac
  minimum_width=$((image_width * 3 / 4))
  minimum_y=$((image_height * 2 / 5))
  minimum_height=$((image_height / 32))
  maximum_height=$((image_height * 2 / 5))
  maximum_bottom=$((image_height * 9 / 10))
  wide_card_outline "$frame" "$minimum_width" "$minimum_y" "$minimum_height" \
    "$maximum_height" "$maximum_bottom"
}

assert_file_result_visible() {
  frame=$1
  [ -f "$frame" ] || return 1
  if ! outline=$(file_result_crop_geometry "$frame"); then
    echo "launcher file search: smoke-document result card is clipped or absent" >&2
    return 1
  fi
  IFS=' ' read -r card_x card_top card_width card_bottom <<EOF
$outline
EOF
  card_height=$((card_bottom - card_top))
  horizontal_margin=$((card_width / 4))
  vertical_margin=$((card_height / 10))
  content_width=$((card_width / 2))
  content_height=$((card_height - vertical_margin * 2))
  content_x=$((card_x + horizontal_margin))
  content_y=$((card_top + vertical_margin))
  bright_fraction=$(magick "$frame" \
    -crop "${content_width}x${content_height}+${content_x}+${content_y}" +repage \
    -colorspace Gray -threshold 75% -format '%[fx:mean]' info:) || return 1
  printf '%s card=%sx%s+%s+%s title-pixels=%s\n' "$(basename "$frame")" \
    "$card_width" "$card_height" "$card_x" "$card_top" "$bright_fraction" \
    >>"$art/launcher-file-indexing.txt"
  if ! awk -v fraction="$bright_fraction" 'BEGIN { exit !(fraction > 0.005) }'; then
    echo "launcher file search: smoke-document row has no visible title or icon" >&2
    return 1
  fi
}

# The catalog is deliberately paused between these frames.  Both images have
# already established that the one matching row is visible; compare only their
# card interiors to ensure the staged refresh retained the same row instead of
# swapping it for an empty or unrelated result.
assert_file_result_survives_update() {
  before=$1
  after=$2
  before_outline=$(file_result_crop_geometry "$before") || return 1
  after_outline=$(file_result_crop_geometry "$after") || return 1
  IFS=' ' read -r before_x before_top before_width before_bottom <<EOF
$before_outline
EOF
  IFS=' ' read -r after_x after_top after_width after_bottom <<EOF
$after_outline
EOF
  before_height=$((before_bottom - before_top))
  after_height=$((after_bottom - after_top))
  if [ "$before_width" -lt "$after_width" ]; then
    crop_width=$((before_width / 2))
  else
    crop_width=$((after_width / 2))
  fi
  if [ "$before_height" -lt "$after_height" ]; then
    crop_height=$((before_height * 4 / 5))
  else
    crop_height=$((after_height * 4 / 5))
  fi
  before_crop_x=$((before_x + (before_width - crop_width) / 2))
  after_crop_x=$((after_x + (after_width - crop_width) / 2))
  before_crop_y=$((before_top + (before_height - crop_height) / 2))
  after_crop_y=$((after_top + (after_height - crop_height) / 2))
  difference=$(magick \
    \( "$before" -crop "${crop_width}x${crop_height}+${before_crop_x}+${before_crop_y}" +repage \) \
    \( "$after" -crop "${crop_width}x${crop_height}+${after_crop_x}+${after_crop_y}" +repage \) \
    -compose Difference -composite -colorspace Gray -format '%[fx:mean]' info:) || return 1
  printf 'staged file result interior difference %s\n' "$difference" \
    >>"$art/launcher-file-indexing.txt"
  if ! awk -v difference="$difference" 'BEGIN { exit !(difference < 0.015) }'; then
    echo "launcher file search: existing result changed during catalog refresh" >&2
    return 1
  fi
}

ensure_demo_backdrop() {
  niri msg action spawn -- gtk4-demo >"$art/spawn-demo.log" 2>&1 || return 1
  waited=0
  while [ "$waited" -lt 20 ]; do
    niri msg windows >"$art/niri-windows.txt" 2>&1 || true
    grep -q 'Demo' "$art/niri-windows.txt" && break
    sleep 1
    waited=$((waited + 1))
  done
  if ! grep -q 'Demo' "$art/niri-windows.txt"; then
    echo "gtk4-demo did not create a backdrop window" >&2
    return 1
  fi
  niri msg action fullscreen-window >>"$art/spawn-demo.log" 2>&1 || return 1
  shot launcher-underlay
}

wait_for_marker() {
  marker=$1
  description=$2
  waited=0
  while [ "$waited" -lt 20 ]; do
    if [ -s "$marker" ] || { [ "${3:-}" = exists ] && [ -e "$marker" ]; }; then
      echo "$description observed after ${waited}s"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "$description was never observed" >&2
  return 1
}

wait_for_exec() {
  expected=$1
  waited=0
  while [ "$waited" -lt 20 ]; do
    if grep -qx "$expected" "$exec_log" 2>/dev/null; then
      echo "Exec $expected observed after ${waited}s"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "Exec $expected was never observed" >&2
  return 1
}

wait_for_dbus_name() {
  waited=0
  while [ "$waited" -lt 20 ]; do
    if gdbus call --session --dest org.freedesktop.DBus \
      --object-path /org/freedesktop/DBus \
      --method org.freedesktop.DBus.NameHasOwner \
      io.github.topbar.SmokeDbus 2>/dev/null | grep -q 'true'; then
      echo "private D-Bus application owns its name after ${waited}s"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "private D-Bus application never owned its name" >&2
  return 1
}

show_launcher() {
  "$SMOKE_TOPBAR" launcher show >>"$art/launcher-ipc.log" 2>&1 || return 1
  assert_mapped topbar-launcher "launcher show" || return 1
  # Mapping precedes niri's keyboard handoff. Wait for a presented frame so
  # the first typed characters cannot land in the application underneath.
  shot launcher-ready topbar-launcher
}

start_chooser() {
  name=$1
  config=$2
  layout=$3
  selected=$4
  input=$5
  "$SMOKE_TOPBAR" --config "$config" choose --layout "$layout" \
    --title "Smoke $name" --message "Standalone chooser fixture" --selected "$selected" \
    <"$input" >"$art/$name.result" 2>"$art/$name.stderr" &
  chooser_pid=$!
  assert_mapped topbar-chooser "$name chooser"
}

finish_chooser_cancelled() {
  key_press Escape
  if wait "$chooser_pid"; then
    echo "$1 unexpectedly accepted" >&2
    fail=1
  fi
  chooser_pid=""
  if [ -s "$art/$1.result" ]; then
    echo "$1 wrote a selection while cancelled" >&2
    fail=1
  fi
}

start_pinentry() {
  name=$1
  request=$2
  # The dedicated wrapper reads its config from the boxed XDG path.
  mkdir -p "$XDG_CONFIG_HOME/topbar"
  cp "$SMOKE_CONFIG" "$XDG_CONFIG_HOME/topbar/config.toml"
  printf '%s\n' "$request" | "$pinentry" >"$art/pinentry-$name.protocol" \
    2>"$art/pinentry-$name.stderr" &
  pinentry_pid=$!
  assert_mapped topbar-pinentry "pinentry $name"
}

finish_pinentry_cancelled() {
  name=$1
  key_press Escape
  if ! wait "$pinentry_pid"; then
    echo "pinentry $name did not exit after cancellation" >&2
    fail=1
  fi
  pinentry_pid=""
  if grep -q '^D ' "$art/pinentry-$name.protocol"; then
    echo "pinentry $name emitted secret data" >&2
    fail=1
  fi
  if ! grep -q '^ERR ' "$art/pinentry-$name.protocol"; then
    echo "pinentry $name did not report cancellation" >&2
    fail=1
  fi
}

assert_pinentry_description_readable() {
  image=$art/pinentry-light-long-description.png
  [ -f "$image" ] || return 1
  image_geometry=$(magick "$image" -format '%w %h' info:) || return 1
  IFS=' ' read -r image_width image_height <<EOF
$image_geometry
EOF
  case "$image_width:$image_height" in
    *[!0-9:]* | :* | *:) return 1 ;;
  esac
  # The centered crop tracks the dialog's logical size at any nested output
  # scale. A whole-label selection fills much of it with midtone grey, while
  # the unselected light dialog is almost entirely pale background and text.
  crop_geometry=$(awk -v scale="${TOPBAR_SMOKE_SCALE:-1.25}" \
    -v width="$image_width" -v height="$image_height" \
    'BEGIN {
      if (scale <= 0 || width <= 0 || height <= 0) exit 1
      crop_width = int(384 * scale)
      crop_height = int(240 * scale)
      if (crop_width > width * 3 / 4) crop_width = int(width * 3 / 4)
      if (crop_height > height * 3 / 4) crop_height = int(height * 3 / 4)
      if (crop_width < 1 || crop_height < 1) exit 1
      printf "%dx%d+0+0", crop_width, crop_height
    }') || return 1
  selected_fraction=$(magick "$image" -gravity center -crop "$crop_geometry" \
    +repage -colorspace Gray \
    \( -clone 0 -threshold 65% \) \( -clone 0 -threshold 85% \) \
    -delete 0 -compose Difference -composite -format '%[fx:mean]' info:) || return 1
  if ! awk -v fraction="$selected_fraction" 'BEGIN { exit !(fraction < 0.10) }'; then
    echo "pinentry description is covered by a selection highlight" >&2
    return 1
  fi
}

if [ "${SMOKE_LAUNCHER_LIGHT_ONLY:-}" = 1 ]; then
  echo "--- light launcher without compositor blur"
  check ensure_demo_backdrop
  check show_launcher
  check shot launcher-light-no-blur topbar-launcher
  if grep -q 'blur: effect object created' "$art/panel.log"; then
    echo "light launcher requested compositor blur despite blur=false" >&2
    fail=1
  fi
  key_press Escape
  check assert_unmapped topbar-launcher "Escape closes light launcher"

  # Standalone dialogs use the light palette after the panel exits. Capture
  # both wallpaper preview states and a long protocol description without
  # submitting a selection or entering a passphrase.
  kill "$SMOKE_PANEL_PID" 2>/dev/null || true
  wait "$SMOKE_PANEL_PID" 2>/dev/null || true
  echo "--- standalone light wallpaper chooser"
  check start_chooser wallpapers-light "$SMOKE_CONFIG" wallpapers valid "$wallpaper_json"
  check shot chooser-wallpapers-light topbar-chooser
  type_text "unreadable"
  check shot chooser-wallpapers-light-unreadable topbar-chooser
  finish_chooser_cancelled wallpapers-light

  echo "--- standalone light pinentry with long description"
  long_description_request='SETTITLE Smoke long description
SETDESC The requested key belongs to the local smoke fixture and this deliberately long explanation must wrap across several lines while the complete title, entry, and Cancel and OK buttons remain visible. This sentence adds more ordinary plain text so the dialog has to grow on a compact fractional output instead of clipping the last lines or hiding either action. No passphrase is entered, stored, or recorded in this visual scenario.
SETPROMPT Test passphrase
GETPIN'
  check start_pinentry light-long-description "$long_description_request"
  check shot pinentry-light-long-description topbar-pinentry
  check assert_pinentry_description_readable
  finish_pinentry_cancelled light-long-description
  niri msg layers >"$art/standalone-layers.txt" 2>&1 || true
  if [ "$fail" -eq 0 ]; then
    echo "--- result: PASS"
  else
    echo "--- result: FAIL"
  fi
  exit "$fail"
fi

hide_launcher() {
  "$SMOKE_TOPBAR" launcher hide >>"$art/launcher-ipc.log" 2>&1 || return 1
  assert_unmapped topbar-launcher "launcher hide"
}

echo "--- launcher backdrop, blur request, and frequent applications"
check ensure_demo_backdrop
blur_before=$(blur_effect_creations)
check show_launcher
check wait_for_launcher_blur "$blur_before"
check shot launcher-clear-blur topbar-launcher
check assert_launcher_backdrop_layers
cp "$art/launcher-clear-blur.png" "$art/launcher-frequent.png"
# The seeded frequent app is Smoke Editor, whereas the ordinary grid begins
# with a catalog entry. Enter without typing must launch the frequent tile.
key_press Return
check assert_unmapped topbar-launcher "default selection launches first frequent application"
check wait_for_exec "editor: from-niri-child"
check show_launcher
check picker_probe "GtkButton launcher-item" "Search applications, actions, windows, and files" "SmkEdtr"
type_text "Smoke Editor"
check shot launcher-applications topbar-launcher
key_press Escape
check assert_unmapped topbar-launcher "Escape closes application search"

echo "--- launcher file-indexing stability"
check wait_for_marker "$fd_log" "staged fd fixture"
if ! grep -q '^fd fixture paused at 128 paths$' "$fd_log" 2>/dev/null; then
  echo "staged fd fixture did not pause at its first catalog batch" >&2
  fail=1
fi
if [ ! -f "$fixture_file" ]; then
  echo "launcher file fixture is missing: $fixture_file" >&2
  fail=1
else
  # The staged private fd fixture keeps indexing across these snapshots. The
  # first matching path is already present, so every frame must retain its row
  # while 128- and 256-path updates reach the launcher.
  check wait_for_file_catalog_progress 128
  check show_launcher
  type_text "smoke-document"
  snap launcher-files-indexing-128 1
  : >"$art/launcher-file-indexing.txt"
  check assert_file_result_visible "$art/launcher-files-indexing-128.png"
  check assert_mapped topbar-launcher "file result survives first catalog update"
  touch "$stage_256"
  check wait_for_file_catalog_progress 256
  snap launcher-files-indexing-256 1
  check assert_file_result_visible "$art/launcher-files-indexing-256.png"
  check assert_file_result_survives_update "$art/launcher-files-indexing-128.png" \
    "$art/launcher-files-indexing-256.png"
  check assert_mapped topbar-launcher "file result survives later catalog update"
  touch "$stage_512"
  key_press Escape
  check assert_unmapped topbar-launcher "Escape closes indexing file search"
fi

echo "--- launcher compact output scroll accessibility"
check show_launcher
type_text "Smoke Catalog"
check shot launcher-catalog-before topbar-launcher
cp "$art/launcher-catalog-before.png" "$art/launcher-compact-before.png"
if hover_xy=$(catalog_hover_point); then
  IFS=' ' read -r hover_x hover_y <<EOF
$hover_xy
EOF
  pointer_to "$hover_x" "$hover_y"
  check shot launcher-catalog-hover topbar-launcher
  if away_y=$(backdrop_y); then
    pointer_to 8 "$away_y"
    check shot launcher-catalog-leave topbar-launcher
    check shot launcher-catalog-leave-settled topbar-launcher
    check assert_catalog_hover_states
  else
    echo "catalog hover: could not determine pointer parking point" >&2
    fail=1
  fi
else
  echo "catalog hover: could not locate selected fixture tile" >&2
  fail=1
fi
key_press Down
key_press Down
key_press Down
key_press Down
check shot launcher-compact-after topbar-launcher
check assert_launcher_results_scrolled
# Manual GTK scrolling must move the viewport, and keyboard navigation must
# still select the prior row after canceling any pending scroll animation.
if deep_outline=$(catalog_first_outline "$art/launcher-compact-after.png"); then
  IFS=' ' read -r deep_x deep_top deep_width deep_bottom <<EOF
$deep_outline
EOF
  if wheel_xy=$(shot_scale "$((deep_x + deep_width / 2))" \
    "$((deep_top + (deep_bottom - deep_top) / 2))") && away_y=$(backdrop_y); then
    IFS=' ' read -r wheel_x wheel_y <<EOF
$wheel_xy
EOF
    scroll_at "$wheel_x" "$wheel_y" 35
    pointer_to 8 "$away_y"
    check shot launcher-compact-wheel topbar-launcher
    key_press Up
    check shot launcher-compact-reverse topbar-launcher
    check assert_launcher_wheel_and_reverse
    key_press Down
    check shot launcher-compact-restored topbar-launcher
    if ! catalog_first_outline "$art/launcher-compact-restored.png" >/dev/null; then
      echo "launcher wheel: restored lower result selection is clipped" >&2
      fail=1
    fi
  else
    echo "launcher wheel: could not locate the selected fixture tile" >&2
    fail=1
  fi
else
  echo "launcher wheel: lower fixture tile has no visible selection ring" >&2
  fail=1
fi
key_press Return
check assert_unmapped topbar-launcher "deep compact Exec result closes launcher"

echo "--- launcher Terminal Exec"
check show_launcher
type_text "Smoke Terminal"
check shot launcher-terminal topbar-launcher
key_press Return
check assert_unmapped topbar-launcher "Terminal entry launches"
check wait_for_marker "$terminal_log" "terminal helper"

echo "--- launcher DBusActivatable uses Exec, not Activate"
python3 "$SMOKE_LAUNCHER_DBUS_HELPER" >"$art/dbus-service.stdout" 2>"$art/dbus-service.stderr" &
dbus_pid=$!
check wait_for_dbus_name
check show_launcher
type_text "Smoke D-Bus"
check shot launcher-dbus-activatable topbar-launcher
key_press Return
check assert_unmapped topbar-launcher "DBusActivatable Exec launches"
check wait_for_exec "dbus: from-niri-child"
if [ -e "$dbus_log" ]; then
  echo "DBusActivatable entry unexpectedly called Activate" >&2
  fail=1
fi
kill "$dbus_pid" 2>/dev/null || true
wait "$dbus_pid" 2>/dev/null || true
dbus_pid=""

echo "--- launcher DBusActivatable without Exec fails visibly"
check show_launcher
type_text "Smoke Unlaunchable"
key_press Return
check assert_mapped topbar-launcher "DBusActivatable without Exec keeps launcher actionable"
check shot launcher-missing-exec topbar-launcher
check hide_launcher

echo "--- launcher persistent Exec child"
check show_launcher
type_text "Smoke Persistent"
key_press Return
check assert_unmapped topbar-launcher "persistent Exec launches"
check wait_for_exec "persistent: from-niri-child"
check wait_for_marker "$persist_pid" "persistent Exec PID"
if [ -s "$persist_pid" ]; then
  persistent_child=$(cat "$persist_pid")
fi
case "$persistent_child" in
  "" | *[!0-9]*) echo "persistent Exec did not report a PID" >&2; fail=1 ;;
  *) if ! kill -0 "$persistent_child" 2>/dev/null; then
       echo "persistent Exec exited before panel shutdown" >&2
       fail=1
     fi ;;
esac

echo "--- launcher hidden and failing desktop entries"
# NoDisplay is GIO's visibility contract.  The exact query has no other
# fixture result. If discovery exposed the hidden entry, Enter would launch
# its harmless command and close the surface; remaining open asserts absence.
check show_launcher
type_text "Hidden Smoke"
check shot launcher-hidden-absent topbar-launcher
key_press Return
check assert_mapped topbar-launcher "NoDisplay entry is absent from results"
check hide_launcher

# A missing executable makes GIO stop advertising the entry at activation.
# The launcher must retain keyboard ownership and render its inline error.
check show_launcher
type_text "Failing Smoke"
check shot launcher-failing-entry topbar-launcher
rm -f "$failing_exec"
key_press Return
check assert_mapped topbar-launcher "failed launch remains actionable"
check shot launcher-launch-error topbar-launcher
check hide_launcher

# gtk4-demo was started above as the non-flat backdrop. A layer-shell chooser
# is not an xdg-toplevel and would not exercise the niri window projection, so
# the actual GTK window remains required before this search.
echo "--- launcher window search"
if ! grep -q 'Demo' "$art/niri-windows.txt"; then
  echo "gtk4-demo is unavailable; launcher window state cannot be driven" >&2
  fail=1
else
  check show_launcher
  type_text "Demo"
  check shot launcher-windows topbar-launcher
  check picker_tail_probe "GtkButton launcher-item" "Search applications, actions, windows, and files" Demo
  key_press Escape
  check assert_unmapped topbar-launcher "Escape closes window search"
fi

echo "--- launcher file and action searches"
if [ ! -f "$fixture_file" ]; then
  echo "launcher file fixture is missing: $fixture_file" >&2
  fail=1
else
  check wait_for_file_catalog
  check show_launcher
  type_text "smoke-document"
  check shot launcher-files topbar-launcher
  check picker_tail_probe "GtkButton launcher-file-row" "Search applications, actions, windows, and files" smoke-document
  key_press Escape
  check assert_unmapped topbar-launcher "Escape closes file search"
fi

check show_launcher
type_text "theme"
check shot launcher-actions topbar-launcher
check picker_tail_probe "GtkButton launcher-item" "Search applications, actions, windows, and files" theme
# The launcher backdrop is deliberately clickable.  This click is below the
# centered content at every supported nested output size.
if ! backdrop_click_y=$(backdrop_y); then
  echo "could not determine nested output size for launcher backdrop click" >&2
  fail=1
else
  click_at 8 "$backdrop_click_y"
  check assert_unmapped topbar-launcher "backdrop click closes launcher"
fi
check show_launcher
type_text "A deliberately unmatched and very long search query that must remain readable and show an empty result"
check shot launcher-empty-long-query topbar-launcher
key_press Escape
check assert_unmapped topbar-launcher "Escape closes empty result"

echo "--- modal handoff and queued standalone input"
check show_launcher
"$SMOKE_TOPBAR" --config "$SMOKE_CONFIG" choose --layout list \
  --title "First queued chooser" <"$theme_json" \
  >"$art/chooser-handoff-first.result" 2>"$art/chooser-handoff-first.stderr" &
chooser_pid=$!
check assert_unmapped topbar-launcher "standalone chooser dismisses launcher"
check shot chooser-modal-handoff topbar-chooser
PICKER_LOG="$art/chooser-handoff-first.stderr" PICKER_PREFIX=chooser-dump \
  check picker_probe "GtkButton chooser-result" Search dwn
"$SMOKE_TOPBAR" --config "$SMOKE_CONFIG" choose --layout list \
  --title "Second queued chooser" <"$theme_json" \
  >"$art/chooser-handoff-second.result" 2>"$art/chooser-handoff-second.stderr" &
queued_chooser_pid=$!
check shot chooser-contention-first topbar-chooser
niri msg layers >"$art/chooser-contention-layers.txt" 2>&1 || true
chooser_surfaces=$(grep -o '"topbar-chooser"' "$art/chooser-contention-layers.txt" | wc -l | tr -d ' ')
chooser_backdrops=$(grep -o '"topbar-chooser-backdrop"' "$art/chooser-contention-layers.txt" | wc -l | tr -d ' ')
if [ "$chooser_surfaces" -ne 1 ] || [ "$chooser_backdrops" -ne 1 ]; then
  echo "queued chooser mapped before input ownership transferred ($chooser_surfaces foregrounds, $chooser_backdrops backdrops)" >&2
  fail=1
fi
key_press Escape
if wait "$chooser_pid"; then
  echo "first queued chooser unexpectedly accepted" >&2
  fail=1
fi
chooser_pid=""
check shot chooser-contention-second topbar-chooser
key_press Escape
if wait "$queued_chooser_pid"; then
  echo "second queued chooser unexpectedly accepted" >&2
  fail=1
fi
queued_chooser_pid=""
niri msg layers >"$art/launcher-layers.txt" 2>&1 || true

# Choosers and pinentry must work when there is no panel process owning the
# transient-surface IPC endpoint.  Stop it only after every daemon-owned
# launcher state has been captured; the outer harness tolerates this expected
# early exit when its normal cleanup runs later.
echo "--- stop panel before standalone dialogs"
kill "$SMOKE_PANEL_PID" 2>/dev/null || true
wait "$SMOKE_PANEL_PID" 2>/dev/null || true
if [ -n "$persistent_child" ] && ! kill -0 "$persistent_child" 2>/dev/null; then
  echo "persistent Exec died when the panel exited" >&2
  fail=1
fi
touch "$persist_release"

echo "--- standalone dark and light theme choosers"
check start_chooser themes-dark "$SMOKE_CONFIG" themes moonlight "$theme_json"
check shot chooser-themes-dark topbar-chooser
# Eight ordered themes exceed the four-row viewport. Keyboard focus must
# scroll the last row into view, not merely move the selected CSS class.
key_press Down
key_press Down
key_press Down
key_press Down
key_press Down
key_press Down
key_press Down
check shot chooser-themes-scrolled topbar-chooser
check assert_theme_results_scrolled
# Home puts focus on a real row: typing must not require returning to Search.
key_press Home
PICKER_LOG="$art/themes-dark.stderr" PICKER_PREFIX=chooser-dump \
  check picker_probe "GtkButton chooser-result" Search dwn
type_text "dawn"
check shot chooser-themes-filtered topbar-chooser
key_press Return
if ! wait "$chooser_pid"; then
  echo "dark theme chooser failed to accept its keyboard selection" >&2
  fail=1
fi
chooser_pid=""
if [ "$(cat "$art/themes-dark.result")" != "dawn" ]; then
  echo "dark theme chooser returned the wrong stable ID" >&2
  fail=1
fi

check start_chooser themes-light "$light_config" themes dawn "$theme_json"
check shot chooser-themes-light topbar-chooser
finish_chooser_cancelled themes-light

echo "--- standalone wallpaper chooser and decode failure"
check start_chooser wallpapers "$SMOKE_CONFIG" wallpapers valid "$wallpaper_json"
check shot chooser-wallpapers-valid topbar-chooser
key_press Down
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump \
  check picker_probe "GtkButton chooser-result" Search vld
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump \
  check picker_focus "GtkButton chooser-result"
key_press space
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump picker_dump
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump check picker_read query Search ""
# Real compose key presses, not wtype injecting a precomposed Unicode string.
wtype -k Multi_key -k apostrophe -k e
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump picker_dump
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump check picker_read query Search "é"
key_press BackSpace
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump picker_dump
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump check picker_read query Search ""
# The first row is pointer-focusable too, without committing a wallpaper.
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump \
  check picker_click "GtkButton chooser-result" 1
type_text "vld"
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump picker_dump
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump check picker_read query Search vld
key_press Down
key_press BackSpace
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump picker_dump
PICKER_LOG="$art/wallpapers.stderr" PICKER_PREFIX=chooser-dump check picker_read query Search vl
wtype -M ctrl -k a -m ctrl
key_press BackSpace
# The fixture contains six wallpapers.  Moving to the sixth proves the fixed
# four-row viewport scrolls instead of clipping the lower choices; the shot
# leaves a visual artifact that also shows the permanent scrollbar.
key_press Down
key_press Down
key_press Down
key_press Down
key_press Down
check shot chooser-wallpapers-scrolled topbar-chooser
if selected_wallpaper=$(wallpaper_selected_outline "$art/chooser-wallpapers-scrolled.png"); then
  IFS=' ' read -r wallpaper_x wallpaper_top wallpaper_width wallpaper_bottom <<EOF
$selected_wallpaper
EOF
  if wheel_xy=$(shot_scale "$((wallpaper_x + wallpaper_width / 2))" \
    "$((wallpaper_top + (wallpaper_bottom - wallpaper_top) / 2))") && \
    away_y=$(backdrop_y); then
    IFS=' ' read -r wheel_x wheel_y <<EOF
$wheel_xy
EOF
    scroll_at "$wheel_x" "$wheel_y" -45
    pointer_to 8 "$away_y"
    check shot chooser-wallpapers-wheel topbar-chooser
    key_press Up
    check shot chooser-wallpapers-reverse topbar-chooser
    check assert_wallpaper_wheel_and_reverse
  else
    echo "wallpaper chooser: could not locate wheel target" >&2
    fail=1
  fi
else
  echo "wallpaper chooser: selected sixth row is not visible" >&2
  fail=1
fi
type_text "unreadable"
# `shot` waits for an actually presented, settled frame.  The invalid image is
# small and the chooser rerenders once decode failure reaches the main thread,
# so the capture records its visible local failure rather than a timer guess.
check shot chooser-wallpapers-unreadable topbar-chooser
finish_chooser_cancelled wallpapers

# A compact output deliberately hides previews to keep its result rows. Scale
# down this focused second chooser so the screenshot exercises preview swaps.
original_scale="${TOPBAR_SMOKE_SCALE:-1.25}"
niri msg output winit scale 0.5 >>"$art/output-scale.log" 2>&1
TOPBAR_SMOKE_SCALE=0.5
export TOPBAR_SMOKE_SCALE
check start_chooser wallpapers-preview "$SMOKE_CONFIG" wallpapers valid "$wallpaper_json"
check shot chooser-wallpapers-preview-valid topbar-chooser
frame_height=$(magick "$art/chooser-wallpapers-preview-valid.png" -format '%h' info:)
first=$(wait_for_wallpaper_selection chooser-wallpapers-preview-valid 0 "$frame_height") || exit 1
IFS=' ' read -r first_top first_bottom <<EOF
$first
EOF
row_height=$((first_bottom - first_top))
check wait_for_wallpaper_preview_colour chooser-wallpapers-preview-valid ready
key_press Down
failed=$(wait_for_wallpaper_selection chooser-wallpapers-preview-failed \
  "$((first_top + row_height * 3 / 4))" \
  "$((first_top + row_height * 3 / 2))") || exit 1
IFS=' ' read -r failed_top failed_bottom <<EOF
$failed
EOF
check wait_for_wallpaper_preview_colour chooser-wallpapers-preview-failed failed
key_press Down
portrait=$(wait_for_wallpaper_selection chooser-wallpapers-preview-portrait \
  "$((failed_top + row_height * 3 / 4))" \
  "$((failed_top + row_height * 3 / 2))") || exit 1
check wait_for_wallpaper_preview
key_press Up
key_press Up
returned=$(wait_for_wallpaper_selection chooser-wallpapers-preview-returned \
  "$((first_top - row_height / 4))" \
  "$((first_top + row_height / 4))") || exit 1
check assert_wallpaper_dialog_stable
check assert_wallpaper_preview_changes
finish_chooser_cancelled wallpapers-preview
niri msg output winit scale "$original_scale" >>"$art/output-scale.log" 2>&1
TOPBAR_SMOKE_SCALE="$original_scale"
export TOPBAR_SMOKE_SCALE

echo "--- one mapped tabbed wallpaper chooser during search and save"
"$SMOKE_TOPBAR" --config "$SMOKE_CONFIG" choose --layout wallpapers \
  --title "Smoke wallpaper tabs" --wallpaper-provider "$SMOKE_WALLPAPER_PROVIDER" \
  <"$SMOKE_WALLPAPER_TABS_JSON" >"$art/wallpaper-tabs.result" \
  2>"$art/wallpaper-tabs.stderr" &
chooser_pid=$!
check shot wallpaper-tabs-pool topbar-chooser
tabbed_bounds=$(wallpaper_dialog_bounds "$art/wallpaper-tabs-pool.png") || fail=1
if [ -e "$SMOKE_WALLPAPER_GATE/search.ready" ]; then
  echo "Wallhaven searched before tab activation" >&2
  fail=1
fi
# Ctrl+Tab switches tabs and focuses the Wallhaven API query.
wtype -M ctrl -k Tab -m ctrl
if wait_for_marker "$SMOKE_WALLPAPER_GATE/search.ready" "Wallhaven search gate"; then
  check assert_mapped topbar-chooser "same chooser during search"
  check assert_mapped topbar-chooser-backdrop "same backdrop during search"
  snap wallpaper-tabs-search-spinner-a 2
  snap wallpaper-tabs-search-spinner-b 2
  searching_bounds=$(wallpaper_dialog_bounds "$art/wallpaper-tabs-search-spinner-a.png") || fail=1
  if [ "$searching_bounds" != "$tabbed_bounds" ]; then
    echo "wallpaper chooser resized when switching Local to Wallhaven search" >&2
    fail=1
  fi
  spinner_motion=$(magick "$art/wallpaper-tabs-search-spinner-a.png" \
    "$art/wallpaper-tabs-search-spinner-b.png" -compose Difference -composite \
    -colorspace Gray -format '%[fx:mean]' info:) || spinner_motion=0
  if ! awk -v difference="$spinner_motion" 'BEGIN { exit !(difference > 0.000001) }'; then
    echo "Wallhaven search spinner did not animate in the mapped chooser" >&2
    fail=1
  fi
  touch "$SMOKE_WALLPAPER_GATE/search.release"
  if wait_for_marker "$SMOKE_WALLPAPER_GATE/search.first.ready" "first streamed Wallhaven row"; then
    first_rows=0
    first_attempt=0
    while [ "$first_attempt" -lt 20 ]; do
      PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump picker_dump || break
      first_rows=$(PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump \
        picker_read count "GtkButton chooser-result") || break
      # Apply becoming sensitive also proves the selected preview has decoded.
      if [ "$first_rows" -eq 1 ] && \
        PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump \
          picker_read enabled "GtkButton | Apply" 2>/dev/null; then
        break
      fi
      sleep 0.1
      first_attempt=$((first_attempt + 1))
    done
    if [ "$first_rows" -ne 1 ]; then
      echo "first Wallhaven row was not rendered before provider completion" >&2
      fail=1
    fi
    PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump \
      check picker_read enabled "GtkButton | Apply"
    searching_labels=$(PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump \
      picker_read count "GtkLabel chooser-subtitle | Searching Wallhaven…") || searching_labels=0
    if [ "$searching_labels" -ne 1 ]; then
      echo "loading status disappeared while streamed Wallhaven rows were arriving" >&2
      fail=1
    fi
    snap wallpaper-tabs-first-result 2
  else
    fail=1
  fi
  touch "$SMOKE_WALLPAPER_GATE/search.finish.release"
  check shot wallpaper-tabs-results topbar-chooser
  results_bounds=$(wallpaper_dialog_bounds "$art/wallpaper-tabs-results.png") || fail=1
  if [ "$results_bounds" != "$tabbed_bounds" ]; then
    echo "wallpaper chooser resized when Wallhaven results loaded" >&2
    fail=1
  fi
  operator_query='+nature -people @someone type:png like:9d82vk & café'
  type_text "$operator_query"
  key_press Home
  key_press Right
  key_press End
  wtype -M ctrl -k a -m ctrl
  key_press BackSpace
  type_text "$operator_query"
  if [ -e "$SMOKE_WALLPAPER_GATE/query.ready" ] || [ -e "$SMOKE_WALLPAPER_GATE/save.ready" ]; then
    echo "editing the API query searched or saved without submission" >&2
    fail=1
  fi
  key_press Return
  if wait_for_marker "$SMOKE_WALLPAPER_GATE/query.ready" "submitted Wallhaven query" exists; then
    if [ "$(cat "$SMOKE_WALLPAPER_GATE/query.value")" != "$operator_query" ]; then
      echo "Wallhaven query operators were not forwarded unchanged" >&2
      fail=1
    fi
    if [ -e "$SMOKE_WALLPAPER_GATE/save.ready" ]; then
      echo "Enter in the Wallhaven query saved instead of searching" >&2
      fail=1
    fi
    check assert_mapped topbar-chooser "query Enter leaves chooser mapped"
    touch "$SMOKE_WALLPAPER_GATE/query.release"
    check shot wallpaper-tabs-query-results topbar-chooser
    # Focus a loaded result; typing routes to Filter, never the API query.
    PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump \
      check picker_probe "GtkButton chooser-result" "Filter results" stcl
    key_press Tab
    type_text "stcl"
    check shot wallpaper-tabs-tag-filtered topbar-chooser
    key_press Down
    key_press BackSpace
    PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump picker_dump
    PICKER_LOG="$art/wallpaper-tabs.stderr" PICKER_PREFIX=chooser-dump \
      check picker_read query "Filter results" stc
    wtype -M ctrl -k a -m ctrl
    key_press BackSpace
    check shot wallpaper-tabs-filter-cleared topbar-chooser
    type_text "stcl"
    check shot wallpaper-tabs-tag-selected topbar-chooser
    for state in query-results tag-filtered filter-cleared tag-selected; do
      measured=$(wallpaper_dialog_bounds "$art/wallpaper-tabs-$state.png") || fail=1
      if [ "$measured" != "$tabbed_bounds" ]; then
        echo "wallpaper chooser resized on $state: $tabbed_bounds -> $measured" >&2
        fail=1
      fi
    done
    # Filter -> sole matching result -> Cancel -> Apply. Space activates the
    # actual Apply button, and the provider rejects any other selected ID.
    key_press Tab
    key_press Tab
    key_press Tab
    key_press space
  fi
  if wait_for_marker "$SMOKE_WALLPAPER_GATE/save.ready" "Wallhaven save gate"; then
    if [ "$(cat "$SMOKE_WALLPAPER_GATE/save.ready")" != ab12cd ]; then
      echo "tag-only fuzzy filtering did not select its matching wallpaper" >&2
      fail=1
    fi
    check assert_mapped topbar-chooser "same chooser during save"
    check assert_mapped topbar-chooser-backdrop "same backdrop during save"
    snap wallpaper-tabs-save-spinner-a 2
    snap wallpaper-tabs-save-spinner-b 2
    spinner_motion=$(magick "$art/wallpaper-tabs-save-spinner-a.png" \
      "$art/wallpaper-tabs-save-spinner-b.png" -compose Difference -composite \
      -colorspace Gray -format '%[fx:mean]' info:) || spinner_motion=0
    if ! awk -v difference="$spinner_motion" 'BEGIN { exit !(difference > 0.000001) }'; then
      echo "Wallhaven save spinner did not animate in the mapped chooser" >&2
      fail=1
    fi
    touch "$SMOKE_WALLPAPER_GATE/save.release"
  fi
fi
# Release every gate and cancel a failed scenario rather than stranding a chooser.
touch "$SMOKE_WALLPAPER_GATE/search.release" "$SMOKE_WALLPAPER_GATE/query.release" \
  "$SMOKE_WALLPAPER_GATE/save.release"
if [ ! -e "$SMOKE_WALLPAPER_GATE/save.ready" ]; then
  fail=1
  key_press Escape
fi
if ! wait "$chooser_pid"; then
  echo "tabbed chooser did not save the selected wallpaper" >&2
  fail=1
fi
chooser_pid=""
if ! grep -q '"kind":"saved"' "$art/wallpaper-tabs.result"; then
  echo "tabbed chooser did not emit a saved image result" >&2
  fail=1
fi
check assert_unmapped topbar-chooser "tabbed chooser closes after save"

echo "--- standalone pinentry password and confirmation cancellation"
password_request='SETTITLE Smoke password prompt
SETDESC A dummy password request. No password is entered or recorded.
SETPROMPT Test passphrase
GETPIN'
check start_pinentry password "$password_request"
check shot pinentry-password topbar-pinentry
# Clicking away must leave an authentication prompt alive.  The following
# Escape is the deliberate cancellation that closes it.
if backdrop_click_y=$(backdrop_y); then
  click_at 8 "$backdrop_click_y"
  check assert_mapped topbar-pinentry "backdrop click does not cancel pinentry"
else
  echo "could not determine nested output size for pinentry backdrop click" >&2
  fail=1
fi
finish_pinentry_cancelled password

confirm_request='SETTITLE Smoke confirmation prompt
SETDESC A dummy confirmation request. Escape must report a denial.
CONFIRM'
check start_pinentry confirmation "$confirm_request"
check shot pinentry-confirmation topbar-pinentry
finish_pinentry_cancelled confirmation

niri msg layers >"$art/standalone-layers.txt" 2>&1 || true
if [ "$fail" -eq 0 ]; then
  echo "--- result: PASS"
else
  echo "--- result: FAIL"
fi
exit "$fail"
