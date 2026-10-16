<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# shellyplug/ — agent notes

This file holds the internals of shellyplug/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
shellyplug/      Rust crate: standalone helper for Shelly Gen2+ smart plugs/
                 relays (Plus/Pro/Gen3/Gen4) over the device's local HTTP RPC
                 API (Switch.Set/GetStatus; ureq). One-shot, stateless — no
                 daemon. Addressed by `-d <ip|host>` and `[id]` switch (default
                 0). Commands: `status [id]`, `state [id]`, `on/off [id]`,
                 `cycle [id]`; `on/off/cycle` confirm by read-back. `doctor
                 [id] [--json]` is the reference helper doctor check (#304):
                 read-only (GetDeviceInfo, Shelly.GetStatus or
                 Switch.GetStatus, never Switch.Set), checks reachable/auth/
                 switch, `--json` exits 0; tests drive the real client against
                 a loopback HTTP stub. Gen2+ only
                 (no Gen1 REST); auth-disabled devices only for now. NB: first
                 helper to reach a LAN device, so first to hit the macOS
                 Local Network privacy gate — see docs/power.md gotchas.

```
