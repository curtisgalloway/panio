# Provisioning a Linux control host

> **Status: design; generator not built.** This doc specifies the
> unattended-install recipe ("seed") for standing up a new Linux control host —
> an x86 mini-PC or a Raspberry Pi — from blank hardware to agent-reachable. It
> complements [pi4-control-host.md](pi4-control-host.md) (per-bench hardware,
> wiring, and UART/gadget configuration), and builds on the distributed-control
> principle that control hosts are disposable. The first `pi-sd` seed was
> hand-built 2026-08-20 on a Pi 5 8 GB running Pi OS Trixie Lite arm64 and
> **hardware-validated**: cloud-init done, key-only SSH up, groups
> and packages applied, the release `.deb` installed. See
> [pi-sd findings](#pi-sd-hardware-findings-2026-08-20) for what the
> first pass corrected. The generator and templates land under `packaging/`
> when built.
>
> **Superseded in part (2026-09-09).** The validated `pi-sd` seed was
> recovered from the master image and now ships, parameterized, as
> [`packaging/host-seed/pi-sd/`](../packaging/host-seed/pi-sd); the operator
> walkthrough is [docs/control-host.md](../docs/control-host.md). Still
> unbuilt: the `x86-usb` flavor and the argument-taking generator sketched
> below. Note that the shipped seed differs from the sketch in this doc —
> it pins a `.deb` version rather than resolving latest, adds
> `adm`/`gpio`/`plugdev` to the group list, installs `avahi-daemon` and
> `openssh-server`, and sets the uplink non-optional. Read the shipped files
> for what was actually validated; read this doc for why.

## The problem

[Distributed control](../docs/distributed-control.md) principle 3 says control hosts are
**stateless executors**: "re-image it, re-run `panio setup` on it, and it
resumes its role from the lab file with nothing to restore." That is doctrine
without tooling — there is no actual re-image path. Standing up a control host
today means a manual OS install plus a checklist of hand-run steps, which is
exactly the kind of tedium the rest of panio exists to delete.

The panio-specific delta on a stock Linux install is deliberately tiny,
because the hard parts already shipped:

- The **Linux `.deb`** (built by `.github/workflows/release.yml` from
  [`packaging/nfpm.yaml`](https://github.com/curtisgalloway/panio/blob/main/packaging/nfpm.yaml),
  published for **amd64 and arm64**, smoke-tested via `apt-get install` in CI)
  carries the CLI, every helper daemon, linuxocr, and the agent skills.
- `panio setup` has a packaged-install mode (no checkout, no Rust toolchain)
  that finishes per-user platform steps.
- Everything host-specific lives in the **lab file**, not on the host.

So the recipe's real job is: get a blank box to the point where that existing
machinery can take over. Everything else would be maintaining a Linux distro
for an audience of two boxes — a full custom image pipeline (mkosi/debos/Yocto)
was considered and rejected as wrong-sized.

## The contract: the seed's job ends at first SSH

**Definition of done for the seed:** after one human action (flash media, plug
in, power on), the box comes up on the network with the operator's SSH key
authorized, the right groups set, and `panio` on `PATH`. An agent can do
everything after that through the existing front door:

1. Add a `[hosts.<name>]` entry to the lab file (`ssh`, `hostname`, `identity`).
2. `panio setup --host <name>` for any remaining per-user platform steps.
3. Discovery-assisted `panio configure` to wire targets, reviewed and
   committed by a human as usual.

What deliberately stays **out** of the seed: lab configuration, target wiring,
daemon state, credentials beyond the one SSH key. Baking any of that into the
install would fork the source of truth away from the lab file.

## Design: one cloud-init core, two wrappers

Both platforms' unattended-install mechanisms converge on **cloud-init** —
Ubuntu's autoinstall embeds it, and Raspberry Pi OS ships it natively since
Trixie — so one shared `user-data` core serves both with a thin delivery
wrapper each:

| Platform | Base OS | Mechanism | Delivery |
|---|---|---|---|
| x86 box (NUC-class or any UEFI PC) | Ubuntu Server 24.04 LTS amd64 | subiquity **autoinstall** (embeds the cloud-init core) | Generator emits a bootable USB installer; installs to internal disk unattended |
| Raspberry Pi 4 / 5 | Raspberry Pi OS **Lite arm64** (Trixie, ≥ 2025-11-24) | cloud-init **NoCloud** — native in Pi OS Trixie | Flash the stock image; generator overwrites `user-data` / `network-config` on the FAT `bootfs` partition |

The Pi path is the lighter of the two — no installer even runs; the image is
the disk and cloud-init applies the seed on first boot. Pi OS Trixie ships the
three NoCloud files (`user-data`, `meta-data`, `network-config`) on `bootfs`
out of the box, hand-editable without Raspberry Pi Imager — **verified on the
2026-06-18 Lite arm64 image**. (Ubuntu Server's arm64 preinstalled image works
the same way via its `system-boot` partition, and remains the fallback if
distro unification with the x86 flavor ever matters more than Pi-native
firmware/kernel integration.)

### The shared cloud-init core

Sketch (the generator templates this from its inputs):

```yaml
hostname: <host>
users:
  - name: <user>
    groups: [sudo, dialout, video]     # dialout/video: what `panio setup` would ask for
    shell: /bin/bash
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    ssh_authorized_keys:
      - <operator pubkey>
ssh_pwauth: false
package_update: true
packages:
  - tesseract-ocr                      # the .deb only Recommends it; be explicit
runcmd:
  # Fetch the panio .deb for this architecture from the GitHub release
  # (latest by default; the generator can pin --version).
  - arch=$(dpkg --print-architecture)
  - curl -fsSL -o /tmp/panio.deb "<release asset URL for ${arch}>"
  - apt-get install -y /tmp/panio.deb
```

Notes:

- **Groups at user creation.** The `.deb` intentionally does not manage group
  membership (per-user concern, see the nfpm manifest comment); cloud-init's
  `users:` block does it declaratively, so first login already has
  serial/video access and `setup` finds nothing to fix.
- **No passwords anywhere.** Key-only SSH; passwordless sudo matches the
  bench-appliance role (single-operator lab hardware, not a multi-user host).
- **`tesseract-ocr` explicitly**, because apt installs Recommends by default
  but the seed shouldn't depend on that default surviving.

### Platform extras

- **x86 wrapper:** the autoinstall YAML adds `identity`/`storage` (whole-disk,
  direct layout) around the shared core, plus a late-command for the `.deb` if
  installing inside the target chroot is preferred over first-boot `runcmd`.
- **Pi wrapper:** hardware-interface enablement — the PL011 GPIO console
  (`enable_uart=1` + `dtoverlay=disable-bt`, getty disabled), the future HID
  gadget (`dtoverlay=dwc2,dr_mode=peripheral`) — is only wanted when the bench
  uses those ports, so these are generator flags, not defaults. Pi OS Trixie's
  cloud-init ships a Pi-specific `cc_raspberry_pi` module that can activate
  serial/I2C/SPI/1-Wire and USB gadget mode declaratively from `user-data`;
  prefer it over hand-appended `config.txt` fragments where it covers the
  need (verify its serial semantics against the getty/console requirements
  before relying on it). Wiring, voltage-level, and topology guidance stays in
  [pi4-control-host.md](pi4-control-host.md).
- **Networking:** netboot's refusal to serve on the primary NIC means a
  control host needs **two** interfaces (uplink + DUT link). The seed can
  carry a netplan stanza for the DUT-link NIC when its name is known at
  generation time (USB-GbE adapters are predictable via `by-id`/MAC match),
  but this is optional — the link can equally be configured later through the
  normal lab-file flow. Wi-Fi-as-uplink on the Pi is supported by
  `network-config`, with the caveat that the PSK sits in plaintext on the
  boot partition.

## pi-sd hardware findings (2026-08-20)

The first real seed run surfaced four facts the templates must encode:

- **sshd is disabled by default on Pi OS, and the announced `enable_ssh: true`
  did NOT enable it** on the 2026-06-18 Trixie Lite arm64 image (the key is a
  Pi OS downstream extension, absent from upstream `cc_raspberry_pi`; on this
  image it was a silent no-op). What works: the classic **empty `ssh` flag
  file** on `bootfs` — `sshswitch.service` survives in Trixie, enabled sshd,
  and consumed the file — plus a `bootcmd` fallback. Ship both; keep
  `openssh-server` in `packages` as insurance.
- **Never `systemctl enable --now ssh` in `bootcmd`.** `bootcmd` runs in
  cloud-init's pre-network local stage; `--now` waits for ssh.service, which
  waits for the network, which waits for cloud-init-local → the boot
  deadlocks (observed: hang at "Local Stage (pre-network)", recovered only by
  power-cut). `systemctl enable ssh` (symlink only) is safe and sufficient.
- **Re-seeding an already-booted card**: edit `user-data` on `bootfs` *and*
  bump `instance_id` in `meta-data` — cloud-init caches per-instance state on
  the rootfs, so an unchanged id means user/packages/runcmd never re-run.
- **`extended_status: degraded done` is normal** on this image: cloud-init
  warns it can't find `cc_netplan_nm_patch` (Pi OS packaging wart). All
  stages complete, `errors: []` — don't let a health check treat it as
  failure; check `errors`/`recoverable_errors`, not the word "degraded".

Bench note, not a seed concern: a board reused from netboot bring-up may have
EEPROM `BOOT_ORDER` set network-first (the test unit's was `0xf12` from its Fuchsia
DUT days, costing ~40 s per boot); set it SD-first (e.g. `0xf21`) when the
board becomes a control host.

## Host sizing

The control-host workload is lighter than it looks. Per subsystem, for one or
two DUTs:

| Load | Cost |
|---|---|
| `serialcap` | ≤ 11.5 KB/s per 115200-baud console — negligible |
| `netbootd` | bursty: kernel+initramfs at line rate for seconds per boot, then idle |
| `hdmicap` | MJPEG dongles are teed compressed to `/preview` **without re-encode** (Linux MJPEG tee, `hdmicap/src/capture.rs`) — a few % of one core |
| `video shot` | one turbojpeg decode on demand |
| `video read` | the only heavy op: decode + Tesseract, on the order of 1–3 s on a Pi 4, ~1 s on a Pi 5 — per-call latency, not sustained load |

Recommendations:

- **Raspberry Pi 4 (4 GB) is the sensible floor; Pi 5 if OCR latency matters.**
  2 GB works. Aggregate CPU of a Pi 4 is in the same class as a low-end x86
  mini-PC (Celeron N3060-class), which already qualifies.
- **Pi 3 / Zero 2 are excluded** as full hosts: 100 Mbps USB-attached
  Ethernet (netboot would crawl), 1 GB RAM, and the `.deb` is arm64-only.
  A serial-only role would work but isn't worth a special seed path.
- **Any 64-bit UEFI x86 box qualifies** — the floor is roughly "anything sold
  as a mini-PC in the last decade."
- **Storage endurance, not CPU, is the Pi's real risk** for an always-on box
  writing capture logs: use a quality SD card, or a USB SSD (Pi 5: NVMe HAT)
  for a bench that runs 24/7.
- OCR **accuracy** is a platform property, not a sizing one: Linux hosts use
  Tesseract (weaker than Apple Vision on small console fonts) regardless of
  how fast the box is.

## Generator shape

Start as a standalone script — `packaging/host-seed/` — with no runtime
dependency on the CLI; promote to a `panio host-seed` subcommand only if it
earns it. Inputs:

| Input | Meaning | Default |
|---|---|---|
| `--hostname` | host name for the box | required |
| `--user` / `--ssh-key` | operator account + authorized pubkey | required |
| `--flavor` | `x86-usb` (autoinstall USB) or `pi-sd` (NoCloud files for a flashed card) | required |
| `--panio-version` | release to fetch | latest |
| `--dut-nic` | netplan stanza for the DUT-link interface | none (configure later) |
| `--pi-uart` / `--pi-gadget` | Pi `config.txt` fragments | off |

Output is either a bootable USB image (x86) or a small set of files to drop on
`system-boot` after flashing the stock Pi image (`user-data`,
`network-config`, optional `config.txt` fragment) — the latter is trivially
scriptable around Raspberry Pi Imager or `dd`.

## Alternatives considered

- **Full custom image pipeline** (mkosi / debos / NixOS / Yocto): rejected —
  the panio delta is ~40 lines of declarative config, and an image pipeline
  buys bit-reproducibility (which stateless hosts don't need — first boot
  starts with `apt upgrade` anyway) at the cost of owning kernel/security
  updates and multi-GB build artifacts.
- **Raspberry Pi OS via Imager `custom.toml` / `firstrun.sh`**: the *legacy*
  Pi OS provisioning path — user/Wi-Fi/SSH only, arbitrary packages and
  first-boot commands require hand-patching `firstrun.sh` — was rejected, and
  an earlier draft of this doc rejected Pi OS wholesale on those grounds.
  That rationale is obsolete: since Pi OS Trixie (2025-11-24), cloud-init is
  the native first-boot mechanism, which is why Pi OS is now the *chosen*
  base for the `pi-sd` flavor above (and it was the operator-proven path
  before this doc existed). The lesson stands recorded: the rejection was
  written from stale knowledge of a moving target.
- **Ubuntu Server preinstalled arm64 on the Pi**: the earlier draft's pick,
  for distro unification with the x86 flavor. Demoted to fallback — Pi OS's
  native firmware/kernel integration and `cc_raspberry_pi` outweigh
  unification, and the shared cloud-init core is distro-agnostic anyway. The
  `config.txt` mechanics from [pi4-control-host.md](pi4-control-host.md)
  apply to both (`/boot/firmware/` layout is the same).
- **PXE-boot the installer via `netbootd`**: not the plan (bootstrap
  chicken-and-egg — it needs an existing control host), but the UEFI PXE path
  is hardware-verified, so a "panio installs its own next control host"
  flow falls out nearly free later. Noted for the demo pile.

## Open questions

- **Version pinning vs. latest** for the `.deb` fetch — latest is convenient;
  pinning + recorded sha256 is more reproducible. Lean: default latest, offer
  `--panio-version`.
- Whether the x86 flavor should install the `.deb` in the installer's
  late-commands (present in the installed system before first boot) or via
  first-boot `runcmd` like the Pi (one shared code path). Lean: shared
  `runcmd` path unless it proves flaky.
- Whether the seed should register the host in DNS / DHCP reservations, or
  leave addressing entirely to the network (current lean: not the seed's job;
  the lab file's `ssh` field is the contract).
