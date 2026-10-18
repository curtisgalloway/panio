<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Console front door: one stable port, server-side fan-out

> **Status: design only — parked.** Captures a design converged on 2026-06-10.
> Not started; queued behind the hidrig/ch9329 work. Builds on
> [distributed-control.md](../docs/distributed-control.md) (the SSH-tunnel transport and
> the hub principle) and supersedes that doc's `?serialws=`-stitching approach to
> the remote dashboard. Phasing: **B first, then C** (see below).

## The problem

`panio console <remote-target>` is fragile to drive remotely because the ports
it forwards are **multiple** and **non-deterministic**. `remote_console`
(`cli/src/main.rs`) starts the daemons on the control host, reads each one's
**OS-assigned** port, opens a *separate* `ssh -L` forward per daemon (video,
serial, optionally hid), and bakes those ephemeral local ports into a single URL
via the dashboard's `?serialws=ws://127.0.0.1:<rand>/stream&hid=…` override.

Two distinct irritants are tangled here:

1. **Multiplicity** — one forwarded port per daemon, so the URL carries several
   port numbers, and a second outer SSH hop (control host → laptop) means
   re-forwarding all of them.
2. **Non-determinism** — the ports are OS-assigned, so they move every launch.
   You can't pin a static `ssh -L`, can't bookmark a URL, can't reconnect a tab.

A third latent gap rides along: the `?serialws=` override stitches exactly **one**
serial WebSocket, so a target with **multiple serial ports** can't be fully
presented over the remote/tunnel path today.

The goal: **one consistent port to the console server, which negotiates the rest.**
Point a browser (or one stable `ssh -L`) at a single fixed local port; the server
figures out — from the lab file — what channels the target has, brings up the
daemons and tunnels, and wires every pane (N serials, video, hid) behind that one
origin.

## The reframe: a *hub-side* proxy is not the proxy we rejected

[distributed-control.md](../docs/distributed-control.md) ("why multi-host rules out a
reverse-proxy") rejected making **hdmicap reverse-proxy serialcap**, because that
forces hdmicap *on one control host* to reach serialcap *on another* — violating
principle 1 (control hosts cannot be assumed to reach each other; only the dev
machine reaches everything).

That rejection does **not** apply here. This design puts the aggregator on the
**dev machine** — the hub — which is by definition the one node allowed to reach
every control host. A hub-side front door is the literal embodiment of "rendezvous
at the dev machine," fully consistent with the existing principles. The earlier
"we rejected reverse proxies" conclusion was about a *control-host* proxy; a
*hub-side* proxy is a different component and is sound.

## Considered and not chosen

- **Pin the daemon ports (fixed instead of OS-assigned).** Removes
  non-determinism but not multiplicity: still several forwards, still `?serialws=`
  in the URL, and it trades "ports move" for "ports collide" — two targets or two
  daemons on a host now need a port-allocation scheme to manage. A band-aid, not
  the single-front-door model asked for.
- **A per-host agent / RPC server (labgrid exporter, "Option B" in `AGENTS.md`).**
  Cleanest multiplexing and a natural home for locking, but it's the deferred
  "someday, only if board-farm scale demands it" option and trades away the
  zero-infrastructure identity. Out of scope here.

## The design

A small **reverse proxy on the dev machine** that listens on **one fixed local
port** and presents every pane of a target under a **single origin** with
path-based routing. It owns the SSH tunnels behind the scenes (exactly the ones
`remote_console` opens today) and proxies the daemons through them:

| Path (single origin) | Proxies to | Notes |
|---|---|---|
| `/` | hdmicap | dashboard page + video endpoints (`/preview`, `/ocr`, frame PNG) |
| `/serial/*` | serialcap (whole origin) | preserves serialcap's per-interface API (`/interfaces`, `/stream?iface=…`) → multiple serial ports for free |
| `/hid` | hid daemon WS | KVM input injection, when the channel exists |
| `/power*` | hdmicap power probe/actions | unchanged behavior |

The browser only ever knows `http://localhost:<FIXED>/`. One stable port to
forward on the outer hop; one URL to bookmark.

### The page-contract change (the real work)

Today hdmicap's embedded dashboard takes `?serial=PORT` / `?serialws=ws://…` /
`?hid=…` and opens **absolute** `ws://<host>:<port>/…` WebSockets. Under a single
origin, the page instead opens **same-origin relative** sockets derived from
`location` — e.g. `new WebSocket(wsBase + '/serial/stream?iface=console')`, where
`wsBase` is `location.origin` with `http`→`ws`. The `?serial*`/`?hid` query
contract goes away (or becomes a private detail of the proxy). This is the
substantive change: hdmicap's page learns relative routing, and the proxy knows
serialcap's per-interface stream path.

Because the proxy mounts serialcap **whole** under `/serial/`, the page's existing
"fetch `/interfaces`, build one xterm.js terminal per interface" logic works
unchanged for **N** serial ports — fixing the single-`?serialws=` limitation as a
side effect.

### Latency / security

The added hop is an in-process localhost proxy sitting in front of an SSH tunnel
that already exists — negligible. The fixed port binds to `127.0.0.1`; reachability
from a laptop stays an SSH-tunnel concern, so no new auth surface is introduced
(do **not** bind `0.0.0.0` without an explicit auth story).

## Phasing

### Phase B — per-invocation front door (do first)

`panio console <target>` binds the fixed local port, sets up the target's
tunnels (as it does now), runs the proxy in the **foreground**, and tears it all
down on Ctrl-C. One target at a time; re-run per session.

- **Optimizes:** kills both irritants at once; fixes multi-serial for free.
- **Keeps:** the property the docs protected — **no persistent local runtime
  state**; the proxy dies with the foreground process, like the tunnels do today.
- **Cost:** the page-contract change above, plus the proxy component.

Write the proxy as a component that *can* serve N targets, but in this cut drive
it for one target in the foreground — so Phase C is a switch-flip, not a rewrite.

### Phase C — persistent console daemon (later)

`panio consoled` (name TBD) listens on the fixed port long-lived, serves a
landing page listing every target in the lab, and **lazily** brings up
tunnels/daemons the first time `/​<target>` is opened. Bookmark
`http://localhost:<FIXED>/` once, forever; click any target; reconnect a tab
across sessions.

- **Optimizes:** the fullest "talk to the console server, it negotiates
  everything" ergonomics — a real server, not a CLI invocation.
- **Cost / the line it crosses:** reintroduces a **local tunnel registry /
  transient runtime state on the dev machine** — exactly what `console --detach`
  was deferred over ([distributed-control.md](../docs/distributed-control.md), "Console
  lifecycle"). Taken on deliberately, only because the persistent-tab ergonomics
  earn it.

## Open questions (for when this is un-parked)

- **Fixed port choice / collision policy** when two front doors run at once
  (two devs, two labs on one machine). A default with a `--port` override is the
  obvious start; Phase C's single daemon mostly dissolves this.
- **Proxy implementation:** in-process `axum`/`hyper` reverse proxy in the CLI vs.
  a tiny dedicated helper. Leaning in-process for Phase B (no new installed
  binary), revisit for C.
- **`?serialws=` deprecation path:** keep the old query override working for
  hand-opened dashboards during the transition, or cut it once the page is
  same-origin? (The `architecture.md` §7 note about the `ws://…:8724/stream`
  fallback for hand-opened pages is the compatibility surface to decide on.)
- **Where the proxy reads channel topology:** straight from the lab file at
  bind time (Phase B) vs. a `/targets` discovery endpoint it serves (Phase C).
