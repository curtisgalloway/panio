<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
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

## Bench note, not a seed concern

A board reused from netboot bring-up may have EEPROM `BOOT_ORDER` set
network-first (`0xf12`), costing about 40 seconds per boot. Set it SD-first
(`0xf21`) with `rpi-eeprom-config --edit`.
