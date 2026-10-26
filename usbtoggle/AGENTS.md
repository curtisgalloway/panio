<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# usbtoggle/ — agent notes

The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
usbtoggle/       Rust crate: a `usb` channel helper (panio runs `<cmd> usb host|target|state`)
                 for a toggle-only USB sharing switch pressed by any relay. Not a power
                 helper. Deps: clap, anyhow, nusb (the same version as phidget).
  src/main.rs    CLI (clap derive, options before the verb): usb host|target|state, press,
                 doctor [--json]. Builds a logic::Config and the real Runner/Probe/Clock.
  src/logic.rs   All behavior, behind three traits: Runner (shell commands), Probe
                 (is the device on the host?), Clock (now/sleep). press, observe,
                 state, switch_to, emit (stdout side, stderr warning). Contains the
                 fakes (`logic::fakes`) the tests drive.
  src/sys.rs     The real Runner (`sh -c` / `cmd /C`), SystemClock, UsbProbe (nusb
                 enumeration, never opens a device), and the VID:PID[:SERIAL] parser.
  src/doctor.rs  The read-only doctor report and its JSON.
  tests/e2e.rs   Unix-only: the built binary with shell commands that flip a state file.
```

## Contract and decisions

- **The switch has no readable position.** It only toggles. The position is
  read from the peripheral: present on the control host = `host`; absent =
  `target`. That second reading is an inference, so `Observed::Target {
  inferred: true }` makes `emit` print a loud warning on stderr. A
  `--target-check-cmd` exiting 0 makes it a positive reading; absent with the
  check failing is `Observed::Neither`, an error, and `switch_to` refuses to
  press blind from it. `--state-cmd` replaces the probe and is mutually
  exclusive with `--device` (clap `conflicts_with`).
- **At most one press per `usb host|target`.** Already at the side: print and
  exit 0, no press. Otherwise press once and poll (50 ms) for up to
  `--settle-ms`; a timeout is an error naming the last observation. No retry:
  a second press would toggle back, and the maintainer wants refusals or loud
  warnings, never quiet surprises.
- **The open command always runs after the close was attempted.** `press`
  holds an `OpenGuard` whose `Drop` runs the open command, so a failed close,
  an early return, or a panic in the hold cannot leave the button held.
  `finish()` runs it on the normal path and reports its failure. The tests
  `open_runs_even_when_close_fails` and `open_runs_when_the_hold_panics` fail
  if the `Drop` body is disabled (checked).
- **stdout is only the side.** The close/open commands' stdout is redirected
  to our stderr (`Stdio::from(io::stderr())`); `--state-cmd` and
  `--target-check-cmd` stdout is captured. Failures print nothing on stdout.
- **doctor never runs close/open.** It checks the probe (or state-cmd),
  whether the device is present, and the target check. A run with neither
  `--device` nor `--state-cmd` reports a `fail`.
- **The relay is not known to this crate.** No relay type is assumed. For
  `dsdrelay` (NC-wired verbs), a button on the NO contact closes with `off` and
  opens with `on`.
- Errors exit 1 (clap usage errors exit 2); there is no daemon and no state.

## Testing

`cargo test` runs the fakes-driven unit tests, a test that really spawns the
platform shell (`exit 0` / `exit 3`, valid in sh and cmd), and, on Unix, the
end-to-end tests. Nothing needs hardware.

## Hardware notes

Verified on hardware (2026-10-25), directly and through `panio usb`, with
[this USB 3.0 sharing switch](https://www.amazon.com/dp/B0HCHDC8X4) and a
DSD TECH SH-UR04A relay on its remote jack, probing a USB mouse.

- **The board** (marked `KL-SW301-V1.2`; look-alikes are sold under many
  names): one peripheral port and no hub; a "7227" USB 2 DPDT switch (U4); a
  20-pin SuperSpeed 2:1 mux marked `A34` (U6, part not identified);
  P-channel MOSFETs marked `A19T` (Q2/Q3) on VBUS; an unmarked SOIC-8 MCU (U3)
  reading the button and the jack. The jack is in parallel with the button.
- **Measured:** VBUS on the peripheral port drops to 0 V when switched away;
  a USB 3 device comes back at 5 Gb/s; a USB 2 device disconnects and comes
  back, about 0.65 s after the relay releases. The LEDs follow every press.
- **Not measured:** the shortest press it accepts, press vs. release, any
  long-press function, and its side after power loss.
- **Relays:** a dry contact works either way round. A Phidgets REL1100
  wired across the jack did not press it: its outputs are low-side switches
  to a shared ground, not contacts.
- **A hub carrying both the switch and the relay board went dead** (both its
  halves empty, the relay unreachable) after a `dsdrelay cycle` hung, until
  the hub was power-cycled. Cause unknown; keep the relay on another hub.
