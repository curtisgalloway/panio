---
name: managed-switch
description: >
  Set up a managed Ethernet switch so panio can drop and restore a target's
  link through the `switchport` helper (the guaranteed link drop for `panio
  netif down-hard`). Use when asked to set up, configure, or enroll a managed
  switch for panio link control, to get a D-Link DGS-1100 (DGS-1100-05V2)
  from factory defaults to a working `switchport --backend snmp` link hook,
  to enable SNMP on a switch, or when `switchport` or its link hooks come up.
  Covers reaching a factory switch, enabling and saving SNMP, finding the
  management port, verifying, and wiring the hooks. Other switches: the
  TP-Link Easy Smart backend (TL-SG108PE, TL-SG205E) already exists; the last section says what to
  measure for a new model.
---

<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Setting up a managed switch for panio link control

A USB Ethernet adapter keeps its PHY (the chip that holds the link up)
powered when the host disables the interface, so the target never sees the
cable go away. A managed switch can disable the target's port, which does
drop the link. The `switchport` helper does that. This skill takes a
**factory-default D-Link DGS-1100-05V2** to a working link hook. Why a switch
and what the hooks do: `docs/netif.md`, "Guaranteed link drop with a managed
switch".

You run every command on the **control host** (the machine cabled to the
switch), unless it says otherwise.

## Terms

- **Management port**: the switch port the control host's own cable plugs
  into. The helper reaches the switch through it, so it must never be turned
  off. `--mgmt-port` names it.
- **SNMP**: a network management protocol. `switchport` uses version 1/2c to
  read and set a port's admin state.
- **Community string**: SNMP's shared password, sent in clear text. The
  factory switch has `private` (read-write) and `public` (read-only).
- **ifIndex**: SNMP's number for an interface. On this switch ifIndex 1 to 5
  are ports 1 to 5.
- **ifAdminStatus**: the SNMP setting that turns a port on (1) or off (2).
- **Link hook**: a shell command stored on a target's netboot channel that
  panio runs to drop or restore the link.

More: `docs/glossary.md`.

## Before you start

- A DGS-1100-05V2 at factory defaults, and an
  Ethernet adapter on the control host cabled to one of its ports. That port
  is the management port; call it `<M>`. The target's adapter goes in
  another port, `<N>`.
- `switchport` installed (`panio helper switchport --help` works).
- `curl`, `openssl` and `sed`.
- Prefer a management adapter that is **not** the netboot link (see Gotchas).
- The factory web password is `admin`. If someone changed it, ask for it;
  never guess. A failed login is not retried (Gotchas).
- Safety: enabling SNMP with the factory `private` community lets anyone on
  that link reconfigure the switch. Use an isolated bench link only.

Placeholders: `<iface>` is the control host's adapter on the switch. The
switch's factory address, 10.90.90.90, is a documented product default.

## Step 1 — give the host an address the switch can answer

The factory switch is at 10.90.90.90 with mask 255.0.0.0, no gateway, and no
DHCP server on the link, so a host adapter gets only a link-local address.
Add a temporary address in **10.90.90.0/24**. Use /24, deliberately not /8:
a /8 would route all of 10/8, which may hold the host's real LAN, out the
bench adapter. The switch still replies, because the host address falls inside
the switch's own /8.

macOS:

```bash
sudo ifconfig <iface> alias 10.90.90.1 netmask 255.255.255.0
```

Linux:

```bash
sudo ip addr add 10.90.90.1/24 dev <iface>
```

Then check that it answers:

```bash
ping -c 3 10.90.90.90
```

If there is no reply, check the cable and that `<iface>` has link. Do not
proceed on a failed ping.

## Step 2 — read the web UI path and confirm the model

The web UI is cleartext HTTP. **Every request needs a `Referer:
http://10.90.90.90/` header**; without it the server returns a small stub
page that resets cookies and redirects to `/`. The static files live under a
prefix that includes the firmware version, so read it from the root page
instead of hardcoding it.

`switch-setup` will hold a session token, so it is created private (mode
0700). It is scratch state: never commit it, and delete it (`rm -r
switch-setup`) whenever you stop, whether the procedure finished or failed.

```bash
mkdir -m 700 switch-setup
curl -sS -H 'Referer: http://10.90.90.90/' http://10.90.90.90/ > switch-setup/root.html
sed -n "s/.*RT='\([^']*\)'.*/\1/p" switch-setup/root.html | head -1 | grep -x -E '/DGS-1100-05V2_[0-9.]+/' > switch-setup/rt
cat switch-setup/rt
```

Expect `/DGS-1100-05V2_1.00.003/`. Measured on hardware A1, firmware
1.00.003, boot 1.0.0. If nothing prints, the page is not this model's: stop
and see the last section. A different firmware version prints a different
number; it is not covered here, so stop and say so. The value is device
output, so it is checked against a strict pattern and saved to
`switch-setup/rt`; later commands read it from that file. Never paste device
output into a command by hand.

## Step 3 — log in

Login takes the lowercase hex MD5 of the password, as `pass=<hash>`, with the
cookie `Gambit=login`. The session comes back **in the response body as
JavaScript**, not as a `Set-Cookie` header:
`document.cookie='SessID=1;path=/';document.cookie='Gambit=<token>;path=/';`.

**Run this once. If it fails, stop and report; do not retry.** Whether this
switch locks out after failed logins was not measured, and panio never
retries authentication.

```bash
{ printf 'pass='; printf '%s' admin | openssl dgst -md5 -r | cut -d' ' -f1 | tr -d '\n'; } > switch-setup/login.body
curl -sS -H 'Referer: http://10.90.90.90/' -H 'Cookie: Gambit=login' --data-binary @switch-setup/login.body http://10.90.90.90/cgi/login.cgi > switch-setup/login.out
sed -n "s/.*Gambit=\([^;']*\);path.*/\1/p" switch-setup/login.out | head -1 > switch-setup/token
test -s switch-setup/token && printf 'Cookie: SessID=1; Gambit=%s\n' "$(cat switch-setup/token)" > switch-setup/cookie.hdr
test -s switch-setup/cookie.hdr && echo logged-in
```

It must print `logged-in`. If the password is not `admin`, put the real one
in place of `admin` (from a file, not typed into shared history).

## Step 4 — confirm the device and the port settings

Page data is in `DS/<Name>.js` files under the prefix saved in `switch-setup/rt`. `const.js` names the model and port
count; `Port.js` has one row per port.

```bash
curl -sS -H 'Referer: http://10.90.90.90/' -H @switch-setup/cookie.hdr "http://10.90.90.90$(cat switch-setup/rt)DS/const.js" | grep -E "g_SwitchInfo|g_PortNum|g_bPoE_Support"
```

Expect `g_SwitchInfo=['DGS-1100-05V2','1.00.003','1.0.0']`, `g_PortNum=5`,
`g_bPoE_Support=0` (no PoE, so the `poe` verbs do not apply).

## Step 5 — find the management port

The host's adapter MAC is in the switch's dynamic MAC table, next to the port
it was learned on. The ping in step 1 populated it. (The switch does not
expose the bridge MIBs over SNMP, so the web table is the way.)

Print the adapter's MAC. macOS:

```bash
ifconfig <iface> | grep ether
```

Linux:

```bash
cat /sys/class/net/<iface>/address
```

Then the table. Each entry is `[port,'AA-BB-CC-DD-EE-FF',vid,type]`:

```bash
curl -sS -H 'Referer: http://10.90.90.90/' -H @switch-setup/cookie.hdr "http://10.90.90.90$(cat switch-setup/rt)DS/DFT.js"
```

The port number in the entry whose MAC matches the adapter (the switch writes
it uppercase with dashes) is `<M>`. If it is not listed, ping again and
reread; do not guess.

## Step 6 — enable SNMP

SNMP is off by default. Enable it (the trap setting stays off):

The reply is the settings page's HTML, which says nothing about success, so
it goes to a file and the stored state is read back instead:

```bash
curl -sS -H 'Referer: http://10.90.90.90/' -H @switch-setup/cookie.hdr --data 'Snmp=1&Trap=0&item=00000' -o switch-setup/snmp.out http://10.90.90.90/cgi/snmp_global.cgi
curl -sS -H 'Referer: http://10.90.90.90/' -H @switch-setup/cookie.hdr "http://10.90.90.90$(cat switch-setup/rt)DS/SNMP_Global.js"
```

It must print `var ds_Snmp = [1,...` (first element `1`). The communities are
in `DS/SNMP_Community.js` (`ds_SnmpCom=['private','public']` at the factory).

## Step 7 — save to flash

Without a save, the settings are lost at power-off; with it they survive (a
power cycle was measured: SNMP answered on a fresh boot with no further
setup). **Save only when every port is admin-up**, so a port disabled while
testing is not made permanent. In `DS/Port.js` each port is one row of
`ds_PortSetting`, and the **second field is the admin state**: `1` enabled,
`0` disabled (measured by disabling a port over SNMP). The first field is the
link (`2` up, `0` down). The command below refuses to save when any row's
second field is `0`, then reads the result message back:

```bash
curl -sS -H 'Referer: http://10.90.90.90/' -H @switch-setup/cookie.hdr "http://10.90.90.90$(cat switch-setup/rt)DS/Port.js" > switch-setup/port.js
grep -q 'ds_PortSetting' switch-setup/port.js && ! grep -q -E "\[[0-9]+,0,[0-9]+,[0-9]+,[0-9]+,'" switch-setup/port.js && curl -sS -H 'Referer: http://10.90.90.90/' -H @switch-setup/cookie.hdr --data '' -o switch-setup/save.out http://10.90.90.90/cgi/save.cgi && curl -sS -H 'Referer: http://10.90.90.90/' -H @switch-setup/cookie.hdr "http://10.90.90.90$(cat switch-setup/rt)DS/Message.js"
```

It must print `var ds_nMsgID=2;`, the UI's "The configuration has been
saved." No output means it did not save: either the port table was not read
or a port is disabled. Look at `switch-setup/port.js`, and stop and ask if a
port is disabled.

## Step 8 — store the community and run doctor

Put the read-write community in a private file, not on a command line:

```bash
mkdir -p "$HOME/.config/panio"
printf 'private' > "$HOME/.config/panio/switch-community"
chmod 600 "$HOME/.config/panio/switch-community"
```

```bash
switchport -d 10.90.90.90 --backend snmp --community-file "$HOME/.config/panio/switch-community" --mgmt-port <M> doctor --json
```

Expect three checks ok: identity `DGS-1100-05V2 Gigabit Ethernet Switch`,
ports `5 reported`, and the management port. A wrong community shows as exit
4 "no usable SNMP reply ... wrong community".

## Step 9 — verify a real drop

Use a test adapter in port `<N>` (not `<M>`). First prove the guard: turning
the management port off is refused before any request (exit 2, "refusing to
turn port `<M>` off"):

```bash
switchport -d 10.90.90.90 --backend snmp --community-file "$HOME/.config/panio/switch-community" --mgmt-port <M> port <M> off
```

Then drop and restore port `<N>`, waiting for the link to follow:

```bash
switchport -d 10.90.90.90 --backend snmp --community-file "$HOME/.config/panio/switch-community" --mgmt-port <M> port <N> off --wait-link 10
switchport -d 10.90.90.90 --backend snmp --community-file "$HOME/.config/panio/switch-community" --mgmt-port <M> port <N> on --wait-link 10
```

Both must exit 0 (22 means the link did not follow). Measured on the bench:
the host adapter on the port lost carrier 0.6 s after `off` and got it back
1.7 s after `on`; admin state and link read back correctly. Also confirm
from the far end (the adapter's own link state or LED), since that is the
point of the exercise.

## Step 10 — wire the hooks

On the control host that owns the target's netboot channel:

```bash
panio netboot set -t target-machine --link-down-cmd 'switchport -d 10.90.90.90 --backend snmp --community-file $HOME/.config/panio/switch-community --mgmt-port <M> port <N> off' --link-up-cmd 'switchport -d 10.90.90.90 --backend snmp --community-file $HOME/.config/panio/switch-community --mgmt-port <M> port <N> on'
```

`panio netif down-hard target-machine` now runs the down hook first. Check
`panio netif status target-machine` shows `link hooks  down+up`, and
confirm the drop from the target side.

Finally remove the switch-setup directory, which holds a session token:

```bash
rm -r switch-setup
```

## Gotchas

- **`--mgmt-port` protects only the port you name, so pin the cabling.** Any
  port works as the management port, but the hooks hardcode its number. If
  the cables are later swapped, a hook still saying `--mgmt-port 1 ... port 5
  off` disables the host's own path to the switch without complaint. This
  happened on the bench: the cables came back in swapped ports, and step 5
  found the management port had moved from 1 to 5. Use one fixed port for the
  control host, label it on the switch, and rerun step 5 after any recabling.
  Recovery if it happens anyway: power-cycle the switch. Port changes made by
  `switchport` are not saved, and step 7 refuses to save while a port is
  disabled, so the switch boots with every port enabled.

- **The host alias is not permanent and the switch stays at 10.90.90.90.**
  The hook reaches the switch at that address, so the alias from step 1 must
  exist whenever a hook runs. Plain `ifconfig`/`ip` aliases vanish on reboot;
  making one persistent is host-specific and not covered here. Changing the
  switch's own address was not measured.
- **Use a management adapter that is not the netboot link.** On macOS,
  `netif mode off` and `down-hard` return the netboot interface's service to
  DHCP, which can remove an alias added by hand, and the next up hook then
  cannot reach the switch.
- **Address clash.** If the control host's real LAN uses 10.90.90.0/24,
  choose another isolated route before step 1; do not proceed blindly.
- **Referer is mandatory** on every web request, or the server resets the
  cookies and redirects.
- **Never retry a failed login.** Stop after one and report.
- **The prefix includes the firmware version.** Read `RT` from the root page
  every time.
- **SNMP with the factory read-write community is an open door** for anyone
  on that link. Acceptable on an isolated bench link only. To harden it,
  change the communities on the switch's SNMP Community Table page and put
  the new one in a secret source: `SWITCHPORT_COMMUNITY`, `--community-file`
  or `--community-command`. That change was not measured and has no
  procedure here.
- **No PoE, no bridge MIBs.** The DGS-1100-05V2 has no POWER-ETHERNET-MIB, and
  `dot1dTpFdbPort`/`dot1qTpFdbPort` return "No Such Instance". Use the web
  MAC table to find a port.
- A browser works too: the web UI has the same settings (SNMP enable, then
  save the configuration); the curl steps exist so an agent can do it without
  one. The UI's page names were not recorded here.

## TP-Link Easy Smart

The `easysmart` backend (the default) drives a TP-Link TL-SG108PE (hardware
3.0) and a TL-SG205E (hardware 1.0, firmware 1.0.1 Build 20250609). It
detects at login which of the two web UIs the switch speaks. The TL-SG205E's
2025 firmware forces a password change at first login: set it in a browser
(at the TP-Link factory default address printed on the label), then give the
helper the new password; if it is not done, the helper says so and stops. It
needs the switch's web password, not SNMP. Its protocol facts and
safety rules are in `switchport/AGENTS.md`; the hook wiring is step 10 with
`--backend easysmart` and `SWITCHPORT_PASSWORD`, `--password-file` or
`--password-command` in place of the community. This skill has no TP-Link
setup procedure.

## Adding another switch model

Measure, on the real hardware, before writing anything down:

1. How to reach it at factory state (address, mask, DHCP) and whether it
   answers a /24 host address.
2. Whether SNMP is off by default, which communities exist, and how to enable
   it without a browser (record the web requests).
3. How to save to flash.
4. `sysDescr`, `sysObjectID`, and whether ifIndex equals the port number
   (otherwise find `--if-index-offset`).
5. Whether `ifAdminStatus` set and read back works, and the timings: carrier
   lost after `off`, carrier back after `on`.
6. Which MIBs are missing (PoE, bridge tables).

If SNMP works, no code is needed: add a section to `switchport/AGENTS.md`, a
row to the supported-switches table in `docs/netif.md`, and a procedure to
this skill. Otherwise a new backend goes in `switchport/src/`.
