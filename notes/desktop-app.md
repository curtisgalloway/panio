<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# A desktop app for the lab: the console front door, with a window

> **Status: design only — nothing built.** Converged 2026-09-14. Shape chosen
> deliberately (Tauri desktop app) rather than inferred. Builds directly on
> [console-front-door.md](console-front-door.md), which diagnosed the same
> problem in June and worked out the proxy design; this note argues the app is
> that design's Phase C in a different runtime, and revises the phasing
> accordingly. The architecture's one load-bearing assumption was **spiked
> against a real daemon the same day and held** — see *Verification status*.

## What prompted it

"How do we make the dashboard easier to access and find the controlled
devices?" — asked after a session that ended with the video path 2.7x faster at
a seventh of the bandwidth (#209), which sharpened rather than answered the
question. A fast dashboard you cannot find is still hard to use.

## The problem, measured

Three frictions, and they compound.

**1. Nothing is bookmarkable, twice over.** Daemons launch with `--port 0`
(OS-assigned) and mint a fresh token on every start. A URL is dead the moment
the daemon restarts. Both properties are deliberate — ephemeral tokens limit
the blast radius of the leak in #196 — and together they mean no browser
bookmark, no pinned `ssh -L`, no reconnecting tab.

**2. One terminal per device.** `panio console <target>` is a foreground
process holding SSH tunnels and printing *"Tunnels to <host> open. Press Ctrl-C
to close."* Two targets means two terminals held open for as long as you want
to watch.

**3. The inventory is text.** `panio target list` prints name, host and
channels for all six targets. It tells you what *exists*, never what is
*happening* — no power state, no whether a daemon is up, no picture. Finding
"the box sitting at the BIOS menu" means opening three dashboards.

[console-front-door.md](console-front-door.md) named the first two in June as
*multiplicity* and *non-determinism* and is still the better write-up of them.
The third is new here, and it is the one an app is uniquely good at.

## Why an app rather than that note's Phase C daemon

The front-door design's Phase C is a persistent `panio consoled` serving a
landing page that lists every target and lazily brings up tunnels. That is the
same component this note proposes. The difference is only what hosts it — and
the note flagged the cost of hosting it as a daemon:

> **Cost / the line it crosses:** reintroduces a **local tunnel registry /
> transient runtime state on the dev machine** — exactly what `console
> --detach` was deferred over.

That objection is about *invisible* state. A background daemon holding tunnels
is state you have to remember exists, discover with `daemons list`, and clean
up when it goes wrong. An application window is the same state made obvious:
you launched it, you can see it, you quit it, and quitting is unambiguously
"drop everything." The Dock icon *is* the tunnel registry's UI.

So the app does not dodge the line Phase C crosses. It crosses it and makes the
crossing legible, which is what the deferral was actually protecting against.

Three further things fall out of choosing an app that a landing page does not
get: it can hold tunnels for its whole lifetime without a terminal; it can show
a thumbnail grid without anyone visiting a URL; and it can be launched from the
Dock by someone who does not currently have a shell open.

## Architecture

### Finding 1: navigate to the dashboard, never frame it

`hdmicap/src/auth.rs:150` rejects any request whose `Origin` is present and not
loopback. A Tauri webview's page origin is *not* `http://127.0.0.1:<port>`, so
every `fetch()` from the app's own window to a daemon would be refused — while
the MJPEG `<img src>` kept working, because a plain image load sends no
`Origin` at all. Video on, every button dead. A cruel way to discover it.

Framing is not the way around it either: `index()` sets
`Content-Security-Policy: frame-ancestors 'none'` and `X-Frame-Options: DENY`
on the dashboard page, deliberately.

The way through is neither. **Open each target's dashboard as its own Tauri
window, navigated directly to `http://127.0.0.1:<port>/?token=…`.** A
navigation is not a frame, so the anti-framing headers do not apply, and the
window's origin *becomes* the daemon's.

The check then passes by the route its own comment names first — *"Origin:
absent (CLI, same-origin page loads) or loopback"*. A same-origin fetch sends
**no `Origin` header at all**, so the page's requests take the `absent` branch
rather than the loopback one. Same outcome, and worth knowing precisely,
because it means the page never depends on the allowlist matching whatever
authority the tunnel happens to expose.

The app therefore has two kinds of window, with two different trust positions:

| Window | Origin | Talks to daemons how |
|---|---|---|
| **Grid** (one) | the app's own | never directly — Tauri IPC to the Rust side, which uses an HTTP client with no browser semantics |
| **Dashboard** (one per open target) | `http://127.0.0.1:<port>` | directly, same-origin, using `assets/index.html` **verbatim** |

The second row is the important one: the existing dashboard is reused
unmodified, and `hdmicap` needs no changes at all to support the app.

### Finding 2: the app drives the CLI, which forces a gap closed

`cli/` is a bin-only crate — there is no `lib.rs` — so the app either grows it a
library target or shells out to `panio`. **Shell out.** It keeps the CLI as
the single control plane, which is already the project's stated position, and it
means the app cannot drift from what the CLI does.

The catch: `panio target list --json` does not exist. Across
`cli/src/main.rs` only two call sites pass `--json` at all, both to helpers.

This is a feature of the plan rather than a snag. The app's data needs — target
inventory, per-target daemon status, power state — are **exactly the `--json`
surface the `cli-conventions` skill says this CLI should already have**, and
agents driving panio want it at least as much as the app does. Building the
app pushes the CLI toward its own documented conventions instead of around
them.

### What the app does *not* need to change

Because dashboard windows navigate to the daemon's own origin, the
**page-contract change** that [console-front-door.md](console-front-door.md)
identified as "the real work" — teaching the page to open same-origin relative
WebSockets instead of absolute `?serialws=` ones — is not required for v1. The
app injects whatever local ports it tunneled, exactly as `remote_console` does
today.

The cost of skipping it is inherited wholesale: the `?serialws=` override
stitches exactly **one** serial WebSocket, so a target with multiple serial
ports still cannot be fully presented over the remote path. See *Phasing*.

## The problem that does not have a cheap answer

A grid of live thumbnails needs a running `hdmicap` per target. Each one holds
its capture device and runs the capture loop continuously — measured on waldo
(Pi 5) against lab-optiplex-1 at 1080p: **~16 ms of CPU per frame, ~47.5% of one
core at 30 fps**, whether or not anyone is looking.

waldo hosts three targets. A grid that naively starts every daemon costs
**roughly 1.5 of that Pi's 4 cores, permanently, to populate a picture nobody
is watching** — on the same host that also runs netbootd and serialcap.

Two ways out, and the cheap one is genuinely cheap:

1. **v1: show only what is already running.** Targets without a daemon get a
   placeholder tile and a start button. Costs nothing, and matches how the lab
   is actually used — you have one or two targets live, not six.
2. **Later: make the capture rate follow demand.** 30 fps while a `/preview`
   stream is attached, ~1 fps when the only consumer is `/snapshot` polling.
   This is a direct extension of the `TARGET_FRAME_INTERVAL` work in #209 —
   the capture loop already has one rate constant and already knows when its
   receivers are gone (`all_receivers_gone`); this teaches it a second rate
   rather than a new mechanism. It composes well with #211, which would cut the
   per-frame cost itself.

Worth stating plainly because it is the kind of thing that derails a project in
week two: **a UI that watches many devices has a different cost model than a
console that drives one, and every daemon in this repo was built for the
second.** The grid is the feature that makes the app worth having and the one
that cannot be built naively.

## v1 scope

Deliberately small, on the principle that the enemy is abandonment.

**In:**

- A grid window listing every target from the lab file, with host, channels and
  power state.
- Live thumbnails for targets whose `hdmicap` daemon is **already running**,
  polled from `/snapshot` by the Rust side at ~1 Hz.
- Click a tile → open that target's dashboard in its own window, starting the
  daemon and opening tunnels if needed.
- Quit → drop every tunnel and leave no daemon the app started behind.
- macOS first, since that is where the work happens; Linux and Windows follow
  the existing cross-platform commitment rather than a separate effort.

**Out, deliberately:**

- Auto-starting daemons for targets just to fill the grid (see above).
- The reverse proxy and the page-contract change (Phase B below).
- Multi-serial over the remote path — inherited limitation, not a regression.
- Any change to `hdmicap`, `serialcap` or the dashboard page.
- Phone/remote access. The app is a dev-machine hub, and the hub is the only
  node allowed to reach every control host.

## Phasing, revised

[console-front-door.md](console-front-door.md) chose **B first, then C**: a
per-invocation foreground proxy, then a persistent daemon. Having picked an app,
that order inverts.

- **App v1 ≈ Phase C without the proxy.** Persistent process, landing surface,
  lazy tunnel bring-up — but each dashboard window points straight at its
  daemon rather than through a single origin. This is cheap *because* the app
  can navigate a window to an arbitrary origin, which a landing page in a
  browser tab cannot.
- **App v2 = Phase B, hosted by the app.** The Rust side runs the reverse proxy
  the front-door note designed; dashboard windows point at
  `http://127.0.0.1:<app-port>/<target>/`; the page learns relative same-origin
  routing and multi-serial works as a side effect. The proxy work does not
  change, only where it lives.

Inverting the order is safe because v1 touches nothing v2 would have to undo:
no daemon changes, no page changes, no new installed binary.

## Open questions

- ~~**Does the Tauri origin behave as assumed?**~~ **Answered 2026-09-14 by a
  spike against a live daemon — it does.** See *Verification status*. The
  estimate below no longer carries the risk of Phase B being pulled into v1.
- **Where does the crate live?** A `desktop/` member of the existing workspace
  keeps it close, but Tauri pulls in a webview toolchain and JS build step to a
  repo that currently builds ten clean Rust crates behind a Makefile, and CI
  runs all of them on three platforms. A separate repo keeps that cost off the
  main build at the price of a version-skew seam against the CLI it shells out
  to. Undecided; lean workspace, measure CI time before committing.
- **Which `--json` verbs, and in what order?** `target list` and a per-target
  status verb are the minimum. Worth designing as the CLI's contract rather than
  as the app's private API, since agents are the other consumer.
- **Signing and distribution.** macOS Gatekeeper wants a signed, notarized
  `.app`. The existing Homebrew tap and apt repo are built for CLI binaries, not
  bundles. Unsolved; not v1-blocking if it is run locally from a build.
- **What happens when a control host is unreachable?** The grid must degrade to
  "host down" per tile rather than hanging on an SSH connect for every target on
  that host.

## Verification status

Verified in this session, on this machine and on waldo:

- `hdmicap/src/auth.rs:150` rejects a non-loopback `Origin`; `index()` sets
  `frame-ancestors 'none'` and `X-Frame-Options: DENY`.
- `cli/` has no `lib.rs`; `panio target list --json` fails with
  `unexpected argument '--json' found`; only two `--json` call sites exist in
  `cli/src/main.rs`.
- Daemons are started with `--port 0` and write port + token into a per-target
  discovery file under `<runtime>/panio-<uid>/hdmicap/<target>/daemon.json`.
- `assets/index.html` is 28,871 bytes and contains the string `preview` exactly
  once — the video pane is one `<img>` and one assignment; the rest is power,
  OCR, serial and the pointer→HID mapping.
- Capture cost on waldo (Pi 5, 1080p MJPG, lab-optiplex-1): 9.49 s of CPU over
  20 s of streaming at 30 fps ≈ 16 ms/frame, 47.5% of one core.
- The lab declares six targets, three of them on waldo.

**Spiked and confirmed (2026-09-14, same day).** A throwaway Tauri 2.11 app on
macOS against a live `hdmicap` 0.3.1 on waldo, reached through an `ssh -L`
tunnel with a logging proxy in front of it so every request's `Origin` was
observed rather than assumed:

- The app window's origin is **`tauri://localhost`**, as predicted.
- A `fetch()` from that window is refused **twice over**: the daemon answers
  **403**, and the webview rejects the response for want of CORS headers before
  any of it reaches JavaScript. What the developer actually sees is
  `TypeError: Load failed` — an opaque client-side error, not a readable 403.
  Worth recording, because that symptom does not point at the cause.
- A window **navigated** to the daemon's URL loaded the whole dashboard: `/`,
  all three vendored xterm assets, the `/preview` MJPEG stream, and the page's
  own `/status` and `/power` calls — **every one 200**, every one carrying no
  `Origin` header at all.

So the two-window architecture stands, `assets/index.html` is reused verbatim,
and `hdmicap` needs no change. The spike is not kept; it was ~80 lines and a
generated icon.

**Still not verified:**

- Bundle size, build time and the real cost of Tauri in CI on three platforms.
  The spike's debug binary was 22 MB and its first build pulled the full
  `tauri`/`wry`/`muda` tree; neither number was measured in a release or CI
  configuration.
- Everything above is macOS only. Windows' origin is `http://tauri.localhost`,
  which `is_loopback_origin` would also reject — `is_loopback_authority` is an
  exact-match allowlist and `tauri.localhost` is not in it — so the same
  architecture should hold there, but it was reasoned, not run.
- That `/snapshot` polling at 1 Hz is cheap on the *daemon* side. It reads the
  warm buffer, so it should be, but the PNG encode is on the `expensive`
  semaphore shared with `/ocr` and was never measured under repeated polling.
