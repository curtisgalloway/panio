---
name: paniolo
description: Control a physical target machine (SBC, e.g. a Raspberry Pi) with the paniolo CLI during low-level bring-up — netboot it over a direct USB-Ethernet link, watch and OCR its HDMI screen, drive its serial console, power-cycle it, switch the link between netboot and ffx, and drive targets on remote control hosts transparently via a single git-tracked lab file over SSH. Use when you need to boot, observe, type into, screenshot, read the screen of, power, or remotely control a target/board through paniolo — including across multiple control hosts.
---

# Paniolo — controlling a target machine

Paniolo drives a physical target machine (a single-board computer such as a
Raspberry Pi) during firmware/OS bring-up. It runs on the control host that is
physically wired to the target (USB-Ethernet for netboot, an HDMI capture
dongle for video, a USB-serial adapter for the console, a smart plug for power).

Almost every command operates on a named **target**. If exactly one target is
configured, the name can be omitted.

## Drive through paniolo — don't touch its devices directly

Paniolo owns the hardware it's configured for, and a background **daemon** holds
most of it open: netbootd (the netboot interface + DHCP/TFTP), serialcap (each
serial port), hdmicap (the capture device), the hid injector. Those daemons
track state, and the netif/netboot commands assume they're the only ones
touching the link and ports. So **always go through paniolo commands; never
reconfigure or open a paniolo-managed device by hand**:

- **Don't** run `ifconfig` / `ip` / `networksetup` / `ethtool` on the netboot
  interface — use `paniolo netif mode …` (and `paniolo netboot start/stop`). A
  stray address or an admin-down behind paniolo's back desyncs what `netif
  status` reports and can silently break DHCP/TFTP.
- **Don't** open a serial port with `screen` / `tio` / `minicom` while a
  `serial watch` daemon holds it (the port is exclusive) — read with `paniolo
  serial log` and write with `paniolo serial send`.
- **Don't** `kill` a daemon or its helper by PID — use `paniolo daemons stop`
  (or the per-subsystem `stop`).
- **Don't** rewrite the lab config to "fix" a mismatch you didn't create —
  neither by editing the lab file nor via `… set`/`… rm` config commands. The
  lab file is the user's reviewed source of truth, and `paniolo doctor` is a
  hint, not ground truth: when doctor flags a channel, **try the channel
  first** (`hid send … ping`, `serial log`, `video shot`). If it works, the
  config is right — report the doctor discrepancy to the user instead of
  "repairing" it.
- **Don't** reach around a remote control host. A remote target is driven by
  the same paniolo commands from where you are (the lab file routes them over
  SSH) — don't ssh over to run helper binaries against its devices, curl a
  daemon's local port, or read `/dev/video*`/serial devices directly; the
  daemons there hold the same exclusive locks.

If a paniolo command for what you want doesn't seem to exist, run `paniolo
--help` or re-read this skill before reaching for the raw device — it almost
certainly does (e.g. `netif mode link`/`off` to toggle the bare link, `daemons`
to find and clear a stuck port).

## When a command fails — read the exit status

The exit status names the kind of failure; branch on it, not on the message
text. `paniolo --help` prints the table.

| Code | Kind | What to do |
|---|---|---|
| 1 | — | Only `doctor`: it found problems (not an error) |
| 2 | `usage` | Fix the command line (`<cmd> --help`) |
| 3 | `not_configured` | Wrong target/channel/interface name, required configuration missing, or a hook/helper missing. Don't retry; check `paniolo config show`, and don't edit the lab to fix it — report it |
| 4 | `unreachable` | The control host did not answer over SSH; the command may or may not have run |
| 22 | `timeout` | Outcome unknown: check state (`power-state`, `serial log`, `video shot`) before repeating a power cycle or other mutation |
| 100 | `daemon_down` | Start the daemon (`serial watch`, `video watch`); `paniolo daemons` lists what is running |
| 101 | `helper_failed` | The hook/helper/daemon ran and failed; its message (and its own code in `child_exit`) says why. On Windows a *missing* hook also lands here (`cmd.exe` exits 1 for it) |
| 109 | `internal` | Unclassified; read the message |

For a machine-readable reason, set `PANIOLO_JSON_ERRORS=1` (or put
`--json-errors` right after `paniolo`, before any trailing arguments: after
`hid send …` or `adb run …` it is taken as part of the payload): the **last line of stderr** is then one JSON object,
`{"error": {"kind", "code", "message", "target", "channel", "host",
"child_exit"}}`. Passthroughs (`helper`, `config edit`, `video shot`/`devices`,
`adb run`/`input`/`devices`, `setup --host`, and the consoles `serial connect`
and `adb shell`) exit with their program's own status instead. `serial log` still reads a stopped daemon's file, with a
`warning:` on stderr; add `--require-live` to get exit 100 instead of stale
output. A control host on paniolo 0.4.x or older exits 1 for every error and
rejects `--json-errors`.

## First-time setup

```
cd ~/src/paniolo
make install               # cargo-installs the `paniolo` CLI, then `paniolo setup`
                           # builds/installs the helper binaries (hdmicap, serialcap,
                           # netbootd, cambrionix, hidrig, ch9329, shellyplug,
                           # amt), the OCR helper, zigplug, and the bundled skills;
                           # on macOS also installs netbootd-bpf-helper
                           # setuid-root (one sudo)
```

Run once per machine; re-run after pulling or editing (it's a full rebuild).
Only the `paniolo` CLI lands in `~/.cargo/bin` — make sure that's on `PATH`.
The helpers (hdmicap, serialcap, netbootd, cambrionix, hidrig, ch9329,
shellyplug, amt, jetkvm (needs Go), zigplug, the
OCR tool) live in the private libexec dir `~/.local/libexec/paniolo/bin`,
off PATH; paniolo resolves them itself, and `paniolo helper [NAME] [ARGS…]`
lists or runs one directly. If a different `paniolo` shadows the CLI on PATH
(e.g. a Homebrew keg from the tap winning over `~/.cargo/bin`), put
`~/.cargo/bin` first or remove the other copy; `make install` warns when it
detects a shadow.

Discover installed helpers with `paniolo helper`. To read a helper's own
help, put `--` before its name: `paniolo helper -- serialcap --help`.
`paniolo helper --help` and `paniolo helper serialcap --help` show paniolo's
outer command help instead. Listing helpers and reading a bundled helper's
help do not open hardware or start a daemon; use these to discover its
commands before invoking one.

## Bounded fresh serial acquisition

Use `paniolo serial wait <target> [-i console] --epoch E --since N` (MCP:
`serial_wait`) with `health.epoch` and `health.last_rx_seq` saved before input.
These are RX chunk counters, not line-log sequence numbers. Optional
`--pattern TEXT` matches literal UTF-8 across fresh raw chunks. Omit it to
collect until the deadline/cap; a match never establishes task success.

The wait is read-only and finite: `--timeout-ms` defaults to 30000 (0 is one
immediate observation, max 3600000). `--max-bytes` caps raw evidence at
1..1048576 (default 65536); `--max-output-bytes` caps total JSON stdout,
including all follow events and final result, at 4096..8388608 (default 8388608).
MCP framing adds transport overhead. Whole records that cannot fit remain in
the journal; `through_rx_seq` does not skip them. Inspect `next_record_bytes`
and `required_output_bytes` before resuming with a larger cap.

Read the JSON `status`: matched/timeout/cap, disconnected/reconnected,
daemon_unavailable, epoch_changed/epoch_missing, gap/capture_error/read_error,
or unsupported. Valid observations return exit0 even on an incomplete wait;
accepted immutable records and timing survive. Diagnostic truncation is explicit
(`diagnostics_truncated`, `raw_truncated`, `raw_length`, `diagnostic_count`);
replay fault details with `serial evidence` if needed.

`--follow` emits completed rows as NDJSON before the final `type:result` object;
`prefix_unknown` marks a first row that may start mid-line at the boundary.
`--include-pending` adds trailing bytes acquired after the boundary. Raw byte
arrays preserve controls and invalid UTF-8. MCP returns the follow objects
when its call finishes. Never automatically send input, reset or restart a
quiet/disconnected target; verify the task-specific response yourself.

## Serial health evidence

Read `paniolo serial status <target> [-i console]` (MCP: `serial_status`) before
and after an action. It emits JSON without opening devices or starting a daemon:
`configured` is the lab identity; `active` is the daemon identity; `process_live`
and `daemon_state` separate stopped/untracked/unreachable/reachable processes.
Each active interface reports port `connected`, capture `health.epoch`,
connection generation, last RX chunk sequence/time/age, RX/TX byte counters,
and capture-thread processing progress. Missing older-daemon fields are unknown.
Linux's active identity is taken from the opened fd; other platforms report
port name. Synthetic stream markers do not count as RX. Reconnect changes the
generation; restart changes the epoch. Capture progress is an assembly watermark,
not a disk durability acknowledgment. TX acceptance and new RX are evidence,
not proof of target command execution; read the task-specific response separately.
Never automatically restart, reset, or resend based on quiet traffic alone.

For immutable raw received chunks use `paniolo serial evidence <target>
[-i console] [--epoch E] [--since RX_SEQ] [--max-bytes N]` (MCP:
`serial_evidence`). Each JSON record preserves exact integer-array bytes,
original `rx_at_ms`, `captured_at_ms`, epoch, RX sequence and connection
generation; `observed_at_ms` belongs to the later read. The pre-send prompt
remains a separate immutable witness when later echo/completion arrives.
Synthetic stream markers are excluded. `--since` requires `--epoch`; these
are RX cursors, not the mutable pending line's sequence. Omitted epoch selects
the latest known epoch, even if quiet. Read-only: no hardware opens or sends.

The raw cap defaults to 65536 bytes (1–1048576). A whole chunk that does not
fit is omitted; `capped`/`next_record_bytes` explain why and `continuation`
stays before it. Increase the cap and resume there, never at `latest` when
capped. Check `gap`, `epoch_missing`, `capture_pending` and `diagnostics`;
retention/corruption/write failures are incomplete evidence, not silence.
JSON overhead is additionally bounded (1024 times the raw cap plus 1 MiB).
The journal holds five 1 MiB segments, appends across restarts, and preserves
invalid UTF-8 and control bytes. `health.evidence_through_seq` tracks successful
journal writes; `health.evidence_error` reports faults. Consult live health
too: an offline journal cannot establish that storage saved every RX byte.

## Configure a target

Config lives in one CLI-managed **lab file** (`~/.config/paniolo/lab.toml`, or
`--lab`/`PANIOLO_LAB`). A target's hardware is described as *channels*:

```
paniolo target add <name> [--host <labhost>] [--description <text>]
paniolo netboot set -t <name> --interface <iface> [--tftp-root <dir>] [--host-ip <ip>] [--boot-file grubaa64.efi] [--http-port 80] [--content-type <mime>]
paniolo serial add console -t <name> --device <path> [--baud 115200] [--sense cts] [--power-button] [--port N]
paniolo power set -t <name> [--cycle-cmd C] [--on-cmd C] [--off-cmd C] [--state-cmd C] [--serial-interface console]
paniolo video set -t <name> --device "<capture id or name>" [--ocr-mode text|gui] [--change-threshold N] [--port N]
paniolo hid set -t <name> --cmd "hidrig -d <uart>" [--port N]   # USB HID injection helper
paniolo usb set -t <name> --cmd "ch9329 -d <uart>"   # switchable USB media (KVM-Go microSD)
paniolo adb set -t <name> [--serial <adb-id>] [--adb <path>]  # an Android DUT over adb
```

`--port N` pins a daemon's port (video, hid, and serialcap — set on any one
serial interface) so a human's standing `ssh -L` keeps working across
restarts; leave it unset unless asked, and `--port 0` clears it.

Every channel-config command also takes `[--host <labhost>]` to bind that
channel to a remote control host (see the lab section below).

- **One subnet per link.** The default `--host-ip` (`192.168.99.1`) is for
  the *first* netboot link on a host. Every further link on that host needs
  its own /24 (`--host-ip 192.168.100.1`, `…101.1`, …) or every route to a
  target on either link is ambiguous. `netboot set` refuses a clash — a second
  link left at the default included — and `doctor` reports an existing one as
  `CONFLICT`. `doctor` also reports `MISMATCH` when the interface holds
  addresses and the configured one is not among them: the lab file says only
  where the link should be, and netbootd re-applies that address only while its
  daemon runs, so a stopped link drifts silently. Fix it with
  `paniolo netif mode link <target>` (or `netboot start`), which assigns the
  configured address and drops the others — not by editing the lab file to
  match what drifted. An interface holding nothing is `mode off`, not drift.
  Check with `paniolo target show <name>`: the host IP is always
  printed, marked `(default)` when unset. Never record the intended subnet in a
  lab-file comment alone; the field is what runs.
- `paniolo netboot devices` lists candidate USB-Ethernet interfaces (the
  primary NIC is excluded); `paniolo discover` (`--json` for machines) lists
  all lab-relevant hardware;
  `paniolo configure <name> -H <host>` proposes a whole block to paste in.
- Hosts: `paniolo host add <name> --ssh <dest>` (plus `--description`,
  `--hostname`, `--identity`, `--paniolo-cmd`, `--control-path`) manages the `[hosts.*]`
  entries from the CLI; `host list` / `host show <name>` / `host set` /
  `host rm` round it out.
- Inspect: `paniolo config show` (whole lab; `config path` prints the file,
  `config edit` opens it) / `paniolo target list` / `paniolo target show
  <name>`; `paniolo target set <name>` updates its `--host`/`--description`.
- Remove: `paniolo target rm <name>`, or per channel (`netboot rm`,
  `serial rm <iface> -t <name>`, `power rm`, `video rm`, `hid rm`, `usb rm`,
  `adb rm`).
- Rename: `paniolo target rename <old> <new>` carries all channels (and your
  lab-file comments) to the new name. Config-only — running daemons keep the
  old name, so `stop` and re-`watch`/`serve` them under the new one.
- `paniolo doctor` probes every configured channel against reality (devices
  exist, netboot interfaces hold the address they are configured for, over SSH
  for remote hosts); `doctor [target]` checks one target,
  `doctor --host <name>` only channels on that host.

## Netboot (DHCP + TFTP + HTTP)

Boot a board over the direct USB-Ethernet link:

```
paniolo netboot start [target]            # serve DHCP + TFTP + HTTP (netbootd)
paniolo netboot tftp-root [target]        # print where to drop boot files
paniolo netboot status [target]
paniolo netboot logs -f [target]          # follow the combined log (-n N for last N lines)
paniolo netboot stop [target]
```

**Naming the target.** Every *runtime* verb takes it either way — positionally
(`paniolo netboot stop pi5`) or as `-t/--target` (`paniolo netboot stop -t
pi5`) — and both at once is refused. Omit it when the lab has one target. The
*config* verbs (`set`, `add`, `rm`) require `-t` and do not take a positional.
`paniolo doctor [target]` is positional-only: with no target it checks them all.

An unset or empty `tftp_root` makes `tftp-root`, `start`, and `netif mode netboot`
exit 3 (`not_configured`), with target/channel/host in the JSON error.
`doctor` flags it as `MISSING` and exits 1 before probing the interface. Report
the missing configuration; these commands never fill it in or create a directory.

`start` refuses an interface that carries the system default route (a primary
NIC) — the netboot link must be a dedicated USB-Ethernet adapter — and refuses
to start a second target on an interface another target's netboot already runs
on (one netboot per interface; `netboot stop` the other first), and refuses to
put the interface in a /24 that any other interface on the host already holds
(one subnet per link; `netif mode off` the other target, or set a different
`--host-ip`). It watches the
daemon for ~2 s before reporting success: if netbootd exits during startup (a
port in use, an interface it cannot pin), `start` fails and quotes the last
lines of the log — read them, do not just retry. Netboot is served by the
single-binary `netbootd` (Rust), which listens only on the netboot interface. On
macOS its BPF send path uses the setuid `netbootd-bpf-helper` installed by
`paniolo setup`. If only the HTTP port is taken, netbootd keeps serving DHCP +
TFTP and logs a warning; HTTP Boot is then unavailable until `--http-port` is
changed.

Put boot files in the target's TFTP root (for a Raspberry Pi 5, the kernel goes
in as `kernel_2712.img`). Needs passwordless `sudo` for `ifconfig` (it assigns
the interface's static IP).

**UEFI clients (PXE / HTTP Boot over IPv4).** netbootd reads the client's DHCP
vendor class and serves the matching path from one config: a `PXEClient` gets
the bootfile over TFTP (with a `PXEClient` echo); an `HTTPClient` gets an
`http://…/<boot_file>` URL and the file over HTTP. Set `--boot-file` to the UEFI
NBP (e.g. `grubaa64.efi`, `ipxe.efi`). **HTTP Boot is preferred** for UEFI — it
runs over kernel TCP (robust under load) and skips the macOS BPF/ARP machinery
the silent Pi bootloader needs. IPv4 + plain HTTP only (no IPv6/HTTPS yet).

**One netboot client at a time, by design.** netbootd locks onto the MAC of
the first device that DHCPs on the link for as long as it keeps running; a
*second* device plugged into the same netboot link is silently ignored (no
DHCP reply at all — check `netboot logs` for the rate-limited warning if a
board that should be netbooting stays dark). There is no way to switch which
device it answers short of `paniolo netboot stop` then `start` again — that
restart is the "release the lock" operation. TFTP separately only answers the
one leased IP and caps itself at a handful of concurrent transfers, which is
invisible in normal use (a boot flow never needs more than that) but means a
misbehaving client flooding RRQs gets dropped rather than served.

## Link mode — netboot · link · ffx · off

`paniolo netif` owns the host side of the USB-Ethernet link and puts it in one of
four mutually-exclusive modes. Use it to flip between them atomically instead of
configuring the interface by hand:

```
paniolo netif mode netboot [target]        # IPv4 + DHCP + TFTP (= netboot start)
paniolo netif mode link [target]           # bare link UP: host IP only, no daemon
paniolo netif mode ffx [target]            # stop netboot, add host fe80::1/64 for ffx
paniolo netif mode off [target]            # soft DOWN: release host IP (+ ffx LL)
paniolo netif down-hard [target]           # hard DOWN: also kill WoL + admin-down the iface
paniolo netif status [target]              # mode, carrier, addresses, ffx peer
```


**Testing the link up/down.** To check that the link itself comes up and drops —
without serving anything — toggle `link` ↔ `off` and read `netif status`:

```
paniolo netif mode link [target]    # up:   host IP assigned, no DHCP/TFTP
paniolo netif status [target]       #       mode=link, carrier up
paniolo netif mode off [target]     # down: host IP released
paniolo netif status [target]       #       mode=off
```

Caveat — **soft "down" (`mode off`) only releases the host IP; it does not force
the carrier down.** `netif status` shows `carrier` separately from `mode` because
a NIC with **Wake-on-LAN** enabled keeps the PHY energized even when the
interface is down, so the peer's link LED stays lit and `carrier` can still read
`up` in `off` mode. **When you need the target to actually *detect* link loss,
use `paniolo netif down-hard`** — it does `mode off` and then disables WoL
(`ethtool -s <iface> wol d` on Linux) and admin-downs the interface, so the peer
sees carrier go away. Bring the link back with `netif mode link` (or `netboot`);
WoL stays off until you re-enable it (`ethtool -s <iface> wol g`) or replug the
adapter. (macOS WoL is a system-wide pref, so there `down-hard` relies on the
admin-down; unplugging the cable is always the unambiguous drop.)

- **Switching to ffx stops netboot first** — otherwise a power-cycle TFTP-boots a
  stale image instead of falling through to the SD card. This is the safe way to
  hand the link off to `ffx`.
- `mode ffx` adds the host-side `fe80::1/64` that `ffx` needs (nothing else sets
  it up, and it's lost on a control-host reboot). Re-run `mode ffx` any time to
  re-add it — every mode is idempotent.
- `netif status` in ffx mode reads the IPv6 neighbor table and prints a
  ready-to-paste `ffx target add fe80::…%<iface>` for the discovered device. If
  no peer shows, power-cycle the target and wait for it to finish SLAAC.

Typical ffx hand-off: `paniolo netif mode ffx fortune` → `paniolo power-cycle
fortune` → `paniolo netif status fortune` (grab the address) → `ffx target list`.
Go back to TFTP bring-up with `paniolo netif mode netboot fortune`. Same
passwordless `sudo` requirement as netboot (`ip` on Linux, `ifconfig` on macOS).

## Video — capture, preview, OCR

```
paniolo video devices                 # list capture devices (with stable ids)
paniolo video set -t <target> --device "<id-or-name>" [--ocr-mode text|gui] [--change-threshold N] [--port N]  # configure the video channel
                                           #   --ocr-mode gui is a Linux-only accuracy switch
                                           #   for GUI screens (unset = platform default)
                                           #   --device rfb+hid: = the RFB video feed of this target's own
                                           #   hid daemon (a JetKVM; set the hid channel first), or
                                           #   rfb://HOST:PORT for a plain network RFB source (no password)
paniolo video watch [target] [--restart]   # start the capture daemon (background);
                                           #   --restart force-restarts a stalled one
paniolo video preview [--open]        # print the daemon's dashboard URL (--open: hand it to a browser instead)
paniolo video shot [target] [--stable] [--out frame.png]   # one lossless PNG
paniolo video shot --changed-since <hex-hash> --timeout <ms>   # block until the
                                           #   frame differs from a previous shot's hash
paniolo video read [target] [--stable]   # OCR the current screen, print text
paniolo video show [target]           # device + daemon status
paniolo video stop [target]           # stop the daemon (on the target's host)
```

- The **dashboard** (the video daemon's URL — ports are OS-assigned, and the
  URL `video watch` and `video preview` print carries the daemon's `?token=`;
  open exactly that URL) shows live video on top (a noVNC view
  you can type and click in when the target has a `hid` channel; MJPEG toggle), a
  serial terminal below, an **OCR button** that reads the current screen, and —
  when the target has a `hid` channel — keyboard and pointer input (click into the
  video; the **⌨ Capture input** button exists only in MJPEG mode; see HID injection below).
- **The token in that URL is a live credential — keep it out of your
  transcript.** Every command prints the token-free
  `http://127.0.0.1:<port>`; `paniolo video preview` is the only one that
  prints the openable form, and it exists for a human about to paste it into a
  browser. Never paste a `?token=` URL into an issue, a commit message, or a
  chat.
- **Opening a browser is a human's job, not yours.** `paniolo video preview
  --open` only works on a machine with a desktop session, and it is refused
  outright when the video channel is on another host. On a headless control
  host — where you usually are — it writes the URL to a `0600` file and prints
  that path instead of opening anything, so it is not a way to get a browser
  open. Hand the human `paniolo console <target>` and let them run it.
- The `device` may be a **stable id** (preferred — `video devices` prints
  `id=…`: the AVFoundation uniqueID on macOS, the `/dev/v4l/by-path` symlink on
  Linux), a name substring, or a `/dev/video*` path. Ids are derived from USB
  port topology, so they survive reboots and tell two identical dongles apart;
  replugging into a different port changes the id. A name substring matching
  more than one device is an error (listing the candidates' ids), not a silent
  first-match guess.
- `--stable` waits for a steady frame before capturing (useful right after a mode
  switch or reboot). `video shot` prints `signal=… hash=…` on stderr — feed that
  hash to `--changed-since` to wait efficiently for the screen to change.
- `shot --out <path>` always writes on the **invoking machine**, even when the
  video channel lives on a remote control host (the PNG streams back over SSH;
  no copy step). `video devices` hides SoC-internal video nodes (Pi pipeline
  stages, codecs) — pass `--all` through `paniolo helper hdmicap devices --all`
  if you truly need them.
- **OCR** (`video read`, which wraps the daemon's `GET /ocr`; also the
  dashboard button) is on-device (Apple Vision on macOS, Tesseract on Linux).
  It reads large boot-screen / BIOS text well; very small console fonts can
  produce a few character confusions (e.g. `1`/`l`, `2`/`Z`). With **no video
  signal** (target display off/unplugged) `read` errors with `no video signal`
  rather than returning empty text — wake the display (a `hid` keypress) and
  retry.

## Serial console

A target can have **several named serial interfaces** (e.g. a main `console` and
a `bmc`). Each port is **exclusive** — only one consumer can hold it at a time —
but a single `watch` daemon **per target** owns *all* of that target's interfaces
at once. (Each target gets its own daemon, so several targets capture concurrently
on one host.)

```
paniolo serial add console -t <target> --device <path>   # add a named interface
paniolo serial add bmc -t <target> --device /dev/ttyUSB1 --baud 9600
paniolo serial set console -t <target> --sense cts        # update fields
paniolo serial rm <name> -t <target>                      # drop a named interface
paniolo serial connect [target] [-i name]      # interactive terminal (tio) in your shell
paniolo serial watch [target]                  # run the daemon for ALL interfaces;
                                               #   they appear in the dashboard pane
paniolo serial send [target] "text"            # send one line of input through
                                               #   the running daemon (see below)
paniolo serial log [target] [-i name] [options] # print captured output (timestamped)
paniolo serial evidence [target] [-i name] [--epoch E] [--since N] [--max-bytes N]
paniolo serial show [target]                   # list interfaces + daemon status
paniolo serial stop [target]                   # release the ports (on the target's host)
paniolo serial devices                         # list serial devices on the host
paniolo serial dtr [target] [-i name] [--ms N] # pulse DTR line (J2 power button header)
paniolo serial reset [target] [-i name]        # soft reset via brief DTR pulse
```

**Argument convention:** every runtime command takes the target as an
optional positional (omit it when the lab has one target); channel-config
commands (`add`/`set`/`rm`) take `-t`. `serial send`/`serial log` accept
`-t` too — `serial send` reads two positionals as `<target> <text>`, one as
just the text. The `-t`-only runtime commands are `hid send`, `adb run`, and
`adb input`, whose positional tails belong to the helper / to `adb`.

The interface name is a required positional on `add`/`set`/`rm` (`console` is
just the conventional name for the main one). With
one interface, `-i`/`--interface` can be omitted everywhere. Don't run `connect`
and `watch` (or an external `screen`/`tio`) on the **same device** at once —
start one, or `stop`/close the other first.

### Updating an older serialcap, hdmicap, ch9329, hidrig, or zigplug daemon

If `serial stop`, `video stop`, or `hid stop` reports that authenticated
shutdown is unavailable, use `paniolo daemons stop <name>` (`serialcap`,
`hdmicap`, or `hid`) and start capture again. A standalone `zigplug stop`
that cannot make the daemon exit reports the same thing — use `paniolo
daemons stop zigplug`. New `serialcap stop`, `hdmicap stop`, `ch9329 stop`,
`hidrig stop`, and `zigplug stop` all use the daemon token and never signal a
discovery-file PID.

**A VM's console is a serial interface.** A pty path (`/dev/ttysNNN` on macOS,
`/dev/pts/N` on Linux — `utmctl attach <vm>` prints it, qemu's `-serial pty`
reports it at startup) can be added like any device, which is how you watch a
guest's firmware or unattended installer before SSH exists. `--baud` is recorded
but not applied — a pty has no line rate. Both `watch` and `connect` work
against it. The pty is bidirectional and firmware reads it as console input, so
`serial send` answers a bootloader prompt headlessly (loop it — boot timing
varies by tens of seconds). That window is firmware-only and shuts early: on a
Windows guest ConIn stops being read when WinPE starts, before Setup runs. The pty number is reassigned on each VM boot, so re-point the interface
after a restart.

### Reading captured output

While `watch` is running, the daemon keeps a **rolling, timestamped capture log**
per interface (persisted on disk, so it survives a daemon restart and you can read
it even after `stop`). The live view stays in the dashboard; use `serial log` when
you want to *read back* what scrolled past:

```
paniolo serial log                    # most recent ~200 lines (sole interface)
paniolo serial log -i bmc --tail 50   # most recent 50 lines of the 'bmc' interface
paniolo serial log --since 1840       # only lines after sequence #1840 (polling)
paniolo serial log --from 1800 --to 1860   # a specific line range
paniolo serial log --json             # JSON Lines (seq, ts_ms, text) for parsing
paniolo serial log --raw              # keep ANSI colors / control bytes
paniolo serial log --no-pending       # omit the current unterminated line
```

With more than one interface configured, pass `-i <name>` to choose one (omitting
it errors and lists the names); with more than one target, name it
(`serial log pi5 …` or `-t pi5`). Each line is shown as `[<UTC timestamp>] #<seq>
<text>`. The `seq` is a stable, monotonic line number — note it, then come back
later with `--since <seq>` to get only what's new, or `--from/--to` to re-read an
exact span. Output is ANSI-stripped by default; a `*` after the sequence number
marks the current unterminated line (e.g. a `login:` prompt with no newline yet).

### Sending input

`paniolo serial send` injects one line through the **running daemon** (start
`watch` first), so input coexists with capture — what you send and what the
target echoes both land in the log:

```
paniolo serial send <target> "reboot"                # CR appended by default
paniolo serial send "reboot"                         # sole-target lab: text only
paniolo serial send --no-newline "partial input"     # suppress the CR
paniolo serial send --pace-ms 8 "long command"       # per-byte pacing for slow
                                                     #   polled consoles
```

Note the input only reaches the target if its console actually reads the UART
(a kernel with a broken serial driver logs output but ignores input).

## adb (Android targets)

When the target is an Android device, the `adb` channel gives you console,
screen, and input over one transport (see `docs/adb.md`). adb runs
on the control host the device is plugged into; paniolo routes per-channel like
any other.

```
paniolo adb devices [-H <host>]          # list attached devices (their serials)
paniolo adb set -t <target> [--serial <id>]   # bind a device (omit --serial = sole device)
paniolo adb show [target]                # config + live device state
paniolo adb shell [target]               # interactive `adb shell` in your terminal
paniolo adb run -t <target> <cmd...>     # one-shot command, output captured (agent path)
paniolo adb screencap [target] [-o frame.png]   # one PNG (exec-out screencap; -o - = stdout)
paniolo adb input -t <target> <args...>  # adb shell input: keyevent/text/tap/swipe
```

- `run`/`input` take `-t` (not a positional) because their tail is free-form;
  put `-t` first and use `--` before a leading-dash argument
  (`paniolo adb run -t pixel -- logcat -d -t 50`).
- `adb run -t <t> getprop ro.product.model` is the read workhorse for agents;
  `-o <path>` on `screencap` always means *this* machine's filesystem — it
  works the same whether the adb channel is local or on a remote control host
  (like `video shot`), so `-o frame.png` and `-o -` (stdout) are equally safe
  to use against a remote target.
- Reboot/power: no adb-specific command — wire `adb reboot` through the power
  hooks (`paniolo power set -t <t> --cycle-cmd "adb -s <id> reboot"`).
- The device must be authorized (`adb devices` shows it as `device`, not
  `unauthorized`/`offline`); paniolo does not manage pairing.

## Power control

```
paniolo power on  [target]             # run on_cmd hook (error with hint if unset)
paniolo power off [target]             # run off_cmd hook (error with hint if unset)
paniolo power-cycle [target]           # run cycle_cmd hook
paniolo power-state [target]           # state_cmd stdout ("on"/"off") or serial sense-line
paniolo serial dtr [target] [--ms N]   # pulse DTR line on J2 header (opt-in; see below)
paniolo serial reset [target]          # soft reset via brief DTR pulse (opt-in)
```

Configure hooks with `paniolo power set`:

```
paniolo power set -t <name> \
    [--cycle-cmd <cmd>] [--on-cmd <cmd>] [--off-cmd <cmd>] [--state-cmd <cmd>] \
    [--serial-interface console]   # default DTR interface when several opt in
```

All four hooks are optional and run via `sh -c`. `power-state` uses `state_cmd`
if set (first whitespace token of stdout must be `on` or `off`); falls back to
the serial sense-line otherwise.

**"Reboot over serial" ≠ DTR reset — pick the right one.** Two unrelated things
share the word "serial/reset":

- **Console reboot (software):** type `reboot` into a logged-in console with
  `paniolo serial send <target> "reboot"`. This is what "use serial to reboot"
  almost always means. If you can't log in, use `paniolo power-cycle <target>`.
- **DTR reset (hardware):** `paniolo serial dtr` / `serial reset` toggle the
  FTDI DTR line wired to the board's J2 power button. A ≤500 ms pulse is a soft
  button event; ≥3000 ms is a hard PMIC power-off.

DTR is **opt-in per interface** — it works only where the interface declares
`power_button = true` (`paniolo serial set <iface> -t <name> --power-button`),
because DTR-to-J2 wiring is rare. On a target that hasn't opted in, `serial dtr`
/ `serial reset` **error** with a hint (they never toggle a lone console blindly).
So: unless DTR wiring is explicitly declared, reboot via the console `reboot` or
`power-cycle` — do **not** assume `serial reset` power-cycles the board.

### Cambrionix hub (example)

The `cambrionix` helper binary drives a Cambrionix USB hub's control UART and
satisfies the `state_cmd` contract (`state <port>` prints `on` or `off`):

```
paniolo power set -t pi5 \
    --cycle-cmd "cambrionix -d /dev/cu.usbserial-AA00BB11 cycle 4" \
    --on-cmd    "cambrionix -d /dev/cu.usbserial-AA00BB11 on 4" \
    --off-cmd   "cambrionix -d /dev/cu.usbserial-AA00BB11 off 4" \
    --state-cmd "cambrionix -d /dev/cu.usbserial-AA00BB11 state 4"
```

See `docs/power.md` for the full `cambrionix` command surface.

### Intel AMT / vPro (example)

The `amt` helper switches vPro machines over the network via their Management
Engine — no plug hardware — and gives **true power-state readback** (the ME
answers with the host on, off, or bare-metal):

```
paniolo power set -t optiplex \
    --cycle-cmd "amt cycle -d 10.0.0.5 -u admin --delay-ms 5000" \
    --on-cmd    "amt on -d 10.0.0.5 -u admin" \
    --off-cmd   "amt off -d 10.0.0.5 -u admin" \
    --state-cmd "amt state -d 10.0.0.5 -u admin"
```

**The AMT password is not in the lab file.** The helper takes it from the
first set of: the `AMT_PASSWORD` environment variable, `--password-file
<path>`, or `--password-command <cmd>` (whose stdout is the password). A hook
that carries its own source needs nothing in the environment, e.g.
`--state-cmd "amt state -d 10.0.0.5 --password-command 'op read op://lab/amt/password'"`;
look at the existing hooks before composing a fetch by hand. Otherwise run
`paniolo power …` with `AMT_PASSWORD` set, e.g.
`op run --env-file .env -- bash -c 'paniolo power-cycle optiplex'` (single
quotes — the parent shell must not expand it). A file path or command in a
hook is resolved on the host where the hook runs (the control host for a
remote power channel); `AMT_PASSWORD` set where *you* run the command is
carried to a remote control host by paniolo's own dispatch, over the remote
command's stdin, never its argv, and wins over the hook's file or command.
See "Setting up the credential source" in `docs/power.md` for the patterns.

## Switchable USB media — hand a card between host and target

Some KVM devices route one physical USB device to either side, never both.
The `usb` channel drives that mux; today only the **Openterface KVM-Go**'s
onboard microSD is supported, through the same `ch9329` helper and the same
serial port as its `hid` channel (setting both channels to the same `--cmd`
is normal).

```
paniolo usb state         -t <name>   # -> host | target
paniolo usb attach-host   -t <name>   # route the card to the control host
paniolo usb attach-target -t <name>   # hand it to the target
```

The use case is hands-free *physical* boot media: image the card host-side,
attach it to the target, and firmware sees a real mass-storage device — which
it generally will not for streamed virtual media.

Four rules, all of which will bite you otherwise:

- **Unmount before switching.** The switch physically detaches the device from
  whichever side has it; a mounted filesystem there is a surprise removal.
  paniolo cannot see mount state and will not stop you. On macOS use
  `diskutil unmount force` — plain `unmount` usually loses to Spotlight.
- **Exit 0 means the mux moved, not that the media is ready.** The USB device
  still has to enumerate on the receiving side, which takes seconds. Poll for
  the block device.
- **Always `usb state`; never assume.** The position resets to the host side
  when the KVM loses power, and a physical button can move it too.
- **A device with no mux does not answer at all** — there is no "unsupported"
  reply in the protocol — so it surfaces as a timeout, which the helper
  reports as unsupported rather than broken.

Full guide: `docs/usb.md`. Protocol: `notes/openterface-usb-mux-spec.md`.

## HID injection — type and click into the target

Three bundled injector helpers speak the same command vocabulary; the `hid`
channel stores an opaque helper command and `paniolo hid send` appends its
arguments to it and runs it on the channel's host:

- **`hidrig`** — the dual-board KB2040 "dumb pipe": the host-side CLI
  composes HID reports and writes binary frames to the **control** board's
  USB-CDC port, which relays them over I2C1 to the **target** board that
  presents a USB keyboard + mouse to the DUT.
- **`ch9329`** — the client for CH9329-based KVM devices (Openterface
  Mini-KVM and KVM-Go, Sipeed NanoKVM-USB): `paniolo hid set -t <name> --cmd
  "ch9329 -d <uart>"`. Same commands; it autodetects the link baud (115200 /
  57600 / 9600) and adds `info` (target USB enumeration + lock LEDs).
  On Linux, failed connections to `1a86:fe0c` include a startup compatibility
  warning. Do not interpret `chip_version` as a firmware release or flash an
  image based on VID/PID alone. A captured Mini-KVM failure involved tty echo
  of unsolicited startup bytes; another tty open after a cold cycle can repeat
  it. The direct-USB drain workaround requires explicit device selection,
  stopping port users, and restoring drivers: follow `docs/hid.md` →
  "Openterface CDC on Linux" and `notes/openterface-v2-linux-serial.md`.
- **`jetkvm`** — the JetKVM network KVM (Go helper): `paniolo hid set -t <name>
  --cmd "jetkvm -d <host>"`; the password comes from `JETKVM_PASSWORD`,
  `--password-file` or `--password-command`, never a flag. Same commands; its
  `info` prints the device's USB, video and keyboard-LED state; its daemon also serves the screen (`video set --device rfb+hid:`). **The JetKVM
  allows ONE session**: the helper's session and a browser tab on the
  device's web UI kick each other, and `OK` does not prove delivery (the
  firmware says success even if the target's USB is not up — check `info`).

```
paniolo hid set -t <name> --cmd "hidrig -d /dev/cu.usbmodemXXXX" [--host <labhost>]
paniolo hid send -t <name> type hello world   # type a string
paniolo hid send -t <name> key ENTER          # tap a key (adafruit_hid Keycode names)
paniolo hid send -t <name> combo LEFT_CONTROL C
paniolo hid send -t <name> move 300 -50       # relative mouse move (negatives OK; keep -t first)
paniolo hid send -t <name> click left | scroll -3 | releaseall
paniolo hid send -t <name> ping               # injector liveness check
```

The same dual-board control board can also back this target's **serial** and
**power** channels: when `hidrig serve` runs it bridges the DUT's serial console
and re-exports it as a PTY (the hid daemon is per-target, so point a `serial`
channel's `device =` at `/tmp/paniolo-<uid>/hid/<target>/console`), and `hidrig
power off|on|cycle` switches DUT
power via a relay (behind the `power` hook) — one USB device for HID, console,
and power. (The relay/power path is hardware-verified, incl. state persistence
across a control-board reset; the console bridge is new and not yet verified.)

Absolute mouse (click-where-you-point): `paniolo hid send -t <name> moveabs
<x> <y>` positions the cursor in a 0..32767 logical space the OS maps across
the screen (the KB2040 firmware advertises the `moveabs` capability).

Sequences: `hidrig run <file>` runs a command file (one command per line,
`# comments`, `delay <ms>` / `sleep <seconds>`) — the file must be on the
channel's host. The target board is powered by the DUT's USB port, so its HID
goes silent while the DUT is off and reboots with it; the control board is
host-powered and independent. Command vocabulary:
`docs/dev/hid-serial-protocol.md`; dual-board design + frame format:
`docs/dev/hid-dual-board-design.md`.

**KVM in the console.** `paniolo console <name>` shows the video as a noVNC view; when the target
has a `hid` channel, click into it to drive the target with your own keyboard + mouse (absolute —
the cursor follows where you point; F13-F24 and exotic keysyms are dropped). In the MJPEG view
(toggle) a **⌨ Capture input** button does the same. `video watch`/`console` start the hid daemon
for any video device, so it owns the injector from then on. It auto-starts the hid daemon (`hidrig serve`), which owns the control
link and re-exposes the command vocabulary over a WebSocket; `paniolo hid send` injections intermix
with what you type in the browser. Manual daemon control: `paniolo hid
serve [target]` / `paniolo hid stop [target]` (positional target — only
`hid send` and `hid set/rm` use `-t`). When the target also has a `power` channel, the overlay
adds an on/off toggle switch + a separate cycle button (each confirms first).

For *operating a GUI* through the `video` + `hid` channels — clicking, typing,
and navigating a desktop/app/installer/BIOS you can only see over the capture
and only touch through emulated HID — read the companion **`kvm-puppeting`**
skill (the look-act-settle-verify discipline): `paniolo skill kvm-puppeting`.

## Companion skills

Paniolo ships its own agent skills; list them and read any one from the CLI,
even on a packaged install:

```
paniolo skill                  # list bundled skills with their descriptions
paniolo skill kvm-puppeting    # print a skill's SKILL.md (e.g. GUI puppeting)
paniolo skill paniolo --path   # the file path, to open or Read it
```

## MCP tools (experimental)

If your harness has a `paniolo` MCP server configured (`paniolo mcp`), its
tools are the runtime verbs above: `target_list`, `video_shot`, `video_read`,
`serial_log`, `serial_send`, `hid_send` (`command` is an array of words, e.g.
`["key", "ENTER"]`), `power_state`, `power_on`, `power_off`, `power_cycle`.
`video_shot` returns the screen as an image plus `signal=…  hash=…`; pass the
hash back as `changed_since` to wait for a change, as with `video shot
--changed-since`. Configuration (`set`, `add`, `watch`) is still the CLI. A
failed tool result carries paniolo's exit code and JSON error object (see
*When a command fails* above), and a write that may or may not have run says
"Outcome unknown". Setup: `docs/mcp.md`.

## Targets on a remote control host (a "lab")

When the machine wired to the target isn't the one you're running paniolo on,
describe your **lab** in one git-tracked TOML file and point paniolo at it with
`--lab <file>` or `PANIOLO_LAB`. Each target's `host` names a control host;
commands then run **transparently on that host over SSH** — you don't ssh by hand.

```toml
# mylab.toml
[hosts.bench1]
ssh = "user@bench1.local"     # ssh destination — how others reach it ("local" = this machine)
# description = "bench Mac mini"  # optional free-text label, shown in `config show` / `host show`
# hostname = "bench1.local"      # this box's FQDN; set it so bench1 recognizes itself when ONE
#                                  shared lab file is run from any machine (matched vs `hostname -f`)
# identity = "~/.ssh/lab_key"    # set this if your ssh-agent offers many keys
#                                  (avoids "Too many authentication failures")
# paniolo_cmd = "~/.local/bin/paniolo"  # if paniolo isn't on the
#                                  host's non-interactive ssh PATH

[targets.fortune]
host = "bench1"
# description = "Pi 5 under test"  # optional free-text label (legacy key: `note`)

[targets.fortune.netboot]
interface = "enx001122334455"
tftp_root = "~/tftp/fortune"

[[targets.fortune.serial]]
name = "console"
device = "/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_AA00BB11-if00-port0"

[targets.fortune.power]
cycle_cmd = "~/scripts/power-cycle.sh"
```

```
export PANIOLO_LAB=~/labs/mylab.toml
paniolo netboot start fortune      # runs on bench1, transparently
paniolo power-cycle fortune
paniolo netboot logs -f fortune    # streams back live
paniolo serial connect fortune     # interactive tio over ssh -t
paniolo console fortune            # dashboard tunnelled to your browser; Ctrl-C to close
```

Notes:
- With **no** `--lab`/`PANIOLO_LAB`, paniolo uses the default lab at
  `~/.config/paniolo/lab.toml` (auto-created on first write).
- The lab file is **CLI-managed and hand-edit friendly**: all the config
  commands edit it surgically (your comments survive), and it stays your
  git-tracked source of truth.
- **Channels can live on different hosts** (per-channel `--host`): each command
  runs on the host of the channel it touches. Composites (`console`) need
  their channels co-located on one host.
- Still over plain SSH if you prefer: `ssh bench1 "paniolo …"` works too, but the
  lab makes location transparent.

### Authoring a lab (discovery + provisioning)

After declaring a host's connection info (`paniolo host add bench1 --ssh
user@bench1.local`, or hand-write `[hosts.bench1] ssh = …`), let
paniolo discover its hardware and propose the target block:

```
paniolo discover                                  # list THIS host's hardware
paniolo --lab mylab.toml configure fortune --host bench1
```

`configure` runs discovery on `bench1` over SSH and prints a proposed
`[targets.fortune]` block — best-guessing the USB-Ethernet interface and serial
device, listing other candidates as comments. It **writes nothing**: you review
it, paste it into the lab file, and commit (the lab is your reviewed source of
truth). Edit the guesses as needed; add `tftp_root`/`power` (not discoverable).

Provision a control host's daemons from your dev machine:

```
paniolo --lab mylab.toml setup --host bench1       # runs `paniolo setup` on bench1
```

(The host needs the paniolo CLI + source already present; this builds the Rust
daemons there over an ssh PTY.)

**If the control host does not exist yet** — a blank Raspberry Pi, or a host
being re-imaged — that is a different job, and it has its own skill:

```
paniolo skill control-host
```

It covers flashing Pi OS to an SD card (including how to identify the right
removable device before writing to it), the cloud-init seed that brings the
box up with SSH authorized and paniolo installed, and enrolling the result
here. Don't improvise a flashing procedure; read that skill.

## Background daemons — one view of everything

Paniolo runs several per-subsystem daemons (serialcap, hdmicap, the hid
injector, zigplug, netbootd). `paniolo daemons` shows all of them at once,
plus **stray helper processes** running out of the libexec dir (e.g. wedged
one-shot hooks — the thing to check when a serial-port-owning helper
misbehaves):

```
paniolo daemons                          # list: name, pid, port, detail + strays
paniolo daemons stop zigplug serialcap   # stop specific daemons (SIGTERM)
paniolo daemons stop 'serialcap[pi5]'    # one target's instance, as listed (quote: [] globs)
paniolo daemons stop --all               # stop every daemon + TERM strays
paniolo daemons stop --all --force       # …and SIGKILL whatever survives 3 s
paniolo daemons restart --stale          # restart capture daemons with proven content changes
paniolo daemons restart serialcap        # restart a named capture daemon
paniolo daemons restart --all            # restart every serialcap/hdmicap
```

netbootd is stopped via its proper teardown (interface cleanup), everything
else by signal. Per-subsystem `stop` commands still work; this is the
sweep-the-bench view.

**Stale daemons after an upgrade.** A daemon keeps the binary it started from;
`make install` / `apt install` replaces the binary on disk but not the running
process. `paniolo daemons` flags **stale** only when executable content differs
(also shown on `serial show` / `video show`). A SHA-256 checksum records content
at spawn; Linux can also read the actual running executable through `/proc`.
Identical bytes remain current after reinstalling, moving paths, or changing
modification times. Missing identity, inaccessible executable content, or
metadata changes without comparable content show **binary identity unknown**
with a reason. Do not infer a required restart from an unknown identity.
`paniolo daemons restart --stale` and automatic `serial watch` / `video watch`
restarts select only proven content changes. Named restart, `--all`, and
`video watch --restart` still intentionally restart a capture daemon, reusing
its lab channel config. netbootd is excluded — cycle it via
`paniolo netboot start/stop`.

**Untracked daemons: `show` says stopped, `watch` says already running.** A
daemon's discovery file is paniolo's only record of it, and on Linux `/tmp` is
swept on an age policy (Debian: 10 days), so a long-running daemon can lose that
file while still holding its capture device. `paniolo video show` reports it as
`running, untracked (pid N)` and `paniolo daemons` lists it under **Untracked
daemons**; `paniolo video watch` / `video stop` and `paniolo serial watch` /
`serial stop` reap it for you. Don't reach for `ps`/`kill` — and don't conclude
the device is free just because a daemon is gone from the tracked list.

## Quick reference — gotchas

- **Drive through paniolo, not around it.** Don't reconfigure or open a
  paniolo-managed device by hand (`ifconfig`/`ip`/`ethtool` on the netboot
  interface, `screen`/`tio` on a serial port, `kill` on a daemon) — it desyncs
  the daemon. Use `netif mode …`, `serial log`/`send`, `daemons stop`.
- `doctor` flags a channel that still works? Believe the working channel and
  report the discrepancy — never rewrite the lab config to silence `doctor`.
- Serial port is exclusive: one of `connect` / `watch` / external `tio`/`screen`.
- A JetKVM's screen arrives as `--device rfb+hid:` on the video channel; the
  `jetkvm` daemon decodes it with `ffmpeg` (must be installed). Video and `hid`
  share the daemon's one session, so they do not evict each other, but a
  browser on the JetKVM web UI evicts both. Its H.264 noise is below the video
  channel's change threshold (`video set --change-threshold`, default 16), so
  the hash stays put on an unchanged screen; a blinking cursor still moves it,
  on any source, so `--changed-since` alone does not mean the content you care
  about changed. Use `--stable` or look at the image.
- JetKVM allows one session at a time: if a `jetkvm` hid command or daemon
  suddenly fails or reports "session taken over", someone opened the
  device's web UI. Don't open the UI to "check" while automating — it kicks
  the helper (and the helper kicks the browser). `jetkvm info` shows whether
  the target's USB is actually up; HID `OK` alone does not.
- `~/.cargo/bin` (the CLI) must be on `PATH`, ahead of any other `paniolo`
  (e.g. a Homebrew keg from the tap can shadow it). The helper binaries are
  *not* on PATH — they live in `~/.local/libexec/paniolo/bin`
  (`paniolo helper <name> …` to run one by hand).
- Helper help needs the separator before the name:
  `paniolo helper -- serialcap --help`. Without it, `--help` shows paniolo's
  outer helper-command help.
- `paniolo console` auto-starts the video daemon, and the serial daemon when
  the target has a serial channel (a KVM-only target opens as video + input,
  with no terminal pane), if they aren't running, and hands
  the browser a URL carrying every daemon's token (`?token=`,
  `?serialws=…token=…`, `?hidws=…`); the daemons answer nothing without it.
  What `console` *prints* is the token-free address, because that line lands in
  your transcript — so do not paste it into a browser and expect the page to
  work. If no browser could be launched, `console` writes the full URL to a
  `0600` file and prints that path — never the URL itself. A daemon left over from an older paniolo has no token —
  a named `paniolo daemons restart` (unknown identity is excluded from `--stale`).
- Netboot requires passwordless `sudo` (`ip` on Linux, `ifconfig` on macOS).
- netboot and ffx are mutually exclusive on the link — use `paniolo netif mode`
  to switch; entering ffx mode stops netboot so a power-cycle boots from SD.
- To test the bare link: `paniolo netif mode link` (up) / `mode off` (soft down).
  Soft "down" only releases the host IP — with Wake-on-LAN on, `carrier` can still
  read `up` (the PHY stays powered). For a drop the target actually detects, use
  `paniolo netif down-hard` (kills WoL + admin-downs the iface), or unplug.
- Remote (lab) targets: if ssh fails with "Too many authentication failures",
  set the host's `identity` (agent key-spray); if the remote can't find paniolo,
  set its `paniolo_cmd` to an absolute path.
- OCR is strongest on large text; tiny console fonts may misread some characters.
- AMT-backed power hooks need a password source: `AMT_PASSWORD` in the
  environment of the `paniolo power …` invocation, or `--password-file` /
  `--password-command` in the hook (see the Intel AMT power example). A hook
  failing with `child_exit` 3 and "the AMT password is not set" (or a file or
  command error) means configure the secret, not a bug.

---

Licensed under the Apache License, Version 2.0.
