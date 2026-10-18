<!--
Copyright 2026 Curtis Galloway

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0
-->

# shellyplug

Power control for **Shelly Gen2+ smart plugs and relays** (Plus, Pro, Gen3,
Gen4) through each device's **local HTTP RPC API** (`GET /rpc/<Method>`): on,
off, power-cycle, and state and power metering. No cloud account, Home Assistant or Matter controller
needed, and no daemon. Pure Rust via [ureq](https://crates.io/crates/ureq);
works on **macOS and Linux**.

- **Supported:** Gen2/3/4 devices (`Switch.Set`, `Switch.GetStatus`,
  `Shelly.GetDeviceInfo`). **Gen1** devices use a different API and are not
  supported.
- **Auth:** only devices with authentication **disabled** (`auth_en: false`,
  the factory default). An auth-enabled device answers HTTP 401, and the tool
  says so.

## Install

```bash
cargo install --git https://github.com/curtisgalloway/panio shellyplug
```

Needs a [Rust toolchain](https://rustup.rs). The binary lands in
`~/.cargo/bin/shellyplug`. It lives in the
[panio](https://github.com/curtisgalloway/panio) repository but builds and
runs on its own.

## Quick start

A Shelly advertises an mDNS name like `shellyplugusg4-<mac>.local`. Then:

```bash
shellyplug -d 10.0.0.5 status     # device info + switch state and power
shellyplug -d 10.0.0.5 state      # prints exactly "on" or "off"
shellyplug -d 10.0.0.5 on         # switch on, confirm by read-back
shellyplug -d 10.0.0.5 off        # switch off, confirm by read-back
shellyplug -d 10.0.0.5 cycle      # off → confirm → 3 s → on → confirm (--delay-ms to change)
```

## Addressing

- **`-d <host>`**: a bare IP or hostname (`10.0.0.5`, `shelly.local`),
  optionally with a scheme or port (`http://10.0.0.5:8080`). Use the `.local`
  name or a DHCP reservation so a lease change doesn't break a saved command.
- **`[id]`**: the switch component id, default `0`. Multi-channel devices (e.g.
  a Pro 4PM) use `0..N`: `shellyplug -d 10.0.0.5 on 2`.

## Commands

```
shellyplug -d <host> status [id]          device info + switch state and power metering
shellyplug -d <host> state  [id]          print exactly "on" or "off"
shellyplug -d <host> on|off [id]          switch + read-back confirm
shellyplug -d <host> cycle  [id]          off → confirm → delay → on → confirm  [--delay-ms 3000]
shellyplug -d <host> doctor [id] [--json] read-only health check: reachable, auth, switch
```

`on`/`off`/`cycle` read `Switch.GetStatus` back and exit non-zero on a
mismatch. A relay that ignores the off command aborts `cycle` before the hold.

`doctor` never switches the relay. `--json` prints the report `panio doctor`
reads when the power channel is set with `--doctor`, and exits 0 whatever the
checks found; without `--json` it prints one line per check and exits 1 if any
failed.

## macOS Local Network permission

If `shellyplug` fails with **`No route to host` (EHOSTUNREACH)** while your
browser and `curl` reach the device, grant the terminal app **Local Network**
access under System Settings → Privacy & Security → Local Network. macOS
Sequoia and later grant LAN access per application.

## License

Apache-2.0. See [LICENSE](LICENSE).
