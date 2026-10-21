# topbar

A GNOME Shell-style top bar for [niri](https://github.com/YaLTeR/niri), built
with GTK4 and `gtk4-layer-shell`. GNOME Shell is the design inspiration only —
topbar is not affiliated with or endorsed by the GNOME Project.

![topbar](assets/screenshots/topbar.png)

One solid, full-width panel pinned to the top of every monitor: workspaces and
indicators on the left, clock and weather in the center, tray, alerts and the
quick-settings aggregate menu on the right. Clicking the clock opens a GNOME
date menu with notifications, calendar, world clocks, media controls and the
weather forecast.

![the control panel](assets/screenshots/topbar-panel.png)

## What is on the bar

- **workspaces** — GNOME Activities-style dots with an animated active pill,
  filtered to the monitor they are on. Urgent workspaces pulse.
- **clock** — any `strftime` format, aligned to the boundary, with the date
  menu behind it: notification history with grouping and Do Not Disturb, a calendar,
  world clocks, MPRIS media controls and a five-day forecast. A dot beside the time
  marks unseen notification history.
- **weather** — Open-Meteo current conditions and forecast, with a location
  search dialog. One cache for the whole panel.
- **crypto** — bitcoin, ethereum and monero prices from CoinGecko, singly or as
  pair ratios, with a popover that edits the list without touching your config.
- **tray** — StatusNotifierItem host with dbusmenu support and an overflow
  chevron.
- **system_monitor** — invisible while the machine is healthy; fades in with
  warning-tinted icons once CPU, memory or disk crosses a threshold.
- **headset** — battery for a 2.4GHz wireless headset, via `headsetcontrol`.
- **keyboard_layout**, **os_logo**, and **custom-\*** widgets that run a script
  and speak Waybar's JSON contract.
- **quick_settings** — the GNOME 45 aggregate menu: Wi-Fi (with a secret agent
  for passwords), Bluetooth (with a pairing agent), VPN, volume and microphone,
  brightness, Power Mode, Caffeine, battery health with charge thresholds,
  pending updates, a resource overview, privacy dots, and a power section that
  asks you to hold the row down.

Plus the surfaces that are not widgets: the notification daemon and its
banners, the volume and brightness OSD capsule, and compositor blur behind
everything the panel draws.

**Configuration is applied live.** Save `config.toml` and the panel changes:
colours are a stylesheet swap, a widget's own section rebuilds that widget, and
only the bar's geometry rebuilds the bars. A file that does not parse changes
nothing and says so in one banner.

## Install

The flake exposes a package and an overlay for `x86_64-linux`.

```nix
# flake.nix
{
  inputs.topbar.url = "github:trevarj/topbar";

  outputs = { nixpkgs, topbar, ... }: {
    nixosConfigurations.yourhost = nixpkgs.lib.nixosSystem {
      modules = [
        ({ pkgs, ... }: {
          nixpkgs.overlays = [ topbar.overlays.default ];
          environment.systemPackages = [ pkgs.topbar ];
        })
      ];
    };
  };
}
```

Or run it without installing:

```sh
nix run github:trevarj/topbar
```

## Running under niri

Add to `~/.config/niri/config.kdl`:

```kdl
spawn-at-startup "topbar"

// Media keys route through the panel so they show an OSD.
binds {
    XF86AudioRaiseVolume  allow-when-locked=true { spawn "topbar" "volume" "inc" "5"; }
    XF86AudioLowerVolume  allow-when-locked=true { spawn "topbar" "volume" "dec" "5"; }
    XF86AudioMute         allow-when-locked=true { spawn "topbar" "volume" "toggle-mute"; }
    XF86MonBrightnessUp                          { spawn "topbar" "brightness" "inc" "5"; }
    XF86MonBrightnessDown                        { spawn "topbar" "brightness" "dec" "5"; }
}

// Blur behind the panel and its popovers, if you want it. `theme.blur = true`
// in topbar's own configuration asks for the region; this is what the
// compositor does with it.
layer-rule {
    match namespace="^topbar$"
    match namespace="^topbar-popover$"

    background-effect {
        xray false
    }
}
```

The panel reserves an exclusive zone, so niri lays windows out beneath it
automatically.

See [Application launching](DEVELOPMENT.md#application-launching) for desktop
entry launching behavior and limitations.

The volume and brightness commands act on PulseAudio and logind **directly**
and only then try to raise an OSD, so a media key still works when the panel is
not running and when the configuration is broken.

See [Picker keyboard interaction](docs/configuration.md#picker-keyboard-interaction)
for filtering and selection, and the
[wallpaper provider protocol](DEVELOPMENT.md#wallpaper-provider-protocol) for
integrating a wallpaper source.

## Configuration

Configuration lives at `~/.config/topbar/config.toml`. Every key is optional;
omitted keys use built-in defaults. Changes apply live, and an invalid file
leaves the last valid configuration running.

The clock's control center is opt-in:

```toml
[widgets.clock]
control_panel = true
```

Inspect the shipped defaults and validate your configuration without replacing
an existing file:

```sh
topbar --print-example-config
topbar --check-config
```

See the [example configuration](config.toml) and
[configuration reference](docs/configuration.md) for all options, lookup order,
live-reload behavior and integrations.

## Switching from gnome-topbar (v1)

See [Coming from v1](docs/configuration.md#coming-from-v1) before switching:
stop the old panel, update executable/package and layer-shell names, and check
removed features and update-count overrides.

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for development commands, the visual
playground and integration protocols, and
[the architecture](docs/architecture.md) for the codebase map.

## License

MIT. See [LICENSE](LICENSE).
