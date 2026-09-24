# Configuration

Every key topbar accepts, what it means, and what it does when it is left out.
The schema lives in `crates/topbar-core/src/config.rs`; the compiled-in example
at the repository root (`config.toml`, printed by `topbar
--print-example-config`) states the defaults in the same order this document
does, and a test asserts that parsing it produces exactly `Config::default()`.

Every section and every key is optional. `Config::default()` *is* the merge:
there is no deep-merge machinery, so a file that names one key gets that key
and the built-in value for everything else.

## Where the file lives

With no `--config`, topbar searches these paths in order and loads the first
one that exists:

1. `$XDG_CONFIG_HOME/topbar/config.toml`
2. `~/.config/topbar/config.toml`
3. `$XDG_CONFIG_HOME/gnome-topbar/config.toml`
4. `~/.config/gnome-topbar/config.toml`
5. `./config.toml`

Both current-name directories sort before both legacy ones, so a user who has
already moved their file never sees a deprecation notice. A file found under
the project's former `gnome-topbar` name still loads in full; it prints one
line saying where the file is and where it belongs now. That line is
deliberately not a configuration warning — `--strict` (below) must not refuse
to start over a file that is merely in the old place.

The working directory stays last and stays un-namespaced. It is the dev-shell
convenience, not somewhere users keep configuration.

`--config PATH` replaces the whole chain. It is used strictly: the file must
exist and must parse, there is no fallback to defaults, and no legacy notice is
printed because the caller named the file it wanted. A running panel remembers
the path it was started with, so a reload re-reads the same file rather than
whatever the search chain would now prefer.

When no file exists anywhere, the built-in defaults are used and the panel says
so in the log.

## Checking a file

```
topbar --check-config             validate, print the source, exit
topbar --check-config --strict    the same, but warnings fail
topbar --print-example-config     print the compiled-in example
topbar dump config                print what the running panel is using
```

Failures come in two kinds and they are treated differently.

**Errors** are values that cannot be honoured at all: a bar position of
`"bottom"`, a size of zero, a colour that is not hex, an out-of-range opacity, a
crypto entry naming an asset that does not exist. Every error in the file is
collected and reported together, so one run tells you everything rather than
one thing at a time. A file with any error does not load.

**Warnings** are keys the schema does not recognise, and keys whose feature was
dropped. They never fail a load on their own; `--strict` is the opt-in that
turns them into a failure, which is what CI wants and what a first start does
not.

`topbar dump config` prints the *effective* configuration, defaults included —
the question that command exists to answer is "what is the panel actually
using", not "what did I write". Its output parses back to the same
configuration, so it is a config file as well as a report.

## Coming from v1

v1 configuration files load unchanged. That is a tested contract, not an
intention: `crates/topbar-core/tests/fixtures/live-config.toml` is a
byte-for-byte copy of a real v1 file, and a test fails if loading it produces
any warning at all.

Keys whose feature was removed in the rewrite are accepted and produce a
*specific* explanation rather than a generic "unknown option". The table is
`DROPPED_KEYS` in `config.rs`, with `DROPPED_WIDGET_KEYS` and
`DROPPED_CUSTOM_KEYS` for the per-widget ones. They fall into a few groups:

- **The outline system.** `bar.outline`, `widgets.outline`, `theme.outline`,
  `theme.outline_width`, `theme.outline_color`, `theme.outline_opacity`, and
  per-widget `outline_color`. Surfaces now use a fixed 1px hairline border and
  panel buttons are transparent until hovered.
- **Theming that is no longer variable.** `theme.scheme`, `theme.wallpaper`,
  `theme.popover`, `theme.shadows`. The native dark and light palettes can each
  be overridden through `[theme.palette]`; popover shadows are always on.
- **Per-widget visibility.** `disabled`, `show_if`, `show_if_interval`. A
  widget is shown by being placed in `left`, `center` or `right`, and widgets
  that have nothing to say hide themselves.
- **Per-widget decoration.** `background_color` inside a widget section.
- **Structural leftovers.** `bar.outputs` (v2 draws on every monitor),
  `updates.terminal` (upgrades open with the XDG default terminal),
  `widgets.media` (the control panel's media card has no configuration),
  `widgets.clock.control_panel_weather_widget`,
  `widgets.workspaces.separator`, and the `custom-*` keys `image` and
  `position`.

Two v1 shapes inside the placement arrays are accepted and normalised: a group
table (`{ group = [...] }`) is flattened, and an inline argument (`clock:short`)
keeps the base name. Both warn.

A dropped toggle set to the value v2 already behaves as is silently ignored:
`theme.outline = false` asks for exactly what v2 does, so there is nothing to
tell anyone. The inverted value warns. The same applies to `bar.outputs = []`,
`theme.shadows = true` and `disabled = false`.

Dropped *values* are handled the same way. `theme.mode` values `auto` and
`gtk` warn and render as `"dark"`; `"light"` is supported. `theme.accent = "gtk"` warns and
falls back to the default accent. The one v1 value that is a hard error instead
is `bar.position = "bottom"`, because there is no honest way to draw it.

## What the panel never writes

topbar never edits `config.toml`. Choices made at runtime go to
`$XDG_STATE_HOME/topbar/state.json` instead: the weather location picked in the
setup dialog, the crypto entries chosen in the settings view, the notification
history, and the Do Not Disturb flag.

That file is the reason two config keys behave as seeds rather than as
settings. `widgets.weather.latitude`/`.longitude` are used only when the dialog
has not saved a location; a location chosen in the UI wins from then on.
`widgets.crypto.entries` is used only when the settings view has not saved a
list. Deleting `state.json` returns both to what the config file says.

---

# Reference

## `[bar]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `position` | string | `"top"` | Screen edge. `"top"` is the only value. |
| `size` | integer | `36` | Bar height, in pixels. |
| `spacing` | integer | `2` | Gap between widgets, in pixels. |
| `screen_margin` | integer | `0` | Gap between the screen edge and the bar window. |
| `inset` | integer | `4` | Gap between the bar edge and the first/last section. |
| `padding` | integer | `0` | Extra vertical padding inside the bar. |
| `border_radius` | integer | `0` | Bar corner radius, in pixels. |
| `popover_offset` | integer | `1` | Gap between the bar and popovers anchored to it. |
| `background_color` | hex string | `"#000000"` | Bar background colour. |
| `background_opacity` | float | `1.0` | Bar background opacity, `0.0`–`1.0`. |

`position = "bottom"` is a hard error: v2 draws a GNOME Shell-style top panel
and there is no bottom variant to fall back to. `size` must be greater than
zero. The bar height is also the layer surface's exclusive zone, which is why
changing it rebuilds the windows rather than resizing them.

`popover_offset` is the only gap the panel adds under the bar; the compositor's
own exclusive-zone arithmetic supplies the rest.

## `[widgets]`

Placement first. A widget appears because it is named in one of the three
arrays, and it appears in the order it is written.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `left` | array of strings | `["workspaces"]` | Left section, in order. |
| `center` | array of strings | `["clock"]` | Center section, in order. |
| `right` | array of strings | `["tray", "quick_settings"]` | Right section, in order. |

The accepted names are `workspaces`, `clock`, `weather`, `crypto`, `tray`,
`quick_settings`, `system_monitor`, `headset`, `keyboard_layout`, `os_logo`, and
any number of `custom-<name>` script widgets. An unrecognised name warns and is
skipped rather than failing the load. A `[widgets.<name>]` table for a widget
that is not placed anywhere also warns — options that will never be read are
almost always a typo in the placement array.

Then the styling keys, which apply to every panel button:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `border_radius` | integer | `50` | Widget corner radius, as a percentage of bar height. |
| `background_color` | hex string | unset | Widget surface colour. Unset uses the theme surface. |
| `background_opacity` | float | `0.0` | Widget background opacity, `0.0`–`1.0`. |
| `popover_background_opacity` | float | unset | Popover opacity. Unset follows the bar's. |

`border_radius` is a percentage of the bar height rather than a pixel count, so
a bar resized from 36 to 44 keeps the shape it had. At `50` or above a panel
button is a full pill; below that the radius is `bar.size * percent / 100`,
capped at half the bar height. The default opacity of `0.0` is what makes panel
buttons invisible until they are hovered, which is how GNOME Shell's panel
behaves.

### Click commands

Every widget section accepts `on_click`, `on_click_right` and
`on_click_middle`: shell command lines run when that button is released.
Commands go through the panel's one process runner, which reaps the child and
turns an immediate failure — a command that does not exist, most often — into a
banner rather than into silence.

Which buttons are actually wired depends on what the widget already does with
them:

- `headset`, `os_logo` and `custom-*` wire all three.
- `quick_settings`, `system_monitor` and `notmuch` wire right and middle only;
  left click opens their popover.
- `clock`, `weather`, `crypto`, `tray`, `keyboard_layout` and `workspaces`
  accept the keys for compatibility but do not act on them.

## `[widgets.workspaces]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `label_type` | string | `"none"` | `"none"` (dots), `"index"`, or `"name"`. |
| `animate` | bool | unset | Animate dot/pill transitions. Unset follows `theme.animations`. |
| `filter_by_output` | bool | `true` | Show only this monitor's workspaces. |
| `show_unoccupied` | bool | `false` | Show workspaces that hold no windows. |

`filter_by_output` is per-bar rather than global: the widget knows which
monitor it was built on, so the same configuration draws different dots on
different screens.

`show_unoccupied` governs niri's *dynamic* workspaces. A workspace named in the
compositor's configuration — `workspace "chat"` — always exists whether or not
anything is open on it, so it always gets a dot, as do the active workspace and
any workspace asking for attention.

## `[widgets.clock]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `format` | string | `"%a %b %-d  %H:%M"` | `strftime` format for the panel label. |
| `control_panel` | bool | `false` | Open the notifications/calendar panel on click. |
| `show_week_numbers` | bool | `true` | Show ISO week numbers in the calendar. |
| `world_clocks` | array of tables | `[]` | Extra time zones listed in the control panel. |

`format` must not be empty. Each `world_clocks` entry is a table with a
non-empty `label` and a non-empty IANA `timezone`:

```toml
world_clocks = [
  { label = "New York", timezone = "America/New_York" },
  { label = "UTC", timezone = "Etc/UTC" },
]
```

`control_panel = true` is what makes the clock draw a GNOME date menu, and it
is also what asks for the weather and media services — the panel's forecast
card and media controls are the only things that use them when no `weather`
widget is placed.

It is also what puts the two indicators beside the time: a bell-with-slash
while Do Not Disturb is on, and an accent-coloured dot while the history holds
something that arrived since the panel was last opened. The dot clears on the
next open, which is what "seen" means; a clock with `control_panel = false`
opens nothing, so it never draws the dot at all.

## `[widgets.weather]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `latitude` | float | unset | Latitude, `-90`–`90`. |
| `longitude` | float | unset | Longitude, `-180`–`180`. |
| `unit` | string | `"celsius"` | `"celsius"`/`"c"` or `"fahrenheit"`/`"f"`. |
| `interval` | integer | `1800` | Seconds between refreshes. Minimum 60. |
| `tooltip` | string | `"Weather"` | Static tooltip prefix. |
| `max_chars` | integer | unset | Ellipsize the panel label past this many characters. |
| `show_description` | bool | `true` | Name the condition on the bar, not just the temperature. |
| `forecast_days` | integer | `5` | Forecast rows in the control panel, `3`–`5`. |

Coordinates are usually not written here. The widget's popover has a location
search that saves what it finds to `state.json`, and a saved location takes
precedence over these keys — so setting `latitude`/`longitude` in the config
after using the dialog appears to do nothing until the saved location is
cleared. Their role is to seed a fresh install, or to pin a location for a
machine whose configuration is managed.

`interval` below 60 seconds is a hard error rather than a silent clamp: it is
somebody else's public API being polled, and quietly ignoring the number is
worse than saying no.

`show_description = false` cuts the bar down to `☁ 21°`. It is the bar's line
only: the tooltip, the popover and the control panel's forecast card all go on
naming the condition in full.

## `[widgets.notmuch]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `query` | string | `"tag:unread and tag:inbox"` | The notmuch query the count and the list come from. |
| `interval` | integer | `300` | Seconds between counts. Minimum 60. |
| `max_items` | integer | `10` | Conversations the popover lists. |
| `tooltip` | string | `"Mail"` | Shown until there is a count to show instead. |

An envelope on the bar, and nothing at all when nothing matches the query. The
count is in the tooltip rather than beside the icon: an envelope is the same
width at three messages and at three hundred, and a widget that changes width
every time mail arrives moves everything next to it. Left click opens a list of
the newest conversations — sender, subject, and when the newest message in each
arrived.

The panel does not sync mail. Something else fills the maildir and runs
`notmuch new` — lieer, mbsync, offlineimap, a systemd timer — and this widget
reads the index that already exists. It never opens a message file and never
writes anything. `notmuch` has to be on `PATH`, and where the mail lives is
`NOTMUCH_CONFIG`'s business; there is deliberately no key here that answers that
question a second time.

The header's number counts **messages** and the rows are **conversations**, so
the two rarely match. A conversation holding more than one unread message says
how many, which is what reconciles them.

`query` is passed to notmuch as a single argument and never through a shell. It
is also not checked beyond being non-empty, because notmuch's parser is lenient
— `tag:unread and ((` returns a number rather than an error — so there is
nothing the panel could inspect that would tell a typo from an intention. If
the count looks wrong, run the query through `notmuch count` and compare.

A `notmuch` that cannot be run, a database that will not open, or output that
does not parse all hide the widget rather than showing a zero: "nothing unread"
and "I could not tell" look identical on a panel, and only one of them is safe
to guess. The reason is logged once per attempt.

## `[widgets.crypto]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `entries` | array of strings | `["btc", "eth", "eth/btc"]` | What to price. |
| `interval` | integer | `1800` | Seconds between refreshes. Minimum 60. |
| `tooltip` | string | `"Crypto prices"` | Static tooltip prefix. |
| `max_chars` | integer | unset | Ellipsize the panel label past this many characters. |

An entry is either a single asset (`"btc"`) or a pair whose value is the ratio
of one to the other (`"eth/btc"`). The asset set is closed: `btc`, `eth`, `xmr`.
Both halves of a pair must be supported assets and they must differ, so
`"btc/btc"` and `"sol/eth"` are errors. The set is closed because a single
request fetches all three whatever is configured, which is what makes turning
one on in the popover's settings view a redraw rather than a round trip.

Entries chosen in that settings view are saved to `state.json` and override this
key from then on.

## `[widgets.tray]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `max_icons` | integer | `12` | Icons shown inline before the overflow chevron. |
| `pixmap_icon_size` | integer | unset | Render size for non-themed icons. Unset uses the theme size. |

`pixmap_icon_size` is read at start-up as well as by the widget: the tray asks
StatusNotifierItem hosts for the pixmap size it will actually draw at, so the
icon that arrives is the icon that is shown rather than one scaled afterwards.

## `[widgets.quick_settings]`

Every key is a row in the menu, and every one defaults to on. Set one to
`false` to hide that row.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `network` | bool | `true` | The Wi-Fi/wired row. |
| `bluetooth` | bool | `true` | The Bluetooth row. |
| `vpn` | bool | `true` | The VPN row. |
| `idle_inhibitor` | bool | `true` | The Caffeine toggle. |
| `updates` | bool | `true` | The pending-updates card. |
| `audio` | bool | `true` | The output volume slider. |
| `mic` | string | `"auto"` | `"auto"` (while a source is in use), `"always"`, or `"never"`. Booleans still parse: `true` is `"auto"`, `false` is `"never"`. |
| `brightness` | bool | `true` | The backlight slider. |
| `power` | bool | `true` | The suspend/restart/shut down section. |
| `battery` | bool | `true` | The battery pill in the panel indicator. |
| `battery_health` | bool | `true` | The battery health and charge-threshold card. |
| `resource_overview` | bool | `true` | The CPU/memory/disk card. |
| `vpn_close_on_connect` | bool | `true` | Close the menu once a VPN connects. |
| `audio_scroll_percentage` | integer | `5` | Volume change per scroll tick, `1`–`25`. |

The VPN row lists every NetworkManager VPN and WireGuard profile. A profile
whose secrets are stored — WireGuard keys, or an OpenVPN password in the
keyring — connects from the panel. One that expects a password to be typed each
time does not, because answering that prompt means running the VPN plugin's own
auth-dialog binary. Such a profile reports the failure under its own row;
connecting it once with `nm-applet` or `nmcli` lets NetworkManager store the
secret and the panel can then use it.

## `[widgets.system_monitor]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `cpu_threshold` | integer | `90` | CPU percentage that makes the widget appear, `1`–`100`. |
| `memory_threshold` | integer | `85` | Memory percentage, `1`–`100`. |
| `disk_threshold` | integer | `90` | Disk percentage, `1`–`100`. |
| `interval` | integer | `5` | Seconds between samples. Must be above zero. |
| `tooltip` | string | `"System load"` | Static tooltip prefix. |

The widget is alert-only: it has zero width while every metric is healthy and
fades in when one crosses its threshold. A permanent readout is a number the
eye stops seeing after a day, and a relayout every five seconds for the
privilege.

Because a threshold on a moving number flickers, each metric runs through
hysteresis: two consecutive samples at or above the threshold to appear, two
consecutive samples five points below it to go away again, and two at
threshold + 8 to escalate to the urgent colour. Nothing at all happens between
"threshold − 5" and the threshold, which is the dead band that stops a machine
sitting on the line from oscillating.

## `[widgets.headset]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `interval` | integer | `5` | Seconds between polls. Must be above zero. |
| `tooltip` | string | `"Headset battery"` | Static tooltip prefix. |
| `max_chars` | integer | unset | Ellipsize the panel label past this many characters. |
| `command` | string | `"headsetcontrol"` | Executable queried for battery state. Must not be empty. |

The widget hides itself when the command reports no headset, so an unplugged
dongle costs no bar space.

## `[widgets.keyboard_layout]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `show_icon` | bool | `true` | Show the keyboard icon. |
| `show_label` | bool | `true` | Show the layout label. |
| `format` | string | `"short"` | `"short"` (`US`) or `"long"` (`English (US)`). |

A session configured with one layout has nothing to switch to, and the widget
draws nothing at all.

## `[widgets.os_logo]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `tooltip` | string | unset | Tooltip override. Unset uses the detected distro name. |
| `label` | string | unset | Label override. Unset uses the detected distro glyph. |
| `max_chars` | integer | `4` | Ellipsize the panel label past this many characters. |

Both overrides exist for distributions whose `/etc/os-release` name is longer
than anybody wants on a panel, and for glyphs that a given Nerd Font does not
carry.

## `[widgets.custom-*]`

Script-backed indicators. The section name carries the widget name after the
`custom-` prefix, and that full name is what goes in a placement array:

```toml
[widgets]
left = ["workspaces", "custom-vpn"]

[widgets.custom-vpn]
exec = "~/bin/vpn-status"
interval = 60
template = "VPN {output}"
icon = "network-vpn-symbolic"
on_click = "nm-connection-editor"
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `exec` | string | unset | Command whose output becomes the label. |
| `interval` | integer | `0` | Seconds between runs. `0` runs once at start-up. |
| `tooltip` | string | unset | Static tooltip. Overridden by JSON output. |
| `max_chars` | integer | unset | Ellipsize the panel label past this many characters. |
| `requires_network` | bool | `false` | Wait for a live connection before running `exec`. |
| `icon` | string | unset | Symbolic icon name shown before the label. |
| `label` | string | `""` | Static or fallback label text. |
| `template` | string | unset | Format for `exec` output; must contain `{output}`. |

A section needs at least one of `exec`, `label` or `icon` — a widget with none
of the three has nothing it could ever draw. `custom-` with nothing after it is
an error. `template` without `{output}` is an error, because it would silently
throw the script's output away.

`exec` output is read either as a plain first line or as Waybar-style JSON
(`{"text": …, "tooltip": …, "class": …}`), so scripts written for Waybar
usually work unchanged. `on_click` runs the configured command and then asks
the widget to refresh, since the point of clicking such a widget is usually
that the thing it reports has just changed.

## `[theme]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `mode` | string | `"dark"` | `"dark"` or `"light"`; selects the panel palette and GTK polarity. |
| `accent` | string | `"#3584e4"` | A hex colour, or `"none"` for monochrome. |
| `animations` | bool | `true` | Master switch for transitions and animations. |
| `ripple` | bool | `true` | Material-style ripple on press. |
| `blur` | bool | `false` | Ask the compositor to blur behind panel surfaces. |

`[theme.palette]` overrides the selected mode's `background`, `surface` and
`foreground` roles with hex colours; omitted roles retain their mode defaults.

`blur` is a request, not a setting. The panel hands the compositor the exact
region of each surface it would like blurred through `ext-background-effect-v1`;
a compositor that does not speak the protocol, or that has no blur configured,
ignores it and the panel looks identical minus the blur. Nothing fails.

### `[theme.icons]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `theme` | string | `"Adwaita"` | GTK icon theme name. Must not be empty. |
| `weight` | integer | `400` | Symbolic icon stroke weight hint. |

`theme` pins the panel's GTK icon theme at startup, so rendering does not
depend on the session's own settings — and the packaged binary ships Adwaita,
so the default is always satisfiable even on a machine with no icon theme
installed. `weight` is accepted and validated, and nothing reads it: it was
the Material icon font's stroke weight, and v2 has no Material backend.

### `[theme.states]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `success` | hex string | `"#4a7a4a"` | Success/connected tint. |
| `warning` | hex string | `"#e5c07b"` | Warning tint. |
| `urgent` | hex string | `"#ff6b6b"` | Urgent/error tint. |

### `[theme.typography]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `font_family` | string | `"Adwaita Sans, Cantarell, Noto Sans, sans-serif"` | CSS font stack. Must not be empty. |

## `[osd]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `true` | Show the volume/brightness capsule at all. |
| `position` | string | `"bottom"` | `"bottom"`, `"top"`, `"left"`, or `"right"`. |
| `show_value` | bool | `false` | Draw the numeric value next to the bar. |
| `timeout_ms` | integer | `1500` | Milliseconds the capsule stays up after the last event. |

The capsule does not appear for the panel's own sliders. A volume change
carries its source, and one made from Quick Settings is filtered out — the
slider under the pointer is already the feedback, and restating it in the
middle of the screen is what GNOME conspicuously does not do. Media keys and
anything else on the machine raise it.

`timeout_ms` must be above zero.

## `[audio]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `allow_overdrive` | bool | `false` | Allow volume above 100%, capped at PulseAudio's UI maximum. |

This one key is also read on its own, by a path that tolerates a file the panel
itself would refuse. `topbar volume up` acts on PulseAudio before it tries to
raise an OSD, so a configuration too broken to start the panel must not take
the volume keys down with it.

## `[updates]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `check_interval` | integer | `3600` | Seconds between checks. Minimum 60. |
| `update_count_command` | string | unset | Override for the counting command. |
| `flake` | string | unset | NixOS only: where the system flake lives. |

With no `update_count_command`, the service reads `/etc/os-release` and deduces
a **read-only** counting command. Nothing it runs syncs a package database,
takes a lock, or downloads anything:

| Distribution | Command |
|---|---|
| Guix | `guix upgrade --dry-run` |
| Debian, Ubuntu, Mint | `apt-get -s -o Debug::NoLocking=true upgrade` |
| Arch, Manjaro | `checkupdates` (needs `pacman-contrib`) |
| Fedora, Nobara, RHEL | `dnf -q check-update` |
| Silverblue, Kinoite, uBlue | `rpm-ostree upgrade --check` |

NixOS is counted differently, because no single command answers "how many
updates are pending" without writing the lock file. The service copies
`flake.nix` and `flake.lock` into a scratch directory, re-locks the copy, and
counts the inputs whose pin moved; the system's own lock file is never written.
The flake is assumed to be at `/etc/nixos`, and `flake` points elsewhere when it
lives in a dotfiles checkout.

`update_count_command` wins over all of that. It is the one documented
exception to the panel's argv-not-shell rule — the key has been a shell command
line since v1 and pipelines are the normal way to write one — and its contract
is unchanged: print either a number, or one pending update per line.

Failure hides the card. A command that could not run, an exit status the
contract does not cover, output that does not parse, an unrecognised
distribution with no override: all of them hide the card rather than reporting
zero. "Up to date" and "I could not tell" look identical on a panel, and only
one of them is safe to guess.

## `[advanced]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `compositor` | string | `"auto"` | `"auto"` or `"niri"`. Both select the niri backend. |
| `pango_font_rendering` | bool | `false` | Apply Pango font attributes instead of relying on GTK CSS. |

GTK sets font sizes from CSS with `pango_font_description_set_absolute_size`,
which bypasses Pango's DPI-aware hinting; on layer-shell surfaces that can clip
tall glyphs at some sizes. `pango_font_rendering = true` re-states each label's
font as a Pango attribute in points, which does go through the DPI-aware path.
It is off by default because it is a workaround for a problem most setups do
not have.

---

# Hot reload

`topbar reload` and saving the configuration file do exactly the same thing.
Both land in one apply path, so whatever the command does to a running panel is
what the editor does.

The file is watched, and so is its directory: an editor saving a file usually
does not write it, it writes a new one and renames it over the old, which
leaves an inode watch pointing at something nobody will ever write again.
Events are debounced for 250 ms before the file is read, because `vim` writes a
backup, renames it and adjusts the mode in between, and reading on the first
event reads a file that is empty or half-written.

What happens is chosen from a derived diff of the two configurations, so the
panel does the smallest correct thing:

| Changed | What it costs |
|---|---|
| `[theme]` colours, `[theme.states]`, `[theme.typography]`, `[widgets]` styling keys | Regenerate the stylesheet and swap the one provider. No widget is touched. |
| `theme.animations`, `theme.ripple` | Flip two switches. |
| `theme.blur` | Re-bind or release the protocol, then rebuild every bar. |
| One `[widgets.<name>]` section | Rebuild that widget on every bar. |
| `[bar]` | Rebuild every bar, and regenerate the stylesheet. |
| `left`, `center`, `right` | Rebuild every bar. |
| `[advanced]` | Rebuild every bar. |
| `[osd]` | Rebuild each bar's capsule. |
| `[audio]`, `[updates]`, and the weather, crypto, system_monitor and headset intervals | Tell the service. Nothing is rebuilt. |

A rebuilt bar closes whatever popover was open and restarts every widget's
timers, which is why the routing is worth having: a changed accent colour
touches no widget at all, and a changed `clock.format` touches one.

`[bar]` appears twice because it feeds both the window geometry and the
stylesheet, and it does both. `[advanced]` rebuilds bars because
`pango_font_rendering` is applied to a window when it is built.

Services with a `configure` seam are told rather than restarted: the weather
keeps the forecast it has while re-timing, crypto keeps its prices, and the
resource sampler keeps its CPU delta. A reload that places a widget whose
service was never started starts it first, so the widget subscribes to
something that is already running.

**An invalid file changes nothing.** A configuration that does not parse, or
that validates with errors, leaves the running panel exactly as it was. One
banner names the first error and says how many more there are, and the full
list goes to the log. There is no partial application: half a configuration is
a state nobody asked for and nobody can reason about.

**One key needs a restart:** `advanced.compositor`. The compositor connection
is made once, at start-up, before GTK exists.
