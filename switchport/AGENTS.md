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
   (`TL-SG108PE 3.0` / `1.0.0 Build 20200415`; `TL-SG205E 1.0` / `1.0.1 Build
   20250609`). Anything else is refused
   before any write; `--allow-unknown-model` overrides with a warning.
5. **Login discipline.** The login page coming back on a GET means no/expired
   session (old UI) or the logged-out stub `var allow = 0` (new UI): log in
   and ask again once. A failed login (`logonInfo` code 1, or `allow` 0) is
   never retried. At most 3 logins per process.

## Easy Smart facts and caveats

- Cleartext HTTP, credentials in a POST body: use isolated bench networks only.
  The 2020 UI (TL-SG108PE) binds the session to the **client IP with no
  cookie**; the 2025 UI (TL-SG205E) uses a **cookie**. Logging in from the
  helper may end a browser session from the same host (and vice versa).
- Both UIs share one backend (`--backend easysmart`). The first page fetch is
  preceded by a login, whose reply says which UI it is: `logonInfo = new
  Array(` is the old one, `var allow = N;` the new one. Page paths are then
  chosen per UI. After that a later login page means an expired session.
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
- **Fixtures** for the new UI are in `tests/fixtures/raw/sg205e/` (scrubbed).
- **PoE is not supported by the easysmart backend yet**: the reference client
  has no PoE page/CGI, so `poe` exits 2 with a "not supported" message. Use
  `--backend snmp` on a switch with POWER-ETHERNET-MIB.

## TP-Link TL-SG205E (2025 web UI) facts

Hardware `TL-SG205E 1.0`, firmware `1.0.1 Build 20250609 Rel.75246`. Recorded
pages: `tests/fixtures/raw/sg205e/`. The management address at factory state is
the TP-Link factory default (printed on the label). The firmware forces a
password change at first login: do it in a browser; the helper reports it
(login reply `allow` 2) and does not retry.

- **Login.** `POST /logon.cgi`, form `username`, `password`, `logon=Login`. The
  switch silently rejects the POST (reply `var allow = 0;`) unless it carries
  browser headers; which of `Origin: <base>`, `Referer: <base>/index.html` and
  a browser-like `User-Agent` it checks is unknown, so all three are sent on
  every login (both UIs). The reply header is `Set-cookie: Cookies=<token>`
  (lowercase `c`; read case-insensitively); the body has `var allow = N;`: 1
  logged in, 0 rejected, 2 password change required.
- **Session** is the cookie, not the client IP. Later requests send `Cookie:
  Cookies=<token>`, `Referer: <base>/homepage.html` and the User-Agent.
  Sessions expire after a few minutes. Without a valid session a protected
  page returns a stub with `var allow = 0` and no `logon.cgi`.
- **Old pages** (`/SystemInfoRpm.htm`, `/PortSettingRpm.htm`) get an empty
  reply (connection closed, no status), hence the login before the first fetch.
- **Identity**: `GET /sys_SystemInfoRpm.html`, `var data_info = {sysnameStr,
  hardwareStr, firmwareStr, ...}`.
- **Admin state, port count**: `GET /sys_portsetting.html`, `var max_port_num =
  [5];` and `var all_info = {state:[1,1,1,1,1,], ...}` (trailing commas; 1 =
  enabled).
- **Link and speed**: `GET /portCur.xml`, e.g. `1,0,0,0,1,&3,1,1,1,3,&...`:
  four `&`-separated groups of per-port values: link (1 up), speed (1 = 10M, 2
  = 100M, 3 = 1000M; meaningless when down), duplex (0 half, 1 full, 2 auto),
  flow control. Mapped onto the old `spd_act` numbering so `speed_name` serves
  both UIs.
- **Write**: `GET /port_setting.cgi?port_bit=<mask>&state=0|1`, mask = `1 <<
  (port-1)` (port 5 = 16); only the changed field is sent. The reply carries a
  `tip` but the helper reads back `sys_portsetting.html` as usual.
- **Timing** (measured): port off, the peer adapter lost carrier 1.1 s later;
  port on, carrier back 2.7 s later.

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

## D-Link DGS-1100-05V2 facts

All measured on hardware: hardware A1, firmware 1.00.003, boot 1.0.0. The
setup procedure is `skills/managed-switch/SKILL.md`; this section is the
protocol reference behind it.

- **Factory state.** Management address 10.90.90.90, mask 255.0.0.0, no
  gateway, no DHCP server on the link. A host reaches it with a temporary
  address in 10.90.90.0/24 (deliberately not /8, so the host does not route all
  of 10/8 out the bench adapter); the switch replies because the host address
  is inside its /8. The web UI is cleartext HTTP. SNMP is disabled; the
  communities are preset to `private` (read-write) and `public` (read-only).
- **SNMP.** v1/v2c. sysDescr `DGS-1100-05V2 Gigabit Ethernet Switch`,
  sysObjectID 1.3.6.1.4.1.171.10.173.1.1. ifIndex 1..5 = ports 1..5, ifDescr
  `port1`..`port5`, so the default `--if-index-offset 0` is right. No
  POWER-ETHERNET-MIB (no PoE): `poe` does not apply. The bridge MIBs are not
  exposed (`dot1dTpFdbPort` and `dot1qTpFdbPort` are "No Such Instance"), so
  the management port is found from the web MAC table. Writes need
  `SWITCHPORT_COMMUNITY=private` (the read-write community).
- **Web UI protocol** (used only to enable SNMP; the helper itself speaks
  SNMP).
  - Every request needs `Referer: http://10.90.90.90/`; without it the server
    returns a 143-byte stub that resets cookies and redirects to `/`.
  - Static files are under `/DGS-1100-05V2_1.00.003/`. The root page sets
    `RT='/DGS-1100-05V2_1.00.003/'`; the prefix carries the firmware version,
    so read it from the root page.
  - Login is password only (factory `admin`): POST `/cgi/login.cgi`, body
    `pass=<lowercase hex MD5 of the password>`, cookie `Gambit=login`. The
    session is not in `Set-Cookie`: it arrives in the body as JavaScript,
    `document.cookie='SessID=1;path=/';document.cookie='Gambit=<token>;path=/';`.
    Later requests send `Cookie: SessID=1; Gambit=<token>`. Never retry a
    failed login.
  - Page data is `<RT>DS/<Name>.js`: `Switch.js` (`ds_IPInfo`, `g_DeviceInfo`
    with the switch MAC), `SNMP_Global.js` (`var ds_Snmp=[enabled,trap,...]`),
    `SNMP_Community.js` (`ds_SnmpCom=['private','public']`), `Port.js`
    (`ds_PortSetting`, a row per port), `DFT.js` (dynamic MAC table
    `ds_DFT=[[port,'AA-BB-CC-DD-EE-FF',vid,type],...]`), `const.js`
    (`g_SwitchInfo=['DGS-1100-05V2','1.00.003','1.0.0']`, `g_PortNum=5`,
    `g_bPoE_Support=0`).
  - Enable SNMP: POST `/cgi/snmp_global.cgi`, body `Snmp=1&Trap=0&item=00000`;
    `SNMP_Global.js`'s first element becomes 1.
  - Save to flash: POST `/cgi/save.cgi` with an empty body; the reply loads
    `DS/Message.js` with `ds_nMsgID=2` ("The configuration has been saved.").
    Without it the settings are lost at power-off. Save only while every port
    is admin-up, so a port disabled for a test is not persisted.
- **Results.** `switchport -d <switch> --backend snmp --mgmt-port <M> doctor
  --json`: identity ok, ports ok ("5 reported"), mgmt-port ok. `port <M> off`
  is refused before any request (exit 2, "refusing to turn port M off: port M
  is --mgmt-port ..."). `port N off`: the host adapter on that port lost
  carrier 0.6 s later; `port N on`: carrier back 1.7 s later (TP-Link: 0.8 s /
  1.7 s). Admin state and link read back correctly.
- **Security.** Enabling SNMP with the factory read-write community lets
  anyone on the link reconfigure the switch: isolated bench links only.
  Changing the communities was not measured.

## Tests

Loopback only; nothing touches a real switch. Easy Smart: a `TcpListener`
thread serving fixture pages, recording requests and mutating port state.
SNMP: a `UdpSocket` thread decoding GET/SET. To prove the management-port
tests bite, make `ops::guard_mgmt` return `Ok(())`: both
`mgmt_port_off_is_refused_without_any_request` tests must fail.
