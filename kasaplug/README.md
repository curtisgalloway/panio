<!--
Copyright 2026 The Panio Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0
-->

# kasaplug

Power control for **TP-Link Kasa smart plugs and power strips** through each
device's **legacy local protocol** (JSON over TCP 9999): on, off, power-cycle,
and state and power metering. No cloud account, TP-Link login or Home
Assistant needed, and no daemon. Pure Rust; works on **macOS and Linux**.

- **Supported:** the legacy protocol (`system.get_sysinfo`,
  `system.set_relay_state`, `emeter.get_realtime`). Tested with an HS300
  six-outlet strip. **KLAP**, the authenticated protocol newer firmware
  switches to, is not supported.
- **Local API off:** newer firmware on some models closes TCP 9999; the device
  then refuses the connection, and the tool says so. On an HS300 (hardware
  2.0, firmware 1.1.2), turning on **Third-Party Integration** in the Kasa app
  turned it back on.

## Install

```bash
cargo install --git https://github.com/curtisgalloway/panio kasaplug
```

Needs a [Rust toolchain](https://rustup.rs). The binary lands in
`~/.cargo/bin/kasaplug`. It lives in the
[panio](https://github.com/curtisgalloway/panio) repository but builds and
runs on its own.

## Quick start

Give the device a DHCP reservation, then:

```bash
kasaplug -d 192.0.2.10 status     # device info + outlet state and power
kasaplug -d 192.0.2.10 state      # prints exactly "on" or "off"
kasaplug -d 192.0.2.10 on         # switch on, confirm by read-back
kasaplug -d 192.0.2.10 off        # switch off, confirm by read-back
kasaplug -d 192.0.2.10 cycle      # off → confirm → 3 s → on → confirm (--delay-ms to change)
```

## Addressing

- **`-d <host>`**: a bare IP or hostname (`192.0.2.10`), optionally with a
  port (`192.0.2.10:9999`). Use a DHCP reservation so a lease change doesn't
  break a saved command.
- **`[id]`**: the outlet index, default `0`. Single-outlet plugs only have
  `0`; a strip uses `0..N-1` in the order the device lists its outlets, so the
  Kasa app's "Plug 5" is `4`: `kasaplug -d 192.0.2.10 on 4`.

## Commands

```
kasaplug -d <host> status [id]          device info + outlet state and power metering
kasaplug -d <host> state  [id]          print exactly "on" or "off"
kasaplug -d <host> on|off [id]          switch + read-back confirm
kasaplug -d <host> cycle  [id]          off → confirm → delay → on → confirm  [--delay-ms 3000]
kasaplug -d <host> doctor [id] [--json] read-only health check: reachable, auth, switch
```

`on`/`off`/`cycle` read `system.get_sysinfo` back and exit non-zero on a
mismatch. An outlet that ignores the off command aborts `cycle` before the
hold. Each request has one 3 s deadline for the connection and the whole
reply.

`doctor` never switches an outlet. `--json` prints the report `panio doctor`
reads when the power channel is set with `--doctor`, and exits 0 whatever the
checks found; without `--json` it prints one line per check and exits 1 if any
failed.

## macOS Local Network permission

If `kasaplug` fails with **`No route to host` (EHOSTUNREACH)** while other
tools reach the device, grant the terminal app **Local Network** access under
System Settings → Privacy & Security → Local Network. macOS Sequoia and later
grant LAN access per application.

## License

Apache-2.0. See [LICENSE](LICENSE).
