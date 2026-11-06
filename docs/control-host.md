<!--
SPDX-FileCopyrightText: 2026 The Panio Authors
SPDX-License-Identifier: Apache-2.0
-->

# Standing up a control host

A **control host** is the small Linux box cabled to your targets (serial,
capture, HID, netboot link, power). Your dev machine drives it over SSH
([distributed control](distributed-control.md)).

Flash a card, plug it in, power on: the box comes up with your key
authorized, the right groups set, and `panio` on `PATH`. It holds no target
config, so it is **disposable**: re-image it, re-run `panio setup`, and it
resumes from the lab file.

## Picking the hardware

Load for one or two targets:

| Subsystem | Cost on the host |
|---|---|
| `serialcap` | 11.5 KB/s per 115200-baud console, at most |
| `netbootd` | a few seconds at line rate per boot, then idle |
| `hdmicap` | a few percent of one core (MJPEG, no re-encode) |
| `video shot` | one JPEG decode, on demand |
| `video read` | the heavy one: OCR takes 1 to 3 s per call on a Pi 4, about 1 s on a Pi 5 |

- **Raspberry Pi 4 with 4 GB is the floor** (2 GB works); a Pi 5 if OCR
  latency matters.
- **Any 64-bit UEFI x86 mini-PC** from the last decade.
- **Not Pi 3 or Zero 2:** slow Ethernet, 1 GB RAM, and the `.deb` is arm64 only.
- **Storage wears out first.** Use a quality SD card, a USB SSD, or on a Pi 5
  an NVMe HAT.
- **Plan for two network interfaces.** `netbootd` refuses to serve on the
  primary NIC, so the target link needs its own adapter.

On Linux, OCR uses Tesseract, which is weaker than macOS's Apple Vision on
small console fonts regardless of hardware.

## Raspberry Pi, from blank card to first SSH

Raspberry Pi OS Trixie (Debian 13) configures itself on first boot with
cloud-init, so the stock image plus four files on its boot partition is
enough. **The exact commands are in the bundled `control-host` skill:**

```bash
panio skill control-host
```

It covers macOS and Linux
([on GitHub](https://github.com/curtisgalloway/panio/blob/main/skills/control-host/SKILL.md)).
The outline:

1. **Download** Raspberry Pi OS Lite arm64, Trixie or newer.
2. **Identify the card.** `dd` to the wrong device silently overwrites a
   disk; confirm the device node by size and removability first.
3. **Write the image**, then mount the FAT boot partition.
4. **Install the seed** from
   [`packaging/host-seed/pi-sd/`](https://github.com/curtisgalloway/panio/tree/main/packaging/host-seed/pi-sd):
   three cloud-init files plus an empty `ssh` file (which enables sshd).
   Names must be exact; cloud-init silently ignores a misnamed file.
5. **Fill the five placeholders** in `user-data` (hostname, account, GECOS
   full name, public key, panio version) and check none remain.
6. **Eject and boot.** First boot takes several minutes and reboots itself.
   Then `ssh <user>@<hostname> panio --version` should answer.
7. **Enroll it**: `panio host add`, then `panio configure <target> -H
   <name>` to get a proposed target block you review and commit.

A seeded host does not need `panio setup`: the seed already adds the
`dialout` and `video` groups and installs `tesseract-ocr`. It carries no lab
config and no credential beyond your public key.

## Raspberry Pi over USB, with no network

The `pi-usb` seed is for a Pi 5 that should have no network of its own. It
installs everything from the card, so first boot needs no connection, and its
USB-C port then appears on the attached host as a USB Ethernet adapter. Reach
it with `ssh <user>@<hostname>.local`.

Validated on a Pi 5 with a macOS host; a Linux host is untested.

- **Cabling:** the USB-C port carries both power and data, so use a splitter
  that passes data through from the host and takes power from its own supply.
  Plug its data side into a USB-A port on the host: on a Mac's USB-C port the
  same splitter never enumerated, likely because it does not pass the CC pins.
- **Console:** the account has no password, so the HDMI console cannot log in
  until you set one over SSH.
- **Addressing:** link-local on IPv4 (`169.254.x.x`) and IPv6, with no DHCP
  server. macOS configures its side by itself. A Linux host may need link-local
  turned on for that adapter, and `avahi` with `nss-mdns` for `.local` names.
- **Extra preparation:** collecting the tesseract packages needs an arm64 Linux
  machine or VM, once per image.
- **Clock:** with no network time and no clock battery, the Pi's clock is wrong
  after each power-up, so do not rely on timestamps in its logs.

The commands are in the `control-host` skill, under the `pi-usb` variant.

## When a boot does not come up

Get on the console (HDMI, or the Pi's serial UART) and ask cloud-init:

```bash
cloud-init status --long
journalctl -u cloud-init -u cloud-init-local -b
```

**`extended_status: degraded done` is normal** (a missing
`cc_netplan_nm_patch` warning). Judge the run by `errors` and
`recoverable_errors`.

- **SSH refuses the connection** after boot: the empty `ssh` file is missing.
  The `enable_ssh: true` cloud-config key does not enable sshd.
- **Boot hangs at "Local Stage (pre-network)"** until a power cut: something
  in `bootcmd` waits on the network. That is why the seed's
  `systemctl enable ssh` has no `--now`.

If you edit the seed on a card that has **already booted once**, also change
`instance_id` in `meta-data`, or cloud-init will not re-run `user-data`.

## x86 mini-PCs

Not built yet: install Ubuntu Server by hand and apply the same account,
group, and package steps as the Pi seed. The planned autoinstall USB is in
[the design note](https://github.com/curtisgalloway/panio/blob/main/notes/control-host-provisioning.md).
