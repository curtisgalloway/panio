# Paniolo — System architecture

**Terms:** a daemon is a background device owner; a channel is a configured
control or observation path; a capture epoch identifies one serial supervisor
lifetime. See the [glossary](../glossary.md).

> Paniolo's design as it stands. For command-level detail see the [subsystem guides](../README.md);
> module notes are in [`AGENTS.md`](https://github.com/curtisgalloway/paniolo/blob/main/AGENTS.md); the planned
> hardware-CI design is under [`ci-integration/`](../README.md#hardware-ci-integration-in-design).
> Keep this in sync as the system changes.

---

## 1. What paniolo is

Paniolo is an **agent-controlled target-machine wrangler** for low-level development
(bootloaders, firmware, OS bring-up). It gives an agent, human, or script the physical controls
of a target board: **netboot it, watch its output, send it input, power-cycle it**, with nobody
at the bench.

It is a **device-control layer**, not a test orchestrator. It owns power, serial, deploy
(netboot), video, HID (keyboard/mouse input), switchable USB media, and adb. It does *not* choose tests or produce
verdicts; in hardware CI those sit *above* it (see
[`ci-integration/`](../README.md#hardware-ci-integration-in-design)).

## 2. Deployment model

```
  ┌─────────────────────┐         ┌──────────────────────── control host ────────────────────────┐
  │  dev machine / agent │  SSH    │  paniolo CLI  +  per-subsystem daemons                        │
  │  (you, or an agent)  │ ──────► │                                                               │
  └─────────────────────┘         │   USB-Ethernet ─────────────────┐                             │
                                   │   USB serial (FTDI) ────────────┤                             │
                                   │   USB HDMI capture ─────────────┤                             │
                                   │   USB HID rig (KB2040) ─────────┤                             │
                                   └─────────────────────────────────┼─────────────────────────────┘
                                                                      ▼
                                                          ┌──────────────────────┐
                                                          │   target board (DUT) │
                                                          └──────────────────────┘
```

- The **control host** runs paniolo and is wired to one or more **targets** (DUTs, devices
  under test).
- The simplest driver is an **agent or script that SSHes into the control host** and runs
  `paniolo …` (see the root [`README.md`](https://github.com/curtisgalloway/paniolo/blob/main/README.md)).
- **Or use a [lab file](../distributed-control.md)** (`--lab` / `PANIOLO_LAB`): the dev machine
  drives a remote target transparently (commands re-exec over SSH, `console` tunnels the
  dashboard back). The dev machine is the data-plane hub; control hosts hold only runtime state
  (§5 "Distributed control").
- Runs on **macOS 10.14+** and **Linux** (x86-64/arm64); platform differences are in §8.

## 3. Process architecture

**One daemon per subsystem, no central server** (*Option A* in `AGENTS.md`). The `paniolo`
binary runs and exits. Per-subsystem **daemons** own a piece of hardware and persist between CLI
invocations. State lives in plain files.

| Component | Language | Role |
|---|---|---|
| `paniolo` CLI | Rust (clap) | The single entry point; spawns/queries daemons, edits the lab file, dispatches remote commands over SSH. |
| `serialcap` | Rust (tokio/axum) | Daemon that **exclusively owns** a target's serial ports; fans output out to a WebSocket + a timestamped capture log; accepts keystrokes back. |
| `hdmicap` | Rust (tokio/axum; ObjC AVFoundation layer on macOS, v4l on Linux) | "Warm-stream" daemon that keeps the USB HDMI capture device open and serves frames + the combined dashboard over HTTP. |
| `netbootd` | Rust (tokio) | Single-binary DHCP+TFTP+HTTP netboot engine. Privilege-separated `/dev/bpf` send path on macOS via a setuid `netbootd-bpf-helper`. |
| `cambrionix` | Rust | Standalone power helper: Cambrionix USB-hub port control, wired in via the generic power hooks. |
| `zigplug` | Python (uv tool, zigpy-znp) | Standalone power helper: Zigbee smart-plug control through a CC2652 coordinator; one-shots proxy through an auto-spawned daemon that owns the ZNP session. |
| `shellyplug` | Rust (ureq) | Standalone power helper: Shelly Gen2+ smart plugs/relays over the device's local HTTP RPC — one-shot, stateless. |
| `amt` | Rust (ureq) | Standalone power helper: Intel AMT/vPro power over WS-Management (verified HTTPS on 16993 with HTTP Digest; legacy HTTP on 16992 requires explicit opt-in), with true power-state readback from the ME. |
| `hidrig` | Rust | HID-injection helper: protocol client + `serve` daemon for the KB2040 injector, wired in via the generic `hid` channel. |
| `ch9329` | Rust | HID-injection helper with the same CLI + `serve` daemon, speaking the CH9329 (serial-to-USB-HID chip) frame protocol for Openterface Mini-KVM / KVM-Go and Sipeed NanoKVM-USB. |
| `jetkvm` | Go (pion/webrtc) | HID-injection helper for the JetKVM network KVM; its daemon also decodes the device's H.264 video (`ffmpeg` subprocess) and serves it as RFB for `hdmicap`'s `rfb+hid:` source. |
| `visionocr` / `linuxocr` | Swift / shell+Tesseract | On-device OCR helpers invoked by `hdmicap` (`GET /ocr`, wrapped by `paniolo video read` and the dashboard OCR button). |
| HID rig firmware (separate repo) | CircuitPython | Two KB2040 (RP2040 microcontroller) boards: a "dumb pipe" relaying host-composed HID reports to the DUT as USB keyboard + mouse. Not in this repo: see [`paniolo-hardware`](https://github.com/curtisgalloway/paniolo-hardware)'s [`hidrig-kb2040/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040), driven by the `hidrig` row above. |

Only `paniolo` lands on PATH (`~/.cargo/bin`). Helpers and daemons install into
`~/.local/libexec/paniolo/bin`: `paniolo helper <name>` runs one directly, and `paniolo daemons`
lists/stops/restarts running ones. Out-of-tree helpers and skills come from **plugin roots**
(`PANIOLO_PLUGIN_PATH`, or a lab host's `plugin_path` on dispatch), searched after every
bundled dir so they can add but never shadow ([power.md](../power.md#out-of-tree-helpers-and-skills-plugin-roots)).

*Option B* (one long-running server with socket RPC) is noted in `AGENTS.md` but **not**
implemented. The only cross-subsystem coupling is the dashboard's hdmicap→serialcap link (§7).

## 4. Configuration and state model

**All configuration lives in one CLI-managed lab file**: `~/.config/paniolo/lab.toml`, or
`--lab` / `PANIOLO_LAB` (e.g. a git-tracked file). It names **hosts** and **targets**; each
target's hardware is a set of *channels* (`netboot`, `serial`, `power`, `video`, `hid`, `usb`, `adb`),
each bound to the host it is attached to. With exactly one target, the target argument may be
omitted. Schema (`cli/src/model.rs`):

```toml
[hosts.mac1]                     # optional — "local" is implicit; entries name remote hosts
ssh = "user@control-mac"         # how others reach it; identity / paniolo_cmd / control_path /
                                 #   description optional
# hostname = "mac1.local"        # this box's FQDN — set it so the host recognizes itself when one
#                                  shared lab file is run from any machine (matched vs `hostname -f`)

[targets.target-machine]
[targets.target-machine.netboot]
interface = "en3"                # USB-Ethernet interface for netboot
host_ip = "192.168.99.1"         # static IP on that interface; also the TFTP/HTTP server address
tftp_root = "/path/to/pxe"       # required to start netboot
# boot_file = "grubaa64.efi"     # UEFI NBP for PXE / HTTP Boot clients (http_port,
#                                #   content_type optional; see netboot.md)

[[targets.target-machine.serial]] # repeatable — a target may have several named consoles
name = "console"
device = "/dev/serial/by-id/…"   # stable symlink preferred (Linux): by-id names the
                                 #   adapter; by-path when it has no serial number
baud = 115200
power_sense_signal = "cts"       # optional; cts|dsr|dcd|ri — modem-control input wired to the rail
# power_button = true            # opt-in: DTR is wired to the board's power button
#                                #   (required by `serial dtr` / `serial reset`)

[targets.target-machine.power]   # generic hooks — device-specific logic lives in helpers
cycle_cmd = "zigplug … cycle …"  # `paniolo power-cycle`
on_cmd = "zigplug … on …"        # `paniolo power on`   (off_cmd / state_cmd likewise)
serial_interface = "console"     # default interface for DTR power commands

[targets.target-machine.video]
device = "USB Video"             # HDMI capture device for hdmicap

[targets.target-machine.hid]
cmd = "hidrig -d /dev/…"         # opaque helper prefix; `paniolo hid send` appends args

[targets.target-machine.usb]
cmd = "ch9329 -d /dev/…"         # helper prefix; paniolo appends only `usb host|target|state`

[targets.target-machine.adb]     # an Android DUT reached over adb
serial = "33271JEGR02033"        # `adb -s <serial>`; omit for the sole device
```

Every channel also takes an optional `host = "<name>"` binding it to a remote control host
(§5 "Distributed control"). Only the CLI edits the file (`paniolo target add`, `netboot set`,
`serial add`, `power set`, `video set`, `hid set`, …). The old per-target files
(`~/.config/paniolo/targets/<name>.toml`, `video.toml`) are not read.

**Runtime state, discovery, and capture** live outside the config tree:

- **Discovery and auth.** Each daemon writes an owner-only **discovery file** (pid + port + a
  per-start bearer **token**) and holds an **advisory lock**. Every request must carry the token
  (`Authorization: Bearer` from the CLI, `?token=` from the dashboard), and daemons accept only
  loopback `Host`/`Origin`. Binding 127.0.0.1 keeps other machines out; token and origin checks
  keep other *web pages* out (`cli/src/daemons.rs` `Endpoint`, each daemon's `auth.rs`).
- **Staleness.** The CLI records the daemon's SHA-256 content digest (a checksum
  of its executable bytes) and process ID at spawn in `binmeta.json`. Linux also
  compares the actual running executable through `/proc/<pid>/exe`, including a
  deleted executable after an atomic install. The digest uses
  `/usr/bin/sha256sum` on Linux, `/usr/bin/shasum -a 256` on macOS, and
  .NET streaming SHA-256 through Windows PowerShell under
  `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe` on Windows.
  The Windows adapter calls `SHA256.ComputeHash` on a read-only file stream and
  disposes the stream and hashing object; it requires no PowerShell modules.
  The hash tool is resolved independently of the helper search `PATH`;
  hashing failures are reported as unknown identity. A proven content difference
  is flagged **stale** in `paniolo daemons`, `serial show`, and `video show`;
  identical bytes at another path or with different metadata remain current.
  Legacy records without a digest remain readable. Missing identity or unavailable
  content produces **binary identity unknown** with a reason. Metadata alone
  cannot prove a content difference. `daemons restart --stale` and automatic
  `watch` restarts select only proven changes; named restart, `--all`, and
  `video watch --restart` remain deliberate restart controls.
- **Instances.** Capture daemons (serialcap/hdmicap/hid) run one instance **per target**;
  zigplug/cambrionix/netbootd run one per host.
- **Helper env vars.** Every helper invocation gets two pre-created directories:
  `PANIOLO_STATE_DIR` (durable state, `~/.config/paniolo/helpers/<name>`) and
  `PANIOLO_RUNTIME_DIR` (`/tmp/paniolo-<uid>/<name>`; the capture daemons append a `/<target>`
  segment). The runtime base honors `$PANIOLO_RUNTIME_BASE` (default `/tmp`)
  (`cli/src/daemons.rs`; contract in [`adding-power-helpers.md`](adding-power-helpers.md)).
- **Private runtime directory.** Created 0700. An existing one must be a real directory owned
  by the current user: a too-open one we own is tightened; a symlink or another owner's is
  refused. Discovery-file readers check the same before trusting a `daemon.json`, and the SSH
  ControlMaster socket dir (`cli/src/ssh.rs`) is created through it. Daemon stderr logs and
  serialcap capture files are 0600.

The paths:

| Purpose | Path |
|---|---|
| Lab file (all config) | `~/.config/paniolo/lab.toml` (or `--lab` / `PANIOLO_LAB`) |
| Helper durable state (`PANIOLO_STATE_DIR`) | `~/.config/paniolo/helpers/<name>/` (e.g. zigplug's `zigbee.db`) |
| Netboot state (pids, uptime) | `~/.local/share/paniolo/<name>/netboot.json` |
| Netboot combined log | `~/.local/share/paniolo/<name>/netboot.log` |
| hdmicap discovery / lock (per target) | `/tmp/paniolo-<uid>/hdmicap/<target>/{daemon.json, daemon.lock}` |
| serialcap discovery / lock (per target) | `/tmp/paniolo-<uid>/serialcap/<target>/{daemon.json, daemon.lock}` |
| hid daemon discovery (channel name, any injector, per target) | `/tmp/paniolo-<uid>/hid/<target>/daemon.json` |
| zigplug daemon discovery (host singleton) | `/tmp/paniolo-<uid>/zigplug/daemon.json` |
| serialcap capture log (per interface) | `/tmp/paniolo-<uid>/serialcap/<target>/capture/<name>/serial.jsonl(.1..)` |
| serialcap pending (unterminated) line | `/tmp/paniolo-<uid>/serialcap/<target>/capture/<name>/pending.json` |

A `/tmp` age policy (Debian's `q /tmp 1777 root root 10d`) can delete these files under a
long-running daemon, which then holds its device while paniolo reports the channel stopped. The
`.deb` ships `/usr/lib/tmpfiles.d/paniolo.conf` (`x /tmp/paniolo-*`) to prevent it, and
`paniolo daemons` / `video show` / `video watch` reap any orphan (GitHub #187).

## 5. Subsystems

### Netboot / deploy ([`netboot.md`](../netboot.md))
`netbootd` runs **DHCP + TFTP + HTTP** as tokio tasks over a **direct USB-Ethernet link** (no
router, switch, or upstream DHCP, no `dnsmasq`).

- **Start.** `paniolo netboot start` assigns the static `host_ip`, then spawns `netbootd`, with
  `sudo` on Linux (ports 67/69 and 80 need root; macOS 10.14+ allows them rootless).
- **DHCP** hands the target a fixed lease and points it at the TFTP root via BOOTP `siaddr` +
  DHCP option 66. **TFTP** is read-only (RFC 1350 + blksize/tsize).
- **UEFI clients.** Dispatch is by DHCP vendor class: a `PXEClient` gets `boot_file` over TFTP;
  an `HTTPClient` gets an `http://…/<boot_file>` URL (HTTP Boot, see [`netboot.md`](../netboot.md)).

**Interface safety checks.** `paniolo netboot start` refuses:

- an interface carrying the system default route: it gets reconfigured to `host_ip`, so the
  netboot link must be a dedicated secondary (USB-Ethernet) interface.
- a second target on an interface another target's live netbootd already serves.

`netif::configure_interface` refuses a /24 that any *other* interface already holds (one subnet
per link, see docs/netboot.md). `LabFile::set_netboot` applies the same rule at edit time
(judging only the edited link, so a lab with two clashing pairs stays repairable), and `doctor`
reports it as `CONFLICT`. `netboot start` watches the daemon for ~2 s and, if it exits, fails
with the log tail instead of recording a dead daemon.

**Listening and privilege.** Every listener (DHCP, TFTP, HTTP) is pinned to the netboot
interface (`IP_BOUND_IF` / `SO_BINDTODEVICE`) before binding; a failed pin is fatal
(`--interface` is required). Startup: validate → bind and pin → drop root → serve. On Linux the
daemon drops to the invoking user (`SUDO_UID`/`SUDO_GID`) once bound, so no packet parsing runs
privileged; its later `sudo ip …` calls need passwordless sudo. The client lease is `host_ip`'s
/24 with last octet `100`. An HTTP bind failure disables only HTTP Boot.

On macOS, `pin::ListenerInterface` holds a system-wide, per-interface file lock
for the daemon lifetime before allowing `SO_REUSEPORT` on listeners. This lets
separate adapters share the standard ports while rejecting a second netbootd
on the same adapter. Wildcard binds still survive temporary loss of the
interface address. Linux keeps its exclusive `SO_BINDTODEVICE` binds.

**One client.** `netbootd` enforces one *client*, not just one lease:

- DHCP locks onto the MAC of the first DISCOVER/REQUEST for the life of the process and ignores
  other MACs, so a second device cannot take the lease, the ARP pin, or TFTP's raw-frame MAC.
- TFTP accepts RRQs only from the leased IP and caps concurrent transfers at `MAX_TRANSFERS`
  (a semaphore), so an RRQ flood cannot grow tasks/sockets/files without bound.

Neither gate resets in-process: a new client needs `paniolo netboot stop` then `start`.

**macOS BPF helper.** `netbootd`'s raw-frame send path (the Sequoia workaround) gets a
`/dev/bpf` descriptor from the setuid-root `netbootd-bpf-helper` over `SCM_RIGHTS`, installed by
`paniolo setup`; the daemon stays unprivileged. The descriptor is write-only with a reject-all
filter (inject, never capture), and the helper serves only its installing user and never the
default-route interface.

### Link mode: netboot · link · ffx · off ([`netif.md`](../netif.md))
The same USB-Ethernet link serves **mutually-exclusive** roles:

| Mode | Host side | Use |
|---|---|---|
| netboot | IPv4 + DHCP + TFTP + HTTP | the target TFTP-boots |
| `link` | host IP up, no daemon | link up/down testing |
| ffx | host IPv6 link-local `fe80::1`/64 | the target boots from SD and is reached over `ffx` (Fuchsia's host tool) at `fe80::…%<iface>` |
| `off` | host IP released | |

`paniolo netif mode <netboot|link|ffx|off>` (`cli/src/netif.rs`) switches atomically and
idempotently. `ffx` runs `netboot stop` first (so a power-cycle boots SD, not a stale TFTP
image) and adds the host `fe80::1` ffx needs.

The active mode is **probed** (daemons + interface addresses), not stored, so
`paniolo netif status` survives reboots. In ffx mode it reports the discovered link-local peer
(`ip -6 neigh`) as a paste-ready `ffx target add`.

`paniolo netif down-hard` goes beyond `mode off` when the target must *detect* link loss: it disables
Wake-on-LAN (which keeps the PHY energized) and admin-downs the interface so the peer sees
carrier drop, using netboot's `sudo` path.

### Serial console ([`serial.md`](../serial.md))
One `serialcap` daemon **per target exclusively owns all its serial interfaces**. Per interface,
a *supervisor* task owns the port (with a reconnect loop) and **fans every byte out three ways**:

1. broadcast to live WebSocket clients (`/stream`);
2. a 64 KB scrollback ring for instant replay;
3. a tee to a capture thread that writes **timestamped, sequence-numbered JSONL** lines to disk
   (rotating, survives restarts) and a separate immutable exact-byte receive journal.

The journal contains only actual RX chunks, carrying the supervisor's epoch,
RX sequence, connection generation and original receive time. The capture
thread supplies a separate capture time and writes owner-only
`evidence.jsonl(.1..4)` segments plus `evidence-state.json` checkpoints.
`serial evidence` / MCP `serial_evidence` read this journal offline, with raw
byte caps and explicit epoch, retention, corruption and write-fault diagnostics;
they do not change the existing line/pending contract. See
[serial receive evidence](../serial.md#immutable-receive-evidence).

WebSocket clients send bytes the supervisor writes to the port. `paniolo serial log` reads the
JSONL **directly**, so it works with or without the daemon. `paniolo serial connect` execs `tio`
for a foreground terminal; it holds the port exclusively, so it conflicts with the daemon.

### Power control ([`power.md`](../power.md))
Two mechanisms:

- **DTR via FTDI.** The serial adapter's DTR line is wired to the board's J2 power-button header: `serial dtr`/`serial reset`, ≤500 ms soft / ≥3 s hard PMIC off.
- **Generic power hooks.** Arbitrary shell commands on the target's `power` channel
  (`cycle_cmd`/`on_cmd`/`off_cmd`/`state_cmd`, run by `paniolo power-cycle`, `power on/off`,
  `power-state`).

Device-specific logic lives in helpers behind those hooks, never in the core: `cambrionix`,
`zigplug`, `shellyplug`, and `amt` ship with paniolo, and `hidrig` can switch a DUT relay
(`hidrig power`). Without a `state_cmd`, `paniolo power-state` falls back to an optional
**power-sense** input on the target rail via serialcap's `/status`.

### Video + OCR ([`video.md`](../video.md))
`hdmicap` keeps a UVC HDMI capture device open (avoiding multi-second reopen latency) and serves
the current frame as PNG/MJPEG plus the dashboard over HTTP. `paniolo video read` (hdmicap's
`GET /ocr`) and the dashboard OCR button run **on-device OCR** on the warm frame: Apple Vision (`visionocr`) on macOS, Tesseract
(`linuxocr`) on Linux. Both are tuned for thin console fonts (2× upscale, black-pad,
`.fast`/lowered min text height).

A video channel can instead name a **network RFB source** (`rfb+hid:` or `rfb://HOST:PORT`).
`hdmicap`'s RFB client (`hdmicap/src/rfb.rs`) then feeds the same warm-frame pipeline, so `/snapshot`,
`/ocr` and the dashboard are unchanged. For `rfb+hid:` the source is the target's own hid daemon:
`jetkvm serve` decodes the JetKVM's H.264 with an `ffmpeg` subprocess and serves RFB at its
token-authenticated `GET /rfb`; `hdmicap` finds it through the daemon's discovery file. Agents only ever
get decoded stills. Decisions: [`notes/network-video-rfb.md`](https://github.com/curtisgalloway/paniolo/blob/main/notes/network-video-rfb.md).

On Linux the capture rate follows demand (`hdmicap/src/demand.rs`): 30 fps while a `/preview`
is open, a `/snapshot` is waiting for a change or a stable signal, or a pull came in the last
300 ms; 5 fps otherwise. A `/snapshot` or `/ocr` that finds the warm frame older than 66 ms
wakes the loop and waits up to 250 ms for a new one, and the V4L2 backend skips queued buffers
older than 66 ms, so a slow idle rate never serves a stale screen. macOS and Windows follow the
device's own cadence.

### HID injection ([`hid.md`](../hid.md))
The dual-board rig appears to the DUT as a USB keyboard + mouse. `hidrig` composes HID reports
on the host and writes frames to the **control** board's USB-CDC port, which relays them over
I2C1 to the **target** board ([hid-dual-board-design.md](hid-dual-board-design.md)). The
[HID serial protocol](hid-serial-protocol.md) is only `hidrig`'s *external* command vocabulary.
paniolo drives the helper through the per-target `hid` channel, an opaque command prefix
(`paniolo hid send` appends arguments), like the power hooks.

### adb / Android targets ([`adb.md`](../adb.md))
The per-target `adb` channel binds an Android device (`adb -s <serial>`) to its control host,
giving the same verbs over one transport:

- console: `paniolo adb shell` (interactive), `adb run` (one-shot);
- screen: `adb screencap`, via `adb exec-out screencap -p` → PNG;
- input: `adb input` → `adb shell input`.

adb is a generic transport, so it lives in the core CLI (`cli/src/adb.rs`), shelling out to the
host's `adb` and routed like every other channel. For reboot/power, wire `adb reboot` through the
power hooks.

### Dashboard ([`dashboard.md`](../dashboard.md))
`paniolo console` opens hdmicap's `GET /`: live video (a noVNC view of hdmicap's `GET /rfb`, with MJPEG as a toggle and fallback) on top, xterm.js terminal(s) below (§7).

### Distributed control ([`distributed-control.md`](../distributed-control.md))
Drives targets on **remote control hosts** over SSH only, with no coordinator daemon.

- The lab file names the hosts and binds each target's channels to one (`cli/src/model.rs`).
  `cli/src/ssh.rs` is the transport (per-host ControlMaster,
  `run`/`forward`/`run_interactive`).
- Commands that touch a remote channel **re-exec** on its host: `cli/src/dispatch.rs` ships the
  relevant lab slice as a temp file and re-invokes with `--lab`.
- `console` **tunnels** the daemon ports back and stitches them together with the dashboard's
  `?serialws=` override.
- `setup --host` provisions a host; `discover`/`configure` propose a lab block for review.
- A target's channels may span hosts; only `console` requires them co-located.

`console --detach` and locking remain design-only (see
[`distributed-control-plan.md`](https://github.com/curtisgalloway/paniolo/blob/main/notes/distributed-control-plan.md)).

## 6. Representative data flows

- **Boot-and-watch:** agent `scp`s an image into `tftp_root` → `netboot start` → target PXE-boots
  over the USB-Ethernet link → boot output streams through `serialcap` to the JSONL log → agent
  polls `serial log --since` (or watches the dashboard / OCRs the screen).
- **Serial round-trip:** UART bytes → supervisor → {WebSocket clients, scrollback, JSONL capture
  thread}; dashboard keystrokes → WebSocket → supervisor → UART.
- **Power-cycle from the dashboard:** browser → hdmicap `POST /power-cycle` → `paniolo
  power-cycle <target>` (target from `PANIOLO_TARGET`) → the target's `cycle_cmd` hook.

## 7. Cross-subsystem coupling (the dashboard)

hdmicap **serves the page** but references serialcap **only by URL**:

- `paniolo console` passes a complete loopback WebSocket URL, with serialcap's own token inside,
  as `?serialws=` (for a remote target, the tunnel's local port). hdmicap's token rides as
  `?token=`.
- The page refuses a non-loopback URL. Its built-in `ws://<host>:8724/stream` fallback applies
  only when the page is opened by hand.
- The page fetches serialcap's `/interfaces` and builds one xterm.js terminal per interface.

The video pane is noVNC dialing hdmicap's own `GET /rfb`. hdmicap diffs the warm frame into
changed 64x64 tiles and sends them as ZRLE; key and pointer events come back over the same
socket and hdmicap forwards them to the target's hid daemon (`/hid`), found through the
`HDMICAP_HID_DISCOVERY` discovery file the CLI sets when the target has a local `hid` channel.
That is the one place hdmicap holds another daemon's token, read from the file, never the
environment or a URL.

No daemon learns another's token through the browser. xterm.js and noVNC are **vendored**, so the dashboard works on an
isolated network. Power controls appear only when hdmicap was started with a target; their
probe (`GET /power`) performs no power action.

## 8. Host-OS differences (macOS vs Linux)

Core power/serial/netboot works on both. Differences:

| Area | macOS | Linux |
|---|---|---|
| Netboot ports 67/69 | rootless (10.14+) | `sudo` (auto-prepended); netbootd drops root to `SUDO_UID` once bound |
| Listener pinning to the netboot interface | `IP_BOUND_IF` | `SO_BINDTODEVICE` (set while still root) |
| Interface config | `networksetup` / `ifconfig` | `ip addr`/`ip link` (iproute2) |
| ARP pinning | `arp -s` | `ip neigh replace … nud permanent` |
| TFTP egress workaround | BPF raw frames (`/dev/bpf*`) for Sequoia routing | normal `sendto()` |
| BPF descriptor access (rust engine) | setuid `netbootd-bpf-helper` passes the fd (daemon stays unprivileged) | n/a (kernel send path) |
| OCR backend | Apple Vision (`visionocr`, `swiftc`) | Tesseract (`linuxocr`, `tesseract-ocr` pkg) |
| Serial device discovery | `/dev/tty.usb*` | `/dev/serial/{by-id,by-path}/*` grouped per port, by-id preferred → `/dev/ttyUSB*`/`ACM*` |
| `paniolo setup` extras | compiles `visionocr` (`swiftc`) + installs setuid `netbootd-bpf-helper` (one sudo) | `linuxocr` (needs `tesseract-ocr` pkg) |

For headless CI (`ci-integration/`), the core path is clean on Linux.

## 9. Lifecycle & exclusivity notes

Serialcap, hdmicap, ch9329, hidrig, and zigplug's daemon accept an authenticated `POST /stop`.
Stop commands use it instead of signaling the discovery-file PID, which may have been reused;
it shares the signal handlers' cleanup and exit path.

- **Serial ports are exclusive.** Only one of `serialcap` / `tio` / `screen` can hold a port, so
  `serial watch` and `serial connect` conflict on the same device.
- **Daemons hard-exit on SIGTERM.** They serve infinite responses (`/preview` MJPEG, `/stream`
  WebSocket), so each removes its discovery file, waits ~300 ms, and calls `exit(0)`.
- **Daemon APIs are authenticated** (§4); no `Access-Control-Allow-Origin: *`. A daemon from an
  older paniolo has no token; replace it with a named `paniolo daemons restart`.
- **Interface configuration needs root.** Use NOPASSWD sudo for unattended agents.
- **Link modes (netboot / link / ffx / off) are mutually exclusive.** netboot and ffx need
  incompatible host addressing; `paniolo netif mode` tears down one when entering the other.

## 10. Where this is going

Paniolo is in daily use for hardware bring-up, with an agent deploying, booting, observing,
and power-cycling targets unattended.

Active work makes its primitives consumable by hardware-CI orchestrators (KernelCI/LAVA,
Fuchsia/botanist) via a stable device-control API plus thin adapters. Done: discrete power verbs
(`power on/off`) and `serial send`. Remaining: raw serial as a TCP socket + PTY, and a JTAG
extension point. Design and rationale are in [`ci-integration/design.md`](ci-integration/design.md) and
[`ci-integration/gap-analysis.md`](ci-integration/gap-analysis.md); progress is tracked in
[`requirements.md`](requirements.md).

## 11. Prior art & why paniolo

The closest tool is **labgrid** (Pengutronix, Python): also a device-control layer *under* a
test framework, with no verdicts of its own.

- **labgrid** is the mature **distributed** board-farm standard (coordinator/exporter/client over
  gRPC, a large driver catalog, multi-user locking), **Linux-only**. Use it for multi-board,
  multi-user farms.
- **Paniolo** serves the single-target bring-up loop: **single control host,
  zero-infrastructure, agent-in-the-loop**, with on-device **OCR**, a **USB-HID injection** rig,
  a combined video+serial dashboard, and first-class **macOS** support.

Where they overlap (raw-socket serial, discrete power verbs, a driver abstraction), labgrid
validates paniolo's CI-integration direction. Full comparison: [`ci-integration/related-work.md`](ci-integration/related-work.md).

Serial health is available through `paniolo serial status` and MCP
`serial_status`: lab identity is separate from daemon identity, and process
liveness is separate from port connection and receive traffic. serialcap tracks
an epoch per interface supervisor, reconnect generation, receive chunk counters
and capture-thread processing progress. The snapshot does not start or restart
a daemon, and a capture watermark does not assert durable disk writes.

`serial wait` forwards a finite receive-boundary acquisition to serialcap on
that channel's host. The helper reads its immutable journal and authenticated
status, preserving accepted chunks on timeout/disconnect/cap. It delivers
optional completed-row NDJSON and a final evidence object; the total encoded
stdout budget covers both. No wait path opens the serial port or changes the
daemon. MCP returns the collected result through its normal command adapter.

## Capabilities

This is the capability list formerly in the root AGENTS.md.

Paniolo is a CLI tool that lets an AI agent fully control a target machine
during low-level software development (bootloader, firmware, OS bring-up).
"Paniolo" is the Hawaiian word for cowboy — the agent wrangles the target.

Current capabilities:
- DHCP + TFTP + HTTP netboot over a direct USB-Ethernet link (`paniolo netboot`) —
  Raspberry Pi (TFTP) plus UEFI **PXE** and **HTTP Boot** (IPv4) for EDK2 boards,
  selected per-request by DHCP vendor class (option 60)
- HDMI/USB capture via hdmicap warm-stream daemon (`paniolo video`)
- Serial console — interactive (tio) or daemon-backed for the web dashboard (`paniolo serial`);
  one daemon **per target** owns that target's several named interfaces, each with a
  timestamped rolling capture log queryable by line range (`paniolo serial log -i <name>`)
- Combined video+serial web dashboard (hdmicap's `GET /`: a noVNC view of hdmicap's own `GET /rfb` on top, with keyboard/mouse through the hid daemon, and an xterm.js terminal below)
- Network video sources via RFB (`paniolo video set --device rfb+hid:` for a JetKVM, or `rfb://HOST:PORT`): hdmicap is the RFB client; the first consumer is the JetKVM, whose daemon decodes H.264 with `ffmpeg` and serves RFB (docs/video.md, notes/network-video-rfb.md)
- On-device OCR of the captured screen (`paniolo video read [target] [--stable]`, which wraps hdmicap's `GET /ocr`; also the dashboard OCR button): Apple Vision on macOS, Tesseract on Linux
- USB HID input (keyboard/mouse injection) via a generic helper hook (`paniolo hid send`); the `hidrig` helper drives the dual-board KB2040 injector — it composes HID reports in Rust and writes binary frames to the control board's USB-CDC endpoint, which relays them over I2C1 to the target board (the "dumb pipe", docs/dev/hid-dual-board-design.md; command vocabulary in docs/dev/hid-serial-protocol.md). `hidrig serve` runs a daemon that owns the control link and re-exposes the command vocabulary over a WebSocket, so `paniolo console` works as a **KVM** — stream the browser's keyboard + absolute mouse (`moveabs`) to the target, intermixed with CLI injection on the one wire. The same control board can also **bridge the DUT serial console** (its hardware UART, re-exported by the daemon as a PTY into the `serial` channel) and **switch DUT power** via a relay (`hidrig power off|on|cycle`), so one USB device backs the target's HID, console, and power (design §6–§7; the relay/power path is hardware-verified, incl. NVM state persistence across a control-board reset — the console bridge is not yet)
- Named target actions (`paniolo action list|run|set|rm`, #299): a per-target `actions` table of opaque hook commands for controls that are not power (reset lines, boot straps, recovery modes), each with a `description` and an optional `requires_power = "on"|"off"` checked against `power-state` (read wherever the power channel lives) before running. Same contract as the power hooks, plus a **per-target hook lock** (`actions.rs`, a lock file under `<runtime>/hooks/<target>/`) shared by action and power hooks, since both often drive one exclusive device; the holder exports `PANIOLO_HOOK_LOCK=<target>` so a nested paniolo call for that target skips it. No extra arguments and no fixed verbs: paniolo stays a dispatcher. MCP `action_list`/`action_run`. Guide: docs/actions.md
- Switchable USB media via a generic per-target `usb` channel (`paniolo usb attach-host|attach-target|state`): one physical USB device routed to the control host or the target, never both. Supported today on the **Openterface KVM-Go**, whose onboard microSD reader sits behind an FSUSB42 mux driven by the same CH32V208 (and the same serial port) as its `hid` channel — so the `ch9329` helper backs both, and the two channels normally carry the same `--cmd`. The point is hands-free *physical* boot media, which firmware can see and streamed virtual media generally cannot. Like the power hooks the helper is opaque, but the vocabulary is **fixed** rather than passed through: paniolo appends `usb host`, `usb target`, or `usb state`, keeping the surface a constrained remote host must expose to three verbs. Guide: docs/usb.md; clean-room protocol: notes/openterface-usb-mux-spec.md. The Mini-KVM's switchable USB-A port uses a different mechanism (a register write over the capture chip's HID config interface) that is documented but not yet implemented by any helper
- Power control via DTR (J2 wiring; **opt-in per serial interface** via `power_button = true` — `serial dtr`/`reset` refuse interfaces that haven't declared it) or generic shell-command hooks (`on_cmd`, `off_cmd`, `cycle_cmd`, `state_cmd`): `paniolo serial dtr`, `paniolo power on/off`, `paniolo power-cycle`, `paniolo power-state`. **Named power levels** (#302, `[targets.T.power.levels.<name>]`, `model::PowerLevel`) add further hook sets beside the default one for targets with several power layers (a `soft` MCU/PMIC switch, a `hard` mains cut); `--level NAME` on `power on/off`, `power-cycle` and `power-state` (and `power set`/`power rm` to edit them) picks one, no `--level` (or `--level default`, hence the reserved name) is the default hooks, and a level lacking the verb's hook is `not_configured` naming the level, never a fallback. Level hooks run through the same `run_power_hook`/`read_state_cmd` and so take the per-target hook lock; levels ride the dispatch slice with the power channel; `power-state` without `--level` prints one line per level, default first (hdmicap's dashboard reads only that first line, and `requires_power` asks for `--level default`); MCP power tools take `level`; `doctor` probes each level. Guide: docs/power.md#power-levels-soft-and-hard. Note: "reboot over the serial console" means `serial send <t> "reboot"` (software), *not* the DTR `serial reset` (hardware). Helpers that wire into the hooks: `cambrionix` (Cambrionix hub port power via control UART), `zigplug` (Zigbee smart plugs via a CC2652 coordinator dongle), `shellyplug` (Shelly Gen2+ smart plugs/relays over the device's local HTTP RPC API — no cloud/HA/Matter), and `amt` (Intel AMT/vPro machines over WS-Management on port 16992 with HTTP Digest auth — per-target power with no plug hardware, plus true power-state readback from the ME; password via `AMT_PASSWORD`, `--password-file` or `--password-command`, never in the lab file). The dual-board `hidrig` control board can also drive a DUT power relay (`hidrig power off|on|cycle`) as a power-helper backend, consolidating HID + console + power on one USB device

