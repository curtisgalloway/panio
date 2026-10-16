<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# secret/ — agent notes

This file holds the internals of secret/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
secret/          Rust library crate (no binary), shared by path dependency —
                 the one crate helpers share rather than copy. Resolves a
                 helper's secret X from env X, then `--x-file <path>` (one
                 trailing newline dropped; warns if group/other-readable),
                 then `--x-command <cmd>` (`sh -c`/`cmd /C`, no stdin, stdout
                 is the value, 30 s timeout) — the restic/borg pattern (#249).
                 Every failure is `NotConfigured`; `is_not_configured` lets a
                 helper's `main` exit 3. Used by amt (AMT_PASSWORD,
                 AMT_RFB_PASSWORD). A new helper that needs a secret uses it
                 with its own `Spec` and flags; see
                 docs/dev/adding-power-helpers.md.

```
