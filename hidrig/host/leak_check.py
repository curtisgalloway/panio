# Copyright 2026 The Panio Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Detect whether injector HID reports leak into the live macOS session.

Sends ONE `moveabs` to screen center over the injector's UART, then checks
whether the real cursor moved. If a capture/exclusion mechanism (seize, USB
claim, ...) is working, the cursor stays put; if reports leak, the cursor
warps to center — we restore it immediately, so the visible effect is a
single ~150ms blip.

Run:
  uv run --with pyserial --with pyobjc-framework-Quartz leak_check.py \
      --device /dev/cu.usbserial-XXXX
"""

import argparse
import sys
import time

import serial
import Quartz

BOOT_BAUD = 115200
FAST_BAUD = 460800


def cursor_pos():
    ev = Quartz.CGEventCreate(None)
    loc = Quartz.CGEventGetLocation(ev)
    return (loc.x, loc.y)


def command(port, cmd):
    port.write(cmd.encode("utf-8") + b"\n")
    reply = port.readline().decode("utf-8", "replace").strip()
    if not reply.startswith("OK"):
        raise RuntimeError("board: %r -> %r" % (cmd, reply))


def open_synced(device):
    port = serial.Serial(device, BOOT_BAUD, timeout=0.5)
    for probe in (BOOT_BAUD, FAST_BAUD):
        port.baudrate = probe
        port.reset_input_buffer()
        try:
            command(port, "ping")
            return port
        except (RuntimeError, serial.SerialException):
            pass
        except Exception:
            pass
    raise SystemExit("board not answering at %d or %d" % (BOOT_BAUD, FAST_BAUD))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--device", required=True)
    args = ap.parse_args()

    port = open_synced(args.device)
    p0 = cursor_pos()
    command(port, "moveabs 16384 16384")
    time.sleep(0.15)
    p1 = cursor_pos()
    moved = abs(p1[0] - p0[0]) + abs(p1[1] - p0[1])
    if moved > 5:
        # Put the user's cursor back where it was.
        Quartz.CGWarpMouseCursorPosition(Quartz.CGPointMake(p0[0], p0[1]))
        print("LEAK: cursor moved %.0fpx (%.0f,%.0f)->(%.0f,%.0f); restored" % (moved, p0[0], p0[1], p1[0], p1[1]))
        return 1
    print("NO LEAK: cursor stayed at (%.0f,%.0f)" % p0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
