---
name: control-host
description: >
  Build a new panio control host from blank media — flash Raspberry Pi OS to
  an SD card, install the cloud-init seed that makes the box come up with SSH
  authorized and panio installed, and enroll it in the lab file. Use when
  asked to provision, image, flash, re-image, seed, or stand up a control host
  or bench host, when a control host must be replaced or rebuilt, or when
  someone has a blank Raspberry Pi that needs to become a panio host. Covers
  identifying the right removable device before writing to it, the placeholder
  render, and the Pi OS first-boot gotchas. This is host *creation*; driving
  targets from a host that already exists is the `panio` skill.
---

<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Building a panio control host

A **control host** is the Linux box physically cabled to your targets: serial
adapters, HDMI capture, HID rig, netboot link, power switching. This skill
takes a blank SD card to a host that answers SSH with panio installed.

You run every command here on your **dev machine** (the one with the card
reader), not on the host being built. It does not exist yet.

The whole configuration is three cloud-init files plus an empty flag file,
dropped on the card's boot partition. No installer runs; the stock image is
the disk. Rationale and hardware sizing: `docs/control-host.md`.

## Before you start

- The seed files, from a panio checkout at `packaging/host-seed/pi-sd/`, or
  from `/usr/share/panio/` on a packaged install. `panio skill
  control-host --path` shows where this file came from if you need to locate
  the install.
- An SD card, 8 GB or larger. A card reader.
- The operator's SSH **public** key, and the account name and hostname they
  want. Ask if you have not been told; do not invent a hostname.
- `xz` and `curl`. Both are standard on macOS and Linux.

## Step 1 — get the image

Raspberry Pi OS **Lite arm64**, Trixie or newer. This redirect always points
at the current Lite arm64 release:

```bash
curl -fL -o /tmp/raspios-lite-arm64.img.xz \
  https://downloads.raspberrypi.com/raspios_lite_arm64_latest
```

Do not use Raspberry Pi Imager's customization screen even if you have it.
cloud-init supersedes it, and the two conflict.

The published checksum is the same URL with `.sha256` appended, if you want to
verify the download before writing it.

## Step 2 — identify the card, and stop

**This is the step that can destroy the user's data.** `dd` to the wrong
device overwrites a disk with no confirmation and no undo. External hard
drives, backup drives, and Time Machine volumes all appear in the same
listing as the SD card.

macOS:

```bash
diskutil list external physical
```

Linux:

```bash
lsblk -do NAME,SIZE,TRAN,RM,MODEL
```

Read the output and find the device whose **size matches the SD card** and
which is removable (`RM` is `1` on Linux; `external, physical` on macOS).
A real listing on a dev machine often includes something like a 4 TB external
drive sitting right next to the card. Picking by position in the list, or
assuming the newest device, is how that drive gets erased.

**Show the listing to the user and confirm the device node before writing to
it.** If exactly one device is unambiguously the card by size and removability
you may say so and proceed on their confirmation, but do not skip the
confirmation. If two devices could plausibly be the card, stop and ask.

Everything below writes `diskN` (macOS) or `sdX` (Linux). Substitute the
confirmed device. Never paste these commands with a guessed device.

## Step 3 — write the image

macOS. Note `rdiskN` (the raw device) in the `dd` line, which is many times
faster than `diskN`:

```bash
diskutil unmountDisk /dev/diskN
xz -dc /tmp/raspios-lite-arm64.img.xz | sudo dd of=/dev/rdiskN bs=1m
```

macOS `dd` prints nothing while it runs. Press Ctrl-T for a progress line. It
takes a few minutes.

Linux:

```bash
sudo umount /dev/sdX?* 2>/dev/null || true
xz -dc /tmp/raspios-lite-arm64.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync status=progress
sudo sync
```

## Step 4 — mount the boot partition

macOS remounts it automatically at `/Volumes/bootfs` once `dd` finishes. If it
does not appear, `diskutil mountDisk /dev/diskN`.

Linux mounts it by hand. It is the first partition, and it is FAT:

```bash
sudo mkdir -p /mnt/bootfs
sudo mount /dev/sdX1 /mnt/bootfs
```

Below, `BOOTFS` means `/Volumes/bootfs` or `/mnt/bootfs` as appropriate.

## Step 5 — install the seed

Copy all three files, keeping the names exactly. **cloud-init silently ignores
a misnamed file** — a typo here produces a box that boots and does nothing.

```bash
cp packaging/host-seed/pi-sd/user-data \
   packaging/host-seed/pi-sd/meta-data \
   packaging/host-seed/pi-sd/network-config "$BOOTFS/"
touch "$BOOTFS/ssh"
```

The empty `ssh` file is what actually enables sshd. It is not optional and it
is not redundant with anything in `user-data`. See the gotchas.

## Step 6 — render the placeholders

`user-data` ships with five placeholders. Fill them in on the card:

| Placeholder | Value |
|---|---|
| `<hostname>` | the host's name |
| `<user>` | operator account name |
| `<full-name>` | GECOS field, cosmetic |
| `<ssh-public-key>` | one full public key line |
| `<version>` | panio release, no `v` prefix; appears twice on one line |

For the current release:

```bash
gh release view --repo curtisgalloway/panio --json tagName --jq .tagName
```

Then edit the file, or render it with `sed`:

```bash
sed -i '' \
  -e 's/<hostname>/bench1/' \
  -e 's/<user>/operator/' \
  -e 's/<full-name>/Bench Operator/' \
  -e 's|<ssh-public-key>|ssh-ed25519 AAAA… operator@dev|' \
  -e 's/<version>/0.2.0/g' "$BOOTFS/user-data"
```

macOS `sed -i` needs that empty `''` argument; on Linux use `sed -i` with no
argument. Note the `|` delimiter on the key line, because a public key
contains `/`, and the trailing `g` on the version, because it appears twice.

**Verify nothing was missed.** This check ignores the file's own comments, so
a clean run means the payload is fully rendered:

```bash
grep -v '^[[:space:]]*#' "$BOOTFS/user-data" | grep '<[^>]*>' \
  && echo "FAIL: unrendered placeholders above" \
  || echo "OK: fully rendered"
```

A plainer `grep '<...>' user-data` will report the explanatory comments
forever and can miss a placeholder containing a space. Use the form above.

## Step 7 — eject, boot, verify

```bash
diskutil eject /dev/diskN          # macOS
sudo umount /mnt/bootfs && sync    # Linux
```

Put the card in the Pi and power it on. **First boot takes several minutes**:
it waits for the network, runs `apt update`, and downloads the panio `.deb`.
It reboots itself along the way. Do not conclude it has failed early.

Then, from the dev machine:

```bash
ssh <user>@<hostname> panio --version
```

If the name does not resolve, try `<hostname>.local` — the seed installs
avahi. Otherwise find the lease on your router.

## Step 8 — enroll it in the lab

```bash
panio host add <name> --ssh <user>@<hostname>
panio configure <target> -H <name>
```

`configure` runs discovery on the new host over SSH and prints a proposed
`[targets.<target>]` block. **It writes nothing.** A human reviews it, pastes
it into the lab file, and commits. Do not edit the lab file silently on their
behalf.

If people or agents will run panio in a shell *on* this host (not only
dispatched from a dev machine), give it a read-only copy of the lab from the
repository that holds it. Run on the host, with an SSH key that can read that
repository:

```bash
panio config sync --git <remote> --path <path/to/lab.toml>
```

If the host should not hold repository credentials, sync from the raw file's
HTTPS URL with a read-only token instead (`--token-file <path>` or
`--token-command <cmd>`; the token itself is never recorded):

```bash
panio config sync --url <https-url-of-lab.toml> --token-file <path>
```

Re-run `panio config sync` after the source changes; commands warn once the
copy is a week old.

You should not need `panio setup` on a seeded host. On Linux its packaged
mode only ensures the `dialout` and `video` groups and warns if Tesseract is
missing, and the seed already does both.

## When it does not come up

The host has no panio on it yet, so there is no remote channel to debug
through. Get a person to the bench with HDMI or the Pi's serial UART, then:

```bash
cloud-init status --long
journalctl -u cloud-init -u cloud-init-local -b
```

- **`extended_status: degraded done` is normal.** cloud-init cannot find
  `cc_netplan_nm_patch` on this image, a Pi OS packaging wart, while every
  stage still completes. Judge the run by `errors` and `recoverable_errors`,
  never by the word "degraded".
- **SSH refused but the boot finished** means the empty `ssh` file is missing.
- **Hung at "Local Stage (pre-network)"** means something in `bootcmd` is
  waiting on the network. Only a power-cut recovers it.
- **Edits to a card that already booted did nothing** means `instance_id` was
  not bumped. See below.

## Gotchas

Each of these cost a debug cycle on the first real build. They are encoded in
the seed files; do not "simplify" them back out.

- **sshd is off by default on Pi OS, and `enable_ssh: true` does not turn it
  on.** That key is a Pi OS downstream extension, absent from upstream
  `cc_raspberry_pi`, and on the validated image it was a silent no-op. The
  empty `ssh` flag file works, because `sshswitch.service` survives in Trixie,
  enables sshd, and consumes the file. The seed ships that, a `bootcmd`
  fallback, and `openssh-server` in packages, deliberately.
- **Never `systemctl enable --now ssh` in `bootcmd`.** `bootcmd` runs
  pre-network. `--now` waits for `ssh.service`, which waits for the network,
  which waits for the stage that is blocked in that very command. The boot
  deadlocks. Plain `systemctl enable ssh` writes a symlink and returns.
- **Re-seeding a card that has already booted takes two edits**: change
  `user-data` *and* bump `instance_id` in `meta-data`. cloud-init caches
  per-instance state on the root filesystem, so an unchanged id means the
  user, package, and runcmd stages never re-run.
- **The uplink is `optional: false` on purpose.** First boot fetches packages
  over the network; without the wait those stages can run early and fail
  quietly.
- **A board reused from netboot bring-up may boot network-first.** EEPROM
  `BOOT_ORDER` of `0xf12` costs about 40 seconds per boot waiting for a PXE
  server that is not there. Set it SD-first with `rpi-eeprom-config --edit`.

## What the seed deliberately does not do

It carries no lab configuration, no target wiring, no daemon state, and no
credential beyond the one public key. Those belong in the lab file, which a
human reviews. Do not extend the seed to bake them in; it would fork the
source of truth away from the lab file.

x86 mini-PCs have no seed yet. Install Ubuntu Server by hand and apply the
same account, group, and package steps. The design is in
`notes/control-host-provisioning.md`.
