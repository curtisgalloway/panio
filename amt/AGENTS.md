<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# amt/ — agent notes

This file holds the internals of amt/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
amt/             Rust crate: standalone helper for Intel AMT (vPro) machine
                 power over WS-Management (SOAP over verified HTTPS, port 16993; ureq).
                 One-shot, stateless — the switch is the machine's own
                 Management Engine, so `state` is a true sensor (ME answers
                 with the host on, off, or bare-metal). HTTP Digest (MD5,
                 RFC 2617) implemented in-crate — AMT 11+ is Digest-only.
                 Addressed by `-d <host|ipv4|[ipv6]>[:port]` (strictly parsed;
                 https:// accepted; http:// requires --allow-insecure-http) and `-u <user>` (default
                 admin); password from AMT_PASSWORD, --password-file or
                 --password-command (first set wins; via the `secret` crate;
                 never the password itself in a flag or the lab file; a
                 credential failure exits 3). Commands: `status`, `state` (prints exactly
                 `on`/`off`; PowerState 2 = on, sleep/hibernate/off = off, any
                 other value is an error, never a guess), `on`, `off` (hard
                 power-off, confirmed as Off - Soft), `cycle [--delay-ms 3000]`
                 (off → confirm → delay → on → confirm; holds off any host not
                 already soft-off, so a sleeping/hibernating host cold-boots
                 rather than resumes). Requests and read-back retry transient
                 transport errors (connection/IO/DNS kinds only) within a ~20 s
                 socket-I/O budget — the AMT NIC drops link around host power
                 transitions. src/tls.rs wraps ureq's standard rustls connector
                 with an absolute socket deadline (including the handshake);
                 platform DNS resolution can exceed it. TLS/certificate failures are terminal;
                 redirects and HTTPS-to-HTTP fallback are forbidden. Trust uses
                 built-in public roots plus --tls-ca-file (AMT_TLS_CA_FILE
                 fallback, flag wins), read on the execution host; that env var
                 is not forwarded. No custom verifier, pinning, or mutual TLS.
                 Legacy HTTP/16992 requires --allow-insecure-http and warns on
                 stderr, also forwarded by successful state hooks. Explicit
                 HTTPS plus that flag, or HTTP plus a CA file, is rejected.
                 TLS requires compatible firmware algorithms, not just TLS 1.2.
                 See docs/power.md.

```
