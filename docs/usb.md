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

---

## Four things that will bite you

**Unmount before you switch.** Switching yanks the device from the side that
has it, and panio cannot see mount state, so it will not stop you. On macOS
use `diskutil unmount force`; plain `diskutil unmount` often fails because
Spotlight holds the volume open.

**Exit 0 means the mux moved, not that the media is ready.** Wait a few
seconds for the block device to appear on the receiving side.

**Never assume the position persisted.** The mux resets to host when the KVM
loses power, and a physical button can move it. Check `panio usb state`.

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
