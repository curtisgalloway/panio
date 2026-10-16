<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# netbootd/ — agent notes

This file holds the internals of netbootd/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## netbootd (Rust netboot engine)

`netbootd/` is a single-binary DHCP + read-only TFTP + HTTP server — all as
tokio tasks in one process. It is the **only** netboot engine for `paniolo
netboot start` (originally ported from a pure-Python `_dhcp`/`_tftp` pair, since
removed).

The pure protocol logic is unit-tested (`dhcp.rs` / `tftp.rs` / `http.rs`
`#[cfg(test)]` modules): packet parse/build, the REQUEST verdict (ACK / NAK /
ignore), client-IP derivation, RRQ option negotiation, path-traversal
rejection, full loopback DATA/ACK transfers (multi-block, OACK,
retransmit-on-loss, error packets, a junk-sending peer, a duplicate RRQ), and
loopback HTTP (Content-Length bounding, HTTP/1.0 close, head timeout). A 65
K-round-trip block-wraparound test is marked `#[ignore]` — run it with `cargo
test -- --ignored`. `dhcp::serve` itself is deliberately *not* driven
end-to-end in tests (it shells out to `sudo arp`/`ip neigh` on every accepted
request, which a test must never trigger for real); `disposition` and
`accept_client`, the pure decision functions it calls, are what carry the
single-client test coverage instead — `disposition` holds the whole
per-packet decision, including the order the message type and the client lock
are settled in, so the tests exercise the sequence `serve` really runs. `tftp::run` *is* driven end-to-end over loopback (no such shell-out
exists there) for the peer-IP gate and the `MAX_TRANSFERS` semaphore bound,
including a stalled-transfer-then-freed-slot case (`tftp.rs`'s "Peer gating"
and "Transfer-slot bound" test groups).

Startup order is fixed: validate → bind and pin every listener → drop root
(Linux) → serve. Each server exposes `bind_server(port, interface)` (sync,
called from `main` before anything runs concurrently; `interface` is a
`pin::ListenerInterface` holding the macOS lock) and `serve(socket, …)`.

Key differences from the Python servers:

- **In-process MAC handoff.** The DHCP task publishes the client's hardware
  address to the TFTP task via `tokio::sync::watch` — no on-disk `client-mac`
  file.
- **Every listener is pinned to the netboot interface** (`src/pin.rs`:
  `IP_BOUND_IF` on macOS, `SO_BINDTODEVICE` on Linux) before it is bound, and
  a pin that fails is fatal — so is a DHCP or TFTP bind failure. `--interface`
  is required. The HTTP bind alone is non-fatal: a port clash logs a warning
  and netbootd runs DHCP + TFTP without HTTP Boot. `main.rs` reads the *actual*
  bound port back from the listener (`local_addr()`) and hands DHCP an
  `Option<u16>` — `Some(real_port)`, correct even for `--http-port 0`'s
  OS-assigned ephemeral port, or `None` when there is no listener at all.
  `dhcp::serve`'s `choose_boot` never builds an `http://` URL without a real
  bound port: an `HTTPClient` DHCPDISCOVER/REQUEST with no HTTP endpoint is
  ignored (rate-limited warning) rather than answered with a URL pointing at
  nothing (#144). The UDP listeners carry no
  `SO_REUSEADDR` (UDP has no TIME_WAIT, and on Linux it would let a duplicate
  daemon silently share the port); the TCP listener keeps it for TIME_WAIT.
  macOS listeners additionally use `SO_REUSEPORT`, allowing separate pinned
  interfaces to share the standard ports (#204). `pin::ListenerInterface`
  holds a system-wide, per-interface `flock` before any listener binds, until
  daemon exit; a duplicate direct launch fails too. The lock file stays in
  `/tmp` (never unlink a live lock). Linux socket options are unchanged.
  The netbootd CI job runs on both Linux and macOS, including real socket
  tests for concurrent binds, loopback ingress isolation, and restart.
- **Root is dropped on Linux** (`src/privdrop.rs`) right after the listeners
  are bound: `setgroups(empty)` → `setgid` → `setuid` to `SUDO_UID`/`SUDO_GID`,
  any failure fatal, then verified irreversible (`setuid(0)` must fail). The
  decision is the pure, unit-tested `drop_target`; the syscalls need a root
  process and are not unit-tested. The later `sudo ip neigh` / `sudo ip addr`
  shell-outs run as the dropped user, hence passwordless sudo on Linux. macOS
  is untouched (never privileged). All shell-outs made while serving go through
  `tokio::process` — none block a runtime worker.
- **One lease, derived from the host IP** (`dhcp::derive_client_ip`: host's
  /24, last octet 100, or 101 when the host is .100; `--client-ip` overrides,
  `validate_client_ip` refuses anything outside the /24). A REQUEST for another
  address is NAKed (`request_verdict`), one addressed to another server
  (option 54) ignored, and non-Ethernet / `hlen != 6` clients dropped.
- **One client, enforced by MAC** (`dhcp::accept_client`, sequenced by
  `dhcp::disposition`). The lease above is the address contract; this is the
  identity one: the first DISCOVER — or first REQUEST this server actually
  answers — locks in that hardware address as the active client for the
  process's lifetime, and a later request from a *different* MAC is ignored
  outright — no OFFER/ACK/NAK, just a `warn!` rate-limited via
  `served::warn_rate_limited` — so a second device on the link cannot steal
  the lease, the ARP pin, or the MAC handed to TFTP's raw-frame sender. A
  retransmission from the active MAC is unaffected (`accept_client` is
  idempotent for it). The message type is settled *before* the lock is
  touched: a DHCPINFORM/DECLINE/RELEASE, or a REQUEST addressed to another
  server (option 54), takes no lock no matter who sends it, so an unrelated
  host on the link cannot lock out the real target with a packet netbootd
  would never have answered (#166). A NAKed REQUEST does take it — a NAK is a
  reply. There is no lease timer and no in-process reset — `paniolo netboot
  start`/`stop` restarts the process per boot session, so a new client means
  restarting netbootd.
- **TFTP is gated to the leased client and bounded in concurrency**
  (`tftp::run`). Every RRQ/WRQ whose source IP is not the one DHCP leases on
  this link (`client_ip`, threaded from `main` into `tftp::serve`) is dropped
  silently. Concurrent transfers are capped by a `MAX_TRANSFERS`-permit
  (currently 4) `tokio::sync::Semaphore`: a permit is acquired with
  `try_acquire_owned` before a transfer task is spawned and moved *into* that
  task, so it releases whenever the task stops running — normal completion,
  a read/send error, or `abort()` when a repeated RRQ from the same TID
  replaces it — with no separate bookkeeping needed. An RRQ that finds every
  slot taken is dropped rather than queued (TFTP has no "try again shortly"),
  so a flood of unique-source RRQs cannot grow tasks, sockets, or open files
  without bound; a rate-limited `warn!` covers a sustained flood of either
  rejection.
- **Hardened against the far end of the link.** TFTP streams blocks from the
  file (`seek` + bounded read; nothing read whole), gives each retransmit
  attempt a fixed `Instant` deadline (junk cannot re-arm it), replaces an
  in-flight transfer on a repeated RRQ from the same TID (`abort` the old
  task), and caps `blksize` at 1468 when replies go out as raw frames
  (`cap_blksize`); `frame::build_udp_frame` refuses a payload the 16-bit
  length fields cannot describe. HTTP: 10 s head/body-drain timeout, 64
  concurrent connections (`Semaphore`), HTTP/1.0 defaults to close, and a body
  is exactly the announced `Content-Length` (grown file → cut, shrunk file →
  connection dropped). Peer-supplied names are passed through
  `served::loggable` (printable ASCII, capped) before they reach the log.
  SIGTERM shuts down cleanly like Ctrl-C.
- **Privilege-separated `/dev/bpf` on macOS.** The macOS raw-frame send path
  (the Sequoia workaround) needs a BPF descriptor, which only root can open.
  Rather than run the daemon as root, a tiny **setuid-root** helper —
  `netbootd-bpf-helper` — opens `/dev/bpfN` **write-only**, binds it
  (`BIOCSETIF`), sets `BIOCSHDRCMPLT`, installs a reject-all filter
  (`BIOCSETF`, `ret #0`), and passes the fd back over a `socketpair` via
  `SCM_RIGHTS` (`src/handoff.rs`), then exits. netbootd itself runs
  **unprivileged** and only `write(2)`s frames to the fd
  (`src/bpf.rs::BpfSender::from_handoff`); the fd cannot capture (`read(2)`
  fails — no `FREAD` — and the filter accepts nothing) and is `FD_CLOEXEC` so
  the daemon's `sudo arp`/`ifconfig` children never inherit it. The helper is
  the *only* component that runs as root; `paniolo setup` installs it setuid
  4755 (the one-time sudo), and it gates itself rather than relying on the
  mode: it refuses any caller whose real uid is not the owner of the directory
  it lives in (`handoff::caller_allowed` — the installing user, or root) and
  refuses the default-route interface (`handoff::interface_refused`, looked
  up by `route::default_route_interface` — `/sbin/route` by absolute path with
  an empty environment). If the helper is missing, not setuid, or refuses,
  netbootd logs its exit status and stderr and falls back to the kernel
  `send_to` path (broken on macOS 15+). `setup` will not setuid a symlink or a
  file the invoking user does not own.
- **Primary-NIC guard.** `netcfg::is_primary_interface` mirrors the Python
  guard; `main()` refuses to start, and `monitor_interface` refuses to enforce,
  on the default-route interface.
- **Layout.** `src/lib.rs` exposes `frame` (frame builder, unit-tested),
  `handoff` (BPF open + fd passing + the helper's caller/interface policy,
  unit-tested) and, on macOS, `route` (the default-route lookup) so both the
  `netbootd` and `netbootd-bpf-helper` binaries share them. The daemon-only
  modules are `dhcp`, `tftp`, `http`, `served` (shared path resolution + log
  sanitizer), `pin` (interface pinning), `privdrop` (the Linux root drop),
  `netcfg` (ARP pin + IP monitor shell-outs) and `bpf`. On Linux netbootd uses
  the kernel send path (no BPF), matching the Python behavior.
- **`paniolo netboot start` checks liveness.** After spawning, the CLI watches
  the child for ~2 s (`try_wait` + the named-process probe) and only then
  writes the state file; an early exit fails `start` with the last 20 log
  lines instead. It also refuses to start a second target on an interface
  another target's live netbootd already serves (`state::running_netboots`,
  one netboot per interface), and `netif::configure_interface` refuses a /24
  any other interface on the host already holds (one subnet per link:
  `model::netboot_subnet_clash` is the lab-file side, refused by `netboot
  set` for the link being edited and reported by `doctor` as `CONFLICT`; the
  live side lists the host's addresses). `target show` always prints the netboot host IP, `(default)`
  when unset.

