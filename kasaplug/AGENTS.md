<!--
SPDX-FileCopyrightText: 2026 The Panio Authors
SPDX-License-Identifier: Apache-2.0
-->

# kasaplug/ — agent notes

This file holds the internals of kasaplug/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
kasaplug/        Rust crate: standalone helper for TP-Link Kasa smart plugs
                 and power strips over the device's legacy local protocol
                 (TCP 9999: 4-byte big-endian length + XOR autokey cipher,
                 key 171; std::net, no HTTP). One-shot, stateless — no
                 daemon, one request per connection. Addressed by
                 `-d <ip|host[:port]>` and `[id]` outlet (default 0): the
                 index in get_sysinfo's `children` for a strip (the app's
                 "Plug 1" is 0), 0 for a plug's own `relay_state`. A strip's
                 request names the outlet in `context.child_ids`. Commands:
                 `status [id]`, `state [id]`, `on/off [id]`, `cycle [id]`;
                 `on/off/cycle` confirm by re-reading get_sysinfo. `doctor
                 [id] [--json]`: read-only (get_sysinfo only, never
                 set_relay_state), checks reachable/auth/switch, `--json`
                 exits 0; a refused TCP 9999 is `reachable` ok + `auth` fail
                 (newer firmware turns the legacy API off; the Kasa app's
                 Third-Party Integration turned it back on for an HS300 hw
                 2.0, fw 1.1.2). KLAP is not implemented. Tests drive the
                 real client against a loopback TCP stub speaking the
                 protocol. src/main.rs: CLI + command logic; src/proto.rs:
                 framing, cipher, get_sysinfo/set_relay_state/emeter.
                 Client::call sets one deadline (TIMEOUT, 3 s) per
                 exchange; connect, send and every read share it, so a
                 peer dripping bytes cannot stretch it, and a reply over
                 MAX_REPLY (64 KiB) is refused. The protocol has no login,
                 so every device string (aliases, model, err_msg) goes
                 through printable() at parse time, as cli/src/doctor.rs
                 does; the child id stays verbatim (sent back, never
                 printed). Only a module-level err_code -1 ("module not
                 support") means "no meter"; -2 (unknown method) and -14
                 (unknown outlet) come inside the method's object.
                 Like shellyplug it reaches a LAN device, so the macOS Local
                 Network privacy gate applies — see docs/power.md gotchas.
```
