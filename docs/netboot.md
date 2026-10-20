# Netboot

panio netboots a target over a direct USB-Ethernet cable, with its own DHCP,
TFTP (the boot-ROM file protocol), and HTTP servers. No router, switch, or
upstream DHCP server is involved.

netbootd picks its reply from the client's DHCP vendor class (option 60):

| Client | How it boots | Served over |
|---|---|---|
| Raspberry Pi 5 bootloader | no vendor class → legacy reply | TFTP |
| UEFI **PXE** client (e.g. EDK2 on an Indiedroid Nova) | `PXEClient` → bootfile + `PXEClient` echo | TFTP |
| UEFI **HTTP Boot** client | `HTTPClient` → `http://` URL + `HTTPClient` echo | HTTP |

For UEFI clients, prefer HTTP Boot **where the firmware allows plain HTTP**:
it is faster and robust under host load. Many EDK2 builds allow only HTTPS; on
those, use **PXE**. See [UEFI clients](#uefi-clients-pxe-http-boot).

---

## Hardware setup

1. Plug a USB-to-Ethernet adapter into your Mac.
2. Cable the adapter directly to the target's Ethernet port (no switch needed).
3. Find the macOS interface name:

```bash
networksetup -listallhardwareports
```

---

## Target configuration

Config lives in the lab file (see [config-redesign.md](https://github.com/curtisgalloway/panio/blob/main/notes/config-redesign.md)),
as a per-target `netboot` channel:

```bash
# Create the target, then configure its netboot channel
panio target add target-machine
panio netboot set -t target-machine \
    --interface en3 \
    --tftp-root ~/src/fuchsia/pxe/tftp-root

# List candidate USB-Ethernet interfaces (primary NIC excluded)
panio netboot devices

# Show all configured targets / a specific one
panio target show
panio target show target-machine

# Remove the netboot channel, or the whole target
panio netboot rm -t target-machine
panio target rm target-machine
```

netboot channel fields:

| Field | Default | Description |
|---|---|---|
| `--interface` | (required) | USB-Ethernet interface name (e.g. `en3`). Every netbootd listener is pinned to it — see [Interface pinning](#interface-pinning) |
| `--host-ip` | `192.168.99.1` | Static IP assigned to the interface; also the TFTP/HTTP server address and the router the client is told about. The client's lease is derived from it (same /24, last octet `100` — `192.168.99.100` by default) — see [Lease](#dhcp-tftp-behavior-notes). **The default is for the first link on a host only**: every further link needs its own /24 — see [One subnet per link](#one-subnet-per-link) |
| `--tftp-root` | (none) | Directory whose contents are served over TFTP **and** HTTP |
| `--boot-file` | `kernel_2712.img` | Boot program (filename under the root, e.g. `grubaa64.efi`); served as a TFTP filename to PXE and wrapped in an `http://` URL for HTTP Boot |
| `--http-port` | `80` | HTTP server port; also embedded in the HTTP Boot URL (omitted from the URL when 80). `0` binds an OS-assigned ephemeral port — the URL always carries the port actually bound, never a literal `0` |
| `--content-type` | `application/octet-stream` | `Content-Type` for HTTP responses (UEFI treats octet-stream as an EFI application) |
| `--host` | target default | Lab host the channel lives on |

Power and DTR control live on the `power` channel (`panio power set …`, see
[power.md](power.md)).

---

## Starting and stopping

```bash
panio netboot start [target-machine]
panio netboot stop  [target-machine]
```

**Naming the target.** Omit `[target-machine]` when only one target is
configured. The runtime verbs (`start`, `stop`, `status`, `logs`, `tftp-root`)
also accept `-t/--target`, but not both at once. The config verbs (`set`, `rm`)
require `-t` and take no positional.

**`start`** assigns the static `host_ip` to the interface and launches
`netbootd`, a single Rust binary serving DHCP, TFTP, and HTTP. No `dnsmasq` or
`tftp-now` is needed.

`start` watches netbootd for about two seconds. If it exits (port in use,
interface it cannot pin, bad client IP), `start` fails with the last 20 lines
of its log (`netbootd exited with … during startup; last lines of
~/.local/share/panio/<name>/netboot.log: …`) and writes no state file.

**`stop`** sends SIGTERM (via `sudo kill` when the daemon is root's), waits up
to 3 s, SIGKILLs a holdout and waits 2 s more, then clears the state file and
restores the interface. It signals only a pid whose command line still names
`netbootd`. If the daemon survives SIGKILL, `stop` fails and keeps the state
file.

**One netboot per interface.** `start` refuses a second target on an interface
whose netbootd is alive (`netboot for '<other>' is already running on <iface>
… stop it first`). Give each target its own adapter.

### One subnet per link

Several targets can netboot from one control host, one adapter each, **as long
as every link is in its own /24.** Otherwise ssh, the AMT helper, ffx and
fastboot follow the routing table out whichever interface the kernel listed
first, and the target is reachable only some of the time.

The default `192.168.99.1` is for the **first** link on a host. Every further
link sets `--host-ip` in an unused /24 (`192.168.100.1`, `192.168.101.1`, …);
the client lease follows.

**One target per link.** A plain switch does not let several targets share
one adapter: netbootd leases a single fixed address per link (`.100` in the
link's /24) and serves one boot file, and it leases to any DHCP client,
including a managed switch's own management interface. Use one adapter per
target for now. Sharing one adapter through per-target VLANs on a managed
switch is tracked in
[#335](https://github.com/curtisgalloway/panio/issues/335).

panio enforces this:

- **`netboot set` refuses** a link whose /24 another target already uses on a
  different interface of the same host, default included: `target 'pi4'
  netboot: host_ip 192.168.99.1 (the default, since none is set) puts eth4 in
  192.168.99.0/24, which target 'optiplex' already uses on eth3 of host
  'bench1'`. An existing clashing lab file still loads (so `netboot stop`
  works); `panio doctor` reports it as `CONFLICT`.
- **`netboot start` and `netif mode link` refuse** when any *other* interface
  on the host holds that /24 now: `refusing to put 'eth4' in 192.168.99.0/24:
  'eth3' already holds 192.168.99.1 on this host`. Release it (`panio netif
  mode off <target>`) or pick another subnet.
- **`panio doctor` reports `MISMATCH`** when the interface holds IPv4
  addresses but not the effective host IP (a stale `netif mode link`, a
  hand-set address, an edit made while the daemon was down). Not a mismatch:
  no IPv4 at all (`netif mode off` or IPv6-only `mode ffx`), or the configured
  address alongside another. This applies to both local and remote checks;
  a successful empty address listing is not an unreachable host.
  Fix with `panio netif mode link <target>` or
  `netboot start <target>`.
- **`target show` and `netboot status` print the host IP**, marked `(default)`
  when unset.

Two targets sharing one adapter (re-cabled between boards) may keep the same
address.

**macOS and Linux can serve multiple netboot links concurrently**, each on
its own adapter and /24. On macOS, listeners use `SO_REUSEPORT` (shared port
binding) alongside `IP_BOUND_IF` (interface pinning). A process-held lock per
interface rejects duplicate netbootd instances even when launched directly,
and releases automatically on exit or crash. Its file under
`/tmp/panio-netboot-interface-<index>.lock` deliberately remains after exit;
do not delete it while a daemon is running. Linux retains exclusive
`SO_BINDTODEVICE` binds without port sharing. When upgrading from an older
macOS daemon, stop it before starting multiple links: its sockets do not
permit port sharing.

### Privileges and sudo

**Privileged ports (67/69, and 80 by default).** On macOS only assigning the
static IP needs sudo. On **Linux**, `start` runs `netbootd` and `ip addr add`
under `sudo`. Configure **NOPASSWD sudo** on the control host for unattended
use. To avoid a privileged HTTP bind, set `--http-port` high (e.g. `8080`); the
boot URL follows it.

**netbootd gives root back (Linux).** After binding and pinning, before serving,
netbootd drops to the invoking user (`SUDO_UID`/`SUDO_GID`) with
`setgroups`/`setgid`/`setuid`, and refuses to start if any fails. The log
records `dropped privileges to uid N gid M`. The ARP pin (`ip neigh`) and the
interface-IP monitor (`ip addr add`) then run through `sudo` as that user, so
passwordless sudo is required on Linux. Started as root without `sudo`,
netbootd stays root and logs it. On macOS netbootd never has root.

**HTTP is optional; DHCP and TFTP are not.** If the HTTP port cannot be bound,
netbootd logs a warning and keeps serving DHCP + TFTP (Pi and PXE still work).
An `HTTPClient` request then gets no offer, with a rate-limited log warning.
Free the port or set `--http-port`. A DHCP or TFTP socket that cannot be bound
or pinned is fatal.

**Interface safety.** `start` **refuses** an interface that carries the system
default route; reconfiguring it would break your networking. Use a dedicated
USB-Ethernet adapter.

### Interface pinning

netbootd leases to any DHCP client and serves the TFTP root to anyone, so every
listen socket (DHCP 67, TFTP 69, HTTP) is **pinned to the netboot interface**
before binding: `IP_BOUND_IF` on macOS, `SO_BINDTODEVICE` on Linux. Traffic on
other interfaces is never seen, and replies leave only via the netboot link.

A pin that cannot be applied is fatal, which is why `netbootd`'s `--interface`
flag is required. Sockets bind the wildcard address, so they survive link
flaps.

**TFTP reply sockets are pinned too, on both platforms.** On Linux they are
created after netbootd drops root (see [netbootd gives root
back](#privileges-and-sudo)); `SO_BINDTODEVICE` has not needed `CAP_NET_RAW`
since Linux 5.7 (Pi OS Trixie 6.x, Debian 12 6.1 are fine). On an older kernel
the pin fails with `EPERM`/`EACCES`; netbootd logs one `warn!` and binds the
reply socket to the interface IP only. Either way the reply socket binds to
`host_ip`, not the wildcard.

Without the reply pin, two links in the same `/24` send OACK and DATA out the
wrong interface and the log repeats `no ACK for OACK`. **To verify:** put one
target at `192.168.99.1/24` in [`netif mode
link`](netif.md#testing-the-link-up-and-down), `panio netboot start` a target
on the *other* link, and PXE-boot it; the log should show `completed <file>`.

**Just the link, no daemon.** To assign or release the host IP without serving
anything, use [`panio netif mode link`](netif.md) and `panio netif mode
off`. "Off" does not force the carrier down, and neither does
`netif down-hard` on many USB adapters. See
[Link mode](netif.md#testing-the-link-up-and-down).

### The netbootd engine

On macOS, netbootd sends raw frames through a `/dev/bpf` descriptor (BPF, the
raw-frame kernel interface), needed for reliable delivery on Sequoia.
`panio setup` (one sudo) installs a tiny **setuid-root** helper,
`netbootd-bpf-helper`, that opens `/dev/bpf`, binds the interface, and hands
the descriptor to the unprivileged `netbootd`. It is the only panio component
that runs as root. If it is missing or not setuid, netbootd warns and falls
back to the kernel send path, which is unreliable on macOS 15+.

The helper is restricted:

- **Only the installing user may invoke it** (root excepted); others get
  `refused: caller uid N is not the installing user (uid M)`.
- **It refuses the default-route interface** (the one `route -n get default`
  names): `refused: <iface> carries the default route`.
- **The descriptor is write-only** (`O_WRONLY`) with a reject-all `BIOCSETF`
  filter, so the daemon can inject frames but not capture.

A failed handoff is logged with the helper's message (`BPF handoff failed
(netbootd-bpf-helper exited with exit status: 1: refused: …)`).

---

## Status and logs

```bash
panio netboot status [target-machine]      # running? interface? uptime?
panio netboot logs   [target-machine]      # tail the combined DHCP + TFTP log
panio netboot logs -f [target-machine]     # follow
```

---

## Getting the TFTP root path

```bash
panio netboot tftp-root [target-machine]
```

Prints the bare path, for shell substitution:

```bash
TFTP_ROOT=$(ssh control-mac "panio netboot tftp-root target-machine")
scp kernel_2712.img control-mac:"${TFTP_ROOT}/kernel_2712.img"
```

An unset or empty `tftp_root` exits **3** (`not_configured`), with the target,
netboot channel, and channel host in the JSON error context. `netboot start`
and `netif mode netboot` reject the same missing configuration before touching
the interface or starting a daemon. `panio doctor [target]` reports it as
`MISSING` and exits **1**; these checks never create a directory or edit the lab.
Set the path explicitly with `panio netboot set -t <target> --tftp-root <dir>`.

---

## Expected TFTP sequence for Raspberry Pi 5

The 404s are normal:

```
404  <serial>/<mac>/start.elf    ← Pi 5 doesn't need it; 404 expected
200  config.txt
200  bcm2712-rpi-5-b.dtb
200  kernel_2712.img              ← your boot shim or kernel
```

The TFTP root must contain at least `config.txt`, `bcm2712-rpi-5-b.dtb`, and
`kernel_2712.img`.

---

## UEFI clients (PXE / HTTP Boot)

`netbootd` serves UEFI PXE and HTTP Boot (IPv4) from the same channel. You
only configure the boot program (NBP, network boot program):

```bash
panio netboot set -t indiedroid \
    --interface en7 \
    --tftp-root ~/indiedroid/boot-root \
    --boot-file grubaa64.efi      # any UEFI NBP: grubaa64.efi, ipxe.efi, a UKI…
panio netboot start indiedroid
```

**PXE (hardware-verified).** Pick **UEFI PXEv4** in the boot menu. A
`PXEClient` (arch 11 = ARM64 UEFI) gets the TFTP reply, a `PXEClient` echo,
and **DHCP option 43** with `PXE_DISCOVERY_CONTROL=0x08`, which tells it to
boot `boot_file` directly (strict EDK2 otherwise prints *"no valid offer
returned"*). The log shows `RRQ <boot_file> … completed`.

**HTTP Boot.** Pick **HTTP Boot (IPv4)**. An `HTTPClient` (arch 19 = ARM64
UEFI HTTP) gets the `HTTPClient` echo and an
`http://<host_ip>[:<http_port>]/<boot_file>` URL in option 67.
`panio netboot logs -f indiedroid` shows the `DISCOVER` (carrying
`HTTPClient:Arch:00019`), the offer, then `HEAD` + `GET /grubaa64.efi`. HTTP
Boot uses ordinary kernel TCP: no `/dev/bpf`, setuid helper, or static ARP
entry.

> **Many EDK2 builds reject plain HTTP.** With
> `PcdAllowHttpConnections=FALSE` (the default) the firmware reports
> *"HTTPS only"*. netbootd has no TLS, so on such firmware (e.g. the
> Indiedroid Nova) **use PXE**.

**IPv6 and HTTPS are not supported.** See
[`notes/uefi-http-boot-design.md`](https://github.com/curtisgalloway/panio/blob/main/notes/uefi-http-boot-design.md)
for the design and IPv6 future work.

---

## DHCP / TFTP behavior notes

**Lease.** One fixed lease **derived from `host_ip`**: same /24, last octet
`100` (or `101` when the host is `.100`). `192.168.99.1` leases
`192.168.99.100`; `10.20.30.1` leases `10.20.30.100`.

- Mask `255.255.255.0`, router `host_ip`.
- `netbootd` accepts a `--client-ip` override (in the host's /24, not the host,
  network or broadcast address, or it refuses to start). The lab file does not
  expose it.
- Both `siaddr` and **DHCP option 66** are set to `host_ip` (the Pi 5 EEPROM
  prefers 66).
- Replies go to the **limited broadcast `255.255.255.255`**, which EDK2
  requires.
- A DHCPREQUEST for any other address (option 50 or `ciaddr`) gets a
  **DHCPNAK**.
- A REQUEST addressed to another server (option 54) is ignored.
- Only Ethernet clients with a 6-byte hardware address are answered.

**Single client.** The first DISCOVER, or first REQUEST netbootd answers, locks
that MAC in for the life of the process. Any other MAC gets no reply, only a
rate-limited warning in `netboot logs`. DHCPINFORM, DHCPDECLINE, DHCPRELEASE,
and REQUESTs for another server cannot take the lock. To switch devices on a
link, stop and start netboot.

**TFTP.**

- **Read-only** (RFC 1350); negotiates `blksize`/`tsize`.
- **One source only**: requests from any IP but the lease are ignored.
- **At most `MAX_TRANSFERS` (4) transfers at once**; extra requests are
  dropped, not queued.
- **Streaming and timeouts.** Files stream block by block; each retransmit
  attempt has a one-second deadline, six attempts maximum.
- **Repeated RRQ** from the same client port replaces the transfer in flight
  (still one of the `MAX_TRANSFERS` slots).
- **Block size on macOS BPF** is capped at 1468 bytes.
- **No escapes.** Only regular files whose real path is under the root are
  served; a symlink out of the root gets TFTP `file not found` / HTTP 404.

**HTTP.** The server:

- sends exactly the announced `Content-Length` (a growing file is cut; a
  shrinking one drops the connection);
- closes HTTP/1.0 connections unless the client asks for keep-alive;
- drops a connection with no complete request within 10 s;
- serves at most 64 connections at once;
- sanitizes request names before logging them.

Both log to `~/.local/share/panio/<name>/netboot.log`.

> **Switching to ffx-over-network?** With NET-first boot order, a running
> netboot makes the next power-cycle TFTP-boot instead of the SD card. Use
> [`panio netif mode ffx`](netif.md) to stop netboot and ready the host IPv6
> side in one step.

---

## Runtime paths

| Purpose | Path |
|---|---|
| Daemon state (netbootd PID, uptime) | `~/.local/share/panio/<name>/netboot.json` |
| Combined log | `~/.local/share/panio/<name>/netboot.log` |

---

## Known issue: TFTP responsiveness under host load

A heavily loaded control host can starve TFTP so the client (e.g. the Pi 5
EEPROM) times out. The stopgap is raising the server's priority with `renice`
to a negative nice value. netbootd has not yet been re-tested under load;
making the send path load-robust is future work.

This affects TFTP clients only (the Pi, UEFI PXE). UEFI firmware that allows
plain HTTP can use [HTTP Boot](#uefi-clients-pxe-http-boot) instead.
