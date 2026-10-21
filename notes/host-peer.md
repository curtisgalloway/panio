<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Privileged host-peer traffic (#325)

Status: **design, not built** (2026-10-21). Decisions below were taken with the
maintainer; open questions are listed at the end.

**Terms**

- **Host peer** — a test program on the control host that exchanges traffic
  with the target over a link panio manages (the netboot Ethernet link, or the
  target's USB device port) and judges what arrives. It belongs to the
  bring-up, not to panio.
- **`AF_PACKET` raw socket** — a Linux socket that sends and receives whole
  Ethernet frames on one interface. Opening one needs `CAP_NET_RAW`.
- **File capability** — a Linux capability (`CAP_NET_RAW` here) attached to
  one executable with `setcap`, so that binary gets that one privilege without
  being setuid-root.
- **fd handoff** — one process opens a file descriptor and gives it to another,
  either over a unix socket (`SCM_RIGHTS`) or by letting a child inherit it.
  The receiver uses the descriptor without being able to open another one.
- **udev rule** — a Linux rule file that sets the owner, group and mode of a
  device node (here `/dev/bus/usb/...`) when the device appears.

## Problem

A ROCK 5T bring-up drove two host peers:

- **An Ethernet peer** (a test protocol over raw frames, EtherType `0x88B5`,
  plus a UDP mode). Its raw modes open an `AF_PACKET` socket on the netboot
  interface, enlarge the receive buffer with `SO_RCVBUFFORCE` (which needs
  `CAP_NET_ADMIN`), then drop root to the `sudo` caller.
- **A USB peer** (pyusb against the board's gadget, Google vendor ID). It runs
  under `sudo` only to get libusb access to the device node.

Each was installed as a root-owned copy under `/usr/local/libexec/` with its
own sudoers rule naming that exact path, and the bring-up pinned its hash.
**Every change to a peer therefore needed a person to reinstall it as root.**
The Ethernet peer moved five times in two days (its decision log, D77), and
the USB peer's install was another manual root step (D88). On a control host
whose agent account has no sudo at all, each reinstall stops the run until a
person is free.

## Decision: panio owns the privilege, not the peer

panio does **not** absorb the peer's protocol. Protocol logic is bring-up
specific and stays with the bring-up (the "no device-specific code in core"
rule). panio supplies the one privileged thing the peer needs, scoped to a
target's own link, and then runs the peer unprivileged from wherever it lives.

### Ethernet: fd handoff from a capability-bearing helper

```
panio peer run -t <target> [--rcvbuf BYTES] -- <peer command ...>
```

1. panio resolves `<target>`'s netboot interface from the lab file. A target
   with no `netboot` channel, or no `interface`, is refused
   (`channel_missing`). The interface is **never** taken from the command
   line: that is what makes the target choice explicit and authorized.
2. panio runs a small helper, `panio-peer-helper`, which is the only
   privileged component. On Linux it carries file capabilities
   `cap_net_raw,cap_net_admin+ep` (set once by `panio setup` or the package's
   post-install), not setuid. The helper:
   - checks its caller (`handoff::caller_allowed`'s rule: the installing user
     or root);
   - refuses the default-route interface (`handoff::interface_refused`), so a
     mistyped lab entry cannot hand the peer the host's uplink;
   - opens `AF_PACKET`/`SOCK_RAW` for the requested EtherType, binds it to the
     interface, applies `SO_RCVBUFFORCE` up to a fixed ceiling (D79's fix:
     the default buffer held only 160 frames of a line-rate burst);
   - passes the descriptor back over a `socketpair` with `SCM_RIGHTS`, and
     exits.
3. panio clears `FD_CLOEXEC` on the received descriptor, execs the peer
   command with it inherited, and exports `PANIO_PEER_FD=<n>`,
   `PANIO_PEER_IFACE=<name>` and `PANIO_PEER_ETHERTYPE=<hex>`.
4. The peer adopts the socket (`socket.socket(fileno=int(os.environ[...]))`
   in Python) instead of opening its own. That is the only change the peer
   needs, and it can keep its `sudo` path as a fallback while both exist.

This is the same pattern netbootd already ships on macOS
(`netbootd-bpf-helper`: a setuid helper opens `/dev/bpf`, applies policy,
hands the fd over `SCM_RIGHTS`, exits). The `handoff` module's fd-passing and
caller/interface policy are reused; the new part is the Linux `AF_PACKET`
open and the capability install.

The peer's UDP mode needs no privilege and needs no panio involvement.

**Not chosen:**

- *A fixed sudoers rule naming a panio launcher.* No peer change, but the peer
  still runs as root and the rule is per host.
- *A root panio daemon relaying frames to the peer.* Most isolation, but adds
  a frame-relay protocol and latency in front of line-rate burst tests, which
  is exactly where the bring-up lost frames (D79).

### USB: a udev rule, no runtime privilege

The USB peer needs read/write on one device node, not a capability. The
target declares the peer device in the lab file, e.g.

```toml
[targets.rock5t.peer]
usb = ["18d1:abcd"]   # example: vendor:product of the target's gadget, as lsusb prints it
```

and `panio setup` (Linux) writes one rules file,
`/etc/udev/rules.d/70-panio-peer.rules`, granting those IDs to `plugdev` (the
Debian convention for USB device access), then reloads udev and adds the user
to `plugdev` the way it already adds them to `dialout` and `video`. That
is one sudo step **per new device ID**, not per peer change. The peer then
runs as the user with no `sudo`. `panio peer run` needs nothing extra for USB;
`panio doctor` reports a declared ID with no rule installed.

## Lifecycle and observation

The acceptance check asks to "select, start, observe, and stop" the peer.

- **v1 — `peer run`, foreground.** panio holds the peer as a child, forwards
  signals, and exits with the peer's status. That makes `peer run` a new
  passthrough command, so it must be added to
  [docs/errors.md](../docs/errors.md#passthrough-commands) and use
  `error::shell_code` (a signal death is 128+N). The bring-up chain runs peers
  as a child with a `--out` JSON file today, so v1 covers its use.
- **v2 — `peer start` / `peer status` / `peer stop`.** A background peer
  tracked like other daemons (PID + identity under the per-target runtime dir,
  `daemons stop` aware), with its stdout/stderr in a 0600 log. Build only if a
  chain needs panio to own a peer across separate commands.

## Cleanup after failure

- The helper exits immediately after the handoff; there is no long-lived
  privileged process to leak.
- The raw socket's lifetime is the peer's: when the peer exits, crashes or is
  killed, the kernel closes it. Nothing to reconfigure on the interface.
- If the handoff fails (helper missing, capability not installed, interface
  refused), `peer run` fails **before** starting the peer, with a kind a
  caller can branch on, and says which: e.g. "panio-peer-helper lacks
  cap_net_raw; run `panio setup`".
- `peer run` never touches the interface's address or mode: that stays with
  `netif`/`netboot` (drive through panio, not around its devices).

## Tests the acceptance check asks for

- Unauthorized target: a target with no netboot channel, an interface that is
  the default route, and a caller other than the helper's owner are each
  refused, before any socket is opened (pure policy functions, unit-tested
  like `handoff`'s today).
- Cleanup after failure: a peer that exits non-zero and one killed by a signal
  give the right passthrough status, and the helper leaves no process behind;
  a handoff failure starts no peer. These execute the spawn path (per
  "test the platform split by executing it"), using a stub peer that reports
  whether it received a usable fd.
- The capability install and a real `AF_PACKET` exchange need Linux with the
  capability set: run in `scripts/ci-local.sh`'s VM or on a control host, not
  in unprivileged CI.

## Platform scope

Linux first: that is where the bring-up runs, and `AF_PACKET` is Linux-only.
macOS could reuse netbootd's BPF helper for a read-write descriptor, but no
user needs it yet. Windows: out of scope.

## Open questions

1. Command name: `panio peer run`, or a flag on an existing group
   (`panio netboot peer`)? `peer` reads better and leaves room for the USB half.
2. Is the helper a new binary, or a mode of `netbootd-bpf-helper` (renamed)?
   One privileged binary is easier to audit; two keep each one's policy small.
3. EtherType: fixed per call (`--ethertype 0x88b5`, default `ETH_P_ALL`
   refused) or any? Refusing `ETH_P_ALL` keeps the descriptor from capturing
   the host's other traffic on that link.
