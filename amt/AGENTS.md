<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# amt/ — agent notes

This file holds the internals of amt/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
amt/             Rust crate: standalone helper for Intel AMT (vPro) machine
                 power over WS-Management (SOAP over HTTP, port 16992; ureq).
                 One-shot, stateless — the switch is the machine's own
                 Management Engine, so `state` is a true sensor (ME answers
                 with the host on, off, or bare-metal). HTTP Digest (MD5,
                 RFC 2617) implemented in-crate — AMT 11+ is Digest-only.
                 Addressed by `-d <host|ipv4|[ipv6]>[:port]` (strictly parsed;
                 anything else URL-shaped is rejected) and `-u <user>` (default
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
                 budget that also bounds each attempt — the AMT NIC drops link
                 around host power transitions. TLS AMT (16993) is unsupported
                 (clear error). Hardware-verified against a Dell OptiPlex 7060
                 (AMT 12). See docs/power.md.

```
