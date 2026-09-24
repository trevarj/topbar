#!/usr/bin/env python3
"""Private-session Hyprland behavior and rendered reload checks; launcher owns isolation."""
import json
import os
from pathlib import Path
import re
import subprocess
import time

ART = Path(os.environ["SMOKE_ARTIFACTS"])
BAR = os.environ["SMOKE_TOPBAR"]
CONFIG = Path(os.environ["XDG_CONFIG_HOME"]) / "topbar/config.toml"
PANEL_LOG = ART / "panel.log"
children = []


def run(*args, check=True):
    return subprocess.run(args, text=True, capture_output=True, check=check, timeout=15)


def hypr(query):
    return json.loads(run("hyprctl", "-j", query).stdout)


def dispatch(expression):
    reply = run("hyprctl", "dispatch", expression).stdout.strip()
    assert reply == "ok", reply


def wait_for(test, message, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = test()
            if result:
                return result
        except (subprocess.CalledProcessError, json.JSONDecodeError, KeyError, IndexError):
            pass
        time.sleep(0.1)
    raise AssertionError(message)


def state():
    return json.loads(run(BAR, "dump", "state", "--json").stdout)


def layers(namespace, connector=None):
    return [layer for name, output in hypr("layers").items()
            if connector is None or name == connector
            for level in output["levels"].values() for layer in level
            if layer["namespace"] == namespace and layer.get("alpha", 1) > 0]


def shot(name, connector, namespace="topbar"):
    wait_for(lambda: layers(namespace, connector), f"{namespace} is not mapped on {connector}")
    destination = ART / f"{name}.png"
    next_frame = ART / f".{name}.png"
    previous = None
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        run("grim", "-o", connector, str(next_frame))
        current = next_frame.read_bytes()
        drawn = namespace == "topbar" or int(run(
            "magick", str(next_frame), "-crop", "100%x100%+0+45",
            "+repage", "-format", "%k", "info:",
        ).stdout) > 1
        if current == previous and drawn:
            next_frame.replace(destination)
            print(f"captured {destination.name}", flush=True)
            return destination
        previous = current
        time.sleep(0.5)
    raise AssertionError(f"{name} did not present two identical frames")


def widget_rect(css):
    offset = PANEL_LOG.stat().st_size
    run(BAR, "popover", "show", "surface-dump")
    def locate():
        with PANEL_LOG.open() as log:
            log.seek(offset)
            block = log.read()
        if "ui-dump: end" not in block:
            return None
        for line in block.splitlines():
            match = re.search(r'ui-dump: \S+ \[([^]]+)\] ".*" (-?\d+) (-?\d+) (\d+) (\d+) sensitive=', line)
            if match and css in match[1].split("."):
                return tuple(map(int, match.groups()[1:]))
        return None
    return wait_for(locate, f"no geometry for {css}")


def click(x, y):
    run("wlrctl", "pointer", "move", "-100000", "-100000")
    run("wlrctl", "pointer", "move", str(int(x)), str(int(y)))
    run("wlrctl", "pointer", "click", "left")


def click_widget(css):
    x, y, width, height = widget_rect(css)
    click(x + width / 2, y + height / 2)


def configuration(mode="dark", blur=True, compositor="auto"):
    return f'''[bar]
background_opacity = 0.86
[widgets]
left = ["workspaces", "keyboard_layout"]
center = ["clock"]
right = ["quick_settings"]
popover_background_opacity = 0.76
[widgets.workspaces]
show_unoccupied = true
label_type = "none"
[widgets.clock]
format = "smoke"
control_panel = true
[theme]
mode = "{mode}"
blur = {str(blur).lower()}
animations = false
[osd]
timeout_ms = 10000
[advanced]
compositor = "{compositor}"
'''


def install(text):
    temporary = CONFIG.with_suffix(".candidate")
    temporary.write_text(text)
    temporary.replace(CONFIG)
    return run(BAR, "reload", check=False)


def begin():
    # The compositor passes its own display/signature to this process, never a scan.
    assert os.environ["XDG_CURRENT_DESKTOP"] == "Hyprland"
    assert "NIRI_SOCKET" not in os.environ
    assert os.environ["WAYLAND_DISPLAY"] != "host-wayland"
    endpoint = Path(os.environ["XDG_RUNTIME_DIR"]) / "hypr" / os.environ["HYPRLAND_INSTANCE_SIGNATURE"]
    assert (endpoint / ".socket.sock").is_socket()
    assert os.environ["DBUS_SYSTEM_BUS_ADDRESS"] == os.environ["DBUS_SESSION_BUS_ADDRESS"]
    assert hypr("configerrors") == [], "Lua smoke config failed before panel startup"
    CONFIG.parent.mkdir(parents=True, exist_ok=True)
    CONFIG.write_text(configuration())
    run(BAR, "--config", str(CONFIG), "--check-config", "--strict")
    panel_log = PANEL_LOG.open("w")
    panel = subprocess.Popen([BAR, "--config", str(CONFIG), "-v"], stdout=panel_log, stderr=subprocess.STDOUT)
    children.append(panel)
    wait_for(lambda: state()["compositor"]["backend"] == "hyprland" and state()["compositor"]["connected"], "panel did not connect to Hyprland")
    (ART / "compositor.json").write_text(json.dumps(state(), indent=2))
    monitor = hypr("monitors")[0]
    connector = monitor["name"]
    wait_for(lambda: len(layers("topbar", connector)) == 1, "one bar per connector")
    wait_for(lambda: hypr("monitors")[0]["reserved"][1] > 0, "bar did not reserve an exclusive zone")

    # Real pointer events, then compositor state, not just an IPC call echo.
    dispatch('hl.dsp.focus({workspace="1"})')
    wait_for(lambda: hypr("activeworkspace")["id"] == 1, "workspace one")
    x, y, width, height = widget_rect("workspaces")
    click(x + width - 12, y + height / 2)
    wait_for(lambda: hypr("activeworkspace")["id"] == 3, "workspace strip click did not focus workspace three")
    named = "name:smoke'notes"
    reply = run("hyprctl", "eval", f"hl.workspace_rule({{workspace={json.dumps(named)},persistent=true}})").stdout.strip()
    assert reply == "ok", reply
    dispatch(f"hl.dsp.focus({{workspace={json.dumps(named)}}})")
    named_id = wait_for(lambda: hypr("activeworkspace")["id"] if hypr("activeworkspace")["id"] < 0 else None, "named workspace did not receive a native negative ID")
    dispatch('hl.dsp.focus({workspace="1"})')
    wait_for(lambda: hypr("activeworkspace")["id"] == 1, "return from named workspace")
    x, y, width, height = widget_rect("workspaces")
    click(x + width - 12, y + height / 2)
    wait_for(lambda: hypr("activeworkspace")["id"] == named_id, "negative-ID workspace click was treated as a relative selector")
    dispatch('hl.dsp.focus({workspace="3"})')
    old_layout = next(k["active_keymap"] for k in hypr("devices")["keyboards"] if k["main"])
    click_widget("keyboard-layout")
    wait_for(lambda: next(k["active_keymap"] for k in hypr("devices")["keyboards"] if k["main"]) != old_layout, "layout click did not switch the main keyboard")
    shot("dark-bar", connector)

    # An ordinary client supplies real pixels and an application focus target.
    demo_log = (ART / "client.log").open("w")
    demo = subprocess.Popen(["gtk4-demo"], stdout=demo_log, stderr=subprocess.STDOUT)
    children.append(demo)
    client = wait_for(lambda: next((c for c in hypr("clients") if c["pid"] == demo.pid), None), "synthetic GTK client did not map")
    assert client["at"][1] >= monitor["y"] + 30, "tiled client overlaps the exclusive zone"
    dispatch('hl.dsp.window.fullscreen({action="set"})')
    wait_for(lambda: hypr("activewindow")["fullscreen"] != 0, "fullscreen did not apply")
    shot("fullscreen", connector)
    dispatch('hl.dsp.window.fullscreen({action="unset"})')

    # Real rendered palette reload keeps the process, history and open ordinary menu.
    run(BAR, "popover", "show", "clock")
    shot("dark-clock", connector, "topbar-popover")
    before_surface = layers("topbar-popover", connector)[0]["address"]
    assert install(configuration("light")).returncode == 0
    assert panel.poll() is None
    wait_for(lambda: json.loads(run(BAR, "dump", "config", "--json").stdout)["theme"]["mode"] == "light", "light config not applied")
    assert layers("topbar-popover", connector)[0]["address"] == before_surface, "palette destroyed the open clock"
    shot("light-clock", connector, "topbar-popover")
    assert (ART / "dark-clock.png").read_bytes() != (ART / "light-clock.png").read_bytes(), "mode changed config but not rendered pixels"
    run(BAR, "popover", "hide")
    run(BAR, "popover", "show", "quick_settings")
    shot("light-quick-settings", connector, "topbar-popover")
    before_surface = layers("topbar-popover", connector)[0]["address"]
    assert install(configuration()).returncode == 0
    assert layers("topbar-popover", connector)[0]["address"] == before_surface, "palette destroyed Quick Settings"
    shot("dark-quick-settings", connector, "topbar-popover")

    # Neither the watcher nor the explicit reload may install even part of a rejected file.
    good = json.loads(run(BAR, "dump", "config", "--json").stdout)
    assert install(configuration("light", compositor="niri")).returncode != 0
    time.sleep(0.4)  # allow the file watcher's 250 ms transaction to run too
    assert json.loads(run(BAR, "dump", "config", "--json").stdout) == good
    assert install(configuration() + '\n[theme.palette]\nforeground = "none"\n').returncode != 0
    time.sleep(0.4)
    assert json.loads(run(BAR, "dump", "config", "--json").stdout) == good
    rejected = shot("rejected-palette", connector, "topbar-popover")
    palette_pixel = lambda path: run("magick", str(path), "-format", "%[pixel:p{100,2}]", "info:").stdout
    assert palette_pixel(rejected) == palette_pixel(ART / "dark-quick-settings.png"), "a rejected reload changed rendered polarity"
    assert install(configuration()).returncode == 0
    run(BAR, "popover", "hide")

    # Theme bundles replace an ancestor symlink, not the watched config file.
    bundles = Path(os.environ["XDG_STATE_HOME"]) / "bundles"
    for mode in ["dark", "light"]:
        (bundles / mode).mkdir(parents=True)
        (bundles / mode / "topbar.toml").write_text(configuration(mode))
    current = bundles / "current"
    current.symlink_to("dark")
    CONFIG.unlink()
    CONFIG.symlink_to(current / "topbar.toml")
    next_bundle = bundles / "next"
    next_bundle.symlink_to("light")
    next_bundle.replace(current)
    run(BAR, "reload")
    assert json.loads(run(BAR, "dump", "config", "--json").stdout)["theme"]["mode"] == "light"
    assert panel.poll() is None
    shot("ancestor-symlink-light", connector)
    CONFIG.unlink()
    CONFIG.write_text(configuration())
    run(BAR, "reload")

    # The same translucent surface, with only blur changed, over the same real client.
    run(BAR, "popover", "show", "clock")
    shot("blur-on", connector, "topbar-popover")
    assert install(configuration(blur=False)).returncode == 0
    run(BAR, "popover", "show", "clock")
    shot("blur-off", connector, "topbar-popover")
    assert (ART / "blur-on.png").read_bytes() != (ART / "blur-off.png").read_bytes(), "blur toggle produced no rendered difference"
    assert install(configuration()).returncode == 0

    # Hotplug, portrait/fractional scale and focused-output routing remain connector-based.
    run("hyprctl", "output", "create", "headless", "SMOKE-2")
    wait_for(lambda: any(m["name"] == "SMOKE-2" for m in hypr("monitors")), "headless output was not created")
    reply = run("hyprctl", "eval", 'hl.monitor({output="SMOKE-2",mode="900x1400@60",position="auto",scale=1.5})').stdout.strip()
    assert reply == "ok", reply
    wait_for(lambda: len(layers("topbar", "SMOKE-2")) == 1, "second connector bar missing")
    dispatch('hl.dsp.focus({monitor="SMOKE-2"})')
    wait_for(lambda: state()["compositor"]["focused_output"] == "SMOKE-2", "focused connector did not update")
    run(BAR, "popover", "show", "clock")
    shot("portrait-clock", "SMOKE-2", "topbar-popover")
    run(BAR, "popover", "hide")
    run(BAR, "popover", "show", "osd-brightness")
    wait_for(lambda: layers("topbar-osd", "SMOKE-2"), "OSD did not route to focused output")
    assert not layers("topbar-osd", connector)
    shot("portrait-osd", "SMOKE-2", "topbar-osd")
    run("notify-send", "-a", client["class"], "-t", "10000", "Hyprland smoke notification", "Clicking this returns to the synthetic client")
    wait_for(lambda: layers("topbar-toast", "SMOKE-2"), "toast did not route to focused output")
    assert not layers("topbar-toast", connector)
    shot("portrait-toast", "SMOKE-2", "topbar-toast")
    # Surface dump coordinates are monitor-local; translate into compositor space.
    x, y, width, height = widget_rect("toast")
    extra = next(m for m in hypr("monitors") if m["name"] == "SMOKE-2")
    click(extra["x"] + x + width / 2, extra["y"] + y + height / 2)
    wait_for(lambda: hypr("activewindow").get("address") == client["address"], "notification did not focus its application")
    run("hyprctl", "output", "remove", "SMOKE-2")
    wait_for(lambda: state()["compositor"]["outputs"] == 1 and state()["compositor"]["connected"], "hot-unplug did not reconcile")
    assert panel.poll() is None
    shot("after-hotplug", connector)
    print("PASS: native selection/connectivity, clicks, focus, routing, exclusive zone, fullscreen, hotplug, blur and transactional light/dark reload", flush=True)


try:
    begin()
finally:
    for child in reversed(children):
        if child.poll() is None:
            child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
