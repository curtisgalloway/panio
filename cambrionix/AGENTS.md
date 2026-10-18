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
                 contract; C/S/I → on, O → off, any other mode letter is an error).
                 `on`/`off`/`cycle` confirm every transition by re-reading the port
                 table (C, S or I confirms on, O confirms off). Responses are
                 bounded (3 s, 64 KiB) and hub error lines fail
                 the command. Built/installed by `make install` / `panio setup`.

```
