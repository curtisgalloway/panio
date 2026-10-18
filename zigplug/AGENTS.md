<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# zigplug/ — agent notes

This file holds the internals of zigplug/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
zigplug/         Python (uv) helper: Zigbee smart plug control via a CC2652 (ZNP)
                 coordinator dongle, using zigpy-znp. CLI wired into panio
                 via generic power hooks, like cambrionix — but operations
                 proxy through a persistent daemon (`_daemon.py`, aiohttp on
                 localhost, standard daemon.json discovery; the file also
                 carries a per-run bearer token, mode 0600, that every request
                 must present) that owns the coordinator session: one-shots
                 reset the CC2652 on every serial open (auto-BSL lines) and
                 collide on the stateful ZNP session, so the daemon serializes
                 ops with hard timeouts.
                 Auto-spawned on first use; hook strings stay one-shot-shaped.
                 Commands: `form` (one-time network setup), `permit` (pairing
                 window), `list`, `on/off/state/cycle <ieee>`, `remove <ieee>`,
                 `serve/stop/status` (daemon; `stop`/`status` need no `-d`),
                 `backup`/`restore` (coordinator
                 NVRAM recovery from zigpy's auto-backups — no re-pairing);
                 `state <ieee>` prints exactly `on` or `off` (state_cmd
                 contract). Device DB at
                 ~/.config/panio/helpers/zigplug/zigbee.db (auto-migrated
                 from the legacy top-level location).
                 Installed by `panio setup` via `uv tool install` when uv is
                 present (shim in the libexec dir via UV_TOOL_BIN_DIR, off
                 PATH). See docs/power.md for pairing, hook wiring, recovery.

```
