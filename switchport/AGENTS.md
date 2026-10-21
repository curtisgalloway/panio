<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# switchport/ — agent notes

The root AGENTS.md rules still apply here. Issue: #334.

## Module layout

```
switchport/      Rust crate: standalone one-shot helper that enables/disables a
                 managed switch's port (a real link drop for `netif down-hard`)
                 and, over SNMP, its PoE. A LINK-control helper, not a power
                 helper: it is wired through the netboot `link_down_cmd` /
                 `link_up_cmd` hooks (`panio netboot set -t <target>
                 --link-down-cmd '<cmd>' --link-up-cmd '<cmd>'`), which
                 `netif down-hard` (before admin-down) and `netif mode
                 link|netboot|ffx` (before raising the host IP) run.
  src/main.rs      clap CLI, backend construction, secrets, doctor, exit codes
  src/ops.rs       Backend trait + EVERY safety rule (written once)
  src/easysmart.rs TP-Link Easy Smart HTTP backend + a small JS-literal parser
  src/snmp.rs      SNMP v2c backend (snmp2 sync API)
```

CLI: `switchport -d <host> [--backend easysmart|snmp] [--mgmt-port N]
port|poe <N> on|off|status [--json] [--wait-link SECS]` and `doctor [--json]`.
Boolean flags go after the verb (panio's doctor-command derivation treats an
option before the verb as taking a value). Exit codes: 0 ok, 1 failed, 2 usage
or unsupported, 3 not configured (missing secret, or `--mgmt-port` for an
`off`), 4 switch unreachable, 22 link did not follow within `--wait-link`.

## Safety rules (all have executing tests, in both backends)

1. **Management-port refusal.** `port N off` and `poe N off` refuse when N ==
   `--mgmt-port`, and when `--mgmt-port` is absent (exit 3). `main` checks it
   before building the backend, and `ops` checks again, so a refusal sends no
   request at all. The error says it would cut the helper's own management path.
2. **Read-back.** Every write is verified (Easy Smart: `PortSettingRpm.htm`
   `state[N-1]`; SNMP: `ifAdminStatus` / `pethPsePortAdminEnable`). A mismatch
   is an error.
3. **Link is reported, not awaited, by default.** `port N on` is run by panio as
   `link_up_cmd` while the target is often powered off, so no link is expected.
   `--wait-link SECS` opts in to polling (0.5 s) for link up/down; a timeout is
   exit 22 with the admin state already applied and stated in the message.
4. **Identity pinning (Easy Smart).** `SystemInfoRpm.htm` `info_ds`
   `hardwareStr` must contain and `firmwareStr` start with an `ALLOWED` pair
   (seed: `TL-SG108PE 3.0` / `1.0.0 Build 20200415`). Anything else is refused
   before any write; `--allow-unknown-model` overrides with a warning.
5. **Login discipline.** The login page coming back on a GET means no/expired
   session: log in and ask again once. A failed login (`logonInfo` code 1) is
   never retried. At most 3 logins per process.

## Easy Smart facts and caveats

- Cleartext HTTP, credentials in a POST body, sessions **bound to the client IP
  with no cookie**: use isolated bench networks only. Logging in from the helper
  may end a browser session from the same host (and vice versa).
- Login is lazy: the helper GETs first and logs in only when handed the login
  page (detected by `logon.cgi` in the body).
- Write: `GET /port_setting.cgi?portid=N&state=0|1&speed=7&flowcontrol=7&apply=Apply`
  (`7` = unchanged). It returns 200 whether or not anything changed, hence the
  read-back.
- `spd_act` 0 = link down; 2..6 = 10M half .. 1000M full (numbering inferred
  from `spd_cfg`; only 6 was observed on the bench).
- **Recorded pages.** `tests/fixtures/raw/` holds pages recorded from the bench
  TL-SG108PE (hardware 3.0, firmware 1.0.0 Build 20200415 Rel.54962):
  `SystemInfoRpm.htm`, `PortSettingRpm.htm`, `logon-success.html` (and two VLAN
  pages, unused here); `recorded_pages_from_the_bench_switch_parse` runs the real
  parsers over them. The fake HTTP server's pages are hand-written in the same
  shape (`var max_port_num`, `var all_info = {state:[…], spd_cfg, spd_act, …}`,
  `var info_ds = {descriStr:[…], …}`), not recordings. The `spd_act` numbering
  2..5 is inferred from the page's `speed_info` table; only 0 and 6 were observed.
  The recorded firmware string carries a `Rel.` suffix, hence the prefix match.
  The login-failure reply text (code 1) and the expired-session page were not
  recorded; the helper detects the latter by `logon.cgi` in the body.
- **PoE is not supported by the easysmart backend yet**: the reference client
  has no PoE page/CGI, so `poe` exits 2 with a "not supported" message. Use
  `--backend snmp` on a switch with POWER-ETHERNET-MIB.

## SNMP facts

- v2c only. `ifIndex = port + --if-index-offset` (default 0, i.e. 1:1; some
  switches number interfaces with an offset or have non-port interfaces first).
  `ifAdminStatus` 1.3.6.1.2.1.2.2.1.7 (up 1 / down 2), `ifOperStatus` .8,
  speed from `ifHighSpeed` 1.3.6.1.2.1.31.1.1.1.15.
- PoE: `pethPsePortAdminEnable` 1.3.6.1.2.1.105.1.1.1.3.<group>.<port>
  (`--poe-group`, default 1), true 1 / false 2.
- No default community: SWITCHPORT_COMMUNITY / `--community-file` /
  `--community-command` (secret crate). A wrong community gets no reply from a
  real agent, so it shows as exit 4 "no usable SNMP reply ... wrong community".
- **Dependency: `snmp2` 0.5 with `default-features = false`** (sync API only;
  its defaults pull tokio and the v3 crypto stack; without them only
  `asn1-rs` is added). Chosen by dep-quality score 4.6 vs 1.2-1.4 for
  `snmp`/`csnmp`. The fake agent in the tests hand-encodes BER and shares no
  code with snmp2, so a codec bug cannot hide on both sides.

## Tests

Loopback only; nothing touches a real switch. Easy Smart: a `TcpListener`
thread serving fixture pages, recording requests and mutating port state.
SNMP: a `UdpSocket` thread decoding GET/SET. To prove the management-port
tests bite, make `ops::guard_mgmt` return `Ok(())`: both
`mgmt_port_off_is_refused_without_any_request` tests must fail.
