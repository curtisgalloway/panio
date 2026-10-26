<!--
Copyright 2026 Curtis Galloway

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# Panio — Agent Instructions

## How these instructions are organized

This root file holds the rules that apply to every change. Each component
directory has its own `AGENTS.md` with its internals (module layout, protocols,
hardware notes, gotchas). **Before editing files in a directory that has an
`AGENTS.md`, read it**; not every agent harness loads nested ones automatically.

Per-OS behavior and the Windows port: [docs/dev/platform-support.md](docs/dev/platform-support.md).
Put new internals documentation in the component's `AGENTS.md`, not here.

## Never commit private infrastructure

**This repository is public. The lab that exercises it is not.** It is
described in a separate, private infrastructure repo, and nothing identifying
it belongs here. This applies to *everything* committed: code, docs, tests,
fixtures, eval scenarios, commit messages, and captured terminal output.

Do not commit:

- **Real hostnames or domains** of machines on a private network, including
  bare short names used as lab-machine labels.
- **Real addresses** on a private network — any RFC 1918 range (`10/8`,
  `172.16/12`, `192.168/16`) — with the one product exception below.
- **Home-directory paths carrying a real account name** (`/home/<user>/…`,
  `/Users/<user>/…`). Use a neutral path or `~`.
- **Identifiers read off real hardware**: MAC addresses, USB or device serial
  numbers, service-processor account names.

Use this vocabulary in examples instead:

| For | Use |
|---|---|
| A control host | `bench1`, `lab-host-1` |
| A target | `target-machine`, `pi5`, `nuc` |
| An address | `192.0.2.10` (RFC 5737 TEST-NET-1, reserved for documentation) |
| A path | `/srv/tftp/<target>`, `~/tftp/<target>` |
| A serial or MAC | `AA00BB11CC22DD33`, `00:11:22:33:44:55` |

**One exception:** `192.168.99.0/24` is panio's *own* default for the
point-to-point netboot link (`DEFAULT_HOST_IP` in `cli/src/model.rs`). It is a
product constant, not anyone's network, and belongs in docs and tests.

**Captured output is the easy way to slip.** Casts, screenshots, and recordings
reproduce whatever the prompt, serial log, and command line contained. Read a
cast before committing it and look at a screenshot; an image can only be
re-made, not scrubbed.

Some older material violates this rule (private-range addresses and internal
names, no credentials). It is cleaned up opportunistically; it is not precedent,
and you should not launch an unrequested scrub.

## Before opening a PR

**Hard gate: do not open the PR until all documentation *and* usage help
reflect the change.** Stale docs/help make the PR incomplete; fix them in the
*same* PR, never a follow-up. Run this checklist before `gh pr create`:

1. **Update docs the PR affects.** For each changed subsystem, check:
   - `docs/<subsystem>.md` — commands, config fields, workflows
   - `docs/dev/architecture.md` — design, data flows, runtime paths (if structure changed)
   - `docs/README.md` — the docs index (if a doc was added/removed)
   - `mkdocs.yml` — a new `docs/*.md` needs a `nav:` entry (end-user doc) or an
     `exclude_docs:` entry (repo-only design record); the docs build runs
     `mkdocs build --strict`
   - `docs/dev/requirements.md` — tracker status (if scope/progress changed)
   - `README.md` — capabilities table, installation steps
   - `AGENTS.md` — the root file and each touched component's
   - `evals/scenarios/*.toml` — scenarios assert CLI behavior.
     `python3.12 evals/run.py --check` (Python ≥ 3.11) verifies only that every
     *single-line* `reference` entry names a real subcommand path with flags it
     accepts, and that the T1-safe allowlist (`evals/graders/t1_config.SAFE`)
     names real groups/subcommands. It does **not** check positionals, flag
     values, multi-line (prose) references, or `goal`/grader expectations; read
     the scenarios that mention the changed command.

   Also check the diff for private infrastructure per
   [Never commit private infrastructure](#never-commit-private-infrastructure);
   read casts and screenshots, don't just grep.

2. **Update the CLI usage help.** Every new or changed command, subcommand,
   flag, or argument needs an accurate clap doc comment (`///`). Verify the
   rendered `panio <cmd> --help` and `<cmd> <sub> --help` of each touched
   command, including parent summaries so they still match their subcommands.

3. **Update the usage skill (`skills/panio/SKILL.md`)**, the agent-facing
   skill for *driving* a target. If the PR adds, removes, or changes a
   user-facing command, flag, or workflow, update the relevant section and the
   "gotchas" list; purely internal changes can skip this. Companions:
   `skills/kvm-puppeting/SKILL.md` (GUI-puppeting doctrine on `video`+`hid`;
   update it if you change the surface it relies on) and
   `skills/control-host/SKILL.md` (the executable procedure for building a
   control host from blank media; it owns those commands and
   `docs/control-host.md` points at it, so keep the procedure in one place) and
   `skills/managed-switch/SKILL.md` (taking a managed switch from factory
   defaults to a working `switchport` link hook; it owns those commands and
   `docs/netif.md` points at it).
   **Adding or removing a skill** needs a `contents` entry in `packaging/nfpm.yaml` (one explicit file→dst line
   per skill); `setup.rs` and the tarball enumerate `skills/` automatically.

4. **Validate before pushing.** For every crate you touched, run all three
   gates from that crate's directory; CI fails the job on any one:

   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

   `cargo fmt --check` is the easy one to skip and covers the whole crate, not
   just your diff; `cargo fmt` fixes it.

   `scripts/ci-local.sh` mirrors every GitHub Linux CI job (`cli`, `serialcap`,
   `netbootd`, `hdmicap`, `cambrionix`, `dsdrelay`, `dsdble`, `phidget`, `usbtoggle`, `ch9329`, `hidrig`, `shellyplug`, `switchport`, `amt`,
   `secret`) and needs a Linux box or VM (it copies the tree to the VM's own
   disk, since builds on a shared virtiofs/9p mount fail), e.g. `limactl shell
   <instance> -- bash -l scripts/ci-local.sh`. It is the fuller check; the three
   commands are the minimum for a pure-Rust change and run on the host, as does
   the macOS-only job (hdmicap AVFoundation + visionocr). Don't let `cli` tests
   rot.

   **Doc-only PRs skip the build and test jobs.** `scripts/ci-doc-only.sh`
   defines doc-only: `*.md` anywhere, everything under `docs/`, `mkdocs.yml`,
   and `LICENSE`. When every changed path matches, the `changes` job reports
   `code=false` and every skippable job in `ci.yml` is skipped (a job-level
   `if:`, never workflow-level `paths-ignore`, which would leave required
   checks pending). The gate fails open: anything unexpected runs everything.
   To keep the skip safe, every skippable job deletes all doc-only files from
   its checkout (`ci-doc-only.sh strip`) on every run, and `ci-local.sh` does
   the same to its copy, so no build or test may read a doc file. A job that
   must read one (for example `profile-pins`, which hashes `AGENTS.md`) goes
   in the always-run set in the comment on the `changes` job, and the
   allowlist should then be revisited. `scripts/tests/test_ci_doc_only.py`
   fails the build when a new job has neither the gate nor the strip step.

   Five CI lint jobs are not mirrored by `ci-local.sh`; run them on the host:
   `shellcheck` (`git ls-files -z '*.sh' | xargs -0 shellcheck`), `zigplug`
   (`cd zigplug && uv sync --frozen && uv run pyink --check . && uv run
   python -m compileall -q src`), `evals-check` (`python3.12 evals/run.py
   --check`, step 1), `actions-pinned` (`bash scripts/ci-actions-pinned.sh`,
   step 7), and `release-guards` (`python3 scripts/ci-release-guards.py`: every
   `release.yml` job outside the package builds publishes, and must require
   `github.event_name == 'push'` as well as a `v*` tag, since a manual dispatch
   against a tag also has `ref_type == 'tag'`; #338).

5. **Every crate has a CI job — no exceptions.** A crate is not finished until
   `.github/workflows/ci.yml` has a job for it (`working-directory: <crate>`,
   fmt + clippy `-D warnings` + test, copied from an existing crate job) *and*
   `scripts/ci-local.sh` has a matching `crate_job` line; the `coverage` job
   (`scripts/ci-coverage-check.sh`) fails the build otherwise. A crate that
   genuinely cannot run in CI goes in that script's `EXEMPT` **with the reason
   written down**; never weaken or delete the check. Why: four crates went
   months with no CI.

6. **Code scanning is advanced setup, not default setup.** CodeQL runs from
   `.github/workflows/codeql.yml`; GitHub's "default setup" must stay disabled
   (the two can't both be active), so don't move back to it to silence a prompt.
   The workflow omits Swift (`ocr/visionocr.swift` is macOS-only, with no Swift
   package for the extractor).

   It pins a **Rust 1.94 sysroot** via `CODEQL_EXTRACTOR_RUST_OPTION_SYSROOT`
   and `..._SYSROOT_SRC`. Don't remove that step: CodeQL 2.26.x can't parse std
   newer than 1.94 (runners ship 1.97+), so macro expansion fails and ~68 of 69
   files are "extracted with errors". Installing `rust-src` for the ambient
   toolchain doesn't help, and `_SYSROOT_SRC` alone leaves the ambient binary
   sysroot in place. Revisit when github/codeql#19982 closes.

7. **Every workflow action is pinned to a commit SHA.** A `uses:` line under
   `.github/workflows/` names a full 40-hex commit with its version in a
   trailing comment (`actions/checkout@<sha> # v6.1.0`), never a movable tag.
   `scripts/ci-actions-pinned.sh` (the `actions-pinned` job) enforces this, and
   `.github/dependabot.yml` bumps SHA and comment together weekly in one grouped
   PR. To add or bump by hand, resolve the tag through the API, not from
   memory: `gh api repos/<owner>/<repo>/git/ref/tags/<tag>`; when
   `.object.type` is `tag` (annotated), dereference via `git/tags/<sha>`.

8. **Tag the PR with the issue it fixes, in a form GitHub parses.** A PR that
   fully fixes an issue has a closing keyword immediately followed by the
   number, on its own line in the PR body: `Fixes #238` or `Closes #238` (one
   line per issue). Put the same line in the commit message, and the number in
   the PR title (`ch9329: drain startup echo on Linux (#238)`).
   - **Only for a full fix.** A partial fix, workaround, or diagnostics PR
     writes `Part of #238` (links without closing) and comments on the issue
     with what is done and what is missing; the issue stays open.
   - **Prose does not count.** "Fixes the autodetect failure in #81" and
     "Related to #238" link nothing; the keyword must be directly followed by
     `#N`. (Why: PRs #84 and #275 used prose and their issues sat open.)
   - **Check after the merge.** `gh issue view <N> --json state` should say
     `CLOSED`; if not, close it with a comment naming the merged PR.

9. **Push, open the PR, and merge only when asked.** Commit locally; `git
   push` and `gh pr create` need the maintainer's explicit instruction, and
   never merge (that decision is the maintainer's).

## Cutting a release

Releases are **tag-driven**: pushing an annotated `v*` tag to `origin` triggers
`.github/workflows/release.yml` (Linux, Windows, and macOS packages; GitHub
Release; Homebrew tap bump; signed apt repository refresh). The **tag is the single
source of truth** for the version: every crate's `Cargo.toml` stays at
`version = "0.1.0"`; don't bump manifests. The workflow derives the version
from the tag (`${GITHUB_REF_NAME#v}`) and exports it as `PANIO_VERSION`
(baked in by `cli/src/main.rs`); a dev build prints `0.1.0 (unversioned dev
build)`. Each package smoke step asserts the versions agree.

1. **Pre-flight.** Be on `main`, clean, and in sync (`git fetch origin && git
   status` → `main...origin/main`), with CI green on the HEAD commit (`gh run
   list --branch main --limit 5`); the release builds that exact commit.
   Also run `bash scripts/ci-profile-pins.sh`: CI only *warns* when a source
   pinned in `RELEASE-TRAIN.md` has changed, but `release.yml` refuses to
   build until the profile is re-read and re-pinned, so fix it before tagging.

2. **Pick the version.** Read the latest tag (`git tag --sort=-creatordate |
   head`), then choose by contents:
   - **Minor** (`0.X.0`) for anything substantive: a new command, channel, or
     helper; a daemon wire-protocol or lifecycle change; any behavioral change
     a user or agent could notice; larger bug fixes, including any that change
     what a target machine sees. Anything making a running daemon incompatible
     with a newer CLI is always minor.
   - **Patch** (`0.x.Y`) only for small bug fixes and documentation.

   When on the line, take the minor. Why: `0.1.1`–`0.1.19` were all patch
   bumps, which hid two compatibility breaks (token-authenticated daemons,
   `0.1.17`; authenticated shutdown, `0.1.19`, which needs `panio daemons stop
   <name>` before a newer CLI can replace an older daemon).

3. **Create an annotated tag** `vX.Y.Z: <lowercase one-line summary of the
   headline change>`, with an optional body paragraph. Use the releaser's GitHub
   noreply email (`<ID>+<user>@users.noreply.github.com`; check `git log
   --format='%ae' | sort -u`) or GitHub rejects the push:

   ```bash
   git -c user.email='<your-github-noreply-email>' -c user.name='<Your Name>' \
       tag -a vX.Y.Z -m 'vX.Y.Z: <summary>' -m '<body paragraph>'
   ```

4. **Push the tag** (outward-facing and hard to reverse; needs the maintainer's
   explicit go-ahead, like push/merge):

   ```bash
   git push origin vX.Y.Z
   ```

   The push fans out to:
   - `package` (amd64, arm64): `.deb`s and tarballs built in a `debian:bookworm`
     container (glibc 2.36, so Debian 12+ and current Raspberry Pi OS), with a
     `.deb` smoke test.
   - `package-windows`: portable `x86_64-pc-windows-msvc` zip, Authenticode-
     signed when `vars.AZURE_SIGNING_ACCOUNT` is set (gated behind the `release`
     environment's reviewer approval), else unsigned.
   - `package-macos`: one **universal** (arm64 + x86_64) tarball in the Homebrew
     keg layout (`bin/panio`, `libexec/bin/<helper>`, `share/panio/skills/`),
     ad-hoc signed, or Developer-ID-signed and notarized when all six of
     `MACOS_SIGNING_CERT_P12`, `MACOS_SIGNING_CERT_PASSWORD`,
     `MACOS_SIGNING_IDENTITY`, `MACOS_NOTARY_APPLE_ID`, `MACOS_NOTARY_PASSWORD`,
     `MACOS_NOTARY_TEAM_ID` are set (untested; no cert configured yet).

   Every asset (`.deb`, `.tar.gz`, `.zip`) ships a `.sha256` sidecar
   (`<hex>  <filename>`, two spaces). Then `release` attaches `dist/*` to a new
   GitHub Release with auto-generated notes; `bump-tap` fires a
   `repository_dispatch` at `curtisgalloway/homebrew-tap` to re-pin its formula
   (needs `HOMEBREW_TAP_DISPATCH_TOKEN`; absent, it warns and skips); and
   `refresh-apt-repo` re-dispatches `.github/workflows/docs.yml` so the Pages
   `/apt/` pool picks up the new `.deb`s (never the `.deb.sha256` sidecars: use
   `gh release download --pattern '*.deb'`). The docs workflow rebuilds the pool
   statelessly from the newest `APT_POOL_RELEASES` (currently 5) Releases and
   signs it with `APT_SIGNING_KEY` (`packaging/scripts/build-apt-repo.sh`). On
   the canonical repo a missing key **fails** the docs build rather than
   deploying a site without `/apt/` (a Pages deploy replaces the whole site).

5. **Watch it land.** `gh run list --workflow=release.yml --limit 1`, then
   `gh release view vX.Y.Z` to confirm artifacts and notes. Confirm the
   dispatched `docs.yml` run went green too, or apt clients keep the old version.

An unshipped botched tag can be moved (`git tag -f` + `git push -f origin
vX.Y.Z`). Once the Release and tap bump are public, roll forward with a new
patch tag.

## Purpose

Panio is a CLI that lets an AI agent fully control a target machine during
low-level software development (bootloader, firmware, OS bring-up). The name
reads as "Pan I/O": the agent wrangles the target's I/O.

Capabilities (full list: docs/dev/architecture.md#capabilities): netboot (`panio
netboot`), HDMI/USB/RFB video with OCR (`video`), serial console with capture logs
(`serial`), combined video+serial dashboard/KVM (`console`), HID injection (`hid`),
named target actions (`action`), whole-target leases (`lease`), switchable USB media (`usb`), and power control
via DTR, hooks, and named levels (`power`, `power-cycle`, `power-state`).

## Architecture

One daemon per hardware subsystem, controlled via SSH. No long-running parent
process; state lives in JSON + PID files under `~/.local/share/panio/<target>/`.
Only `panio` needs to be in PATH; each daemon is a backgrounded subprocess.

## Rust control plane (`cli/`)

The CLI, orchestration, and device glue are Rust. Design and status:
[`notes/config-redesign.md`](notes/config-redesign.md). All config is one
CLI-managed lab file (`~/.config/panio/lab.toml`, or `--lab`/`PANIO_LAB`);
commands re-exec over SSH on the host of the channel they touch. Full internals
(dispatch, ports, helpers, plugin roots, `daemons`, env forwarding, cli/src
module map): cli/AGENTS.md. Cross-cutting rules:

- **Daemon APIs are authenticated.** Every request to a daemon (serialcap,
  hdmicap, hidrig, ch9329) must carry the token in its owner-only discovery file
  and name a loopback `Host`/`Origin` (`src/auth.rs`, byte-identical across the
  four). **`video preview` is the only command that prints a token-bearing URL**;
  every other command prints the token-free `http://127.0.0.1:<port>`, because
  output lands in agent transcripts and CI logs (#196).
- **Helpers live off PATH** in `~/.local/libexec/panio/bin` (only `panio`
  installs to `~/.cargo/bin`); run them with `panio helper [NAME] [ARGS…]`.
- **CLI argument convention**: every runtime command takes the target either as
  an optional positional or as `-t/--target`, never both. A new runtime verb
  flattens `TargetArg` (main.rs) instead of declaring its own target field.
  Channel-config commands (`set`/`add`/`rm`) take `-t/--target` only; so do
  `hid send`, `adb run|input`, `action run`, the `usb` verbs, and `serial send`
  (their positional tail is the helper's args).
- **Name rule**: target, host, and serial interface names follow
  `model::NAME_RULE` (letters, digits, `.`, `_`, `-`; never `.`/`..`; never a
  leading `-`), enforced by `model::validate_name` at `add`/`rename`/`set`.
  Names become path components, so sanitize any name reaching a path.
- **A daemon is stale** only on a proven content difference from the binary on
  disk; missing identity is "unknown", never a restart recommendation.

**Errors follow the exit-status contract** ([`docs/errors.md`](docs/errors.md);
design in `docs/dev/error-contract/design.md`). A failure a consumer might
branch on returns an `error::PanioError` of the right kind (`target_not_found`,
`channel_missing`, `hook_failed`, `daemon_down`, `daemon_request_failed`,
`unreachable_host`); a bare `anyhow!` exits 109 `internal`. Never
`std::process::exit(status.code()…)` with a child's status outside the
documented passthroughs, and use `error::shell_code` there so a signal death is
128+N, not 255. Changing a code is a breaking change. Adding a kind means
updating `Kind`, `Kind::ALL` (an exhaustive match beside it stops the test build
until you look), `EXIT_STATUS_HELP` (a test pins it to `Kind::ALL`),
`docs/errors.md`, and `skills/panio/SKILL.md`.

**Helper state/runtime-dir API** (daemons.rs `helper_env`): panio exports
pre-created `PANIO_STATE_DIR` (durable; `~/.config/panio/helpers/<name>/`)
and `PANIO_RUNTIME_DIR` (discovery/locks/logs; `/tmp/panio-<uid>/<name>/`)
on every helper invocation. Contract: docs/dev/adding-power-helpers.md.

## Module layout

Each directory has an `AGENTS.md`. `cli/` (the `panio` control plane), `hdmicap/` (HDMI capture, RFB,
dashboard), `serialcap/` (serial daemon), `netbootd/` (DHCP + TFTP + HTTP
netboot), `hidrig/` (dual-board KB2040 HID injector), `ch9329/` (CH9329 HID
helper), and `secret/` (shared secret-resolution library) are Rust; `jetkvm/`
(JetKVM HID + H.264-to-RFB helper) is Go; `zigplug/` (Zigbee power helper) is
Python; `cambrionix/`, `dsdrelay/`, `dsdble/`, `phidget/` (Phidgets VINT relay modules over raw USB), `usbtoggle/` (USB sharing-switch `usb` channel helper), `shellyplug/`, and `amt/` are Rust power helpers; `switchport/`
(managed-switch port link/PoE control, TP-Link Easy Smart + SNMP; a link helper wired via
the netboot link hooks, not a power helper) is a Rust helper; `ocr/`
holds the OCR helpers (Apple Vision on macOS, Tesseract on Linux).

## Runtime paths

The runtime base `/tmp/panio-<uid>/` is private: created 0700, and if it
exists it must be a real directory owned by the current user with no group/other
bits (a too-open base we own is tightened; a symlink or another owner's
directory is refused: `platform::ensure_private_dir`). The discovery-file
readers and the ControlMaster socket dir (`cli/src/ssh.rs`) apply the same
check. Daemon stderr logs and serialcap capture files are created 0600.

The base is `${PANIO_RUNTIME_BASE:-/tmp}`, deliberately not `$TMPDIR` (macOS
varies it per environment, hiding a running daemon from other shells) and not
`$XDG_RUNTIME_DIR` (systemd deletes it when the user's last session ends,
breaking daemons that outlive their SSH session). Capture daemons
(hdmicap/serialcap/hid) are per target, in `<base>/panio-<uid>/<daemon>/<target>/`;
host-singleton daemons (zigplug/cambrionix/netbootd) are one per host, in
`<base>/panio-<uid>/<daemon>/`.

| Purpose | Path |
|---|---|
| Lab file | `~/.config/panio/lab.toml` (or `--lab`/`PANIO_LAB`) |
| Helper durable state | `~/.config/panio/helpers/<name>/` (`PANIO_STATE_DIR`) |
| Plugin helpers / skills | `<root>/libexec/panio/bin/`, `<root>/share/panio/skills/` per `PANIO_PLUGIN_PATH` root |
| Netboot state / log | `~/.local/share/panio/<name>/netboot.json`, `netboot.log` |
| Target lease | `~/.local/share/panio/<name>/lease.json` (0600), `lease-history.jsonl`, `lease.lock`; on the target's default host. Guide: docs/leases.md |
| Discovery file | `<daemon dir>/daemon.json`: `{pid, port, token}` + `interfaces:[{name, device, baud}]` (serialcap) or `device` (hid); owner-only |
| Advisory lock / stderr log | `<daemon dir>/daemon.lock`, `daemon.log` (hdmicap and serialcap; truncated on each CLI-spawned start) |
| serialcap capture | `<daemon dir>/capture/<name>/serial.jsonl(.1..)` (rotated JSONL, per interface), `pending.json` (current unterminated line) |

## Source code constraints

- **No hardcoded network addresses, URLs, or hostnames.** Site-specific values
  go in config files under `~/.config/panio/`, populated via setup commands.
  Error messages must be generic. The same applies to docs, fixtures, and
  captured output; see
  [Never commit private infrastructure](#never-commit-private-infrastructure).
- **No new dependencies without discussion.** Keep each crate's dependency set
  lean and justify any new crate in review. (`zigplug` is the one remaining
  Python component; its deps live in `zigplug/pyproject.toml`.)
- **Format and lint.** CI runs `cargo fmt --check` and `cargo clippy
  --all-targets -- -D warnings` per crate (every crate in the Makefile `CRATES`
  list has a job); keep both clean. `make fmt` formats every crate. `zigplug`
  uses `pyink` at line-length 88.
- **Don't call a reference an "oracle"** (prose, identifiers, tests, commits).
  Use `reference`, `expected` vs `actual`, `known-good` (a build or board
  trusted because it has worked), `baseline` (a prior version of our own
  output), or `ground truth` only when the reference cannot itself be wrong (a
  human-verified screen reading, a measured value).
- **`panio setup`** builds native components from the source tree when run
  from a clone (`setup::find_repo_root` walks up from the cwd). Outside a
  checkout (Homebrew, .deb, tarball) it runs only `setup::run_packaged`: setuid
  the installed `netbootd-bpf-helper` (macOS), group membership (Linux).
  `--rust-only` requires a clone and skips steps needing sudo or a second
  toolchain. That split is `setup::SourceStep::on_the_fast_path`; every
  skippable step in `setup::run` is gated on `source_steps()` (#207), so a new
  step means a new `SourceStep` variant, which the compiler forces you to
  classify.

## Remote control pattern

```bash
ssh control-mac "panio netboot set -t target-machine --interface en3 --tftp-root ~/pxe"
ssh control-mac "panio netboot start target-machine"
TFTP_ROOT=$(ssh control-mac "panio netboot tftp-root target-machine")
scp kernel.img control-mac:"${TFTP_ROOT}/kernel_2712.img"
op run --env-file .env -- ssh control-mac "panio power-cycle target-machine"
```

## Adding a new subsystem

**New power-switching hardware is not a subsystem**; it is a standalone helper
binary wired in via the generic power hooks. Follow
[docs/dev/adding-power-helpers.md](docs/dev/adding-power-helpers.md) (hook
contract, CLI conventions, Rust/Python skeletons, verification ladder, PR
checklist). Exemplars: `cambrionix/`, `zigplug/`, and `shellyplug/` (the
simplest: a stateless HTTP one-shot).

A genuine new subsystem (a channel with its own commands/daemon) goes in the
Rust `cli/` crate: a module `cli/src/<subsystem>.rs`; a clap subcommand group +
handlers in `cli/src/main.rs`; config fields in `cli/src/model.rs` and
round-tripping lab-file editing in `cli/src/labfile.rs`; for a daemon with a
PID, state/discovery handling in `cli/src/state.rs` and `cli/src/daemons.rs`.
Hand-update `skills/panio/SKILL.md`, this file, and `docs/`.

A new **crate** also needs a CI job and a `scripts/ci-local.sh` line (PR
checklist step 5), its own `AGENTS.md` for internals, and a mention in Module
layout above.

A new **helper binary** must be registered everywhere the helper set is named,
or it builds green in CI and never reaches users:
- `Makefile` — `CRATES`
- `cli/src/setup.rs` — `HELPER_CRATES` (what `panio setup` installs from a clone)
- `.github/workflows/release.yml` — the `HELPERS` env list **and** the rust-cache
  `workspaces` block (v0.1.13 shipped without `amt` because this was missed)
- `packaging/nfpm.yaml` — the helper list in the package description

`scripts/ci-coverage-check.sh` (the `coverage` job) fails the build when the
`Makefile`, release `HELPERS`, or `HELPER_CRATES` lists omit a crate; the release
smoke test checks every `HELPERS` binary landed in the `.deb`.

## Platform support

macOS (Apple Silicon), Linux (Debian/Ubuntu), and Windows (x86_64-msvc) are
supported; all ten crates build, lint clean under `clippy -D warnings`, and pass
unit tests on all three. The `windows` CI job runs the same fmt/clippy/test
triple as Linux, since Unix-only CI can't tell whether the Windows `#[cfg]` arm
compiles. Artifacts: Homebrew tap (macOS universal tarball, an `all` bottle),
`.deb` + tarball + bottle (Linux), portable zip + winget (Windows).

**Write portable code through `cli/src/platform.rs`** (`current_uid`,
`runtime_root`, `ensure_private_dir`, `make_executable`, `pid_alive`,
`signal_pid` / `try_signal_pid` / `terminate_pid`, `is_superuser`, `detach`,
`exec_replace`), not `libc` or `std::os::unix` at a call site. A trimmed copy is
duplicated into hdmicap, serialcap, ch9329, and hidrig (standalone cargo
projects); **keep the five copies in sync**. Windows has no signals (a daemon
never runs graceful shutdown) and no uid (the runtime namespace hashes
`%USERNAME%`).

**Test the platform split by executing it.** A `#[cfg]` that compiles is not one
that works: a string-shape test passes identically everywhere and never shows
whether a command can launch. Anything that spawns a process, touches the
filesystem, or resolves a name gets a test that executes it.

Windows dispatch, packaging, bench host, signals/uid details, and per-subsystem
OS behavior: docs/dev/platform-support.md

## Removed: `usbhub` (per-port USB hub power)

The `usbhub` helper was removed in full; do not rebuild it without reading
notes/usbhub-removed.md. Use `shellyplug`, `zigplug`, `amt`, `cambrionix`, `dsdrelay`, `dsdble`, `phidget`, or a
relay driven through the generic power hooks instead.

## Known limitations / gotchas

- **Drive through panio, not around its devices.** Never reconfigure or open
  a panio-managed device by hand: `ifconfig`/`ip`/`ethtool` on the netboot
  interface, `screen`/`tio` on a serial port, `kill` on a daemon (also in
  `skills/panio/SKILL.md`). Daemons (netbootd/serialcap/hdmicap/hid) hold the
  device and track state; touching it directly desyncs that state (a stray
  interface address changes what `netif status` reports; opening the serial
  port collides with serialcap). Use `netif mode …`, `serial log`/`send`,
  `daemons stop`.
- **`netif mode off`/`link` toggle the host IP, not the carrier, and `netif
  down-hard` often does not drop it either.** Neither mode admin-downs the
  interface, and `netif status` reports `carrier` independently of `mode`: a NIC
  with **Wake-on-LAN** keeps the PHY energized when down, so `carrier` can read
  `up` in `off` mode. `netif down-hard` does `mode off` + disables WoL
  (`ethtool -s <iface> wol d`, Linux) + admin-down; `mode link`/`netboot` bring
  it back up, but WoL stays off until re-enabled or the adapter is replugged.
  **Admin-down leaves the PHY up on common USB adapters** (measured: AX88179A/B
  and RTL8153 on macOS, RTL8153 under Linux `r8152`; Apple's NCM/ECM drivers
  cannot power the PHY off at all), and once the interface is down the host's
  own `carrier` reads `down` regardless, so `netif status` cannot confirm the
  drop. Check from the target side; for a guaranteed drop, unplug or disable
  the port on a managed switch (docs/netif.md; measured working, helper not
  built yet: #334). One netboot link serves one target; several targets behind
  one adapter need per-target VLANs (`panio netif vlan add`; docs/netboot.md
  "Several targets through one switch"). `del_host_ip` releases the macOS static
  IP via `networksetup -setdhcp` (an `ifconfig` delete won't unset a `-setmanual`
  IP) *and then* `ifconfig <iface> inet <ip> -alias`.
- **Interface configuration requires root:** `netif::configure_interface()` needs
  NOPASSWD sudo (`ifconfig`/`networksetup` on macOS, `ip` on Linux).
- **SSH PATH.** Non-interactive SSH shells often lack `/opt/homebrew/bin`;
  panio probes the Homebrew paths on macOS and `/usr/sbin`+`/sbin` on Linux.
- **Helper `stop` commands authenticate shutdown.** `serialcap`/`hdmicap`/
  `ch9329`/`hidrig stop` call the daemon's token-protected `POST /stop` instead
  of signaling a discovery-file PID (which may be recycled); zigplug does the
  same over its HTTP API. Older daemons without the endpoint need `panio
  daemons stop <name>` (it checks process identity first) before restarting.
- **Daemon shutdown hard-exits.** hdmicap (`/preview`) and serialcap (`/stream`)
  serve infinite responses, so graceful axum shutdown would block forever. On
  SIGTERM each daemon removes its discovery file, waits 300 ms, then
  `std::process::exit(0)`.
- **macOS Local Network privacy gate.** On macOS (Sequoia+), a freshly built,
  non-Apple-signed helper that reaches a LAN host (e.g. `shellyplug`) fails with
  `No route to host` (EHOSTUNREACH) until the launching app is granted Local
  Network access (System Settings → Privacy & Security → Local Network); iTerm
  can need a restart for the grant to take. The tell: it reaches the internet
  but not a same-subnet host. See docs/power.md.
- **Homebrew tap upgrades.** A same-version change to install logic won't `brew
  upgrade`; use `brew reinstall` or bump the formula `revision`. After a local
  `make rust`, the Homebrew keg can shadow the `~/.cargo/bin` panio on PATH.
- **Component gotchas live with the component.** Capture-device identity and the
  macOS capture layer: hdmicap/AGENTS.md. The pty line-rate rule and serial-port
  exclusivity (`tio`/`screen`/serialcap; `serial watch` vs `serial connect`):
  serialcap/AGENTS.md. OCR character confusions: ocr/AGENTS.md.
- **Do not change the target's console font to improve OCR**; other agents rely
  on it (see ocr/AGENTS.md).
