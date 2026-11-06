<!--
SPDX-FileCopyrightText: 2026 The Panio Authors
SPDX-License-Identifier: Apache-2.0
-->

# Control-host seed files

cloud-init files that turn a blank Raspberry Pi into an **agent-reachable
panio control host** with one human action: flash a card, plug it in, power
on. Raspberry Pi OS reads `user-data`, `meta-data` and `network-config` from the
boot partition on first boot.

The walkthrough is [docs/control-host.md](../../docs/control-host.md); design
rationale is in
[notes/control-host-provisioning.md](../../notes/control-host-provisioning.md).

| Flavor | Directory | Status |
|---|---|---|
| `pi-sd` — Raspberry Pi 4/5, Raspberry Pi OS Trixie Lite arm64 | [`pi-sd/`](pi-sd) | **Hardware-validated** (Pi 5 8 GB, 2026-08-20) |
| `pi-usb` — Raspberry Pi 5, Raspberry Pi OS Trixie Lite arm64, no network needed, reached over the USB-C port | [`pi-usb/`](pi-usb) | **Hardware-validated** with a macOS host (Pi 5, 2026-10-06 image, 2026-11-06); Linux host untested |
| `x86-usb` — UEFI mini-PC, Ubuntu Server autoinstall | — | Designed, not built |

There is no generator: copy these files and fill in five placeholders.

## What the seed does, and where it stops

The seed:

- creates the operator account with the right groups
- authorizes one SSH key and disables password login
- installs the panio `.deb` and the packages it wants
- brings up the wired uplink on DHCP

It carries no lab configuration, target wiring, daemon state, or credential
beyond that one public key; those live in the lab file. Its job ends at first
SSH.

## Using them

**Follow `panio skill control-host`** for the exact macOS and Linux commands:
download the image, identify the right removable device before writing to it,
flash, mount, install these files, render them, and enroll the result.

In short: copy all three files to the FAT `bootfs` partition under exactly
these names, `touch` an empty `ssh` file beside them, render the placeholders,
and boot.

Only `user-data` has placeholders. Replace all five (table below), then confirm
none survive. This check strips the file's comments first:

```bash
grep -v '^[[:space:]]*#' /Volumes/bootfs/user-data | grep '<[^>]*>' \
  && echo "FAIL: unrendered placeholders above" \
  || echo "OK: fully rendered"
```

Do not simplify it to `grep '<[a-z-]*>' user-data`: that matches the template's
comments, so it never reports success, and it misses a placeholder containing a
space.

| Placeholder | Value |
|---|---|
| `<hostname>` | the host's name, matching what the lab file's `ssh` field will resolve |
| `<user>` | operator account name |
| `<full-name>` | GECOS field, cosmetic |
| `<ssh-public-key>` | one full public key line, e.g. `ssh-ed25519 AAAA… you@dev` |
| `<version>` | panio release to install, without the `v` — e.g. `0.2.0`, appearing twice on the same line |

## Gotchas these files encode

Do not "simplify" these back.

- **sshd is off by default on Pi OS, and `enable_ssh: true` does not turn it
  on** (it is a silent no-op; upstream `cc_raspberry_pi` lacks it). The empty
  **`ssh` flag file** on `bootfs` works: `sshswitch.service` enables sshd and
  consumes the file. The seed also has a `bootcmd` fallback and
  `openssh-server` in `packages`, because losing SSH on a headless box means a
  trip to the bench.
- **Never `systemctl enable --now ssh` in `bootcmd`.** It deadlocks boot
  (`--now` waits for `ssh.service`, which waits on the network, which waits on `cloud-init-local`, which is
  running that command); only a power-cut recovers. Plain
  `systemctl enable ssh` is safe.
- **Re-seeding an already-booted card takes two edits**: change `user-data`
  *and* bump `instance_id` in `meta-data`. Otherwise cloud-init's cached
  per-instance state means the user, package and runcmd stages never re-run.
- **`optional: false` on the uplink** makes boot wait for the link, so package
  and `.deb` downloads don't run early and fail quietly.

## The `pi-usb` flavor

`pi-usb` provisions with no network and turns the Pi's USB-C port into a USB
Ethernet device, so one cable through a power-and-data splitter is the whole
connection. It differs from `pi-sd` in these ways:

- **Nothing is downloaded.** The panio `.deb` and the tesseract `.deb` set sit
  in `panio-offline/` on `bootfs` (about 34 MB of its 512 MiB) and first boot
  installs them from there.
- **Four placeholders**, not five: the version comes from the `.deb` you copy.
- **One extra flash step**: append the `dwc2` overlay to `config.txt`.
- **Addresses are link-local** on both IPv4 and IPv6, and the host name comes
  from avahi as `<hostname>.local`. There is no DHCP server on the link.

`panio skill control-host` has the procedure. Do not "simplify" these back:

- **`dpkg -i`, not `apt-get install`.** The image ships stale package lists, and
  apt fails trying to fetch even with `--no-download`.
- **Collect the tesseract set from the image you flash.** It is the exact set
  missing from that image, so a set from another image may not satisfy
  `dpkg`.
- **No `rpi: enable_usb_gadget: true`.** That runs `rpi-usb-gadget`, which sets
  up a DHCP and NAT server on `10.12.194.1` with IPv6 off.
- **The udev rule is required.** `85-nm-unmanaged.rules` makes NetworkManager
  ignore gadget interfaces, so the link-local profile would never apply.
- **`eth0` is `optional: true`**, or boot waits for a cable that is not there.
- **The MAC addresses and USB serial are derived from the board serial.**
  `g_ether` otherwise picks new MAC addresses on each load, and the image's
  serial string is blank. They take effect from the second boot, because the
  module is already loaded when first boot writes them.

Two limits to know about:

- **A splitter needs a USB-A port on the host.** On a Mac's USB-C port the Pi
  saw VBUS and connected but no host ever reset the bus, while the same
  splitter worked at once from a USB-A port. The likely cause is that a USB-C
  host needs the CC pins to detect a device and the splitter does not pass
  them. A plain USB-C cable works, but then the host powers the Pi.
- **The console has no login.** The account has no password, so a broken link
  leaves no way in. Set one over SSH with `sudo passwd <user>` if you want a
  console fallback.

## Bench note, not a seed concern

A board reused from netboot bring-up may have EEPROM `BOOT_ORDER` set
network-first (`0xf12`), costing about 40 seconds per boot. Set it SD-first
(`0xf21`) with `rpi-eeprom-config --edit`.
