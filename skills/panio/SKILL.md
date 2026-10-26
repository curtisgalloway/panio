---
name: panio
description: Control a physical target machine (SBC, e.g. a Raspberry Pi) with the panio CLI during low-level bring-up — netboot it over a direct USB-Ethernet link, watch and OCR its HDMI screen, drive its serial console, power-cycle it, switch the link between netboot and ffx, and drive targets on remote control hosts transparently via a single git-tracked lab file over SSH. Use when you need to boot, observe, type into, screenshot, read the screen of, power, or remotely control a target/board through panio — including across multiple control hosts.
---

# Panio — controlling a target machine

Panio drives a physical target machine (a single-board computer such as a
Raspberry Pi) during firmware/OS bring-up. It runs on the control host that is
physically wired to the target (USB-Ethernet for netboot, an HDMI capture
dongle for video, a USB-serial adapter for the console, a smart plug for power).

Almost every command operates on a named **target**. If exactly one target is
configured, the name can be omitted.

## Drive through panio — don't touch its devices directly

Panio owns the hardware it's configured for, and a background **daemon** holds
most of it open: netbootd (the netboot interface + DHCP/TFTP), serialcap (each
serial port), hdmicap (the capture device), the hid injector. Those daemons
track state, and the netif/netboot commands assume they're the only ones
touching the link and ports. So **always go through panio commands; never
reconfigure or open a panio-managed device by hand**:

- **Don't** run `ifconfig` / `ip` / `networksetup` / `ethtool` on the netboot
  interface — use `panio netif mode …` (and `panio netboot start/stop`). A
  stray address or an admin-down behind panio's back desyncs what `netif
  status` reports and can silently break DHCP/TFTP.
- **Don't** open a serial port with `screen` / `tio` / `minicom` while a
  `serial watch` daemon holds it (the port is exclusive) — read with `panio
  serial log` and write with `panio serial send`.
- **Don't** `kill` a daemon or its helper by PID — use `panio daemons stop`
  (or the per-subsystem `stop`).
- **Don't** rewrite the lab config to "fix" a mismatch you didn't create —
  neither by editing the lab file nor via `… set`/`… rm` config commands. The
  lab file is the user's reviewed source of truth, and `panio doctor` is a
  hint, not ground truth: when doctor flags a channel, **try the channel
  first** (`hid send … ping`, `serial log`, `video shot`). If it works, the
  config is right — report the doctor discrepancy to the user instead of
  "repairing" it.
- **Don't** reach around a remote control host. A remote target is driven by
  the same panio commands from where you are (the lab file routes them over
  SSH) — don't ssh over to run helper binaries against its devices, curl a
  daemon's local port, or read `/dev/video*`/serial devices directly; the
  daemons there hold the same exclusive locks.

If a panio command for what you want doesn't seem to exist, run `panio
--help` or re-read this skill before reaching for the raw device — it almost
certainly does (e.g. `netif mode link`/`off` to toggle the bare link, `daemons`
to find and clear a stuck port).

## When a command fails — read the exit status

The exit status names the kind of failure; branch on it, not on the message
text. `panio --help` prints the table.

| Code | Kind | What to do |
|---|---|---|
| 1 | — | Only `doctor`: it found problems (not an error) |
| 2 | `usage` | Fix the command line (`<cmd> --help`) |
| 3 | `not_configured` | Wrong target/channel/interface name, required configuration missing, or a hook/helper missing. Don't retry; check `panio config show`, and don't edit the lab to fix it — report it |
| 4 | `unreachable` | The control host did not answer over SSH; the command may or may not have run |
| 22 | `timeout` | Outcome unknown: check state (`power-state`, `serial log`, `video shot`) before repeating a power cycle or other mutation |
| 23 | `target_leased` | The target is leased to someone else, or the lab requires a lease and none is held; the message names the holder and when it ends. Wait (`panio lease acquire --wait`) and retry |
| 24 | `target_cleanup_blocked` | The last lease's cleanup failed (or its record is unreadable), so the target is refused to everyone, `--preempt` too. Do not retry or work around it. `panio lease status <target>` shows the cleanup id and failed steps; `panio lease cleanup <target> --id <id>` retries, `panio lease override-cleanup <target> --id <id> --reason "..."` clears it unverified (ask the user first) |
| 100 | `daemon_down` | Start the daemon (`serial watch`, `video watch`); `panio daemons` lists what is running |
| 101 | `helper_failed` | The hook/helper/daemon ran and failed; its message (and its own code in `child_exit`) says why. On Windows a *missing* hook also lands here (`cmd.exe` exits 1 for it) |
| 109 | `internal` | Unclassified; read the message |

Piping into a reader that quits early (`| head`, `| grep -q`) exits 0; before
0.9.1 it exited 101 with a "failed printing to stdout" panic, which is not a
helper failure.

For a machine-readable reason, set `PANIO_JSON_ERRORS=1` (or put
`--json-errors` right after `panio`, before any trailing arguments: after
`hid send …` or `adb run …` it is taken as part of the payload): the **last line of stderr** is then one JSON object,
`{"error": {"kind", "code", "category", "message", "target", "channel",
"host", "child_exit"}}`. `category` groups the kinds by where to look:
`infrastructure` (3, 4, 100: fix the setup, the target was not reached),
`transient` (22: a retry may work, but check state first; 23: retry once the lease ends), `device_state`
(101: the target or its hardware refused; 24: a lease's cleanup failed), or null (2, 109). Exit codes do not
change with it; a control host older than 0.13 omits the field. Passthroughs (`helper`, `config edit`, `video shot`/`devices`,
`adb run`/`input`/`devices`, `setup --host`, and the consoles `serial connect`
and `adb shell`) exit with their program's own status instead. `serial log` still reads a stopped daemon's file, with a
`warning:` on stderr; add `--require-live` to get exit 100 instead of stale
output. A control host on panio 0.4.x or older exits 1 for every error and
rejects `--json-errors`.

## First-time setup

```
cd ~/src/panio
make install               # cargo-installs the `panio` CLI, then `panio setup`
                           # builds/installs the helper binaries (hdmicap, serialcap,
                           # netbootd, cambrionix, dsdrelay, dsdble, phidget, usbtoggle, hidrig, ch9329, shellyplug,
                           # amt), the OCR helper, zigplug, and the bundled skills;
                           # on macOS also installs netbootd-bpf-helper
                           # setuid-root (one sudo)
```

Run once per machine; re-run after pulling or editing (it's a full rebuild).
Only the `panio` CLI lands in `~/.cargo/bin` — make sure that's on `PATH`.
The helpers (hdmicap, serialcap, netbootd, cambrionix, dsdrelay, dsdble, phidget, usbtoggle, hidrig, ch9329,
shellyplug, amt, jetkvm (needs Go), zigplug, the
OCR tool) live in the private libexec dir `~/.local/libexec/panio/bin`,
off PATH; panio resolves them itself, and `panio helper [NAME] [ARGS…]`
lists or runs one directly. If a different `panio` shadows the CLI on PATH
(e.g. a Homebrew keg from the tap winning over `~/.cargo/bin`), put
`~/.cargo/bin` first or remove the other copy; `make install` warns when it
detects a shadow.

The OCR helpers also accept `--help` and `-h` without an image:
`panio helper visionocr --help` on macOS or `panio helper winocr --help`
on Windows lists their input and output options.

Discover installed helpers with `panio helper`. Everything after the
helper name goes to the helper, so `panio helper serialcap --help` is the
helper's own help (`panio helper --help` is panio's). Listing helpers and
reading a bundled helper's help do not open hardware or start a daemon; use
these to discover its commands before invoking one.

A helper runs **locally** unless told otherwise, and the hardware is usually
on a control host. Put the placement flags before the helper name:
`panio helper -t <target> --channel power cambrionix -d <dev> state` runs
on the host the lab binds that channel to (`-i <iface>` for a serial
interface; `-t` alone when the target has one host), and
`panio helper --host <labhost> zigplug -d <dev> list` names a host
directly. Prefer these over `ssh <host> panio helper …`.

A lab can add its own helpers and skills through **plugin roots**: prefixes in
`PANIO_PLUGIN_PATH` (`:`-separated; a control host gets its lab
`plugin_path`) holding `libexec/panio/bin/<helper>` and
`share/panio/skills/<name>/SKILL.md`. In `panio helper` and `panio
skill` output a plugin entry reads `<name><TAB>plugin: <root>`; take the
first field as the name. Hooks can name a plugin helper bare. A plugin never
replaces a bundled helper or skill of the same name.

## Bounded fresh serial acquisition

Use `panio serial wait <target> [-i console] --epoch E --since N` (MCP:
`serial_wait`) with `health.epoch` and `health.last_rx_seq` saved before input.
These are RX chunk counters, not line-log sequence numbers. A healthy journal
that lags the saved live cursor is allowed to catch up within the wait deadline;
expiration is `timeout`, not `gap` (also for `serial transact`). Real journal
faults and cursors beyond live RX remain explicit failures. Optional
`--pattern TEXT` matches literal UTF-8 across fresh raw chunks. Omit it to
collect until the deadline/cap; a match never establishes task success.
The interface's lab **fault rules** (`panio serial fault set -t T [-i I] NAME
PATTERN`, `... fault rm`) apply to every wait: literal patterns for output that
means the run is lost (`Kernel panic`, `Oops:`). The earliest hit among
`--pattern` and the rules wins (tie: `--pattern`, then the rules); a rule hit
ends the wait with `status: fault`, `fault: {name, pattern, offset}`. Every
result lists the checked `fault_rules`. `serial transact` ignores them.

The wait is read-only and finite: `--timeout-ms` defaults to 30000 (0 is one
immediate observation, max 3600000). `--max-bytes` caps raw evidence at
1..1048576 (default 65536); `--max-output-bytes` caps total JSON stdout,
including all follow events and final result, at 4096..8388608 (default 8388608).
MCP framing adds transport overhead. Whole records that cannot fit remain in
the journal; `through_rx_seq` does not skip them. Inspect `next_record_bytes`
and `required_output_bytes` before resuming with a larger cap.

Read the JSON `status`: matched/fault/timeout/cap, disconnected/reconnected,
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

Read `panio serial status <target> [-i console]` (MCP: `serial_status`) before
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

For immutable raw received chunks use `panio serial evidence <target>
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

Config lives in one CLI-managed **lab file** (`~/.config/panio/lab.toml`, or
`--lab`/`PANIO_LAB`). A target's hardware is described as *channels*:

```
panio target add <name> [--host <labhost>] [--description <text>]
panio netboot set -t <name> --interface <iface> [--tftp-root <dir>] [--host-ip <ip>] [--boot-file grubaa64.efi] [--http-port 80] [--content-type <mime>] [--link-down-cmd <cmd>] [--link-up-cmd <cmd>]
panio serial add console -t <name> --device <path> [--baud 115200] [--sense cts] [--power-button] [--port N]
panio power set -t <name> [--cycle-cmd C] [--on-cmd C] [--off-cmd C] [--state-cmd C] [--serial-interface console]
panio power set -t <name> --level hard [--cycle-cmd C] [--on-cmd C] [--off-cmd C] [--state-cmd C]   # a named power level
panio video set -t <name> --device "<capture id or name>" [--ocr-mode text|gui] [--change-threshold N] [--port N]
panio hid set -t <name> --cmd "hidrig -d <uart>" [--port N]   # USB HID injection helper
panio usb set -t <name> --cmd "ch9329 -d <uart>"   # switchable USB media (KVM-Go microSD)
panio adb set -t <name> [--serial <adb-id>] [--adb <path>]  # an Android DUT over adb
```

`--port N` pins a daemon's port (video, hid, and serialcap — set on any one
serial interface) so a human's standing `ssh -L` keeps working across
restarts; leave it unset unless asked, and `--port 0` clears it.

Every channel-config command also takes `[--host <labhost>]` to bind that
channel to a remote control host (see the lab section below).

- **Concurrent netboot links work on macOS and Linux**, with one adapter and
  /24 per link. macOS netbootd locks its interface even when invoked directly;
  stop older daemons before using concurrent links after an upgrade. Never
  delete its `/tmp/panio-netboot-interface-<index>.lock` file while running.
- **One subnet per link.** The default `--host-ip` (`192.168.99.1`) is for
  the *first* netboot link on a host. Every further link on that host needs
  its own /24 (`--host-ip 192.168.100.1`, `…101.1`, …) or every route to a
  target on either link is ambiguous. `netboot set` refuses a clash — a second
  link left at the default included — and `doctor` reports an existing one as
  `CONFLICT`. `doctor` also reports `MISMATCH` when the interface holds
  addresses and the configured one is not among them: the lab file says only
  where the link should be, and netbootd re-applies that address only while its
  daemon runs, so a stopped link drifts silently. Fix it with
  `panio netif mode link <target>` (or `netboot start`), which assigns the
  configured address and drops the others — not by editing the lab file to
  match what drifted. An interface holding no IPv4 address (as in `mode off`
  or IPv6-only `mode ffx`) is not drift, locally or over SSH.
  Check with `panio target show <name>`: the host IP is always
  printed, marked `(default)` when unset. Never record the intended subnet in a
  lab-file comment alone; the field is what runs.
- `panio netboot devices` lists candidate USB-Ethernet interfaces (the
  primary NIC is excluded); `panio discover` (`--json` for machines) lists
  all lab-relevant hardware;
  `panio configure <name> -H <host>` proposes a whole block to paste in. It
  never guesses between interfaces of one multi-interface serial adapter
  (FT2232/FT4232 `-if00`/`-if01`, where `-if00` is often JTAG); pick the
  UART channel from the comments it prints.
- Hosts: `panio host add <name> --ssh <dest>` (plus `--description`,
  `--hostname`, `--identity`, `--panio-cmd`, `--control-path`) manages the `[hosts.*]`
  entries from the CLI; `host list` / `host show <name>` / `host set` /
  `host rm` round it out.
- Inspect: `panio config show` (whole lab; `config path` prints the file,
  `config edit` opens it) / `panio target list` / `panio target show
  <name>`; `panio target set <name>` updates its `--host`/`--description`.
- Synced lab: `panio config sync --git <remote> --path <file>` (or `--url
  <https-url>` with `--token-file`/`--token-command`/`$PANIO_LAB_TOKEN`) copies
  the lab and makes it read-only here; bare `config sync` pulls again.
  `config show` prints the source and commit.
- Remove: `panio target rm <name>`, or per channel (`netboot rm`,
  `serial rm <iface> -t <name>`, `power rm`, `video rm`, `hid rm`, `usb rm`,
  `adb rm`).
- Rename: `panio target rename <old> <new>` carries all channels (and your
  lab-file comments) to the new name. Config-only — running daemons keep the
  old name, so `stop` and re-`watch`/`serve` them under the new one.
- `panio doctor` probes every configured channel against reality (devices
  exist, netboot interfaces hold the address they are configured for, over SSH
  for remote hosts); `doctor [target]` checks one target,
  `doctor --host <name>` only channels on that host.
- A power, hid, usb, or actions channel set with `--doctor` (`power set -t
  <name> --doctor`, likewise `hid set`, `usb set`, `action set`; `--no-doctor`
  turns it off) also has `doctor` run the helper's own read-only check,
  `<program> <transport options> doctor --json`. Its results appear under the
  channel as `power doctor: <check>` lines (on a power channel it covers every
  named level too; with more than one run each line says which, e.g.
  `power doctor (level hard): <check>`): `FAIL` is a problem (exit 1),
  `warn` and `no-check` (the helper has no doctor check) are not. Of the
  bundled helpers only `shellyplug` implements it. A doctor `FAIL` on a channel
  that still works is still a hint, not ground truth (see the gotchas).

## Netboot (DHCP + TFTP + HTTP)

Boot a board over the direct USB-Ethernet link:

```
panio netboot start [target]            # serve DHCP + TFTP + HTTP (netbootd)
panio netboot tftp-root [target]        # print where to drop boot files
panio netboot status [target]
panio netboot logs -f [target]          # follow the combined log (-n N for last N lines)
panio netboot stop [target]
```

**Naming the target.** Every *runtime* verb takes it either way — positionally
(`panio netboot stop pi5`) or as `-t/--target` (`panio netboot stop -t
pi5`) — and both at once is refused. Omit it when the lab has one target. The
*config* verbs (`set`, `add`, `rm`) require `-t` and do not take a positional.
`panio doctor [target]` is positional-only: with no target it checks them all.

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
`panio setup`. If only the HTTP port is taken, netbootd keeps serving DHCP +
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
device it answers short of `panio netboot stop` then `start` again — that
restart is the "release the lock" operation. TFTP separately only answers the
one leased IP and caps itself at a handful of concurrent transfers, which is
invisible in normal use (a boot flow never needs more than that) but means a
misbehaving client flooding RRQs gets dropped rather than served.

## Link mode — netboot · link · ffx · off

`panio netif` owns the host side of the USB-Ethernet link and puts it in one of
four mutually-exclusive modes. Use it to flip between them atomically instead of
configuring the interface by hand:

```
panio netif mode netboot [target]        # IPv4 + DHCP + TFTP (= netboot start)
panio netif mode link [target]           # bare link UP: host IP only, no daemon
panio netif mode ffx [target]            # stop netboot, add host fe80::1/64 for ffx
panio netif mode off [target]            # soft DOWN: release host IP (+ ffx LL)
panio netif down-hard [target]           # harder DOWN: also kill WoL + admin-down (USB NICs may keep link); runs link_down_cmd if set
panio netif status [target]              # mode, carrier, addresses, ffx peer
panio netif vlan add --parent <if> --tag <n> [-H host]   # VLAN interface for one more target; prints device
panio netif vlan list [-H host]          # device, parent, tag, service
panio netif vlan rm <device> [--force] [-H host]         # refuses while it holds an address
```

**Several targets on one adapter** need a managed switch with one 802.1Q VLAN
per target (target port untagged with that PVID, uplink tagged). Create one
VLAN interface per target with `netif vlan add`, then `netboot set -t <t>
--interface <vlan device> --host-ip <own /24>`; each VLAN interface is a normal
one-target link. Linux VLAN interfaces vanish on reboot (re-run `vlan add`; it
is idempotent); macOS ones persist. Give the switch a **static** management
address first: its DHCP client asks on target VLANs too and can take a target's
netboot lease (`netboot logs` shows "lease is held by" another MAC; fix the
switch, then `netboot stop`/`start`). On macOS `vlan add` sets the VLAN MTU to
1496: at 1500, USB adapters drop full-size tagged frames, so DHCP works but HTTP
boot and other large TCP transfers stall. Details: docs/netboot.md "Several
targets through one switch".


**Testing the link up/down.** To check that the link itself comes up and drops —
without serving anything — toggle `link` ↔ `off` and read `netif status`:

```
panio netif mode link [target]    # up:   host IP assigned, no DHCP/TFTP
panio netif status [target]       #       mode=link, carrier up
panio netif mode off [target]     # down: host IP released
panio netif status [target]       #       mode=off
```

Caveat — **soft "down" (`mode off`) only releases the host IP; it does not force
the carrier down.** `netif status` shows `carrier` separately from `mode` because
a NIC with **Wake-on-LAN** enabled keeps the PHY energized even when the
interface is down, so the peer's link LED stays lit and `carrier` can still read
`up` in `off` mode. `panio netif down-hard` goes further: `mode off`, then
disables WoL (`ethtool -s <iface> wol d` on Linux) and admin-downs the
interface. Bring the link back with `netif mode link` (or `netboot`); WoL stays
off until you re-enable it (`ethtool -s <iface> wol g`) or replug the adapter.
`mode off` (and so `down-hard`) exits nonzero when it cannot confirm the host
IPv4 and ffx link-local were released; `down-hard` still runs WoL-off and
admin-down before failing.

**`down-hard` does not guarantee the target sees link loss.** Common USB
adapters keep the PHY up when admin-down (measured: AX88179A/B and RTL8153 on
macOS, RTL8153 on Linux), and after admin-down the host's own `carrier` reads
`down` no matter what, so `netif status` is not evidence. Confirm from the
target side (its link state, serial log, or link LED). When the target *must*
detect link loss, unplug the cable or disable its port on a managed switch
(measured: link lost within 0.8 s, back ~1.7 s after re-enable). The `switchport`
helper drives the port (#334); never disable the port the host manages the switch
through. Setting up a switch for it (D-Link DGS-1100-05V2 over SNMP, from
factory defaults): `panio skill managed-switch`. Configure the channel's link hooks to get that guaranteed drop through
panio: `panio netboot set -t <name> --link-down-cmd 'switchport -d sw1
--mgmt-port 8 port 3 off' --link-up-cmd 'switchport -d sw1 --mgmt-port 8 port 3 on'`.
`down-hard` then runs the down hook first, before its host-side steps (a failing
hook exits `hook_failed` after those steps were still attempted); `netif mode
link|netboot|ffx` run the up hook first, so it must be idempotent; `mode off`
runs none; `netif status` shows a `link hooks` line. An empty value clears a
hook. One netboot link serves one target; don't put several targets behind
a plain switch on one adapter: give each its own VLAN (above).

- **Switching to ffx stops netboot first** — otherwise a power-cycle TFTP-boots a
  stale image instead of falling through to the SD card. This is the safe way to
  hand the link off to `ffx`.
- `mode ffx` adds the host-side `fe80::1/64` that `ffx` needs (nothing else sets
  it up, and it's lost on a control-host reboot). Re-run `mode ffx` any time to
  re-add it — every mode is idempotent.
- `netif status` in ffx mode reads the IPv6 neighbor table and prints a
  ready-to-paste `ffx target add fe80::…%<iface>` for the discovered device. If
  no peer shows, power-cycle the target and wait for it to finish SLAAC.

Typical ffx hand-off: `panio netif mode ffx fortune` → `panio power-cycle
fortune` → `panio netif status fortune` (grab the address) → `ffx target list`.
Go back to TFTP bring-up with `panio netif mode netboot fortune`. Same
passwordless `sudo` requirement as netboot (`ip` on Linux, `ifconfig` on macOS).

## Video — capture, preview, OCR

```
panio video devices                 # list capture devices (with stable ids)
panio video set -t <target> --device "<id-or-name>" [--ocr-mode text|gui] [--change-threshold N] [--port N]  # configure the video channel
                                           #   --ocr-mode gui is a Linux-only accuracy switch
                                           #   for GUI screens (unset = platform default)
                                           #   --device rfb+hid: = the RFB video feed of this target's own
                                           #   hid daemon (a JetKVM; set the hid channel first), or
                                           #   rfb://HOST:PORT for a plain network RFB source (no password)
panio video watch [target] [--restart]   # start the capture daemon (background);
                                           #   --restart force-restarts a stalled one
panio video preview [--open]        # print the daemon's dashboard URL (--open: hand it to a browser instead)
panio video shot [target] [--stable] [--out frame.png]   # one lossless PNG
panio video shot --changed-since <hex-hash> --timeout <ms>   # block until the
                                           #   frame differs from a previous shot's hash
panio video read [target] [--stable]   # OCR the current screen, print text
panio video show [target]           # device + daemon status
panio video stop [target]           # stop the daemon (on the target's host)
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
  `http://127.0.0.1:<port>`; `panio video preview` is the only one that
  prints the openable form, and it exists for a human about to paste it into a
  browser. Never paste a `?token=` URL into an issue, a commit message, or a
  chat.
- **Opening a browser is a human's job, not yours.** `panio video preview
  --open` only works on a machine with a desktop session, and it is refused
  outright when the video channel is on another host. On a headless control
  host — where you usually are — it opens nothing and prints a single-use
  link, so it is not a way to get a browser open. Hand the human `panio console <target>` and let them run it.
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
  stages, codecs) — pass `--all` through `panio helper hdmicap devices --all`
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
panio serial add console -t <target> --device <path>   # add a named interface
panio serial add bmc -t <target> --device /dev/ttyUSB1 --baud 9600
panio serial set console -t <target> --sense cts        # update fields
panio serial rm <name> -t <target>                      # drop a named interface
panio serial connect [target] [-i name]      # interactive terminal (tio) in your shell
panio serial watch [target]                  # run the daemon for ALL interfaces;
                                               #   they appear in the dashboard pane
panio serial send [target] "text"            # send one line of input through
                                               #   the running daemon (see below)
panio serial transact -t <target> [-i name] --until 'OK' -- "cmd"
                                               # send + wait for the reply, locked
panio serial log [target] [-i name] [options] # print captured output (timestamped)
panio serial evidence [target] [-i name] [--epoch E] [--since N] [--max-bytes N]
panio serial show [target]                   # list interfaces + daemon status
panio serial stop [target]                   # release the ports (on the target's host)
panio serial devices                         # list serial devices on the host
panio serial dtr [target] [-i name] [--ms N] # pulse DTR line (J2 power button header)
panio serial reset [target] [-i name]        # soft reset via brief DTR pulse
```

**Argument convention:** every runtime command takes the target as an
optional positional (omit it when the lab has one target); channel-config
commands (`add`/`set`/`rm`) take `-t`. `serial send`/`serial log` accept
`-t` too — `serial send` reads two positionals as `<target> <text>`, one as
just the text. The `-t`-only runtime commands are `hid send`, `adb run`,
`adb input` and `serial transact`, whose positional tails belong to the
helper / to `adb` / are the text to send.

The interface name is a required positional on `add`/`set`/`rm` (`console` is
just the conventional name for the main one). With
one interface, `-i`/`--interface` can be omitted everywhere. Don't run `connect`
and `watch` (or an external `screen`/`tio`) on the **same device** at once —
start one, or `stop`/close the other first.

### Updating an older serialcap, hdmicap, ch9329, hidrig, or zigplug daemon

If `serial stop`, `video stop`, or `hid stop` reports that authenticated
shutdown is unavailable, use `panio daemons stop <name>` (`serialcap`,
`hdmicap`, or `hid`) and start capture again. A standalone `zigplug stop`
that cannot make the daemon exit reports the same thing — use `panio
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
panio serial log                    # most recent ~200 lines (sole interface)
panio serial log -i bmc --tail 50   # most recent 50 lines of the 'bmc' interface
panio serial log --since 1840       # only lines after sequence #1840 (polling)
panio serial log --from 1800 --to 1860   # a specific line range
panio serial log --json             # JSON Lines (seq, ts_ms, text) for parsing
panio serial log --raw              # keep ANSI colors / control bytes
panio serial log --no-pending       # omit the current unterminated line
```

With more than one interface configured, pass `-i <name>` to choose one (omitting
it errors and lists the names); with more than one target, name it
(`serial log pi5 …` or `-t pi5`). Each line is shown as `[<UTC timestamp>] #<seq>
<text>`. The `seq` is a stable, monotonic line number — note it, then come back
later with `--since <seq>` to get only what's new, or `--from/--to` to re-read an
exact span. Output is ANSI-stripped by default; a `*` after the sequence number
marks the current unterminated line (e.g. a `login:` prompt with no newline yet).

### Sending input

`panio serial send` injects one line through the **running daemon** (start
`watch` first), so input coexists with capture — what you send and what the
target echoes both land in the log:

```
panio serial send <target> "reboot"                # CR appended by default
panio serial send "reboot"                         # sole-target lab: text only
panio serial send --no-newline "partial input"     # suppress the CR
panio serial send --pace-ms 8 "long command"       # per-byte pacing for slow
                                                     #   polled consoles
```

Note the input only reaches the target if its console actually reads the UART
(a kernel with a broken serial driver logs output but ignores input).

### Command and reply in one call (`serial transact`)

For a request/response console (U-Boot prompt, debug monitor, AT port, EC
console), `serial transact` (MCP: `serial_transact`) sends one command and
waits for its reply under the interface's transaction lock:

```
panio serial transact -t <target> -i console --until '=> ' -- "printenv bootcmd"
panio serial transact -t <target> --until 'OK' --timeout-ms 5000 -- "AT"
```

- It records the receive boundary **before** sending, so the reply holds only
  bytes received after the send; a stale prompt cannot match.
- Output is the `serial wait` result JSON (`status`, `records`, `pending`, …)
  plus `sent_bytes`; exit 0 for any status. Read `status`: `matched` means the
  `--until` bytes arrived, not that the command succeeded. `timeout`/`cap`
  keep what arrived. Nothing is retried; never re-send blindly.
- `--timeout-ms` (default 30000) covers queueing behind another transaction
  too; a transaction that never gets the lock fails *interface busy* and sends
  nothing. A CR is appended unless `--no-newline`; `--pace-ms` as for `send`.
- While a transaction runs, `serial send` fails (409, nothing sent) and
  dashboard keystrokes are dropped with an `input refused` marker. Retry the
  send after the transaction ends; don't loop on it.

## RAM-only USB fastboot staging

Use the contract in `docs/fastboot.md` for an existing device-scoped,
download-only client. Panio has no fastboot channel or bundled guarded
runner; `panio usb` switches media and `adb` is a different protocol.
A board-specific runner must make these guards inseparable from each send:

- Hold one external ownership reservation over serial input, power/reset and
  USB for the whole run. Panio's transaction and action locks cover shorter
  operations; they do not reserve the entire target workflow.
- Before **each artifact**, enter the firmware service and validate its
  complete fresh response through `serial transact` and raw evidence. Exit 0,
  a matched echo, a stale prompt or the previous artifact's receipt is not
  service-entry proof. Reconnect/startup changes invalidate readiness.
- Observe the exact USB device separately. Successful empty discovery is
  pending only within a fixed readiness deadline; retain every observation.
  Errors, wrong/ambiguous devices or expiration stop the operation. Bind the
  observed identity at open; never select another device or silently resend.
- Send once with finite supervision on the USB host, able to terminate the
  sender's entire process tree in its privilege domain. SSH timeout alone is
  insufficient. Keep partial receipts; unknown termination keeps hardware
  reserved or quarantined. No automatic reset or retry.
- Require a fresh RAM-preserving return to the serial loader and verify the
  exact RAM range against the artifact checksum before the next artifact.
  Flash/erase/update/unlock, persistent changes and image execution are outside
  staging. Use only necessary firmware queries, not broad partition/A-B scans.

Do not improvise raw sender calls when the guarded runner is unavailable.

## adb (Android targets)

When the target is an Android device, the `adb` channel gives you console,
screen, and input over one transport (see `docs/adb.md`). adb runs
on the control host the device is plugged into; panio routes per-channel like
any other.

```
panio adb devices [-H <host>]          # list attached devices (their serials)
panio adb set -t <target> [--serial <id>]   # bind a device (omit --serial = sole device)
panio adb show [target]                # config + live device state
panio adb shell [target]               # interactive `adb shell` in your terminal
panio adb run -t <target> <cmd...>     # one-shot command, output captured (agent path)
panio adb screencap [target] [-o frame.png]   # one PNG (exec-out screencap; -o - = stdout)
panio adb input -t <target> <args...>  # adb shell input: keyevent/text/tap/swipe
```

- `run`/`input` take `-t` (not a positional) because their tail is free-form;
  put `-t` first and use `--` before a leading-dash argument
  (`panio adb run -t pixel -- logcat -d -t 50`).
- `adb run -t <t> getprop ro.product.model` is the read workhorse for agents;
  `-o <path>` on `screencap` always means *this* machine's filesystem — it
  works the same whether the adb channel is local or on a remote control host
  (like `video shot`), so `-o frame.png` and `-o -` (stdout) are equally safe
  to use against a remote target.
- Reboot/power: no adb-specific command — wire `adb reboot` through the power
  hooks (`panio power set -t <t> --cycle-cmd "adb -s <id> reboot"`).
- The device must be authorized (`adb devices` shows it as `device`, not
  `unauthorized`/`offline`); panio does not manage pairing.

## Power control

```
panio power on  [target] [--level L] # run on_cmd hook (error with hint if unset)
panio power off [target] [--level L] # run off_cmd hook (error with hint if unset)
panio power-cycle [target] [--level L]  # run cycle_cmd hook
panio power-state [target] [--level L]  # state_cmd stdout ("on"/"off") or serial sense-line
panio serial dtr [target] [--ms N]   # pulse DTR line on J2 header (opt-in; see below)
panio serial reset [target]          # soft reset via brief DTR pulse (opt-in)
```

Configure hooks with `panio power set`:

```
panio power set -t <name> \
    [--cycle-cmd <cmd>] [--on-cmd <cmd>] [--off-cmd <cmd>] [--state-cmd <cmd>] \
    [--serial-interface console]   # default DTR interface when several opt in
```

All four hooks are optional and run via `sh -c`. `power-state` uses `state_cmd`
if set (first whitespace token of stdout must be `on` or `off`); falls back to
the serial sense-line otherwise.

**Power levels (soft vs hard).** A target with more than one power layer (a
board MCU or PMIC switch, and a mains/DC cut behind it) may define named
levels beside the default hooks: `[targets.<t>.power.levels.hard]`, set with
`panio power set -t <t> --level hard …`, removed with `panio power rm -t
<t> --level hard`. `target show` lists them (`levels=hard`).

- No `--level` = the default hooks, exactly as before. `--level default` says
  the same thing explicitly.
- A level never falls back to the default hooks: a level without the verb's
  hook exits 3 (`not_configured`) naming the level. Do not retry without
  `--level` to "make it work"; that runs a different layer.
- Escalate in order: try the default `power-cycle` first; reach for `--level
  hard` when the target stays wedged after it (a back-powered board, a hung
  board-control MCU).
- `power-state` without `--level` prints one line per level when there are
  levels, default first (`Power ON  (pi5)`, `Power OFF  (pi5, level hard)`,
  `Power UNKNOWN  (pi5, level pmic: no state_cmd)`).
- The dashboard's power controls and an action's `requires_power` check use
  the default level only.

**"Reboot over serial" ≠ DTR reset — pick the right one.** Two unrelated things
share the word "serial/reset":

- **Console reboot (software):** type `reboot` into a logged-in console with
  `panio serial send <target> "reboot"`. This is what "use serial to reboot"
  almost always means. If you can't log in, use `panio power-cycle <target>`.
- **DTR reset (hardware):** `panio serial dtr` / `serial reset` toggle the
  FTDI DTR line wired to the board's J2 power button. A ≤500 ms pulse is a soft
  button event; ≥3000 ms is a hard PMIC power-off.

DTR is **opt-in per interface** — it works only where the interface declares
`power_button = true` (`panio serial set <iface> -t <name> --power-button`),
because DTR-to-J2 wiring is rare. On a target that hasn't opted in, `serial dtr`
/ `serial reset` **error** with a hint (they never toggle a lone console blindly).
So: unless DTR wiring is explicitly declared, reboot via the console `reboot` or
`power-cycle` — do **not** assume `serial reset` power-cycles the board.

### Cambrionix hub (example)

The `cambrionix` helper binary drives a Cambrionix USB hub's control UART and
satisfies the `state_cmd` contract (`state <port>` prints `on` or `off`):

```
panio power set -t pi5 \
    --cycle-cmd "cambrionix -d /dev/cu.usbserial-AA00BB11 cycle 4" \
    --on-cmd    "cambrionix -d /dev/cu.usbserial-AA00BB11 on 4" \
    --off-cmd   "cambrionix -d /dev/cu.usbserial-AA00BB11 off 4" \
    --state-cmd "cambrionix -d /dev/cu.usbserial-AA00BB11 state 4"
```

A Cambrionix port `off`/`cycle` cuts VBUS only; D+/D- stay connected. A
device that does not sense VBUS stays enumerated through the cycle, so do not
treat a cambrionix cycle as a USB unplug when a test needs re-enumeration.

See `docs/power.md` for the full `cambrionix` command surface.

### DSD TECH USB serial relay board (dsdrelay)

The `dsdrelay` helper switches a channel of a DSD TECH SH-UR01A (1-channel,
CP2102N) or SH-UR04A (4-channel, older CP2102; all four channels
hardware-verified) USB serial relay board. Targets are wired through the relay's **NC (normally
closed)** contact: `on` releases the coil, `off` energizes it, and an
unpowered board leaves the target powered. Select the port with `-d <path>` or
by USB serial number with `-s <usb-serial>` (survives re-plugs). Use `-s` for
the SH-UR01A (unique serial). The SH-UR04A ships with the shared factory
serial `0001`: use `-d` for it (Linux: `/dev/serial/by-path/...`) unless its
serial was rewritten, in which case `-s` works. Rewriting the serial is the
user's call, at their own risk (`docs/power.md`, "Giving an SH-UR04A a unique
serial"); never do it on your own initiative.

```
panio power set -t pi5 \
    --cycle-cmd "dsdrelay -s AA00BB11CC22DD33 cycle 1" \
    --on-cmd    "dsdrelay -s AA00BB11CC22DD33 on 1" \
    --off-cmd   "dsdrelay -s AA00BB11CC22DD33 off 1" \
    --state-cmd "dsdrelay -s AA00BB11CC22DD33 state 1" \
    --doctor
```

Gotchas: `dsdrelay list` enumerates the CP2102N ports without opening them;
never open one by hand to identify it, because a Sonoff ZBDongle-P Zigbee
coordinator shares the VID:PID and opening its port can reset it into the
bootloader. A channel the board lacks is an error ("channel N not present on
this relay"). Commands must end in `\r\n` (the helper handles this and resyncs
with `?\r\n` before each command, so a leftover `AT+CH1=1` is never completed, so never write to the port by hand; leftover bytes survive
a close/reopen and corrupt the next command), and SH-UR04A replies end in a NUL.
`list` warns when serials are not unique. See `docs/power.md` for the full `dsdrelay` command surface.

### DSD TECH Bluetooth LE relay board (dsdble)

The `dsdble` helper switches a channel of a DSD TECH SH-BT01E Bluetooth LE
relay board over BLE. It is usable but blind, so prefer `dsdrelay` or
`shellyplug` (which read state back) and use this as the wireless fallback. Wiring is the same hardcoded **NC** as `dsdrelay`
(`on` releases the coil, `off` energizes it; an unpowered board leaves the
target powered). Select the board with `-a <addr>`: its MAC on Linux, the
per-host CoreBluetooth UUID on macOS (find either with `dsdble scan`, which
connects to nothing).

**The board cannot report state.** `on`/`off`/`cycle` succeed when the BLE
write is acknowledged, which does not prove the target changed state, and
`dsdble state` always fails with `state unknown` rather than guess. Do not
wire a `--state-cmd`; confirm power changes another way (serial console,
power-sense, the target's network presence).

```
panio power set -t pi5 \
    --cycle-cmd "dsdble -a 00:11:22:33:44:55 cycle 1" \
    --on-cmd    "dsdble -a 00:11:22:33:44:55 on 1" \
    --off-cmd   "dsdble -a 00:11:22:33:44:55 off 1" \
    --doctor
```

Gotchas: use it from a Linux control host. On macOS the binary only works
from a terminal app that has Bluetooth permission and is killed over SSH or
from a hook. A board still connected to a phone app does not advertise and is
not found. See `docs/power.md` for the full `dsdble` command surface.

### Phidgets VINT relays (phidget)

The `phidget` helper switches a channel of a Phidgets REL1100 relay module
through a HUB0002 hub over raw USB. Like `dsdble` it is blind: the protocol
has no read-back, so `on`/`off`/`cycle` succeed on the hub's ACK (a module is
attached), not on proof the relay moved, and `phidget state` always fails with
`state unknown`. Do not wire a `--state-cmd`. The REL1100 is wired normally
open (`on` conducts), so a rebooted module leaves the output off. Address an
output as `<PORT>/<CHANNEL>`, both 0-based; select a hub with `-s <serial>`
before the verb (optional with one hub; `phidget list` shows serials).

```
panio power set -t pi5 \
    --cycle-cmd "phidget -s AA00BB11 cycle 0/0" \
    --on-cmd    "phidget -s AA00BB11 on 0/0" \
    --off-cmd   "phidget -s AA00BB11 off 0/0" \
    --doctor
```

Gotchas: on Linux it needs a udev rule for USB vendor 06c2; another process
holding the hub (Phidget Control Panel, `phidget22networkserver`) makes it
fail busy after 3 s. See `docs/power.md` for the full `phidget` command surface.

### Intel AMT / vPro (example)

The `amt` helper switches vPro machines over the network via their Management
Engine — no plug hardware — and gives **true power-state readback** (the ME
answers with the host on, off, or bare-metal):

```
panio power set -t optiplex \
    --cycle-cmd "amt cycle -d amt-host.example -u admin --delay-ms 5000" \
    --on-cmd    "amt on -d amt-host.example -u admin" \
    --off-cmd   "amt off -d amt-host.example -u admin" \
    --state-cmd "amt state -d amt-host.example -u admin"
```

**The AMT password is not in the lab file.** The helper takes it from the
first set of: the `AMT_PASSWORD` environment variable, `--password-file
<path>`, or `--password-command <cmd>` (whose stdout is the password). A hook
that carries its own source needs nothing in the environment, e.g.
`--state-cmd "amt state -d amt-host.example --password-command 'op read op://lab/amt/password'"`;
look at the existing hooks before composing a fetch by hand. Otherwise run
`panio power …` with `AMT_PASSWORD` set, e.g.
`op run --env-file .env -- bash -c 'panio power-cycle optiplex'` (single
quotes — the parent shell must not expand it). A file path or command in a
hook is resolved on the host where the hook runs (the control host for a
remote power channel); `AMT_PASSWORD` set where *you* run the command is
carried to a remote control host by panio's own dispatch, over the remote
command's stdin, never its argv, and wins over the hook's file or command.
See "Setting up the credential source" in `docs/power.md` for the patterns.

AMT uses **verified HTTPS on 16993 by default**, with no HTTP fallback.
Use the certificate's name. For a private CA (certificate authority), add
`--tls-ca-file /etc/panio/amt-ca.pem` to all four hooks; it adds trust to
built-in public roots and does not bypass name or validity checks. The flag
wins over `AMT_TLS_CA_FILE`. The file and fallback are read on the execution
host; panio does not forward `AMT_TLS_CA_FILE` to remote hosts.

For deliberately retained legacy HTTP only, add `--allow-insecure-http` to
**all four hooks**, for example `amt --allow-insecure-http -d
http://amt-host.example state`. Bare addresses with that flag also mean
HTTP/16992. Explicit HTTPS plus the flag, or HTTP plus a CA file, is rejected.
A port alone never chooses HTTP: bare `host:16992` requests TLS on 16992.
HTTP warnings remain on stderr, including successful `panio power-state`.
Do not retry TLS errors with HTTP or disable verification. TLS 1.2 alone is
not enough if firmware only supports obsolete algorithms. KVM settings gain
TLS; the separate RFB screen session does not. See `docs/power.md` and the
[glossary](../../docs/glossary.md) for TLS and certificate terminology.

## Named actions — reset, recovery, boot straps

A target may define **named actions** for controls that are not power:

```
panio action list [-t target]          # name, description, (requires power on|off)
panio action run -t <target> <name>    # run one; -t only, the positional is the name
panio action set -t <target> <name> --cmd <cmd> [--description <text>] \
    [--requires-power on|off|none] [--host <labhost>]
panio action rm -t <target> <name>
```

- **Check `action list` before a power cycle.** If the target defines
  `reset`, prefer it: it is gentler and often the only reset that clears a
  wedged SoC without dropping attached debug hardware. `recovery` and
  `warm-reset` are the other conventional names.
- An action marked `requires_power off` is refused (exit 2, `usage`) while the
  target is on. Run `panio power off` first; never edit the lab to drop the
  requirement.
- Action and power hooks for one target run one at a time; a hook that waits
  more than 120 s for another fails with `timeout` (exit 22, outcome unknown).
- Actions take no extra arguments. Each variant is its own action.

Guide: `docs/actions.md`.

## Switchable USB media — hand a card between host and target

Some KVM devices route one physical USB device to either side, never both.
The `usb` channel drives that mux. Two kinds are supported: the **Openterface
KVM-Go**'s onboard microSD, through the same `ch9329` helper and the same
serial port as its `hid` channel (setting both channels to the same `--cmd`
is normal), and a cheap toggle-only **USB sharing switch** pressed by a relay,
through the `usbtoggle` helper (below).

```
panio usb state         -t <name>   # -> host | target
panio usb attach-host   -t <name>   # route the card to the control host
panio usb attach-target -t <name>   # hand it to the target
```

The use cases are hands-free *physical* boot media (image the card host-side,
attach it to the target, and firmware sees a real mass-storage device, which
it generally will not for streamed virtual media) and, with a sharing switch,
a repeatable USB plug/unplug of any device for the target.

Four rules, all of which will bite you otherwise:

- **Unmount before switching.** The switch physically detaches the device from
  whichever side has it; a mounted filesystem there is a surprise removal.
  panio cannot see mount state and will not stop you. On macOS use
  `diskutil unmount force` — plain `unmount` usually loses to Spotlight.
- **Exit 0 means the mux moved, not that the media is ready.** The USB device
  still has to enumerate on the receiving side, which takes seconds. Poll for
  the block device.
- **Always `usb state`; never assume.** The position resets to the host side
  when the KVM loses power, and a physical button can move it too.
- **A device with no mux does not answer at all** — there is no "unsupported"
  reply in the protocol — so it surfaces as a timeout, which the helper
  reports as unsupported rather than broken.

### USB sharing switch (usbtoggle)

A 2-computer USB sharing switch only toggles; it has no readable position, so
`usbtoggle` presses its button through any relay (`--close-cmd`/`--open-cmd`)
and reads the position by probing for the peripheral on the control host
(`--device VID:PID[:SERIAL]`, present = `host`) or from `--state-cmd`. The
`--cmd` is run by a shell, so nest the quotes:

```
panio usb set -t <name> --doctor --cmd "usbtoggle --device 046d:c08b \
  --close-cmd 'dsdrelay -s AA00BB11CC22DD33 off 1' \
  --open-cmd 'dsdrelay -s AA00BB11CC22DD33 on 1'"
```

Gotchas:

- **`target` is inferred** (device absent from the host) unless
  `--target-check-cmd` confirms it; without it `usbtoggle` prints a loud
  warning on stderr every time. Treat `target` without a check as "probably".
- **It presses at most once and never retries.** A timeout (`--settle-ms`,
  default 5000) is an error that says what was last observed; look before
  running it again, because a second press toggles back.
- **"Seen on neither side"** (absent and the check failed) is an error, and it
  refuses to press blind.
- `dsdrelay` assumes NC wiring; a button is on the NO contact, so closing the
  button is `dsdrelay off <ch>` and opening is `on <ch>`.
- A hand on the switch's own button moves it too; re-read with `panio usb state`.
- **Not every sharing switch is a real unplug.** Some leave VBUS or the
  SuperSpeed pairs connected, or hide a hub. Check a new model first
  (`docs/usb.md`, "Check a switch before you rely on it").
- Keep the relay's USB connection off the hub the switch is on: a wedged hub
  takes the relay with it, and then nothing can press the button.

Full guide: `docs/usb.md`. Protocol: `notes/openterface-usb-mux-spec.md`.

## HID injection — type and click into the target

Three bundled injector helpers speak the same command vocabulary; the `hid`
channel stores an opaque helper command and `panio hid send` appends its
arguments to it and runs it on the channel's host:

- **`hidrig`** — the dual-board KB2040 "dumb pipe": the host-side CLI
  composes HID reports and writes binary frames to the **control** board's
  USB-CDC port, which relays them over I2C1 to the **target** board that
  presents a USB keyboard + mouse to the DUT.
- **`ch9329`** — the client for CH9329-based KVM devices (Openterface
  Mini-KVM and KVM-Go, Sipeed NanoKVM-USB): `panio hid set -t <name> --cmd
  "ch9329 -d <uart>"`. Same commands; it autodetects the link baud (115200 /
  57600 / 9600) and adds `info` (target USB enumeration + lock LEDs). Through
  its daemon, a long `type` gets a longer timeout (10 s + 100 ms per
  character), and a client that times out or is killed cancels the running
  command, which releases its keys. After a timeout `ch9329` exits 40
  (cancelled and confirmed stopped) or 41 (could not confirm; it may still be
  typing); `panio hid send` shows them as `helper_failed` with `child_exit`
  40/41.
  On Linux, a `1a86:fe0c` port that does not answer at open gets a
  parser flush (261 zero bytes) and one retry automatically, so a cold
  Openterface needs no manual step; a failure after that carries a startup
  compatibility warning. Do not interpret `chip_version` as a firmware release or flash an
  image based on VID/PID alone. A captured Mini-KVM failure involved tty echo
  of unsolicited startup bytes; another tty open after a cold cycle can repeat
  it. The direct-USB drain workaround (only if the automatic flush fails) requires explicit device selection,
  stopping port users, and restoring drivers: follow `docs/hid.md` →
  "Openterface CDC on Linux" and `notes/openterface-v2-linux-serial.md`.
- **`jetkvm`** — the JetKVM network KVM (Go helper): `panio hid set -t <name>
  --cmd "jetkvm -d <host>"`; the password comes from `JETKVM_PASSWORD`,
  `--password-file` or `--password-command`, never a flag. Same commands; its
  `info` prints the device's USB, video and keyboard-LED state; its daemon also serves the screen (`video set --device rfb+hid:`). **The JetKVM
  allows ONE session**: the helper's session and a browser tab on the
  device's web UI kick each other, and `OK` does not prove delivery (the
  firmware says success even if the target's USB is not up — check `info`).

```
panio hid set -t <name> --cmd "hidrig -d /dev/cu.usbmodemXXXX" [--host <labhost>]
panio hid send -t <name> type hello world   # type a string
panio hid send -t <name> key ENTER          # tap a key (adafruit_hid Keycode names)
panio hid send -t <name> combo LEFT_CONTROL C
panio hid send -t <name> move 300 -50       # relative mouse move (negatives OK; keep -t first)
panio hid send -t <name> click left | scroll -3 | releaseall
panio hid send -t <name> ping               # injector liveness check
```

The same dual-board control board can also back this target's **serial** and
**power** channels: when `hidrig serve` runs it bridges the DUT's serial console
and re-exports it as a PTY (the hid daemon is per-target, so point a `serial`
channel's `device =` at `/tmp/panio-<uid>/hid/<target>/console`), and `hidrig
power off|on|cycle` switches DUT
power via a relay (behind the `power` hook) — one USB device for HID, console,
and power. (The relay/power path is hardware-verified, incl. state persistence
across a control-board reset; the console bridge is new and not yet verified.)

Absolute mouse (click-where-you-point): `panio hid send -t <name> moveabs
<x> <y>` positions the cursor in a 0..32767 logical space the OS maps across
the screen (the KB2040 firmware advertises the `moveabs` capability).

Sequences: `hidrig run <file>` runs a command file (one command per line,
`# comments`, `delay <ms>` / `sleep <seconds>`) — the file must be on the
channel's host. The target board is powered by the DUT's USB port, so its HID
goes silent while the DUT is off and reboots with it; the control board is
host-powered and independent. Command vocabulary:
`docs/dev/hid-serial-protocol.md`; dual-board design + frame format:
`docs/dev/hid-dual-board-design.md`.

**KVM in the console.** `panio console <name>` shows the video as a noVNC view; when the target
has a `hid` channel, click into it to drive the target with your own keyboard + mouse (absolute —
the cursor follows where you point; F13-F24 and exotic keysyms are dropped). In the MJPEG view
(toggle) a **⌨ Capture input** button does the same. `video watch`/`console` start the hid daemon
for any video device, so it owns the injector from then on. It auto-starts the hid daemon (`hidrig serve`), which owns the control
link and re-exposes the command vocabulary over a WebSocket; `panio hid send` injections intermix
with what you type in the browser. Manual daemon control: `panio hid
serve [target]` / `panio hid stop [target]` (positional target — only
`hid send` and `hid set/rm` use `-t`). When the target also has a `power` channel, the overlay
adds an on/off toggle switch + a separate cycle button (each confirms first).
`hid stop` waits for the daemon to exit before returning successfully, so it is
safe to follow immediately with `console` or `video watch`.

For *operating a GUI* through the `video` + `hid` channels — clicking, typing,
and navigating a desktop/app/installer/BIOS you can only see over the capture
and only touch through emulated HID — read the companion **`kvm-puppeting`**
skill (the look-act-settle-verify discipline): `panio skill kvm-puppeting`.

## Leases — reserve a board before a multi-step job

A **lease** gives one holder exclusive control of a whole target; everyone
else can still read from it (`video shot`, `serial log`, `power-state`) but
cannot change it. Full guide: `docs/leases.md`.

**When to acquire.** Before a multi-step job that another agent or person
could wreck midway: a flash, an OS install, a netboot-and-test cycle. Do not
bother for a one-shot read. If the lab sets `lease_required` on a target, you
must.

```
export PANIO_LEASE=$(panio lease acquire pi5)       # prints the token alone
panio power-cycle pi5                                # carries the token
panio lease release pi5                              # when done
```

- Hand the token to sub-agents that share the board (`export
  PANIO_LEASE=<token>` in their shell). Treat it as a secret: only `acquire`
  prints it, `lease status` never does.
- Acquire **per target**; a token for one target does nothing on another.
- `acquire --wait 30m` waits for a busy board; `--duration`/`--idle` change
  the 2 h / 30 min defaults; `lease renew` extends a lease you hold. Long
  jobs: renew before the end.
- **Release when done.** Release stops the target's daemons and netboot
  session and takes the link down; it never changes power. If any of that
  fails the lease still ends but the target is blocked (exit 24, below). Releasing a lease
  that is already over exits 0 and says on stderr how it ended (it does not
  disturb a new holder), so an end-of-job `release` is safe to run twice; on a
  target nobody holds it exits 0 (`not leased`). `renew` on an expired lease
  exits 23.
- **The dashboard does not keep a lease alive.** `console` and `video preview`
  hand you a web page whose use is not a panio command; with a token set they
  print a note. `console` refreshes the lease itself while it runs and you
  hold the token (it stays in the foreground and Ctrl-C closes it; if the lease
  is released, taken or expires it prints why and exits 23); with no token a local `console` returns at
  once. For `video preview`, `lease renew` while you use the page.
- Do not use `--preempt` (on `acquire` or `release`) unless the user told you
  to: it ends someone else's work.
- MCP: `lease_acquire`, `lease_renew`, `lease_release`, `lease_status`. The
  server keeps the token itself, so there is no `PANIO_LEASE` to export.

**Exit 23 (`target_leased`).** A mutating command was refused: the message
names the holder and when the lease ends. Run `panio lease status <target>`,
then wait (`panio lease acquire <target> --wait 30m`) or tell the user. Do not
retry in a loop and do not work around it by touching the daemons or devices
directly.

**Exit 24 (`target_cleanup_blocked`).** The previous lease ended but cleaning
up after it failed (a daemon would not stop, a process survived, the netboot
address could not be released), so nobody can acquire, renew, release or
change the target; `--preempt` does not help. Reads still run, with a warning.
`panio lease status <target>` (exit 0) shows `state: "blocked"`, the
`cleanup_id` and each step's result. Tell the user. To recover, run
`panio lease cleanup <target> --id <cleanup_id>`: it retries every step and
clears the block when they pass. Only when the user agrees that cleanup cannot
succeed, `panio lease override-cleanup <target> --id <id> --reason "..."`
clears it without verifying and records the reason. Neither command acquires
the target. An unreadable lease record gives the same exit; the fix is
`override-cleanup <target> --corrupt --reason "..."`. Older panio versions
and direct helper access do not honor the block.

**Your own token stopped working.** If your lease was released, preempted or
expired, a mutating command exits 23 with how and when it ended (and who holds
the board now); a read warns on stderr and still runs, including when someone
else holds the board now. Do not keep driving the
board: the cleanup stopped your daemons. Re-acquire (`panio lease acquire`) if
the job is still yours, or report the preemption. A warning that
`PANIO_LEASE` is set but the target is "not leased" means the token matches
nothing there; it ran without a lease.

## Companion skills

Panio ships its own agent skills; list them and read any one from the CLI,
even on a packaged install:

```
panio skill                  # list bundled skills with their descriptions
panio skill kvm-puppeting    # print a skill's SKILL.md (e.g. GUI puppeting)
panio skill panio --path   # the file path, to open or Read it
```

## MCP tools (experimental)

If your harness has a `panio` MCP server configured (`panio mcp`), its
tools are the runtime verbs above: `target_list`, `video_shot`, `video_read`,
`serial_log`, `serial_send`, `hid_send` (`command` is an array of words, e.g.
`["key", "ENTER"]`), `action_list`, `action_run`, `power_state`, `power_on`,
`power_off`, `power_cycle` (the power tools take an optional `level`, as
`--level`), and `lease_acquire`, `lease_renew`, `lease_release`,
`lease_status` (see *Leases*).
`video_shot` returns the screen as an image plus `signal=…  hash=…`; pass the
hash back as `changed_since` to wait for a change, as with `video shot
--changed-since`. Configuration (`set`, `add`, `watch`) is still the CLI. A
failed tool result carries panio's exit code and JSON error object (see
*When a command fails* above), and a write that may or may not have run says
"Outcome unknown". Setup: `docs/mcp.md`.

## Targets on a remote control host (a "lab")

When the machine wired to the target isn't the one you're running panio on,
describe your **lab** in one git-tracked TOML file and point panio at it with
`--lab <file>` or `PANIO_LAB`. Each target's `host` names a control host;
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
# panio_cmd = "~/.local/bin/panio"  # if panio isn't on the
#                                  host's non-interactive ssh PATH
# plugin_path = "/opt/lab-plugins"  # plugin roots on bench1, passed on every
#                                  dispatch there (`host set --plugin-path`)

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
export PANIO_LAB=~/labs/mylab.toml
panio netboot start fortune      # runs on bench1, transparently
panio power-cycle fortune
panio netboot logs -f fortune    # streams back live
panio serial connect fortune     # interactive tio over ssh -t
panio console fortune            # dashboard tunnelled to your browser; Ctrl-C to close
```

Notes:
- With **no** `--lab`/`PANIO_LAB`, panio uses the default lab at
  `~/.config/panio/lab.toml` (auto-created on first write).
- The lab file is **CLI-managed and hand-edit friendly**: all the config
  commands edit it surgically (your comments survive), and it stays your
  git-tracked source of truth.
- **Channels can live on different hosts** (per-channel `--host`): each command
  runs on the host of the channel it touches. Composites (`console`) need
  their channels co-located on one host.
- Still over plain SSH if you prefer: `ssh bench1 "panio …"` works too, but the
  lab makes location transparent.

### Authoring a lab (discovery + provisioning)

After declaring a host's connection info (`panio host add bench1 --ssh
user@bench1.local`, or hand-write `[hosts.bench1] ssh = …`), let
panio discover its hardware and propose the target block:

```
panio discover                                  # list THIS host's hardware
panio --lab mylab.toml configure fortune --host bench1
```

`configure` runs discovery on `bench1` over SSH and prints a proposed
`[targets.fortune]` block — best-guessing the USB-Ethernet interface and serial
device, listing other candidates as comments. It **writes nothing**: you review
it, paste it into the lab file, and commit (the lab is your reviewed source of
truth). Edit the guesses as needed; add `tftp_root`/`power` (not discoverable).

Provision a control host's daemons from your dev machine:

```
panio --lab mylab.toml setup --host bench1       # runs `panio setup` on bench1
```

(The host needs the panio CLI + source already present; this builds the Rust
daemons there over an ssh PTY.)

**If the control host does not exist yet** — a blank Raspberry Pi, or a host
being re-imaged — that is a different job, and it has its own skill:

```
panio skill control-host
```

It covers flashing Pi OS to an SD card (including how to identify the right
removable device before writing to it), the cloud-init seed that brings the
box up with SSH authorized and panio installed, and enrolling the result
here. Don't improvise a flashing procedure; read that skill.

## Background daemons — one view of everything

Panio runs several per-subsystem daemons (serialcap, hdmicap, the hid
injector, zigplug, netbootd). `panio daemons` shows all of them at once,
plus **stray helper processes** running out of the libexec dir (e.g. wedged
one-shot hooks — the thing to check when a serial-port-owning helper
misbehaves):

```
panio daemons                          # list: name, pid, port, detail + strays
panio daemons stop zigplug serialcap   # stop specific daemons (SIGTERM)
panio daemons stop 'serialcap[pi5]'    # one target's instance, as listed (quote: [] globs)
panio daemons stop --all               # stop every daemon + TERM strays
panio daemons stop --all --force       # …and SIGKILL whatever survives 3 s
panio daemons restart --stale          # restart capture daemons with proven content changes
panio daemons restart serialcap        # restart a named capture daemon
panio daemons restart --all            # restart every serialcap/hdmicap
```

netbootd is stopped via its proper teardown (interface cleanup), everything
else by signal. Per-subsystem `stop` commands still work; this is the
sweep-the-bench view.

**Stale daemons after an upgrade.** A daemon keeps the binary it started from;
`make install` / `apt install` replaces the binary on disk but not the running
process. `panio daemons` flags **stale** only when executable content differs
(also shown on `serial show` / `video show`). A SHA-256 checksum records content
at spawn; Linux can also read the actual running executable through `/proc`.
Identical bytes remain current after reinstalling, moving paths, or changing
modification times. Missing identity, inaccessible executable content, or
metadata changes without comparable content show **binary identity unknown**
with a reason. Do not infer a required restart from an unknown identity.
`panio daemons restart --stale` and automatic `serial watch` / `video watch`
restarts select only proven content changes. Named restart, `--all`, and
`video watch --restart` still intentionally restart a capture daemon, reusing
its lab channel config. netbootd is excluded — cycle it via
`panio netboot start/stop`.

**Untracked daemons: `show` says stopped, `watch` says already running.** A
daemon's discovery file is panio's only record of it, and on Linux `/tmp` is
swept on an age policy (Debian: 10 days), so a long-running daemon can lose that
file while still holding its capture device. `panio video show` reports it as
`running, untracked (pid N)` and `panio daemons` lists it under **Untracked
daemons**; `panio video watch` / `video stop` and `panio serial watch` /
`serial stop` reap it for you. Don't reach for `ps`/`kill` — and don't conclude
the device is free just because a daemon is gone from the tracked list.

## Quick reference — gotchas

- **Synced lab.** `set`/`add`/`rm` exiting 2 with "is synced … and read-only"
  means the lab is a copy from git or a URL (`panio config sync`). Don't `--detach` or
  `--force` to get an edit in; the change belongs in the source repository, so
  report it. A "last synced N days ago" warning means run `panio config sync`.

- **Leased target.** Exit 23 means someone holds the board or your token's
  lease ended; exit 24 means the last cleanup failed and the target is blocked
  for everyone: `panio lease status <target>`, never retry blindly. Reads still
  work on a leased target. `daemons stop|restart` of a leased target's daemon
  is refused too (this includes a target's netboot session, `netbootd[target]`);
  `stop --all` and `restart --all|--stale` skip them. `lease_acquire` over MCP
  waits at most 5 minutes; call it again to keep waiting.

- **USB fastboot RAM staging needs an external guarded runner.** Require fresh
  service entry for every artifact, separate bounded USB readiness, USB-host
  supervision with termination authority, and verified RAM contents. See
  `docs/fastboot.md`; `panio usb` is a media mux, not a fastboot channel.
- **Drive through panio, not around it.** Don't reconfigure or open a
  panio-managed device by hand (`ifconfig`/`ip`/`ethtool` on the netboot
  interface, `screen`/`tio` on a serial port, `kill` on a daemon) — it desyncs
  the daemon. Use `netif mode …`, `serial log`/`send`, `daemons stop`.
- `hid send` ending `helper_failed` with `child_exit` 41 (`ch9329`) means the
  send timed out and the daemon could not be shown to have stopped typing:
  check the target before sending more, and send `releaseall`. `child_exit`
  40 means it did stop.
- `doctor` flags a channel that still works? Believe the working channel and
  report the discrepancy — never rewrite the lab config to silence `doctor`.
- Serial port is exclusive: one of `connect` / `watch` / external `tio`/`screen`.
- `serial wait` ending `status: fault` (exit 0) means a lab fault rule's pattern
  arrived before anything else you waited for; read `fault.name` and the
  records, and check the target. It is an observation, not a diagnosis, and
  `fault_rules: []` means no rules were configured, not that the target is fine.
- `serial send` failing with *interface busy* (409) means a `serial transact`
  holds the console; nothing was sent. Wait for it, then send again.
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
- `~/.cargo/bin` (the CLI) must be on `PATH`, ahead of any other `panio`
  (e.g. a Homebrew keg from the tap can shadow it). The helper binaries are
  *not* on PATH — they live in `~/.local/libexec/panio/bin`
  (`panio helper <name> …` to run one by hand).
- `panio helper` runs on the local machine by default. For hardware on a
  control host, put `-t <target> --channel <kind>` (or `--host <labhost>`)
  **before** the helper name; flags after the name all go to the helper.
- `panio console` auto-starts the video daemon, and the serial daemon when
  the target has a serial channel (a KVM-only target opens as video + input,
  with no terminal pane), if they aren't running, and opens the browser on a
  link carrying a **single-use ticket** (expires after 2 minutes), never a
  daemon token. It prints that link; once the browser has used it, or it has
  expired, the link opens nothing ("expired or was already used"), so to hand
  the human a working link, rerun `panio console <target>` and pass on the
  fresh one. Inside an SSH session it opens no browser and prints an `ssh -N
  -L …` forwarding command (one port) and the link, for the human to run on
  their own machine. If `console` says the video daemon "predates single-use
  dashboard links", or that the serial terminal or KVM is disabled because the
  daemon was started without it, run `panio video stop <target>` and rerun
  `console`. A daemon left over from an older panio may have no token at all —
  a named `panio daemons restart` (unknown identity is excluded from `--stale`).
- Netboot requires passwordless `sudo` (`ip` on Linux, `ifconfig` on macOS).
- `netif down-hard` alone does not drop a USB adapter's carrier. If the target
  must see link loss, check `panio netif status` for `link hooks`: with none
  configured, ask for `netboot set --link-down-cmd/--link-up-cmd` (switch port
  hooks) rather than driving the switch by hand.
- netboot and ffx are mutually exclusive on the link — use `panio netif mode`
  to switch; entering ffx mode stops netboot so a power-cycle boots from SD.
- To test the bare link: `panio netif mode link` (up) / `mode off` (soft down).
  Soft "down" only releases the host IP — with Wake-on-LAN on, `carrier` can still
  read `up` (the PHY stays powered). `panio netif down-hard` (kills WoL +
  admin-downs the iface) still leaves the PHY up on common USB adapters, and the
  host then reports `carrier down` regardless; confirm from the target side, or
  unplug / disable a switch port for a guaranteed drop.
- Remote (lab) targets: if ssh fails with "Too many authentication failures",
  set the host's `identity` (agent key-spray); if the remote can't find panio,
  set its `panio_cmd` to an absolute path.
- OCR is strongest on large text; tiny console fonts may misread some characters.
- AMT bare addresses require verified HTTPS/16993. Update every power hook
  together for private CA trust or explicit legacy HTTP; never auto-downgrade.
- AMT-backed power hooks need a password source: `AMT_PASSWORD` in the
  environment of the `panio power …` invocation, or `--password-file` /
  `--password-command` in the hook (see the Intel AMT power example). A hook
  failing with `child_exit` 3 and "the AMT password is not set" (or a file or
  command error) means configure the secret, not a bug.

---

Licensed under the Apache License, Version 2.0.
