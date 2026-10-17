# adb (Android targets)

The **adb channel** drives a phone, tablet, or Android board that `adb` can
reach: console (`adb shell`), screen (`adb exec-out screencap`), and input
(`adb shell input`), over one transport.

paniolo runs the host's `adb` binary itself (core CLI, `cli/src/adb.rs`; no
helper). The device is named by its `adb -s <serial>` id and bound to the
control host it is plugged into, reached locally or over SSH
([dispatch](distributed-control.md)).

> **Power:** use the [power hooks](power.md):
> `paniolo power set -t pixel --off-cmd "adb -s <id> reboot -p" --cycle-cmd "adb -s <id> reboot"`.

---

## Setup

```bash
# Discover attached devices (their serials), locally or on a control host
paniolo adb devices
paniolo adb devices -H bench1          # over SSH on a remote control host

# Or let paniolo propose a whole target block from discovered hardware —
# `adb` appears alongside the serial/video/netboot it finds (authorized
# devices only). Review and paste it into the lab; paniolo never writes it.
paniolo configure pixel -H bench1
paniolo discover                       # raw inventory of the local host

# Bind a device to a target in the lab file
paniolo target add pixel
paniolo adb set -t pixel --serial 33271JEGR02033

# Sole attached device? Omit --serial.
paniolo adb set -t pixel

# Device on a remote control host (adb runs there; reached over SSH)
paniolo adb set -t pixel --serial 33271JEGR02033 --host bench1

# Pin a non-PATH adb binary
paniolo adb set -t pixel --adb /opt/platform-tools/adb

# Remove the channel
paniolo adb rm -t pixel
```

**Prerequisite:** authorize the device first (USB debugging on, host key
accepted); paniolo does not pair. `adb devices` must show `device`, not
`unauthorized`/`offline`.

`paniolo doctor` runs `adb get-state` on the channel's host and reports:

- `ok` when the device answers in the `device` state;
- `MISSING` when it does not;
- *"adb not installed"* when there is no `adb` on `PATH`.

---

## Commands

```bash
# Console
paniolo adb shell pixel                       # interactive `adb shell` (PTY)
paniolo adb run -t pixel getprop ro.product.model   # one-shot, captured
paniolo adb run -t pixel -- logcat -d -t 50         # `--` guards leading flags

# Screen
paniolo adb screencap pixel -o shot.png       # PNG to a file — always this machine's
paniolo adb screencap pixel -o - > shot.png   # PNG to stdout

# Input
paniolo adb input -t pixel keyevent KEYCODE_HOME
paniolo adb input -t pixel text "hello world"
paniolo adb input -t pixel tap 540 1200
paniolo adb input -t pixel swipe 540 1800 540 600 200

# Discovery (no configured channel required)
paniolo adb devices [-H <host>]
paniolo adb show pixel                         # config + live device state
```

- `run` is the one-shot: it captures output and returns the command's exit code.
- `run` and `input` name the target with `-t/--target`, not a positional. Put
  `-t` first, and `--` before any argument that starts with a dash.

`screencap` uses `adb exec-out screencap -p`. `-o <path>` is always on
**this machine**, even for a remote channel, and is replaced only when the
capture succeeds. `-o -` (the default) writes to stdout.

---

## Lab file shape

```toml
[targets.pixel.adb]
serial = "33271JEGR02033"   # adb -s id; omit for the sole attached device
# adb  = "/opt/platform-tools/adb"   # override the adb binary (default: adb on PATH)
# host = "bench1"                    # control host the device is plugged into
```

---

## Relationship to the other channels

| Capability | Wired bring-up channel | adb equivalent |
|---|---|---|
| Console (interactive) | `serial connect` | `adb shell` |
| Console (one-shot/agent) | `serial send` / `serial log` | `adb run` |
| Screen | `video shot` (UVC capture) | `adb screencap` |
| Input | `hid send` (USB HID rig) | `adb input` |
| Reboot / power | `power` hooks, DTR | `power` hooks (`adb reboot`) |

adb sees only running Android, not the bootloader or firmware. To watch those
too, give the target both an `adb` channel and the wired channels.
For firmware that accepts USB fastboot downloads into RAM, use the
[guarded staging workflow](fastboot.md) with an external client and runner.
