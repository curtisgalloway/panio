# Switchable USB media

`panio usb` gives a target hands-free *physical* boot media: write an image
to a card on the control host, then hand the card to the target, which sees an
ordinary USB mass-storage device that firmware can boot.

It drives a USB mux (switch) inside some KVMs through a per-target
**usb channel**. The mux routes one device to the control host or the
target, **never both at once**.

For downloading bytes into a target's RAM over USB, see
[Guarded RAM-only USB fastboot staging](fastboot.md). That workflow uses an
external fastboot client; the media-switching commands below do not stage RAM.

---

## Supported hardware

| Device | Switched thing | Status |
|---|---|---|
| **Openterface KVM-Go** | onboard microSD reader | ✅ works, hardware-verified |
| **Openterface Mini-KVM** | switchable USB-A port | ⚠️ mechanism known, no helper support yet |
| **USB sharing switch + any relay** ([`usbtoggle`](#usb-sharing-switch-usbtoggle)) | one peripheral (keyboard, drive, hub) shared by two computers | ✅ works, hardware-verified on a KL-SW301-board switch with a DSD TECH relay on its remote jack |

**KVM-Go:** the mux uses the same serial port as the `ch9329` helper, so a
device already set up as a `hid` channel needs only a second channel.

**Mini-KVM:** the mux needs a register write over the capture chip's HID
interface ([mux spec](https://github.com/curtisgalloway/panio/blob/main/notes/openterface-usb-mux-spec.md));
no helper implements it, so `panio usb` cannot drive a Mini-KVM.

## Wiring it up

```bash
panio usb set -t <target> --cmd "ch9329 -d /dev/cu.usbmodemXXXXX"
```

On a KVM-Go, use the same device path as the `hid` channel:

```bash
panio hid set -t pi5 --cmd "ch9329 -d /dev/cu.usbmodem51201"
panio usb set -t pi5 --cmd "ch9329 -d /dev/cu.usbmodem51201"
```

`--doctor` on `usb set` has `panio doctor` also run `<cmd> doctor --json`,
the helper's own read-only health check (see
[Helper health checks](power.md#helper-health-checks-doctor)). `ch9329` does
not implement it yet, so doctor reports `no-check` for it.

## Using it

```bash
panio usb state          -t pi5    # -> host | target
panio usb attach-host    -t pi5    # route the card to the control host
panio usb attach-target  -t pi5    # hand it to the target
```

A typical hand-off:

```bash
panio usb attach-host -t pi5
# ... wait for the block device, write your image to it, unmount ...
panio usb attach-target -t pi5
panio power-cycle -t pi5
```

## USB sharing switch (usbtoggle)

A cheap 2-computer USB sharing switch has one peripheral port, two computer
ports, and a momentary button; many also have a jack for a remote button.
Pressing the button **toggles** which computer the peripheral is connected to
(VBUS, USB 2 and SuperSpeed all switch). The `usbtoggle` helper turns one into
a `usb` channel: computer port A goes to the control host, port B to the
target, and a relay of your choice presses the button.

**Why it needs a probe.** The switch has no readable position; it only
toggles. So `usbtoggle` reads the position from the peripheral itself: it
looks for the peripheral on the control host's USB bus (`--device`). Present
means `host`. Absent means `target`, but that is an *inference*: the
peripheral could be unplugged or a hub dead, so `usbtoggle` says so on
stderr every time unless you add `--target-check-cmd` (exit 0 when the target
sees the device) or use `--state-cmd` (a command that prints `host` or
`target`). With a check configured, "absent and the check fails" is an error
("seen on neither side"), never a guess. After a press it polls for up to
`--settle-ms` (5000 by default) for the expected position and fails on
timeout; it **never presses again on its own**.

**Wiring.** Put the relay's contacts across the button, at the remote jack or
the button's own pads.

- A dry-contact relay (a DSD TECH board, most relay modules) is polarity-free:
  wire its two contacts to the jack's two conductors.
- A low-side output such as the Phidgets REL1100 needs the jack's pulled-up
  contact on the output and the jack's ground on the module ground.
- The relay can be any type; `usbtoggle` only runs the shell commands you give
  it. Test the wiring alone with `usbtoggle --close-cmd ... --open-cmd ...
  press` before configuring the channel.

**Configure the channel.** `--cmd` is run by a shell, so quote the nested
commands with single quotes inside the double quotes (the example uses
placeholders; take the real VID:PID from `lsusb` or System Information):

```bash
panio usb set -t pi5 --doctor --cmd "usbtoggle --device 046d:c08b \
  --close-cmd 'dsdrelay -s AA00BB11CC22DD33 off 1' \
  --open-cmd 'dsdrelay -s AA00BB11CC22DD33 on 1'"
```

`--device` also takes an optional serial (`046d:c08b:AA00BB11CC22DD33`) to tell
two identical peripherals apart. Options are written before the verb, as with
every panio helper.

**dsdrelay users.** `dsdrelay`'s verbs assume the target is wired through the
relay's NC contact. A button is wired to the NO contact, so the sense flips:
`off` energizes the coil (the contact closes: a press) and `on` releases it.
That is why the example above closes with `off 1` and opens with `on 1`.

**What happens when.** `usb host` / `usb target` read the position first; if
the peripheral is already on the requested side they print it and exit 0
without pressing. Otherwise they press once (close, hold `--hold-ms`, default
200, then open) and verify. The open command always runs after the close was
attempted, even when the close fails. The relay commands' stdout goes to
`usbtoggle`'s stderr; stdout carries only `host` or `target`. `usbtoggle
doctor` is read-only and never presses.

---

## Four things that will bite you

**Unmount before you switch.** Switching yanks the device from the side that
has it, and panio cannot see mount state, so it will not stop you. On macOS
use `diskutil unmount force`; plain `diskutil unmount` often fails because
Spotlight holds the volume open.

**Exit 0 means the mux moved, not that the media is ready.** Wait a few
seconds for the block device to appear on the receiving side.

**Never assume the position persisted.** The mux resets to host when the KVM
loses power, and a physical button can move it. Check `panio usb state`. (A
sharing switch has no power-up default; a press while it is unpowered or a
hand on its button also moves it.)

**Not every device has a mux.** A device without one does not answer, which
the helper reports as a timeout.

---

## The helper contract

As with the [power hooks](power.md) and [hid channel](hid.md), panio runs
the configured command with a verb appended:

| panio command | what it runs |
|---|---|
| `panio usb attach-host` | `<cmd> usb host` |
| `panio usb attach-target` | `<cmd> usb target` |
| `panio usb state` | `<cmd> usb state` |

A helper implementing this contract must:

- exit non-zero on failure, and print the resulting side (`host` or `target`) on
  stdout;
- **verify** the switch: compare the position the device reports with the one
  requested (a KVM-Go that ignored the request still reports the old position).

Unlike `panio hid send`, no arguments are passed through; the vocabulary is
fixed at these three verbs.
