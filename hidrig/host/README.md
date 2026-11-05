<!--
Copyright 2026 The Panio Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# HID injector host tools (macOS)

Tools for testing the KB2040 injector (an RP2040 board acting as a USB keyboard
and mouse) with its USB plugged into the **same Mac** that drives the control
link.

Build the C/Objective-C tools:

```bash
make            # builds hid_capture_usb and hid_seize_reports
```

The Python tools run under `uv`; no build step.

| Tool | Use it for | Works with |
|---|---|---|
| `hid_capture_usb` | Leak-safe capture of the injector's reports (use this one) | Either firmware |
| `hid_seize_reports` | Passive timestamped tap; **not** exclusive | Either firmware |
| `hid_bench.py` | Latency and throughput | Retired single-board firmware only |
| `leak_check.py` | Assert that injection does not move the real cursor | Retired single-board firmware only |

The two C tools observe the DUT-facing HID board, which has the same VID/PID on
both firmwares.

## hid_capture_usb — leak-safe HID capture

**Always start this tool before injecting**, or the reports leak into your live
session.

It detaches the injector from macOS with IOUSBHost whole-device capture
(`IOUSBHostObjectInitOptionsDeviceCapture`; root passes the same gate as the
`com.apple.vm.device-access` entitlement) and prints each interrupt-IN report
with timestamps. Injected input reaches **only** this tool.

```bash
sudo ./hid_capture_usb            # defaults to the injector serial
sudo ./hid_capture_usb <serial>   # there may be >1 KB2040 (same VID/PID) attached
HID_CAPTURE_PROBE=1 sudo -E ./hid_capture_usb   # hold the capture and sleep, for hidutil/leak inspection
```

Each line is `report ts=<sec.usec> dt=<usec-since-prev> len=<n>: <hex>`. The
first payload byte is the report ID (1 = keyboard, 2 = absolute mouse). While
it runs, the injector is absent from `hidutil list`.

## hid_seize_reports — passive raw-report tap (NOT exclusive)

Uses `IOHIDDeviceOpen(..., kIOHIDOptionsTypeSeizeDevice)`, which on Darwin
24/25 is **not** exclusive: injected mouse moves still move the real cursor.
Use it only as a passive tap. Requires `sudo` plus an Input Monitoring grant in
System Settings.

## hid_bench.py — latency / throughput (retired single-board path)

> **Note:** `hid_bench.py` and `leak_check.py` speak only the retired
> single-board firmware's line protocol over a USB-serial adapter (`OK`/`ERR`
> replies, 115200→460800 baud negotiation). They cannot drive the dual-board
> rig's USB-CDC binary-frame link. The retired firmware is paniolo-hardware
> `hidrig-kb2040/firmware/single-board/{boot,code,config}.py`
> (https://github.com/curtisgalloway/paniolo-hardware).

It times command round trips over the UART (setting `IOSSDATALAT` itself).
Start `hid_capture_usb` first.

```bash
uv run --with pyserial hid_bench.py --device /dev/cu.usbserial-XXXX \
    --baud 460800 --latency-us 1 --mode latency --count 100
```

| Mode | Measures |
|---|---|
| `latency` | per-sample cmd→OK round trip + percentiles |
| `rr` | back-to-back request/reply rate |
| `pipe` | windowed pipelining |

`--latency-us` sets the macOS serial read-latency timer (1 ≈ floor; 0 leaves
the default). Typical: `ping` ~3 ms, `moveabs` ~8 ms (USB `bInterval` floor),
~123 moveabs/s.

## leak_check.py — assert no leak (retired single-board path)

Injects one centered `moveabs` and checks whether the real cursor moved. Expect `NO LEAK` while `hid_capture_usb` holds the device.

```bash
uv run --with pyserial --with pyobjc-framework-Quartz leak_check.py \
    --device /dev/cu.usbserial-XXXX
```
