<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# UEFI HTTP Boot (IPv4) for netbootd — design

> **Status:** implemented (PRs #57, #58) — PXE/IPv4 is hardware-verified on
> the Nova; HTTP Boot is served, but that board's EDK2 build only accepts
> `https://` URLs, so PXE is the working path there. Shipped behavior is
> documented in [netboot.md](../docs/netboot.md); this is the design record.
> **Design date:** 2026-06-13.
> Target hardware: **Indiedroid Nova** (Rockchip RK3588S) running Tianocore
> **EDK2** firmware, which offers PXE and UEFI HTTP Boot over IPv4 and IPv6.
> This doc designs **HTTP Boot over IPv4 only**. PXE/IPv4 is a near-sibling
> (§9) and IPv6 (both flavors) is explicitly out of scope (§9). It extends the
> existing `netbootd` engine; it does not replace the Raspberry Pi netboot path,
> which stays byte-for-byte as-is.

---

## 1. Why HTTP/IPv4

The Nova's EDK2 firmware can netboot four ways (PXE/HTTP × IPv4/IPv6). Of those,
**UEFI HTTP Boot over IPv4** is the right first target for panio:

- **It reuses almost everything.** `netbootd` is already a DHCPv4 server plus a
  file server over the dedicated point-to-point USB-Ethernet link. HTTP Boot
  needs the same DHCP handshake (with two extra options) plus an HTTP GET in
  place of the TFTP transfer.
- **It fixes a known weakness.** The TFTP path starves under host load (see
  [netboot.md](../docs/netboot.md#known-issue-tftp-responsiveness-under-host-load)):
  lock-step 512-byte (or negotiated) blocks, one ACK per block, application-level
  retransmit. HTTP runs over kernel TCP, which gives us flow control, loss
  recovery, and large windows for free. Big images (kernels, UKIs, installers)
  transfer far faster and don't fall over when the Mac is busy.
- **It sheds the macOS raw-frame hack.** The `/dev/bpf` send path and the static
  ARP pin exist *only* because the Pi bootloader is a silent client that never
  answers ARP (see [tftp.rs](../netbootd/src/tftp.rs) and
  [dhcp.rs](../netbootd/src/dhcp.rs)). A UEFI client has a complete IP/ARP/TCP
  stack: it answers ARP, so the host kernel delivers to it normally. The HTTP
  transfer needs **no BPF, no setuid helper, no static ARP** — see §5.3.

PXE/IPv4 (§9) is genuinely smaller to build, but it inherits the TFTP frailty.
HTTP is the destination worth investing in; we can land PXE/IPv4 alongside it for
near-zero extra cost since the DHCP branching is shared.

---

## 2. How netbootd works today (the parts we touch)

`netbootd` (`netbootd/src/`) is one binary running two tokio tasks:

- **`dhcp::serve`** — binds `0.0.0.0:67`, parses BOOTP/DHCP, answers
  `DISCOVER→OFFER` and `REQUEST→ACK` for a single fixed lease
  (`192.168.99.100`). The reply sets `siaddr`, option 66 (TFTP server, as a
  dotted-quad **string**), option 67 (bootfile), and the BOOTP `file` field. It
  also pins a static ARP entry for the silent Pi and publishes the client MAC to
  the TFTP task. `parse_request` currently extracts **only** xid, chaddr, and
  message type — it does not look at option 60 or 93.
- **`tftp::serve`** — binds `0.0.0.0:69`, read-only RRQ with `blksize`/`tsize`
  negotiation, traversal-safe path resolution (`resolve()`), and the macOS BPF
  send path for the silent client.

The bootfile is hardcoded: `netbootd --boot-file` defaults to `kernel_2712.img`
(`main.rs`), and the CLI never passes `--boot-file` (`cli/src/netboot.rs::start`).
`NetbootChannel` (`cli/src/model.rs`) has no `boot_file` field at all.

Everything Pi-specific — the `192.168.99.100` single-client pin, the static ARP,
option 66 as a string, the BPF send path — stays untouched on the legacy path.
The UEFI path is built **beside** it and selected per-request by the client's
vendor class (§5.5).

---

## 3. The UEFI HTTP Boot/IPv4 protocol

UEFI HTTP Boot is a two-step exchange in the single-server (no separate DHCP)
model panio uses, where `netbootd` is *both* the address server and the boot
server.

### 3.1 DHCP handshake

The EDK2 HTTP Boot driver sends a normal DHCPv4 `DISCOVER`/`REQUEST`, but with:

| Option | Name | Value from an ARM64 EDK2 HTTP Boot client |
|---|---|---|
| 60 | Vendor Class Identifier | `HTTPClient:Arch:00019:UNDI:003000` (begins `HTTPClient`) |
| 93 | Client System Architecture | `0x0013` = **19** (ARM 64-bit UEFI, HTTP) |
| 55 | Parameter Request List | includes 60, 67, … |
| 94, 97 | Client NDI, UUID | informational |

The server's reply **must**:

1. **Echo option 60 set to a string beginning `HTTPClient`.** This is not
   optional. The EDK2 driver validates the *server's* option 60 to confirm the
   offer is a real HTTP Boot offer and to distinguish it from a PXE offer. Omit
   it and the client rejects the offer ("not a valid HTTP boot offer") and never
   issues the GET. (Contrast PXE, where the option-60 echo is usually optional.)
2. **Put a full URI in option 67 (Bootfile-Name):**
   `http://192.168.99.1/boot.efi`. Not a bare filename — the whole URL. We use
   an **IP-literal** URL so no DNS server (option 6) is needed on the
   point-to-point link.
3. Set the usual address fields (yiaddr, subnet, router, lease). `siaddr` and
   option 66 are irrelevant to HTTP and are omitted on this path.

Architecture codes for reference (RFC 4578 + IANA): ARM64 UEFI PXE = **11**
(`0x000B`), ARM64 UEFI HTTP = **19** (`0x0013`); x86-64 counterparts are 7/9 and
16. We branch on the option-60 prefix, not the arch number, but parse 93 for
logging and future multi-arch use.

### 3.2 HTTP transfer

After the ACK the client resolves the URI (trivial — it's an IP literal) and
fetches it over HTTP/1.1 from `192.168.99.1`. EDK2's `HttpBootDxe`:

- issues a **HEAD** to read `Content-Length`, then a **GET** (some versions GET
  directly) — we must support **both methods**;
- relies on a correct **`Content-Length`**; we always know the file size, so we
  always send it and avoid chunked transfer entirely;
- inspects **`Content-Type`** to classify the payload. For a plain `.efi` network
  boot program, `application/octet-stream` is accepted and the binary is loaded
  and executed as an EFI application. (ISO/disk-image MIME types exist for other
  payloads; out of scope here — we serve an EFI app.)
- expects a **200** with the body; we serve directly, no redirects.

Once the NBP runs, any further fetches (e.g. GRUB reading `grub.cfg`, or an iPXE
script chainloading more files) are just additional HTTP GETs to the same root —
the server handles them identically.

---

## 4. Scope

**In scope:** UEFI HTTP Boot over IPv4 against a single client on the existing
point-to-point link; serving one EFI NBP (and any follow-on files it requests)
from the existing served root; the lab/CLI config to drive it.

**Out of scope (this doc):** IPv6 (needs DHCPv6 + Router Advertisements — a new
subsystem); HTTPS/TLS (the link is a private point-to-point cable); HTTP Range
requests (small NBPs don't need them; revisit if a payload does); multi-client
leasing; serving ISO/disk-image ramdisks.

---

## 5. Design

### 5.1 DHCP: parse the client class, branch the reply

Extend `parse_request` (`dhcp.rs`) to also capture:

- **option 60** (vendor class identifier) → `Option<String>`
- **option 93** (client arch) → `Option<u16>` (for logging / future use)

Then branch in `serve`:

```text
match client_class {
    Some(c) if c.starts_with("HTTPClient") => build_http_reply(...)   // §5
    Some(c) if c.starts_with("PXEClient")  => build_pxe_reply(...)    // §9, optional now
    _                                       => build_reply(...)       // legacy Pi path, unchanged
}
```

`build_http_reply` differs from today's `build_reply` only in the options block:

- option 53 (message type): OFFER / ACK — same as today;
- option 54 (server id), 51 (lease), 1 (subnet), 3 (router): same as today;
- **option 60 = `"HTTPClient"`** (the mandatory echo);
- **option 67 = `format!("http://{host_ip}:{port}/{boot_file}")`** (the URL; the
  `:{port}` is omitted when port is 80);
- **no option 66, no `siaddr`, empty `file` field** (URLs can exceed the 128-byte
  `file` field; option 67 is the carrier).

The DHCP reply is still a UDP broadcast to the `.255` address on port 68 over the
ordinary socket — that mechanism already works on macOS for the Pi, so nothing
changes there.

### 5.2 New HTTP server task

Add a third tokio task, `http::serve`, to `main.rs` alongside DHCP and TFTP.
It is **always on** — the client picks its protocol by how it DHCPs, so we don't
need a mode flag; a single `netbootd` instance can serve the legacy Pi (TFTP),
a UEFI PXE client (TFTP), and a UEFI HTTP client (HTTP) from one config.

Minimum viable HTTP/1.1 server, read-only:

- `GET` and `HEAD` only; everything else → `405`.
- Path resolution **reuses `resolve()`** from `tftp.rs` (traversal-safe, rooted
  at the served directory) — lift it to a shared module so both servers share it.
- Always send `Content-Length` (we read the file fully or `stat` it); never
  chunk.
- `Content-Type`: default `application/octet-stream`; allow a per-channel
  override (some payloads want a specific MIME). Map by extension only if we
  later need ISO/img support.
- `200` for a hit, `404` for a miss, `405` for an unsupported method. No
  redirects, no keep-alive complexity required (`Connection: close` is fine,
  though keep-alive is cheap to support and EDK2 tolerates either).
- Bind `0.0.0.0:{http_port}`.

**Port choice / privilege.** Port 80 is privileged on Linux (needs
root/`CAP_NET_BIND_SERVICE`) — but the CLI already spawns `netbootd` under `sudo`
on Linux for ports 67/69, so 80 rides along at no extra cost. On macOS the
existing rootless-privileged-port behavior that covers 67/69 should cover 80.
Because the port is embedded in the boot URL (option 67), we can also choose an
**unprivileged high port** (e.g. `8080`) to sidestep privilege entirely — the
client honors the port in the URL. Decision in §8.

**Implementation choice — hand-rolled vs. a framework.** Two options:

- **Hand-rolled async HTTP/1.1 GET/HEAD over `tokio::net::TcpListener`** (~120
  lines, **zero new dependencies**). This matches `netbootd`'s entire ethos: it
  hand-rolls DHCP and TFTP from raw bytes precisely to stay a tiny,
  dependency-light, audit-able daemon. A read-only single-root GET/HEAD server
  needs no routing, middleware, or TLS.
- **`axum`** (already a workspace dep via `hdmicap`). More batteries (and an easy
  `tower-http::ServeDir`), but it pulls hyper + tower into a daemon that today
  has none of that, for a server with two methods and one resource tree.

**Recommendation: hand-roll it.** The surface is trivial and the no-dep,
hand-rolled style is exactly what the rest of `netbootd` already commits to.
`axum` is the fallback if we later want Range support or HTTP niceties cheaply.

### 5.3 Why the macOS BPF/ARP machinery doesn't apply

The Pi bootloader never answers ARP, so on macOS 15+ the kernel misdelivers TFTP
DATA frames even with a static ARP entry — hence the `/dev/bpf` raw-frame send
path and the setuid `netbootd-bpf-helper`. A UEFI client is the opposite: it
completes DHCP, owns an IP, and answers ARP. The host kernel's TCP stack
delivers to it with no help. Therefore the HTTP path:

- needs **no** BPF descriptor, **no** setuid helper, **no** static ARP pin;
- works the same on macOS and Linux through the ordinary socket API.

The DHCP exchange already works on macOS today (the Pi gets its lease), so the
HTTP path adds only a standard TCP listener. This is a real simplification, not
just a reuse.

### 5.4 Lab schema + CLI surface

Add to `NetbootChannel` (`cli/src/model.rs`), both optional:

- `boot_file: Option<String>` — the NBP path relative to the served root (e.g.
  `boot.efi`, `grubaa64.efi`, `ipxe.efi`). Shared with the future PXE path, where
  it's advertised as a bare filename instead of wrapped in a URL.
- `http_port: Option<u16>` — defaults per §8.
- (optional) `content_type: Option<String>` — override the default
  `application/octet-stream`.

Wire them through `resolved_target` / `push_opt`, `validate` (no new host refs),
and the `netboot set` editor in `cli/src/labfile.rs`. Then `cli/src/netboot.rs::start`
passes `--boot-file`, `--http-port` (already-needed flags) to `netbootd`, which
gains the matching clap args. The served root is the existing `tftp_root`
(consider renaming the concept to `boot_root`/`serve_root` in docs since it now
backs both TFTP and HTTP — but keep the field name for compatibility, or migrate
deliberately; see §8).

CLI examples (proposed):

```bash
panio netboot set -t nova \
    --interface en7 \
    --tftp-root ~/nova/boot-root \
    --boot-file grubaa64.efi
panio netboot start nova
# netbootd now answers an HTTPClient DISCOVER with
#   option 60 = HTTPClient, option 67 = http://192.168.99.1/grubaa64.efi
# and serves grubaa64.efi (and grub.cfg, …) over HTTP.
```

### 5.5 Keeping the Pi path intact

The branch in §5.1 keys on option 60. The Pi sends no `HTTPClient`/`PXEClient`
class, so it falls to the **default arm — the current `build_reply`, unchanged.**
The Pi keeps getting option 66 + bootfile + the static ARP pin + the BPF TFTP
transfer. No regression risk to the verified Pi 5 boot path; the UEFI logic is
purely additive.

---

## 6. Testing plan

**Unit (DHCP, mirroring the existing `dhcp.rs` tests):**

- `parse_request` extracts option 60 and option 93 when present; still parses
  when they're absent (Pi case).
- An `HTTPClient` DISCOVER yields a reply whose option 60 == `HTTPClient` and
  whose option 67 == the expected `http://…/…` URL; option 66 and `siaddr` are
  absent.
- A class-less DISCOVER still yields today's exact Pi reply (regression guard).

**Unit/loopback (HTTP, mirroring the `tftp.rs` loopback tests):**

- `GET` of a known file returns 200 + correct `Content-Length` + body bytes.
- `HEAD` returns the same headers with no body.
- Traversal (`../secret`) is rejected (shared `resolve()`).
- Missing file → 404; unsupported method → 405.

**Hardware bring-up (the Nova):**

1. `panio netboot set`/`start` as in §5.4 with a real ARM64 NBP in the root.
2. In EDK2's boot menu choose **HTTP Boot (IPv4)**.
3. `panio netboot logs -f nova` should show: a `DISCOVER` carrying
   `HTTPClient:Arch:00019`, the OFFER/ACK, then `HEAD` + `GET /grubaa64.efi`, then
   the transfer completing.
4. Confirm the NBP executes (GRUB menu / iPXE prompt / kernel banner on the
   serial console via `panio serial`/`console`).

---

## 7. Wire-level reference

DHCP reply options on the HTTP path (TLV in the options block after the magic
cookie `63 82 53 63`):

| Tag | Name | Value |
|---|---|---|
| 53 | Message type | OFFER (2) / ACK (5) |
| 54 | Server identifier | `host_ip` (4 bytes) |
| 51 | Lease time | 12 h (as today) |
| 1 | Subnet mask | `255.255.255.0` |
| 3 | Router | `host_ip` |
| 60 | Vendor class | `HTTPClient` (ASCII) — **required echo** |
| 67 | Bootfile name | `http://<host_ip>[:<port>]/<boot_file>` (ASCII) |
| 255 | End | — |

Omitted vs. the Pi path: option 66 (TFTP server) and a non-zero `siaddr`.

---

## 8. Resolved decisions

Settled 2026-06-13:

1. **Default `http_port` = `80`.** Cleanest boot URL (no `:port` in option 67)
   and it rides the existing sudo/rootless-privileged-port story that already
   covers 67/69. `--http-port` overrides it. One verification gate during
   implementation: confirm macOS binds `0.0.0.0:80` rootless exactly as it does
   67/69; if it doesn't, the override is the escape hatch and we revisit the
   default.
2. **HTTP server: hand-rolled** async GET/HEAD over `tokio::net::TcpListener`,
   zero new dependencies — matching the rest of `netbootd` (§5.2). `axum` was the
   alternative; not taken.
3. **`tftp_root` keeps its field name**, clarified in docs to note it now backs
   both TFTP and HTTP. No `serve_root` rename/alias — not worth the churn.
4. **PXE/IPv4 lands in the same PR**, as a follow-up commit. The DHCP branch is
   shared, so the `PXEClient` arm (option-60 echo + bare-filename option 67,
   reusing today's TFTP server) is nearly free and gives the Nova a second
   working method.

---

## 9. Out of scope / future

- **PXE/IPv4** — the smallest method; just `boot_file` made configurable + an
  optional `PXEClient` option-60 echo, reusing today's DHCP+TFTP. A natural
  sibling commit (decision §8.4). Inherits TFTP's under-load frailty, which is
  exactly why HTTP is the strategic target.
- **HTTP/IPv6 and PXE/IPv6** — need a **DHCPv6** server (UDP 547, multicast
  `ff02::1:2`, a different wire format; boot URL in option 59) and almost
  certainly an **ICMPv6 Router Advertisement** sender so the client forms a
  routable address + default route on the point-to-point link. panio's only
  IPv6 today is `netif`'s `fe80::1/64` link-local for Fuchsia ffx — no RA, no
  DHCPv6. A genuine new subsystem; defer until an IPv6 requirement is concrete.
- **HTTP Range requests / keep-alive tuning** — only if a payload needs them.

---

## 10. Doc & PR checklist (when this is built)

Per the repo's standing PR checklist, the implementation PR must also update:

- **[netboot.md](../docs/netboot.md)** — document HTTP Boot, the `boot_file`/`http_port`
  fields, and that one `netbootd` serves Pi/PXE/HTTP from one config; revise the
  TFTP-starvation note to point at HTTP as the robust path for UEFI clients.
- **[AGENTS.md](../AGENTS.md)** — netboot capabilities / lab schema.
- **`skills/panio/SKILL.md`** — the `netboot set --boot-file/--http-port` surface.
- **README** — if the feature list mentions netboot methods.
- **mkdocs.yml** — this design record is added to `exclude_docs` (a point-in-time
  design doc, like `config-redesign.md`); the *user-facing* HTTP Boot
  documentation lives in `netboot.md`, which is already in the nav.

---

## 11. Hardware findings — Indiedroid Nova / EDK2 (2026-06-13)

First real-silicon bring-up against a Tianocore EDK2 board (Indiedroid Nova,
RK3588S). The plain-HTTP design met three obstacles a loopback test could never
surface; netbootd was fixed for two of them, and **PXE/IPv4 is now verified
end-to-end** (the firmware netbooted a UEFI Shell). Status by method:

| Method | Result |
|---|---|
| **PXE / IPv4** | ✅ verified end-to-end (TFTP'd a 1 MB UEFI Shell, dropped to `Shell>`) |
| **HTTP / IPv4** | ⛔ blocked by firmware policy (HTTPS-only), see #3 — the netbootd side is correct up to that wall |

### 1. DHCP must use the limited broadcast `255.255.255.255` (fixed)

netbootd originally broadcast replies to the *subnet-directed* address
(`192.168.99.255`). A strict UEFI IP4 stack sits at `0.0.0.0` with no address on
that subnet and **drops the packet at the IP layer** — it only accepts the limited
broadcast `255.255.255.255` (which RFC 2131 §4.1 mandates here anyway). The Pi
firmware is lenient and accepted the directed broadcast, so this never showed up
before. Symptom: endless `DISCOVER → OFFER` with no `REQUEST`. **Fix:** reply to
`255.255.255.255`, with the DHCP socket pinned to the netboot interface
(`IP_BOUND_IF` / `SO_BINDTODEVICE`) so it still egresses the right link.

### 2. UEFI PXE needs DHCP option 43 (fixed)

A `PXEClient` offer with just the bootfile + `PXEClient` echo makes EDK2 complete
DHCP and then go hunting for a boot server (BINL, port 4011); netbootd doesn't
answer, so the client prints *"no valid offer returned."* **Fix:** add option 43
(vendor-specific) with `PXE_DISCOVERY_CONTROL=0x08` (`06 01 08 ff`), which tells
the client to boot the bootfile named in this very offer and skip discovery. With
that, the Nova TFTP'd the NBP and booted. (Minor: the client requested RFC 7440
`windowsize`, which netbootd doesn't negotiate; it retried once without it and
completed — a possible future TFTP enhancement, not a blocker.)

### 3. EDK2 HTTP Boot enforces HTTPS-only (firmware policy — not fixed here)

EDK2's HTTP Boot ships with `PcdAllowHttpConnections=FALSE`, so the firmware
**rejects a plain `http://` URL and demands `https://`** ("HTTPS only"). On the
Nova there was no runtime toggle to allow HTTP. netbootd serves plain HTTP (TLS
was explicitly out of scope, §4), so HTTP Boot is unreachable on such firmware.
This makes **PXE the reliable UEFI path on locked-down EDK2**, and re-frames the
original "HTTP Boot recommended" stance: prefer HTTP only where the firmware
permits plain HTTP, otherwise PXE. Adding HTTPS would mean a TLS listener *plus*
enrolling a CA/cert into the firmware's UEFI TLS trust store — a real subsystem,
deferred with IPv6.

### 4. Board prerequisite: the NIC MAC eFuse (not a netbootd issue)

The Nova's RTL8168H shipped with a **blank MAC eFuse** → UEFI read `00:00:00:00:00:00`
→ DHCP completed but the zero MAC broke unicast (ARP/TCP), so even fixes #1–#2
stalled until a real MAC was burned into the eFuse. That's a board bring-up step,
documented in the companion `nova-bringup` repo (`docs/ethernet-mac-efuse.md`,
`scripts/nova-macburn.sh`), not in panio.
