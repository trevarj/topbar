#!/usr/bin/env python3
"""Private-bus org.freedesktop.Application fixture for launcher smoke runs.

Gio.Application exports the standard application interface itself.  The smoke
driver starts it only inside the nested dbus-run-session, then the marker is
written after the real Activate call arrives.  It never opens a system-bus
connection and exits shortly after handling one activation.
"""

import os
import sys

from gi.repository import Gio, GLib


APP_ID = "io.github.topbar.SmokeDbus"
MARKER_ENV = "SMOKE_LAUNCHER_DBUS_LOG"


def write_marker(message: str) -> None:
    marker = os.environ.get(MARKER_ENV)
    if not marker:
        raise RuntimeError(f"{MARKER_ENV} is required")
    with open(marker, "a", encoding="utf-8") as output:
        output.write(f"{message}\n")


class SmokeApplication(Gio.Application):
    def __init__(self) -> None:
        super().__init__(
            application_id=APP_ID,
            flags=Gio.ApplicationFlags.IS_SERVICE,
        )

    def do_startup(self) -> None:
        Gio.Application.do_startup(self)
        # A settled nested screenshot can take longer than Gio's idle service
        # timeout. Keep the fixture available until the launcher activates it.
        self.hold()

    def do_activate(self) -> None:
        write_marker("dbus activation received")

        def stop() -> bool:
            self.quit()
            return False

        GLib.timeout_add(250, stop)


def main() -> int:
    # Gio.Application uses only DBUS_SESSION_BUS_ADDRESS.  Refuse to start
    # without the private dbus-run-session address rather than letting GIO
    # choose any other transport.
    if not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
        print("smoke D-Bus application has no session bus", file=sys.stderr)
        return 2
    try:
        return SmokeApplication().run(sys.argv)
    except Exception as error:
        print(f"smoke D-Bus application failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
