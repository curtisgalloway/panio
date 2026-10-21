<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# cambrionix/ — agent notes

This file holds the internals of cambrionix/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
cambrionix/      Rust crate: standalone helper binary for Cambrionix USB hub control
                 (control UART, 115200 8N1); wired into panio via generic power hooks.
                 Commands: `state [port]`, `on <port>`, `off <port>`, `cycle <port>`
                 `state <port>` prints exactly `on` or `off` (matches panio state_cmd
                 contract). The mode is the 2nd flag of the PDSync `state` row,
                 positional and case-sensitive: I/S/C/F/c → on, O → off, any
                 other letter is an error (`proto::vbus_on`; table from the
                 Cambrionix CLI User Manual, "Flags for the PDSync and TS3-C10
                 firmware range"). `on`/`off`/`cycle` confirm every transition
                 by re-reading the port table with the same mapping. Responses
                 are bounded (3 s, 64 KiB) and hub error lines fail the command.
                 `off` cuts VBUS only; D+/D- stay connected (#330).
                 Built/installed by `make install` / `panio setup`.

```
