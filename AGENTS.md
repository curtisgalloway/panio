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

# Paniolo — Agent Instructions

## Never commit private infrastructure

**This repository is public. The lab that exercises it is not** — it is
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

**One exception:** `192.168.99.0/24` is paniolo's *own* default for the
point-to-point netboot link (`DEFAULT_HOST_IP` in `cli/src/model.rs`). It is a
product constant, not anyone's network, and belongs in docs and tests.

**Captured output is the easy way to slip.** Terminal casts, screenshots, and
screen recordings reproduce whatever the shell prompt, serial log, and command
line happened to contain at the time. Read a cast before committing it, and
look at a screenshot — a rendered image cannot be scrubbed by a later text
edit, only re-made.

Some material committed before this rule still violates it. That is known, is
not urgent (it is private-range addresses and internal names, no credentials),
and is being cleaned up opportunistically. Do not treat it as precedent, and do
not launch an unrequested scrub.

## Before opening a PR

**Precondition (hard gate): the PR may not be opened until all documentation
*and* usage help reflect the change.** A behavioral or surface change with stale
docs/help is an incomplete PR — bring them current in the *same* PR, never a
follow-up. Run through this checklist before calling `gh pr create`:

1. **Update docs that the PR affects.** For each changed subsystem, check:
   - `docs/<subsystem>.md` — commands, config fields, workflows
   - `docs/dev/architecture.md` — whole-system design, data flows, runtime paths (if structure changed)
   - `docs/README.md` — the docs index (if a doc was added/removed)
   - `mkdocs.yml` — a new `docs/*.md` needs a `nav:` entry (end-user doc) or an
     `exclude_docs:` entry (repo-only design record); the docs build runs
     `mkdocs build --strict`
   - `docs/dev/requirements.md` — the requirements tracker status (if scope/progress changed)
   - `README.md` — capabilities table, installation steps
   - `AGENTS.md` — module layout, command descriptions, architecture notes
   - `evals/scenarios/*.toml` — the agent-eval scenarios assert CLI behavior.
     `python3.12 evals/run.py --check` (Python ≥ 3.11) verifies only that every
     *single-line* `reference` entry names a real subcommand path with flags
     that subcommand accepts, and that the T1-safe allowlist
     (`evals/graders/t1_config.SAFE`) names real groups/subcommands. It does
     **not** check positional arguments, flag values, multi-line (prose)
     references, or a scenario's `goal`/grader expectations — read the
     scenarios that mention the changed command yourself.
   Include doc updates in the same PR, not a follow-up.

   Also check the diff for private infrastructure — real hostnames, private
   addresses, `/home/<user>` paths, hardware serials — per
   [Never commit private infrastructure](#never-commit-private-infrastructure).
   Captured casts and screenshots need reading, not just grepping.

2. **Update the CLI usage help.** Every new/changed command, subcommand, flag,
   or argument must have an accurate clap doc comment (the `///` lines that
   become `--help` text). Verify the rendered output for each touched command
   (`paniolo <cmd> --help`, `paniolo <cmd> <sub> --help`) — including parent
   summaries (e.g. a group's one-line description) so they still match the
   subcommands beneath them.

3. **Update the usage skill (`skills/paniolo/SKILL.md`).** This is the
   agent-facing skill for *driving* a target. If the PR adds, removes,
   or changes a user-facing command, flag, or workflow, update the relevant
   section (and the "gotchas" list) so an agent using paniolo sees the new
   surface. The repo copy at `skills/paniolo/SKILL.md` is the canonical source;
   edit it here (however you install or link it into your agent's skills
   directory). Purely internal changes that don't alter the CLI surface can skip
   this. A companion skill, `skills/kvm-puppeting/SKILL.md`, teaches the
   GUI-puppeting *doctrine* (the look-act-settle-verify loop, keyboard-first
   navigation, pixel→logical mouse scaling) on top of the `video`+`hid`
   commands; update it too if you change the surface it relies on. A third,
   `skills/control-host/SKILL.md`, is the executable procedure for *building*
   a control host from blank media (flashing, the cloud-init seed, enrollment)
   — it owns those commands, and `docs/control-host.md` points at it rather
   than repeating them, so keep the procedure in one place. These
   skills ship with paniolo and are reachable via `paniolo skill` (see the
   Rust control-plane notes). **Adding or removing a skill** also means a new
   `contents` entry in `packaging/nfpm.yaml` (one explicit file→dst line per
   skill) and a copy line is unnecessary for `setup.rs`/the tarball (both
   enumerate `skills/` automatically).

4. **Validate before pushing.** For every crate you touched, run all three
   gates from that crate's directory. CI runs them per crate and fails the
   job on any one of them:

   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

   `cargo fmt --check` is the one that is easy to skip and cheap to fail on.
   Passing tests and clean clippy say nothing about it, and a formatting diff
   fails the crate's job exactly as hard as a broken test. Run `cargo fmt`
   (no `--check`) to fix it, and note the check covers the whole crate, not
   just your diff.

   To catch the Linux-only failures without a round-trip, run
   `scripts/ci-local.sh` — it mirrors every GitHub Linux CI job (`cli`,
   `serialcap`, `netbootd`, `hdmicap`, `cambrionix`, `ch9329`, `hidrig`,
   `shellyplug`, `amt`, `secret`) in a Linux environment, e.g. a Lima VM:
   `limactl shell <instance> -- bash -l scripts/ci-local.sh`. It needs a Linux
   box or VM — it apt-installs and copies the tree — so treat it as the fuller
   check rather than the quick one; the three commands above are the minimum
   for a pure-Rust change and run fine on the host. (The macOS-only job —
   hdmicap AVFoundation + visionocr — runs on the host.) Note `cli` is the
   primary control-plane crate; don't let its tests rot.

   CI also runs four lint jobs that are not crate jobs and that
   `scripts/ci-local.sh` does not mirror — none needs a Linux box, so run
   them on the host: `shellcheck` over every tracked script
   (`git ls-files -z '*.sh' | xargs -0 shellcheck`), `zigplug`
   (`cd zigplug && uv sync --frozen && uv run pyink --check . && uv run
   python -m compileall -q src`), `evals-check` (`python3.12 evals/run.py
   --check`, from step 1), and `actions-pinned`
   (`bash scripts/ci-actions-pinned.sh`, step 7).

5. **Every crate has a CI job — no exceptions.** A crate is not finished until
   `.github/workflows/ci.yml` has a job for it (`working-directory: <crate>`,
   then fmt + clippy `-D warnings` + test, copied from an existing crate job)
   *and* `scripts/ci-local.sh` has a matching `crate_job` line. This is
   enforced mechanically, not by memory: the `coverage` job runs
   `scripts/ci-coverage-check.sh`, which fails the build when a crate has
   neither. If a crate genuinely cannot run in CI, add it to `EXEMPT` in that
   script **with the reason written down** — never weaken or delete the check.

   The rule exists because five crates (`cambrionix`, `ch9329`, `hidrig`,
   `shellyplug`) went months with no CI at all: jobs were added
   per-crate as each one happened to matter, and nothing noticed the ones that
   were skipped.

6. **Code scanning is advanced setup, not default setup.** CodeQL runs from
   `.github/workflows/codeql.yml`, which this repo owns; GitHub's "default
   setup" must stay disabled, because the two configurations cannot both be
   active. The workflow exists so the language list is explicit — it
   deliberately omits Swift, since `ocr/visionocr.swift` is a macOS-only
   Vision wrapper with no Swift package for the extractor to build — and so
   there is somewhere to put extractor options, which default setup does not
   expose. Don't move it back to default setup to silence a prompt.

   The workflow pins a **Rust 1.94 sysroot** for the extractor via
   `CODEQL_EXTRACTOR_RUST_OPTION_SYSROOT` and `..._SYSROOT_SRC`. Don't remove
   that step: CodeQL 2.26.x cannot parse std newer than 1.94 and the runners
   ship 1.97+, so without it macro expansion fails for `format!`, `vec!`,
   `assert_eq!` and friends, and ~68 of 69 files come back "extracted with
   errors" (upstream github/codeql#19982). Two dead ends already ruled out:
   installing `rust-src` for the ambient toolchain does nothing (1577 macro
   failures with and without it, identical), and setting only `_SYSROOT_SRC`
   leaves the ambient binary sysroot in place. Revisit the pin when #19982
   closes.

7. **Every workflow action is pinned to a commit SHA.** A `uses:` line under
   `.github/workflows/` names a full 40-hex commit with the version it
   resolves to in a trailing comment (`actions/checkout@<sha> # v6.1.0`),
   never a movable tag. `scripts/ci-actions-pinned.sh` (the `actions-pinned`
   job) fails the build otherwise, and `.github/dependabot.yml` bumps the SHA
   and its comment together, weekly, in one grouped PR. To add or bump one by
   hand, resolve the tag through the API rather than from memory:
   `gh api repos/<owner>/<repo>/git/ref/tags/<tag>`, and when
   `.object.type` is `tag` (annotated) dereference it via `git/tags/<sha>`
   to reach the commit.

8. **Push, open the PR, and merge only when asked.** Get the branch ready by
   committing locally, but treat `git push`, `gh pr create`, and merging as
   gated on the maintainer's explicit instruction — don't push or open a PR on
   your own initiative, and never merge. When told to, open with `gh pr create`;
   the merge decision always belongs to the maintainer.

## Cutting a release

Releases are **tag-driven**: pushing an annotated `v*` tag to `origin` triggers
`.github/workflows/release.yml`, which builds the Linux, Windows, and macOS
packages, publishes a GitHub Release, bumps the Homebrew tap, and refreshes
the signed apt repository served from the GitHub Pages docs site. There is no version to edit in
source — the **tag is the single source of truth**. Every crate's
`Cargo.toml` stays at `version = "0.1.0"`; the workflow derives the package
version from the tag name (`${GITHUB_REF_NAME#v}`), so don't bump the manifests.
`paniolo --version` reports that same string because every package build
exports it as `PANIOLO_VERSION` and `cli/src/main.rs` bakes it in at compile
time; a build without the variable (`cargo install --path`, a dev checkout)
prints `0.1.0 (unversioned dev build)` so nobody mistakes the placeholder
for an installed version. Each package smoke step asserts the two agree.

Steps:

1. **Pre-flight.** Be on `main`, clean, and in sync (`git fetch origin && git
   status` → `main...origin/main`, nothing ahead/behind). Confirm CI is green on
   the HEAD commit you're about to tag (`gh run list --branch main --limit 5`) —
   the release builds that exact commit, so a red main means a broken release.

2. **Pick the version.** Read the latest tag (`git tag --sort=-creatordate |
   head`), then choose by what the release contains, not by habit:

   - **Minor** (`0.X.0`) for anything substantive: a new command, channel or
     helper; a change to a daemon's wire protocol or its lifecycle; a
     behavioral change a user or an agent could notice; and larger bug fixes,
     including any fix that changes what a target machine actually sees.
     Anything that makes an already-running daemon incompatible with a newer
     CLI belongs here without question.
   - **Patch** (`0.x.Y`) only for small bug fixes and documentation.

   The project spent `0.1.1` through `0.1.19` patch-bumping everything, which
   put two compatibility breaks — token-authenticated daemons in `0.1.17`, and
   `0.1.19`'s authenticated shutdown, which needs `paniolo daemons stop <name>`
   before a newer CLI can replace an older daemon — behind a version number
   that signalled nothing. When a release is on the line between the two, take
   the minor: the cost of a spare minor bump is nothing, and the cost of a
   silent break is somebody's bench at midnight.

3. **Create an annotated tag** whose subject mirrors the prior ones —
   `vX.Y.Z: <lowercase one-line summary of the headline change>` — with an
   optional body paragraph for detail. Tag as the releaser, matching the
   email convention already in the history so GitHub doesn't reject the push
   (`git log --format='%ae' | sort -u`; the project uses each author's
   `<ID>+<user>@users.noreply.github.com` form):

   ```bash
   git -c user.email='<your-github-noreply-email>' -c user.name='<Your Name>' \
       tag -a vX.Y.Z -m 'vX.Y.Z: <summary>' -m '<body paragraph>'
   ```

4. **Push the tag** (this is the outward-facing, hard-to-reverse step — gated on
   the maintainer's explicit go-ahead, like push/merge):

   ```bash
   git push origin vX.Y.Z
   ```

   The push fans out to a package job per platform, then the jobs that
   publish them:
   - `package` (matrix: amd64, arm64) builds `.deb`s and tarballs inside a
     `debian:bookworm` container (glibc 2.36, so the packages run on
     Debian 12+ and current Raspberry Pi OS) and smoke-tests the `.deb`.
   - `package-windows` builds a portable `x86_64-pc-windows-msvc` zip,
     Authenticode-signed when `vars.AZURE_SIGNING_ACCOUNT` is set (gated
     behind the `release` environment's reviewer approval; absent, it ships
     unsigned).
   - `package-macos` builds one **universal** (arm64 + x86_64) tarball in the
     Homebrew keg layout (`bin/paniolo`, `libexec/bin/<helper>`,
     `share/paniolo/skills/`), ad-hoc signed by default — which is all
     Homebrew needs, since it sets no quarantine attribute on what it
     installs — or Developer-ID-signed and notarized when six secrets
     (`MACOS_SIGNING_CERT_P12`, `MACOS_SIGNING_CERT_PASSWORD`,
     `MACOS_SIGNING_IDENTITY`, `MACOS_NOTARY_APPLE_ID`,
     `MACOS_NOTARY_PASSWORD`, `MACOS_NOTARY_TEAM_ID`) are all set. That path
     is untested as of this writing — no signing cert is configured on the
     repo yet.

   Every asset from all three — every `.deb`, `.tar.gz`, and `.zip` — ships
   with a `.sha256` sidecar (`sha256sum`/`shasum -a 256` format: `<hex>
   <two spaces><filename>`), so a consumer can verify or re-pin a download
   without fetching it first; the Homebrew tap's binary-only formula is the
   first one that needs this. `release` attaches everything (`dist/*` from
   every job) to a new GitHub Release with auto-generated notes; `bump-tap`
   fires a `repository_dispatch` at `curtisgalloway/homebrew-tap` so it
   re-pins its formula to the new tag (needs the `HOMEBREW_TAP_DISPATCH_TOKEN`
   secret — absent, it warns and skips rather than failing); and
   `refresh-apt-repo` re-dispatches `.github/workflows/docs.yml` after the
   Release is published, so the Pages site's `/apt/` pool picks up the new
   `.deb`s (never the `.deb.sha256` sidecars — `gh release download --pattern
   '*.deb'` only matches names actually ending `.deb`). The docs workflow
   rebuilds the apt pool statelessly from the newest `APT_POOL_RELEASES`
   (currently 5) GitHub Releases and signs it with the `APT_SIGNING_KEY`
   secret (`packaging/scripts/build-apt-repo.sh`); on the canonical repo a
   missing key **fails** the docs build rather than deploying a site without
   `/apt/`, since a Pages deploy replaces the whole site.

5. **Watch it land.** `gh run list --workflow=release.yml --limit 1`, then
   `gh release view vX.Y.Z` once green to confirm the artifacts and notes.
   Also confirm the dispatched `docs.yml` run went green — a failed apt-pool
   refresh means apt clients keep seeing the previous version.

A botched tag that hasn't shipped can be moved (`git tag -f` + `git push -f
origin vX.Y.Z`), but once the Release and tap bump are public, roll forward with
a new patch tag instead.

## Purpose

Paniolo is a CLI tool that lets an AI agent fully control a target machine
during low-level software development (bootloader, firmware, OS bring-up).
"Paniolo" is the Hawaiian word for cowboy — the agent wrangles the target.

Current capabilities:
- DHCP + TFTP + HTTP netboot over a direct USB-Ethernet link (`paniolo netboot`) —
  Raspberry Pi (TFTP) plus UEFI **PXE** and **HTTP Boot** (IPv4) for EDK2 boards,
  selected per-request by DHCP vendor class (option 60)
- HDMI/USB capture via hdmicap warm-stream daemon (`paniolo video`)
- Serial console — interactive (tio) or daemon-backed for the web dashboard (`paniolo serial`);
  one daemon **per target** owns that target's several named interfaces, each with a
  timestamped rolling capture log queryable by line range (`paniolo serial log -i <name>`)
- Combined video+serial web dashboard (hdmicap's `GET /`: a noVNC view of hdmicap's own `GET /rfb` on top, with keyboard/mouse through the hid daemon, and an xterm.js terminal below)
- Network video sources via RFB (`paniolo video set --device rfb+hid:` for a JetKVM, or `rfb://HOST:PORT`): hdmicap is the RFB client; the first consumer is the JetKVM, whose daemon decodes H.264 with `ffmpeg` and serves RFB (docs/video.md, notes/network-video-rfb.md)
- On-device OCR of the captured screen (`paniolo video read [target] [--stable]`, which wraps hdmicap's `GET /ocr`; also the dashboard OCR button): Apple Vision on macOS, Tesseract on Linux
- USB HID input (keyboard/mouse injection) via a generic helper hook (`paniolo hid send`); the `hidrig` helper drives the dual-board KB2040 injector — it composes HID reports in Rust and writes binary frames to the control board's USB-CDC endpoint, which relays them over I2C1 to the target board (the "dumb pipe", docs/dev/hid-dual-board-design.md; command vocabulary in docs/dev/hid-serial-protocol.md). `hidrig serve` runs a daemon that owns the control link and re-exposes the command vocabulary over a WebSocket, so `paniolo console` works as a **KVM** — stream the browser's keyboard + absolute mouse (`moveabs`) to the target, intermixed with CLI injection on the one wire. The same control board can also **bridge the DUT serial console** (its hardware UART, re-exported by the daemon as a PTY into the `serial` channel) and **switch DUT power** via a relay (`hidrig power off|on|cycle`), so one USB device backs the target's HID, console, and power (design §6–§7; the relay/power path is hardware-verified, incl. NVM state persistence across a control-board reset — the console bridge is not yet)
- Switchable USB media via a generic per-target `usb` channel (`paniolo usb attach-host|attach-target|state`): one physical USB device routed to the control host or the target, never both. Supported today on the **Openterface KVM-Go**, whose onboard microSD reader sits behind an FSUSB42 mux driven by the same CH32V208 (and the same serial port) as its `hid` channel — so the `ch9329` helper backs both, and the two channels normally carry the same `--cmd`. The point is hands-free *physical* boot media, which firmware can see and streamed virtual media generally cannot. Like the power hooks the helper is opaque, but the vocabulary is **fixed** rather than passed through: paniolo appends `usb host`, `usb target`, or `usb state`, keeping the surface a constrained remote host must expose to three verbs. Guide: docs/usb.md; clean-room protocol: notes/openterface-usb-mux-spec.md. The Mini-KVM's switchable USB-A port uses a different mechanism (a register write over the capture chip's HID config interface) that is documented but not yet implemented by any helper
- Power control via DTR (J2 wiring; **opt-in per serial interface** via `power_button = true` — `serial dtr`/`reset` refuse interfaces that haven't declared it) or generic shell-command hooks (`on_cmd`, `off_cmd`, `cycle_cmd`, `state_cmd`): `paniolo serial dtr`, `paniolo power on/off`, `paniolo power-cycle`, `paniolo power-state`. Note: "reboot over the serial console" means `serial send <t> "reboot"` (software), *not* the DTR `serial reset` (hardware). Helpers that wire into the hooks: `cambrionix` (Cambrionix hub port power via control UART), `zigplug` (Zigbee smart plugs via a CC2652 coordinator dongle), `shellyplug` (Shelly Gen2+ smart plugs/relays over the device's local HTTP RPC API — no cloud/HA/Matter), and `amt` (Intel AMT/vPro machines over WS-Management on port 16992 with HTTP Digest auth — per-target power with no plug hardware, plus true power-state readback from the ME; password via `AMT_PASSWORD`, `--password-file` or `--password-command`, never in the lab file). The dual-board `hidrig` control board can also drive a DUT power relay (`hidrig power off|on|cycle`) as a power-helper backend, consolidating HID + console + power on one USB device

## Architecture

One daemon per hardware subsystem, controlled via SSH. No
long-running parent process for control operations; state lives in JSON + PID files under
`~/.local/share/paniolo/<target>/`. The `paniolo` binary is the only process
that needs to persist in PATH; each subsystem daemon is a backgrounded
subprocess.

## Rust control plane (`cli/`)

The CLI + orchestration + device glue is rewritten in Rust (the `cli/` crate),
finishing the Python→Rust migration the daemons started. Design + status:
[`notes/config-redesign.md`](notes/config-redesign.md). Key differences from the
Python tree below:

- **Config is one CLI-managed lab file** (`~/.config/paniolo/lab.toml`, or
  `--lab`/`PANIOLO_LAB`): hosts + targets, each target's hardware as *channels*
  (`netboot`, `serial[]`, `power`, `video`, `hid`, `usb`, `adb`) with per-channel host
  binding.
  Edited surgically via `toml_edit` (hand-comments survive); validated on load
  and before every save. The legacy `~/.config/paniolo/targets/*.toml` files are
  not used by the Rust CLI.
- **Dispatch is per-channel**: a command resolves the host of the channel it
  touches and re-execs there over SSH against a shipped one-target slice.
  Composites (`console`) require co-located channels.
- **Daemons bind OS-assigned ports** (port 0) and are found via their
  `daemon.json` discovery files — fixed defaults collided with stale tunnels.
- **Daemon APIs are authenticated.** Each daemon (serialcap, hdmicap, hidrig,
  ch9329) generates a 32-byte token at start and publishes it as `token` in
  its discovery file, written owner-only (0600, temp file + rename). Every
  request must carry it — `Authorization: Bearer <token>` (the CLI, via
  `daemons::Endpoint`; helper one-shots routed through a daemon) or
  `?token=<token>` (the dashboard's page load, `<img>` and WebSockets, which
  cannot set headers) — and must name a loopback `Host` and, when a browser
  sends one, a loopback `Origin`; `Access-Control-Allow-Origin` echoes that
  one origin, never `*`. The layer is each daemon's `src/auth.rs`,
  byte-identical across the four like `platform.rs`; only hdmicap's vendored
  `/xterm*` assets are exempt. A daemon started by an older paniolo has no
  token (the CLI then sends none) — a named `paniolo daemons restart`
  replaces it. The URL handed to a **browser** always carries `?token=`, and
  `dispatch::remote_daemon_endpoint` reads the token over SSH so a tunnelled
  `console` carries it too — but **`video preview` is the only command that
  prints it**, and the only place that builds one (inline, in its own arm:
  there is no `preview_url` helper any more, so nothing else can reach for it
  by accident). Every other command prints the token-free
  `http://127.0.0.1:<port>` from `Endpoint::base_url`, because that output
  lands in agent transcripts and CI logs (#196). `video preview --open` prints
  nothing openable at all, and a `console` whose browser launch fails writes
  the URL to a 0600 file rather than printing it.
- **Netboot is rust-engine only** (netbootd); the pure-Python DHCP/TFTP engine
  exists only in the legacy tree.
- **Helpers live off PATH** in the private libexec dir
  (`~/.local/libexec/paniolo/bin`): only `paniolo` itself installs to
  `~/.cargo/bin`. `daemons::find_binary` resolves libexec → PATH → legacy
  `~/.cargo/bin`; hook commands (`*_cmd`, hid `cmd`) run via `sh -c` with
  libexec prepended to PATH, so lab files keep referencing helpers by bare
  name. `paniolo helper [NAME] [ARGS…]` lists or runs them directly.
- **Bundled skills are self-describing**: the agent skills under `skills/`
  (`paniolo`, `kvm-puppeting`, `control-host`) install to
  `~/.local/share/paniolo/skills` (and `/usr/share/paniolo/skills` for the
  Linux packages). `paniolo skill [NAME]` lists them with their frontmatter
  descriptions, or prints one `SKILL.md` (`--path` for the file path) — the
  share/ analogue of `paniolo helper`, so an agent can discover and read them
  without the harness pre-loading them (skills.rs).
- **CLI argument convention**: every runtime command takes the target
  **either** as an optional positional (`netboot start pi5`, `video stop pi5`)
  **or** as `-t/--target` (`netboot start -t pi5`) — never both at once, which
  clap refuses. One flattened `TargetArg` (main.rs) supplies both spellings and
  the single `name()` that merges them, so the two cannot drift apart per verb
  the way they had (#203) and so `resolve_single_target`'s "specify one with
  -t" is true of every command that prints it. A new runtime verb flattens
  `TargetArg`; it does not spell out its own target field.
  Channel-config commands (`set`/`add`/`rm`) take `-t/--target` only, and
  refuse a positional: a command that creates or destroys configuration should
  not act on an implicit target. `hid send`, `adb run`, `adb input`, and the
  `usb` verbs also take `-t` only, because their positional tail is the
  helper's / `adb`'s args; `serial send` likewise (it reads two positionals as
  `<target> <text>`, one as just the text). `doctor` keeps a bare positional:
  omitting it checks **all** targets rather than a single implied one, so it is
  not the same argument. One config-command exception: `target rename OLD
  NEW` takes two bare positionals and no `-t` (it renames the target itself,
  carrying all channels and lab-file comments; config-only — running daemons
  keep their runtime dirs under the old name, so `stop` and re-`watch` them).
- **Name rule**: target, host, and serial interface names are constrained to
  `model::NAME_RULE` — letters, digits, `.`, `_`, `-`; never `.` or `..`;
  never leading `-` (it would read as an option where the name becomes a
  positional). Enforced by `model::validate_name` in the labfile editor at
  `add`/`rename`/`set` time, not on load — a hand-written lab with an odd name
  still reads, but the CLI never writes one. Names become path components
  (`state::target_dir`, `daemons::runtime_rel`/`sanitize_component`), so
  anything that reaches a filesystem path from a name that predates this rule
  is sanitized again, defensively, at that point.
- **`video set --ocr-mode {text|gui}`** selects the OCR engine on Linux (see
  [dev/ocr.md](docs/dev/ocr.md#linux-needs-two-engines-the-other-platforms-need-one));
  unset is the platform default everywhere. The field travels with the video
  channel through remote dispatch, same as `--device`.
- **Env forwarding to a control host**: a fixed allowlist (`ssh::FORWARDED_ENV`
  — today just `AMT_PASSWORD`) is carried across a non-interactive dispatch
  (`run`/`run_passthrough`/`run_stdout_to`) over the remote command's stdin, so
  it never appears in the control host's `ps` output; `run_interactive` (`serial
  connect`, `adb shell`) forwards nothing — its stdin is the user's own
  terminal. See
  [distributed-control.md](docs/distributed-control.md#env-forwarding-to-a-control-host).
- **`paniolo daemons`** is the unified daemon inventory: every discovery-file
  daemon under `/tmp/paniolo-<uid>/` (the per-target capture daemons listed as
  `serialcap[<target>]`, `hdmicap[<target>]`, `hid[<target>]`; plus host-singleton
  zigplug), netbootd via its state files, plus *stray* helper processes running out of
  the libexec dir (wedged one-shots). `paniolo daemons stop [NAME…|--all]
  [--force]` TERMs them (netbootd via its proper interface-restoring stop),
  escalating to KILL with `--force` after a 3 s grace period. A `NAME` is
  either form the list prints: bare `serialcap` (every instance) or
  `'serialcap[pi5]'` (one target's; quote it, `[]` is a shell glob). Before
  any signal the pid's command line is re-checked for the daemon's binary
  name, so a pid the kernel has recycled is skipped with a message, and a
  signal the OS refuses (EPERM) is reported and fails the command rather than
  being printed as `TERM`. A stray is a process whose *program path* (argv[0])
  is in a helper dir — not one whose arguments merely mention it (the `cargo
  install --root …/libexec/paniolo` that `make install` runs) — and children
  of a known daemon (sudo's netbootd on Linux, hdmicap's OCR helper) are
  never strays.
  A stray running a `daemon` subcommand is not a wedged one-shot but an
  **untracked daemon** — one that outlived its discovery file, which is
  paniolo's only record of it. On Linux the runtime base is under `/tmp`, and
  Debian's stock `q /tmp 1777 root root 10d` deletes a discovery file once its
  daemon has been running ten days without a command against it; the daemon
  keeps its capture device, `show` calls the channel stopped, and the next
  `watch` dies on the advisory lock the orphan still holds. `paniolo daemons`
  lists these separately, `video show` / `serial show` report `running,
  untracked (pid N)`, and `video watch` / `video stop` / `serial watch` /
  `serial stop` reap one (TERM, then KILL) before starting a replacement — its
  port and token died with the file, so a signal is the only handle left
  (`daemons::untracked_of`/`untracked_on_devices`, GitHub #187). A daemon is
  matched to a channel by the devices its command line names, and the two
  capture daemons spell that differently: hdmicap takes one `--device`,
  serialcap repeats `--interface NAME=DEVICE@BAUD[:SENSE]`, so the device is
  parsed out of each interface value. An overlap of one device is enough — that
  is the one the replacement would fail to open. The
  `.deb` ships `/usr/lib/tmpfiles.d/paniolo.conf` (`x /tmp/paniolo-*`) so the
  files stop being swept; a `make install` host should add that drop-in itself.
  A daemon keeps running its binary from when it started; an upgrade or rebuild
  replaces that binary on disk but not the running process. The CLI stamps each
  capture daemon's SHA-256 content digest and pid at spawn (`binmeta.json`);
  Linux also reads `/proc/<pid>/exe` to compare the actual running bytes.
  Only a proven content difference is **stale** in `paniolo daemons` (and on
  `serial show` / `video show`). Identical bytes after reinstall/path/mtime
  changes remain current. Missing identity or unavailable content is explicitly
  **binary identity unknown**, never a restart recommendation; metadata alone
  proves nothing. Digest fields are optional so legacy records still load.
  The platform hash tools are best-effort (Linux sha256sum, macOS shasum,
  Windows Get-FileHash); unavailable hashing keeps the status unknown.
  Automatic watch restarts and `--stale` select only proven content changes.
  `paniolo daemons restart [NAME…|--all|--stale]`
  cleanly cycles serialcap/hdmicap from the current binary (reusing the lab's
  channel config; it waits for the old process to exit so the new one doesn't
  race it for an exclusive device). netbootd is not auto-restarted — cycle it
  via `paniolo netboot start/stop`, since that touches an in-flight boot.
  Replacing a daemon has two traps, both of which bit in #193 when a restart
  across the `0.1.17` token boundary made every graceful stop fail and every
  daemon get SIGKILLed. First, "the old process is gone" is
  `state::is_named_process_pending`, not a bare name match: a process between
  SIGKILL and reaping is alive to `kill(pid, 0)` and *nameless* to a
  command-line match, so a name match alone releases the device to a
  replacement while the corpse still holds its advisory lock. Second, a killed
  daemon cannot remove its own discovery file, so the replacement's startup
  wait must refuse it — `daemons::wait_for_replacement` rejects the pid being
  replaced, and `stop_capture_daemon_and_wait` reaps the file (guarded by pid)
  once the process is gone. Without both, a failed restart reports success and
  quotes the dead daemon's port as the new one's.

```
cli/src/
  main.rs       clap CLI — all command groups + runtime handler bodies
  model.rs      typed lab (serde), validate(), resolved per-channel view, channel_host
  labfile.rs    toml_edit comment-preserving lab editor (the write side)
  error.rs      the error contract: Kind → exit code, PanioloError (kind +
                target/channel/host/child_exit), classify/report for main(),
                hook_failed, shell_code (passthroughs), the --json-errors
                object and the --help exit-status table (docs/errors.md)
  dispatch.rs   per-channel re-exec: slice building/shipping, maybe_dispatch,
                run_subcommand, remote_daemon_endpoint (port + token over ssh)
  ssh.rs        SSH transport: ControlMaster run/passthrough/interactive, forward
                (tunnels); FORWARDED_ENV + stdin_prelude carry an allowlisted var
                (AMT_PASSWORD) to a non-interactive remote command over its stdin
  daemons.rs    shared daemon contract: find_binary (libexec → PATH →
                legacy ~/.cargo/bin), hook_path, daemon.json discovery
                (Endpoint = port + token; get/post attach the bearer header,
                http_url/ws_url carry it as ?token= for a browser), wait
  serial.rs     bounded `serial wait` forwards a receive boundary/deadline/raw
                and total-output caps to serialcap without opening devices;
                MCP serial_wait maps the same read-only surface.
                serialcap orchestration + tio exec + /input + device listing
                + read-only health_snapshot (`serial status`, MCP serial_status)
                 + offline read_evidence (`serial evidence`, MCP serial_evidence)
  video.rs      hdmicap orchestration (daemon start/stop, client passthrough)
  adb.rs        adb transport (argv build, shell exec, run/input passthrough,
                exec-out screencap → PNG) — a generic transport in core, no helper
  netboot.rs    netbootd lifecycle (spawn with log, stop, status)
  netif.rs      interface discovery/config (sudo), netboot/link/ffx/off modes
  power.rs      generic power hooks (on/off/cycle/state_cmd via sh -c), DTR via
                serialcap /button (+ direct-serial fallback), power_on sense
  state.rs      netboot state files (JSON-compatible with the Python's)
  doctor.rs     config-vs-reality probing (local + over SSH), including
                whether a netboot interface holds its configured host_ip
  discover.rs   hardware inventory + the configure proposal block
  setup.rs      installer: paniolo CLI onto PATH (~/.cargo/bin); helpers into
                the private libexec dir (~/.local/libexec/paniolo/bin) via
                cargo install --root; bpf-helper setuid, OCR helpers, zigplug
                (uv tool, shim in libexec), Linux groups; --rust-only fast path;
                installs the bundled skills into ~/.local/share/paniolo/skills
  skills.rs     `paniolo skill`: discover + read the bundled agent skills
                (skills_dirs: repo checkout → ~/.local/share → CLI-relative
                share → /usr/share/paniolo/skills, like daemons.rs helper_dirs
                but under share/), list with frontmatter descriptions, print
                one SKILL.md (or --path), install_bundled() for setup.rs
  mcp.rs        `paniolo mcp` (experimental): hand-written MCP server,
                newline-delimited JSON-RPC over stdio (initialize, ping,
                tools/list, tools/call). stdout is the protocol channel, so a
                tool that drives a channel runs this binary as a child (stdin
                null, stdout captured) instead of calling a handler that
                prints; target_list reads the lab in-process. Each
                CLI-backed tool is one cli_call() → argv (a test parses every
                one with the real clap Cli); writes flag an unknown outcome
                on exit 4/22. Design: notes/mcp-server.md; user doc:
                docs/mcp.md
```

**Errors follow the exit-status contract** ([`docs/errors.md`](docs/errors.md);
design in `docs/dev/error-contract/design.md`). A new failure a consumer might
branch on returns an `error::PanioloError` of the right kind (use
`target_not_found`, `channel_missing`, `hook_failed`, `daemon_down`,
`daemon_request_failed`, `unreachable_host`); a bare `anyhow!` exits 109
`internal`. Never `std::process::exit(status.code()…)` with a child's status
outside the documented passthroughs, and use `error::shell_code` there so a
signal death is 128+N, not 255. Changing a code is a breaking change; adding a
kind means updating `Kind`, `Kind::ALL` (an exhaustive match beside it stops
the test build until you look), `EXIT_STATUS_HELP` (a test pins it to `Kind::ALL`),
`docs/errors.md` and `skills/paniolo/SKILL.md`.

**Helper state/runtime-dir API** (daemons.rs `helper_env`): paniolo exports
`PANIOLO_STATE_DIR` (`~/.config/paniolo/helpers/<name>/`, durable) and
`PANIOLO_RUNTIME_DIR` (`/tmp/paniolo-<uid>/<name>/`, discovery/locks/logs) —
directories pre-created — on every helper invocation: hook commands (named by
the hook's program basename, see `hook_helper_name`; it skips leading
`VAR=value` assignments — `AMT_PASSWORD=... amt-tool cycle` names `amt-tool`,
not the assignment — and sanitizes the result the same way a target name does,
since a hook string is hand-written and never runs through
`model::validate_name`), `paniolo helper` passthrough (sanitized the same way
on its host-singleton branch), and daemon spawns. `doctor.rs` probes a hook's
program the same way (`daemons::first_program_token`), so a credential-prefixed
hook isn't misreported as a missing binary. The per-target capture daemons
(serialcap/hdmicap/hid) append a `/<target>` segment to their runtime dir
(`/tmp/paniolo-<uid>/<name>/<target>/`) so multiple targets capture concurrently
on one host; host-singleton helpers (zigplug/cambrionix/netbootd) use the base
`<name>/` form above. The base honors `$PANIOLO_RUNTIME_BASE` (default `/tmp`).
Channel daemons use the channel name (hidrig
publishes under `hid`). Helpers prefer the env vars, falling back to the same
literal paths standalone; hdmicap/serialcap/hidrig/zigplug all do, and
zigplug lazily migrates its `zigbee.db` from the legacy top-level
`~/.config/paniolo/` location into its namespaced dir. Contract for new
helpers: docs/dev/adding-power-helpers.md.

## Module layout

```
hdmicap/         Rust crate: warm-stream HDMI capture daemon
  build.rs       compiles src/capture_avf.m via cc on macOS, links AVFoundation
  src/
    main.rs      CLI subcommands: daemon, devices, shot, watch, preview, stop
    capture.rs   capture backends: v4l (Linux, raw MJPEG tee + turbojpeg);
                 macOS module wraps the C ABI of the ObjC layer below
    capture_avf.m  our ObjC AVFoundation layer (macOS): enumeration, open at
                 native resolution with NV12 delivery, blocking frame wait.
                 Never sets frame durations — MS2109-class HDMI sticks throw
                 NSException on those setters
    capture_thread.rs  std::thread owning device, publishes into watch channel.
                 A watchdog thread flags (not kills) a stall — no new frame for
                 12s after open, or 4s of no progress thereafter — and the main
                 loop reopens the backend in place the next time backend.frame()
                 returns, publishing no_device while it does; StallTracker exits
                 the process only after 8 consecutive stalls with no healthy
                 frame in between. If the flag is still set a full watchdog poll
                 later (backend.frame() itself never returned to let the main
                 thread see it), that IS the wedged case and it still exits
    rfb_serve.rs RFB server at `GET /rfb` for every video source: WebSocket
                 (subprotocol `binary`, token via header or `?token=`), RFB
                 3.8/3.7/3.3, security None, at most MAX_CLIENTS (8) sessions.
                 Sends only changed 64x64 tiles; ZRLE, Raw fallback,
                 DesktopSize; honors 32bpp true-color SetPixelFormat. Key and
                 pointer events go to hid_link.rs; held keys/buttons are
                 released on disconnect. No plain TCP listener. Tests in
                 rfb_serve/tests.rs
    zrle.rs      ZRLE encoder (one zlib stream per connection, lossless)
    keysym.rs    X11 keysym -> hid key name: printable ASCII, editing and
                 navigation, F1-F12, modifiers (Meta -> Alt, AltGr -> RIGHT_ALT),
                 keypad -> main-block keys; F13-F24 and unmapped keysyms are
                 dropped (the hid vocabulary has no F13+)
    hid_link.rs  client of the target's hid daemon `/hid` WebSocket, found via
                 the discovery file named by env `HDMICAP_HID_DISCOVERY` (set
                 by the CLI whenever the target has a local hid channel,
                 whatever the video device); reconnects, re-reading port and
                 token; absent = the noVNC view is watch-only
    rfb.rs       platform-independent RFB client capture backend for network
                 video sources (device strings `rfb://HOST:PORT` plain TCP,
                 security None only; `rfb+ws://127.0.0.1:PORT/rfb` with the
                 bearer token in env `HDMICAP_RFB_TOKEN`, never argv;
                 `rfb+discovery:`, which re-reads the discovery file named by
                 env `HDMICAP_RFB_DISCOVERY` on every reconnect so a restarted
                 daemon's port and token are picked up). RFB 3.3/3.7/3.8; Raw,
                 CopyRect, DesktopSize. Keepalive: a non-incremental update
                 request every 5 s; no bytes for 15 s = link dead, publish
                 no_device and reopen. Deps: tokio-tungstenite (no TLS),
                 futures-util. No VNC password auth yet. Design:
                 notes/network-video-rfb.md
    frame.rs     FrameState, Signal enum, one-pass strided classification
                 (exact frame digest + no-signal from 4k luma samples)
                 + Settler: the published hash only moves when some pixel's
                 luma differs from the last accepted frame by more than
                 --change-threshold (default 16, 0 = exact); compares run
                 only when the exact digest changes
    pixel.rs     PixelData (Rgb/Nv12/Empty) + NV12/YUYV -> RGB converters;
                 compact_nv12() re-packs a strided (row-padded) NV12 buffer
                 into tight Y/CbCr planes — needed by the Windows backend,
                 whose samples may pad rows past the visible width
    server.rs    axum HTTP API: GET / (dashboard; CSP frame-ancestors 'none'),
                 /status (adds `rfb_clients`, `rfb_input`), /snapshot, /preview,
                 /ocr, /devices, POST /power-cycle, GET /rfb (the RFB server,
                 below), GET /novnc.js, and the /xterm.* and /novnc.js static
                 assets (the only token-exempt routes; `PUBLIC_ASSETS`).
                 /snapshot matches the inner Result of `rx.changed()`, not just
                 the outer timeout — otherwise a dropped capture-thread Sender
                 makes it spin at 100% CPU until the deadline instead of
                 answering 503 "capture thread gone" right away. Readiness is
                 snapshot_ready(): wait=stable and changed_since given together
                 require BOTH (stable AND a differing hash) — not stability
                 alone, which used to hand back the very frame the caller said
                 they already had. PNG
                 encode/decode (incl. Linux turbojpeg) and the preview JPEG
                 fallback run on spawn_blocking, not inline on a tokio worker;
                 PNG encoding, the OCR subprocess, and /preview's fallback
                 encode all share AppState.expensive (a 2-permit Semaphore) so
                 repeated dashboard clicks (or a handful of open preview tabs)
                 queue rather than piling up CPU work or visionocr children.
                 /preview also coalesces: AppState.preview_cache (a std Mutex,
                 never held across an await) keeps the last fallback-encoded
                 JPEG keyed by the frame's captured_at, so every client
                 watching one frame costs one encode, not N. And /preview
                 checks FrameState::effective_signal(), not just
                 signal == NoDevice — once the warm frame goes Stale (or
                 NoSignal/NoDevice), the stream stops sending that frame's
                 bytes and instead sends a placeholder JPEG (dark field, red
                 X) once per transition, with every part carrying an
                 X-Signal header naming the effective signal. A live frame
                 whose fallback encode FAILS (malformed RGB buffer) is recorded
                 as Served::Failed(captured_at) and skipped (should_attempt_live)
                 until a new frame arrives — attempted once + one warn!, not
                 re-encoded every 67 ms tick draining the semaphore. Subprocess
                 hooks all set kill_on_drop(true) and run under wait_with_timeout
                 — 504 on timeout, process killed: the OCR child under
                 OCR_TIMEOUT (30 s), and every power hook (/power on/off/cycle
                 via run_paniolo_action, /power-state) under POWER_TIMEOUT (60 s,
                 generous for a real power-cycle). /devices runs the synchronous
                 capture::enumerate() on spawn_blocking, off the tokio worker
    auth.rs      token + loopback Host/Origin layer over the whole router
                 (byte-identical in serialcap/hidrig/ch9329)
    daemon.rs    advisory lock, discovery file (pid, port, token; owner-only),
                 tokio runtime, graceful shutdown. Shutdown removes daemon.json
                 only — NOT the lock file, which stays open (flock'd) until
                 this process exits; unlinking it while still held would let a
                 next daemon lock a fresh inode at the same path while this
                 process (and its lock on the old inode) is still alive
  assets/        index.html (combined dashboard) + vendored xterm.js/css/fit addon
                 + novnc.js (noVNC v1.7.0 as one esbuild ESM bundle, MPL-2.0 plus
                 pako MIT, notices in novnc-LICENSE.txt; rebuild reproducibly with
                 scripts/vendor-novnc.sh, pinned sha512 and esbuild)

cambrionix/      Rust crate: standalone helper binary for Cambrionix USB hub control
                 (control UART, 115200 8N1); wired into paniolo via generic power hooks.
                 Commands: `state [port]`, `on <port>`, `off <port>`, `cycle <port>`
                 `state <port>` prints exactly `on` or `off` (matches paniolo state_cmd
                 contract; C/S/I → on, O → off, any other mode letter is an error).
                 `on`/`off`/`cycle` confirm every transition by re-reading the port
                 table (C, S or I confirms on, O confirms off). Responses are
                 bounded (3 s, 64 KiB) and hub error lines fail
                 the command. Built/installed by `make install` / `paniolo setup`.

shellyplug/      Rust crate: standalone helper for Shelly Gen2+ smart plugs/
                 relays (Plus/Pro/Gen3/Gen4) over the device's local HTTP RPC
                 API (Switch.Set/GetStatus; ureq). One-shot, stateless — no
                 daemon. Addressed by `-d <ip|host>` and `[id]` switch (default
                 0). Commands: `status [id]`, `state [id]`, `on/off [id]`,
                 `cycle [id]`; `on/off/cycle` confirm by read-back. Gen2+ only
                 (no Gen1 REST); auth-disabled devices only for now. NB: first
                 helper to reach a LAN device, so first to hit the macOS
                 Local Network privacy gate — see docs/power.md gotchas.

amt/             Rust crate: standalone helper for Intel AMT (vPro) machine
                 power over WS-Management (SOAP over HTTP, port 16992; ureq).
                 One-shot, stateless — the switch is the machine's own
                 Management Engine, so `state` is a true sensor (ME answers
                 with the host on, off, or bare-metal). HTTP Digest (MD5,
                 RFC 2617) implemented in-crate — AMT 11+ is Digest-only.
                 Addressed by `-d <host|ipv4|[ipv6]>[:port]` (strictly parsed;
                 anything else URL-shaped is rejected) and `-u <user>` (default
                 admin); password from AMT_PASSWORD, --password-file or
                 --password-command (first set wins; via the `secret` crate;
                 never the password itself in a flag or the lab file; a
                 credential failure exits 3). Commands: `status`, `state` (prints exactly
                 `on`/`off`; PowerState 2 = on, sleep/hibernate/off = off, any
                 other value is an error, never a guess), `on`, `off` (hard
                 power-off, confirmed as Off - Soft), `cycle [--delay-ms 3000]`
                 (off → confirm → delay → on → confirm; holds off any host not
                 already soft-off, so a sleeping/hibernating host cold-boots
                 rather than resumes). Requests and read-back retry transient
                 transport errors (connection/IO/DNS kinds only) within a ~20 s
                 budget that also bounds each attempt — the AMT NIC drops link
                 around host power transitions. TLS AMT (16993) is unsupported
                 (clear error). Hardware-verified against a Dell OptiPlex 7060
                 (AMT 12). See docs/power.md.

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

zigplug/         Python (uv) helper: Zigbee smart plug control via a CC2652 (ZNP)
                 coordinator dongle, using zigpy-znp. CLI wired into paniolo
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
                 ~/.config/paniolo/helpers/zigplug/zigbee.db (auto-migrated
                 from the legacy top-level location).
                 Installed by `paniolo setup` via `uv tool install` when uv is
                 present (shim in the libexec dir via UV_TOOL_BIN_DIR, off
                 PATH). See docs/power.md for pairing, hook wiring, recovery.

serialcap/       Rust crate: serial console daemon (parallels hdmicap)
  src/
    main.rs      CLI subcommands: daemon (--interface NAME=DEV[@BAUD], repeatable),
                 log (-i NAME), evidence (-i NAME, --epoch, --since, --max-bytes), devices, stop
    evidence.rs  Immutable exact-byte RX journal, distinct from line/pending.
                 Records keep epoch/RX sequence, generation and original RX time;
                 capture and observation times are separate. Five owner-only
                 1 MiB segments append across restarts, with progress checkpoint.
                 Offline reader caps raw bytes without skipping oversized chunks,
                 reports continuation, retention/epoch gaps and raw fault diagnostics.
                 Successful journal progress is health.evidence_through_seq;
                 evidence_error is sticky. Process writes, not fsync durability.
    serial_io.rs Health tracks per-supervisor epoch, connection generation,
                 opened identity, RX/TX counters and capture processing watermark;
                 GET /status and `serial status` expose them without acting.
                 Synthetic markers never advance RX counters.
                 One supervisor per interface: tokio-serial port owner; reconnect
                 loop; broadcast fan-out to WS clients; mpsc client->port; 64KB
                 scrollback ring; tees every chunk to that interface's capture
                 thread (off the live fan-out path). `Serials` holds the named set.
                 Outbound writes queue in an `Outbox` and drain at most 256 B per
                 select! pass, so a large /input paste can't starve the read arm
                 (was write_all-to-completion, ~6s of dead reads at 115200 baud
                 for 64 KiB — bytes lost to a full kernel tty buffer). A DTR press
                 (`dtr_press`) pulses the already-open port and stays on it — it
                 does not close/reopen, since the OS raises/drops DTR on
                 open/close and a reopen would add a second, driver-timed press;
                 opens with `.dtr_on_open(false)` for the same reason. `attach()`
                 (scrollback snapshot + broadcast subscribe under one lock)
                 replaces separate scrollback()+subscribe() calls, which could
                 double-deliver or drop a chunk at the attach boundary.
                 `OpenFailures` backs off a missing device's log line from every
                 750ms retry to once a minute. `is_pty` resolves symlinks
                 (`resolve_links`, read_link-based — canonicalize needs the
                 target to exist) before the path-prefix test, so a stable
                 symlink to a pty (e.g. hidrig's console bridge) is recognized
    wait.rs      bounded fresh acquisition after epoch/RX seq; exact immutable
                 chunks, literal patterns, explicit incomplete outcomes, NDJSON
                 completed rows and optional final pending. Output budget includes
                 every follow event and final JSON; no input/reset/restart.
    capture.rs   line assembler: splits bytes into timestamped, sequence-numbered
                 lines; appends them to a rotating on-disk JSONL log under
                 capture/<name>/ (survives restarts; resumes the seq counter);
                 mirrors the current unterminated line to a pending sidecar,
                 debounced to ~10 writes/s (SIDECAR_DEBOUNCE) rather than a
                 write+rename per ingested chunk — spawn_capture in serial_io.rs
                 waits on `sidecar_due()`'s deadline so an idle port still gets
                 the last partial line mirrored. A MAX_PENDING force-flush splits
                 at a UTF-8 char boundary (`utf8_floor_boundary`), not a raw byte
                 offset, so a chunk boundary mid-multibyte-character doesn't turn
                 into a U+FFFD. `strip_ansi` also consumes DCS/APC/PM/SOS
                 (ESC P/_/^/X) payloads to their own terminator (previously only
                 the introducer was eaten, leaking the payload) and drops Unicode
                 format (Cf) characters — bidi overrides like U+202E, zero-width
                 marks — that `char::is_control` doesn't cover. Also the `log`
                 reader (interface select; tail / range / since, ANSI-stripped by
                 default) + UTC formatting
    server.rs    axum: GET /stream (bidirectional WebSocket, 64 KiB messages),
                 /status, /interfaces, /devices; POST /button (DTR pulse),
                 /input (write bytes to port, 64 KiB body cap; ?pace_ms=N drips
                 one byte per N ms for a slow polled console, capped at 10 000).
                 Per-interface endpoints take ?interface=NAME, defaulting to the
                 first configured interface. A `/stream` client that falls behind
                 the broadcast channel (RecvError::Lagged) gets an in-band marker
                 in its own stream, not just a debug log; client->port keystrokes
                 use write_tx.send().await (backpressure), not try_send (which
                 silently dropped input once the 256-message queue filled)
    auth.rs      token + loopback Host/Origin layer over the whole router
                 (byte-identical in hdmicap/hidrig/ch9329)
    daemon.rs    advisory lock, discovery file (pid, port, token; owner-only),
                 tokio runtime, graceful shutdown; spawns one supervisor per
                 interface. Shutdown removes daemon.json only — NOT the lock
                 file, which stays open (flock'd) until this process exits; see
                 the identical note on hdmicap's daemon.rs above

ocr/             OCR helpers (compiled/installed binaries are gitignored):
                   visionocr.swift  Apple Vision OCR (macOS); built by paniolo setup via swiftc
                   linuxocr         Tesseract OCR wrapper (Linux); copied by paniolo setup

hidrig/          USB HID injector: host CLI + daemon (Rust). The dual-board KB2040
                 firmware it drives is a separate custom-hardware design, now in the
                 paniolo-hardware repo (hidrig-kb2040/), not in this tree
  src/main.rs      `hidrig` CLI — one-shot subcommands of the HID command
                   vocabulary (type/key/.../moveabs/ping/version) + `run` command
                   files; `serve`/`stop` for the daemon. A `Sender` routes each
                   one-shot through a running daemon (POST /send) when one owns
                   the same device, else opens the control CDC link and composes
                   frames in-process
  src/compose.rs   HID composition: turns each command into HID report bytes and
                   wraps them in the binary frames the boards relay (F_HID 0x01 /
                   F_CTRL 0x02). Holds the held-key + virtual-cursor state so
                   relative `move` and `moveabs` share one absolute-pointer device
  src/proto.rs     control-link transport for the *direct one-shot* path: writes
                   a 258-byte all-zero resync preamble then binary frames to the
                   control board's data CDC endpoint (no baud negotiation — CDC;
                   nominal 115200); `read_control_reply` demultiplexes inbound
                   bytes with uart.rs's `split_frames` rather than assuming the
                   next bytes are the `0x02` reply — the board interleaves `0x03`
                   console frames on the same stream whenever the DUT UART has
                   bytes, so e.g. `power cycle` during a boot log still gets
                   acked; command-file sequence parser (delay/sleep bounded to
                   0..=3600s, `type` keeps trailing whitespace) + clamp_abs
  src/uart.rs      the control-link owner (daemon path): a dedicated *blocking-
                   serialport* thread (NOT tokio-serial — its async reads don't
                   get read-readiness on a macOS tty) running a full-duplex poll
                   loop. It drains an mpsc command queue (Line + a shutdown
                   Release variant, CLI + web serialized onto one wire), pumps
                   PTY console input down as 0x03, then reads + demuxes inbound
                   frames: 0x02 replies fulfil the in-flight control request
                   (deadline-tracked), 0x03 payloads go to the console PTY
                   master. HID is fire-and-forget; a request whose client already
                   gave up (its reply channel is closed) is dropped without
                   composing or writing, so a timed-out command is not injected
                   late; broadcast transcript; lazy open (via proto::open_port, so
                   it also writes the resync preamble) + reopen-on-transport-error;
                   `split_frames` is `pub(crate)`, shared with proto.rs's one-shot
                   reader
  src/pty.rs       allocates a PTY (libc posix_openpt) for the DUT serial-console
                   bridge and opens its own raw-mode handle on the slave, held
                   for the daemon's lifetime — otherwise the slave sits at
                   cooked tty defaults (ECHO|ICANON) until some other reader
                   attaches, and every byte written to the master in that window
                   echoes back and gets framed to the DUT UART as if typed;
                   paniolo's serial channel opens a *second*, separate handle on
                   the slave via the stable symlink the daemon publishes
  src/server.rs    axum: GET /hid + POST /send (WebSocket message / body sized to
                   one full `type` line — MAX_TYPE_CHARS + the `type ` verb — so
                   the documented 4096-char `type` cap is actually reachable),
                   POST /stop (authenticated shutdown; `hidrig stop` never signals
                   the discovery-file PID), /status, /version. WS clients send
                   command lines; all results are broadcast as `evt ok|err …`
                   frames so observers see the intermixed stream
  src/auth.rs      token + loopback Host/Origin layer over the whole router
                   (byte-identical in serialcap/hdmicap/ch9329)
  src/daemon.rs    advisory lock, discovery file at /tmp/paniolo-<uid>/hid/<target>/
                   (the channel name, not "hidrig", so paniolo finds it without
                   knowing the helper); brings up the console PTY (removing a
                   stale `console` symlink from a previous daemon first) and
                   publishes both the stable symlink (discovery `console`, a
                   convenience — falls back to the raw device path if the
                   symlink can't be made) and the real slave path (discovery
                   `console_device`, always present, the source of truth); tokio
                   runtime, graceful shutdown — on SIGTERM/SIGINT or a `POST
                   /stop`-woken `Notify` alike — releases held keys/buttons via
                   uart.rs's Release request, then removes daemon.json and the
                   console symlink. Shutdown removes daemon.json only — NOT
                   the lock file, which stays open (flock'd) until this
                   process exits; see the identical note on hdmicap's
                   daemon.rs above
  (no firmware/ here — the dual-board KB2040 CircuitPython firmware, and the
                   retired single-board "smart" firmware, moved to the paniolo-hardware
                   repo: https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware
                   Summary, for readers of this crate: the **control** board
                   (`firmware/control/`) is a USB-CDC <-> I2C1 controller; it reads
                   framed input from usb_cdc.data, relays 0x01 HID frames verbatim
                   over I2C1 to the target, answers 0x02 control frames
                   (ping/version/power -> dual-control/1; power drives a DUT relay on
                   D5) locally, and bridges 0x03 console frames to/from the DUT UART
                   (TX=GP0/RX=GP1). The **target** board (`firmware/target/`) is an
                   I2C1 peripheral that relays report bytes to usb_hid send_report —
                   no adafruit_hid, no parsing; its boot.py holds the HID descriptor
                   (keyboard + custom absolute-pointer, 0..32767 axes) and the
                   dev/HID-only NVM flag (BOOT button GP11 toggles; D2->GND at reset
                   forces dev). Both discard a partial frame after 50ms of no
                   completion (`_rxbuf_started`/`discard_stale_rxbuf`) so a
                   killed-and-restarted daemon's resync preamble has something to
                   resync — UNVERIFIED on hardware. Any change to the HID descriptor
                   or frame format there requires a matching change to this crate's
                   `src/compose.rs` — see docs/dev/hid-dual-board-design.md.
  host/hid_seize_reports.c  macOS IOKit tool: seizes the HID device exclusively
                   and prints raw input reports — for pipeline testing without
                   keystrokes reaching the focused app. Build with host/Makefile.
  README.md        topology, wiring, frame protocol, CLI usage. The command
                   vocabulary spec is docs/dev/hid-serial-protocol.md; the dual-board
                   design + frame format is docs/dev/hid-dual-board-design.md

ch9329/          Rust crate: the *other* hid helper — a WCH CH9329 UART->USB-HID
                 bridge client, hardware-verified against an Openterface
                 Mini-KVM and an Openterface KVM-Go (a CH32V208 emulating the
                 CH9329 protocol; reports chip_version=0x01, not a real chip's
                 0x38 — see notes/openterface-kvm-go.md). The Sipeed
                 NanoKVM-USB speaks the same frame protocol at 57600 but is
                 not bench-verified here. Same
                 CLI surface as hidrig, so it drops into a `hid` channel
                 identically (`paniolo hid set --cmd "ch9329 -d <uart>"`); the
                 chip *is* the HID device, so it speaks the binary frame
                 protocol (HEAD 57 AB / ADDR / CMD / LEN / DATA / SUM) rather
                 than relaying the line protocol. Clean-room spec:
                 notes/ch9329-spec.md
  src/proto.rs     the HID serial protocol grammar executed against a Session
                   instead of forwarded to a microcontroller; `execute_line` is
                   the one backend for CLI subcommands and `run` files, so the
                   accepted command set matches hidrig exactly (sequence parser
                   — delay/sleep bounded to 0..=3600s, `type` keeps trailing
                   whitespace, an embedded CR/LF is refused on this direct path
                   too — + moveabs clamp ported from hidrig/src/proto.rs)
  src/session.rs   the link itself: framing/checksum (rejects a reply from the
                   wrong `raddr`), GET_INFO, held-key/pointer state, and
                   `open()`'s baud probe (BAUD_CANDIDATES = 115200, 57600 — the
                   NanoKVM-USB default — then 9600; force one with -b).
                   Failed Linux opens add a compatibility hint only for the
                   selected USB 1a86:fe0c port (stable aliases resolved).
                   `chip_version` is not a reliable Openterface firmware release;
                   no minimum-version check is possible from it. Captured startup
                   echo and the direct-USB drain workaround are recorded in
                   notes/openterface-v2-linux-serial.md and docs/hid.md.
                   `open()` pushes an all-zero keyboard + mouse report once
                   GET_INFO confirms the chip, since the chip's own HID state
                   outlives the daemon process; `tap`/`combo`/`type_text` best-
                   effort release whatever they pressed if a later step fails,
                   `combo` refuses (ERR) a chord needing more than the 6 key
                   slots rather than truncating it, and typing a usage already
                   held via `down` releases it first so the press is a real
                   edge. `usb_cmd` (the mux-switch command) reworks a reply
                   timeout into a "not supported" message that does NOT read as
                   transport loss, so a mux-less device's `usb state` query
                   never trips uart.rs's reopen. A `#[cfg(test)] pub(crate)
                   test_support` module (FakePort, a full `SerialPort` stub over
                   an in-memory byte queue + write log) lets proto.rs/uart.rs/
                   session.rs tests execute real wire behaviour without
                   hardware. Holds two verified CH9329-on-Linux workarounds —
                   clicks go through the *relative* report (libinput coalesces a
                   button transition in an absolute report at an unchanged
                   coordinate), and `moveabs` nudges one unit before the exact
                   target (an absolute report equal to the previous one is
                   coalesced away)
  src/uart.rs      the UART owner (daemon path): one dedicated thread holding a
                   long-lived Session, serializing CLI- and WebSocket-injected
                   commands (plus a shutdown Release request) onto the one wire,
                   one in flight — which is also what makes held state survive
                   across separate invocations. Only an idempotent query
                   (ping/info/version) is retried once on a lost reply; an input
                   command (key/type/move/click/...) is NOT — re-running the whole
                   line would inject a duplicate keystroke or click, so a lost ACK
                   surfaces as the timeout for the caller to decide about (a
                   persistent timeout still reopens the session; a reopen briefly
                   toggles DTR/RTS, which resets a KVM-Go's MCU). A request whose
                   client already gave up (its reply channel is closed) is dropped
                   without executing, so a timed-out command is not injected late;
                   a shutdown Release reopens the link if a transport error had
                   dropped it before clearing, since the chip holds its last
                   report independent of this process
  src/keys.rs      key-name -> USB HID usage mapping (adafruit_hid Keycode names,
                   US layout, incl. PRINT_SCREEN/SCROLL_LOCK/PAUSE/NUM_LOCK/
                   APPLICATION), shared with the hidrig vocabulary
  src/server.rs    axum: GET /hid + POST /send (WebSocket message / body sized to
                   one full `type` line — MAX_TYPE_CHARS + the `type ` verb — so
                   the documented 4096-char `type` cap is actually reachable and
                   not ~5 characters short; an empty /send body is a no-op, not a
                   forwarded blank command), POST /stop (authenticated shutdown;
                   `ch9329 stop` never signals the discovery-file PID), /status,
                   /version
  src/auth.rs      token + loopback Host/Origin layer over the whole router
                   (byte-identical in serialcap/hdmicap/hidrig)
  src/daemon.rs    `serve`/`stop`: owns the UART, publishes the same
                   /tmp/paniolo-<uid>/hid/ discovery file paniolo's console
                   reads; graceful shutdown — on SIGTERM/SIGINT or a `POST
                   /stop`-woken `Notify` alike — releases held keys/buttons
                   (via uart.rs's Release request) before exiting, and never
                   touches the USB mux. Shutdown removes daemon.json only — NOT
                   the lock file, which stays open (flock'd) until this process
                   exits; see the identical note on hdmicap's daemon.rs above
  README.md        wiring, extras beyond hidrig's surface (`info` reports target
                   USB enumeration + lock LEDs; `baud` persists a rate to flash),
                   and the hardware-verified status notes
```

jetkvm/          Go module (the one non-Rust helper besides zigplug): hid helper
                 for the JetKVM network KVM (firmware 0.5.9), whose keyboard and
                 mouse exist ONLY over WebRTC -- hence Go and pion/webrtc. Same
                 CLI/daemon surface as ch9329 (`-d <host[:port]>`, `serve`,
                 `stop`, `run`, plus `info`), so it is a drop-in `hid` channel
                 (`--cmd "jetkvm -d 192.0.2.10"`); paniolo starts it as
                 `<cmd> serve --port 0` like the others, no jetkvm code in cli/.
                 One flat `package main`: rpc.go (login -> signaling WebSocket ->
                 "rpc" data channel, JSON-RPC, `Link`), hid.go (`Session`
                 composing keyboardReport/absMouseReport/relMouseReport/
                 wheelReport, `executeLine`, `parseSequence`; the vocabulary and
                 bounds are ported from ch9329), keys.go (US layout), owner.go
                 (`Owner`: the ONE session, lazy connect, a single serializing
                 queue, transcript broadcast, background reconnect), server.go
                 (/status /version /send /hid /stop), auth.go (token +
                 loopback Host/Origin, the Go twin of auth.rs), daemon.go
                 (discovery file 0600 temp+rename, daemon.lock never unlinked,
                 runtime dir rules of platform::ensure_private_dir),
                 secret.go (JETKVM_PASSWORD -> --password-file ->
                 --password-command, the `secret` crate's order; exit 3).
                 **The firmware allows one session**: a new offer kicks the old
                 one with an `otherSessionConnected` event. The daemon treats
                 that as "a person's browser took it" and does NOT redial in
                 the background (the next command reconnects); only a plain
                 drop is retried, with 1-30 s backoff. HID RPCs succeed even
                 when the target's USB is down, so `ok` is not delivery (`info`).
                 **Video (milestone 2)**: the daemon adds a receive-only H.264
                 track to its ONE session (video.go: RTP -> Annex-B -> an
                 `ffmpeg` subprocess, `serve --ffmpeg PATH`, else PATH, else
                 Homebrew/usr paths; ffmpeg is a runtime requirement for
                 JetKVM video only, HID works without it) and serves RFB 3.8
                 over WebSocket at the token-authenticated `GET /rfb`
                 (rfb.go; subprotocol `binary` accepted, so noVNC works;
                 security None; Raw full frames; DesktopSize; input messages
                 ignored, input goes through the hid path). Decoding runs only
                 while an /rfb client is attached. `/status` and `info` gain a
                 `video`/`stream` object. A lab video channel reaches it with
                 `device = "rfb+hid:"` (the RFB feed of this target's own hid
                 daemon; needs a hid channel on the same host, validated, and
                 `hid rm` is refused while used; `video watch`/`console`/
                 `daemons restart` start the hid daemon first, then hdmicap
                 with `rfb+discovery:`). cli/ knows only that a hid channel
                 can host a feed. H.264 decode noise (measured: up to 8 luma
                 levels on an unchanged screen) is absorbed by hdmicap's
                 change threshold (frame.rs `Settler`, default 16), so the
                 frame hash and `changed_since` stay put on it.
                 Tests run a fake JetKVM (httptest + a pion answerer) end to end.
                 CI: its own `jetkvm` job (gofmt, vet, `go test -race`) and
                 `go_job` in scripts/ci-local.sh; `ci-coverage-check.sh` checks
                 Go modules too. `paniolo setup` builds it with `go build` into
                 libexec when `go` is present (`SourceStep::JetKvm`,
                 `GO_HELPERS` in setup.rs). Release packaging is NOT done yet
                 (milestone 1b; the exemption is written in the coverage script).

### hid daemon + KVM (`hidrig serve`)

The control link can have only one owner, so KVM streaming and CLI injection
can't both open it. `hidrig serve` resolves this: it owns the link and
re-exposes the command vocabulary over a WebSocket (`GET /hid`) and `POST
/send`. Every command — from a
browser, from `paniolo hid send`, from another script — flows through one
`mpsc` queue in `uart.rs`, one in flight, request/reply; that single queue is
what makes events intermix correctly. `paniolo console` starts the daemon when
the target has a `hid` channel (local: `?hid=PORT`; remote: an SSH-tunnelled
`?hidws=` URL). In the default noVNC view the browser's key and pointer events reach
this daemon through hdmicap (`/rfb` -> hid_link.rs), not from the page; `video watch`,
`console` and `daemons restart` start the local hid daemon for every video device (only
`rfb+hid:` errors if it will not start), so on a USB-capture target with a hid channel
starting video also makes the hid daemon own the injector. In MJPEG mode the
**`⌨ Capture input`** overlay button toggles capture (no click-to-grab, no
host-key release): engaged, the page streams `down`/`up`/`moveabs`/`scroll` to
the daemon; click the button again to release. The mouse is absolute (the
firmware's custom HID descriptor), so the cursor follows where you point in the
video, and the local cursor stays **visible as a crosshair** (no Pointer Lock —
deliberately, so you never lose your pointer). Mouse listeners live on the
`<img>`, so the overlay buttons never inject; window blur releases. paniolo
discovers the daemon by the
channel name `hid` (`daemons::daemon_port("hid")`), staying agnostic to the
helper. Hardware-verified end-to-end on the pi5 Linux desktop (2026-06-04).

**Latency.** HID frames are **fire-and-forget** over the USB-CDC link (no
per-frame round-trip), so streaming stays responsive without a baud
negotiation — the control board is USB-CDC and USB sets the rate. The dashboard
also **coalesces mouse moves** to one `moveabs` per `requestAnimationFrame`
(newest position only). The remaining floor is the target board's USB interrupt
`bInterval` (~8 ms per report on the CircuitPython firmware). Only `0x02`
control frames (ping/version) draw a reply; macOS drops the `IOSSDATALAT` read
timer on open to keep those round trips prompt.

## Combined dashboard (video + serial)

hdmicap's `GET /` serves a two-pane page: a noVNC view of `GET /rfb` on top (a
**video VNC | MJPEG** toggle, stored in `localStorage` as `paniolo-video-mode`; MJPEG
also the automatic fallback when `/rfb` cannot connect; the Capture-input overlay
exists only in MJPEG mode), an xterm.js terminal below. The terminal opens a WebSocket to **serialcap** (a separate
daemon/port), so the two subsystems stay decoupled — hdmicap only references
serialcap by URL. Defaults to `ws://<host>:8724/stream`; override with
`?serialws=<url>` (or `?serial=<port>`, which cannot carry a token). `paniolo
console` passes `?serialws=` — serialcap's `/stream` with serialcap's own
`?token=` inside, percent-encoded — on both the local and the remote/tunnel
path, plus hdmicap's token as `?token=`; the page refuses a non-loopback URL.
serialcap sends serial bytes as binary
frames and accepts keystrokes back over the same socket. xterm.js and noVNC are vendored
(not CDN) so the dashboard works on an isolated lab network. This is the first
concrete instance of the "Option B" inter-subsystem coordination described above.

**Multi-pane serial:** the page fetches `GET /interfaces` from serialcap on
load and calls `buildPanes(names)`. With one interface a single terminal fills
the panel and connection status appears in the top bar. With multiple interfaces
each gets its own `.serial-pane` div (label + status bar + xterm.js terminal),
laid out side by side in bottom mode or stacked in right-panel mode. All fits
are tracked in `allFits[]` so resize and layout-toggle events re-fit every
terminal. `?interface=<name>` bypasses the fetch and opens single-pane mode
pinned to that interface.

**Layout toggle:** a button in the status bar switches the serial panel between
bottom (default, 40 vh) and right-panel (380 px fixed, video fills remaining
width) layouts. The choice is persisted in `localStorage` under the key
`paniolo-serial-layout`.

**Power controls:** an on/off **toggle switch** (`Power [switch] ON/OFF`,
reflecting live state) plus a separate **⟳ Cycle** button appear in the video
overlay, each gated by a confirmation modal. Availability + state come from
**`GET /power`** — non-acting: it runs `paniolo power-state <target>` and returns
`on`/`off`/`unknown`, and the dashboard polls it every 5 s to keep the toggle
synced. The actions are **`POST /power-on` | `/power-off` | `/power-cycle`** →
`paniolo power on|off` / `power-cycle <target>`. All use the `PANIOLO_TARGET` env
var set when the daemon starts via `paniolo video watch <target>`; the controls
are hidden (501) if no target was passed, so shared dashboards are safe.
(Previously the availability probe was `POST /power-cycle`, which *triggered* a
cycle on every page load — the probe is now the read-only `GET /power`.)

## OCR

Two entry points, both feeding the same warm frame:
- **`paniolo video read [target] [--stable]`** — OCRs the current frame (wraps
  the daemon's `GET /ocr`).
- **Dashboard button + hdmicap `GET /ocr`** — the daemon PNG-encodes the current
  frame and pipes it to the OCR tool (`tokio::process`), returning the text. The
  daemon finds the tool via `PANIOLO_VISIONOCR` (the installed path, set by
  `paniolo video watch`), then a `visionocr`/`linuxocr` sibling of its own
  executable (both live in the libexec dir), then bare PATH; if absent, `/ocr`
  returns 501 and the button shows an error.

`paniolo setup` installs the platform-appropriate tool. `PANIOLO_VISIONOCR` is
set to the resolved path when the daemon starts, so the daemon always uses the
installed binary (never a stale PATH hit).

**macOS — `ocr/visionocr.swift`** (`VNRecognizeTextRequest`, Apple Vision):
on-device, no network, no model download. `paniolo setup` compiles it (`swiftc`)
into the libexec dir (`~/.local/libexec/paniolo/bin`).

Tuning that matters for small console text:
- `recognitionLevel = .accurate` is the default; `--fast` selects `.fast`. The
  old `.fast` default rested on the belief that `.accurate` (tuned for natural
  document text) misses thin console fonts; measured on the captures in
  `evals/ocr/dataset` it is better or equal on every frame, while `.fast`
  garbles BIOS menus and reads MAC zeros as the letter O (see the comment at
  `recognitionLevel` in the source).
- The tool 2×-upscales and black-pads the frame before recognition (fixes colon
  misreads and first-character clipping at the frame edge).
- `minimumTextHeight` is lowered (it's a fraction of image height; the default
  1/32 skips ~16px console text).

**Linux — `ocr/linuxocr`** (Tesseract via `tesseract-ocr` system package):
`paniolo setup` copies the script into the libexec dir. Requires
`sudo apt-get install tesseract-ocr`; Pillow (`pip install Pillow`) is optional
but enables the same 2×-upscale + black-pad preprocessing as visionocr.

**Do not change the target's console font** to try to improve OCR accuracy —
the font is relied upon by other agents (e.g. the Fuchsia bring-up agent that
reads kernel/bootloader output). Character confusions on thin console fonts
(`1`↔`l`↔`I`, IPv6 colons, etc.) are better addressed by increasing capture
resolution or adjusting Tesseract's `--psm` mode.

## netbootd (Rust netboot engine)

`netbootd/` is a single-binary DHCP + read-only TFTP + HTTP server — all as
tokio tasks in one process. It is the **only** netboot engine for `paniolo
netboot start` (originally ported from a pure-Python `_dhcp`/`_tftp` pair, since
removed).

The pure protocol logic is unit-tested (`dhcp.rs` / `tftp.rs` / `http.rs`
`#[cfg(test)]` modules): packet parse/build, the REQUEST verdict (ACK / NAK /
ignore), client-IP derivation, RRQ option negotiation, path-traversal
rejection, full loopback DATA/ACK transfers (multi-block, OACK,
retransmit-on-loss, error packets, a junk-sending peer, a duplicate RRQ), and
loopback HTTP (Content-Length bounding, HTTP/1.0 close, head timeout). A 65
K-round-trip block-wraparound test is marked `#[ignore]` — run it with `cargo
test -- --ignored`. `dhcp::serve` itself is deliberately *not* driven
end-to-end in tests (it shells out to `sudo arp`/`ip neigh` on every accepted
request, which a test must never trigger for real); `disposition` and
`accept_client`, the pure decision functions it calls, are what carry the
single-client test coverage instead — `disposition` holds the whole
per-packet decision, including the order the message type and the client lock
are settled in, so the tests exercise the sequence `serve` really runs. `tftp::run` *is* driven end-to-end over loopback (no such shell-out
exists there) for the peer-IP gate and the `MAX_TRANSFERS` semaphore bound,
including a stalled-transfer-then-freed-slot case (`tftp.rs`'s "Peer gating"
and "Transfer-slot bound" test groups).

Startup order is fixed: validate → bind and pin every listener → drop root
(Linux) → serve. Each server exposes `bind_server(port, interface)` (sync,
called from `main` before anything runs concurrently) and `serve(socket, …)`.

Key differences from the Python servers:

- **In-process MAC handoff.** The DHCP task publishes the client's hardware
  address to the TFTP task via `tokio::sync::watch` — no on-disk `client-mac`
  file.
- **Every listener is pinned to the netboot interface** (`src/pin.rs`:
  `IP_BOUND_IF` on macOS, `SO_BINDTODEVICE` on Linux) before it is bound, and
  a pin that fails is fatal — so is a DHCP or TFTP bind failure. `--interface`
  is required. The HTTP bind alone is non-fatal: a port clash logs a warning
  and netbootd runs DHCP + TFTP without HTTP Boot. `main.rs` reads the *actual*
  bound port back from the listener (`local_addr()`) and hands DHCP an
  `Option<u16>` — `Some(real_port)`, correct even for `--http-port 0`'s
  OS-assigned ephemeral port, or `None` when there is no listener at all.
  `dhcp::serve`'s `choose_boot` never builds an `http://` URL without a real
  bound port: an `HTTPClient` DHCPDISCOVER/REQUEST with no HTTP endpoint is
  ignored (rate-limited warning) rather than answered with a URL pointing at
  nothing (#144). The UDP listeners carry no
  `SO_REUSEADDR` (UDP has no TIME_WAIT, and on Linux it would let a duplicate
  daemon silently share the port); the TCP listener keeps it for TIME_WAIT.
- **Root is dropped on Linux** (`src/privdrop.rs`) right after the listeners
  are bound: `setgroups(empty)` → `setgid` → `setuid` to `SUDO_UID`/`SUDO_GID`,
  any failure fatal, then verified irreversible (`setuid(0)` must fail). The
  decision is the pure, unit-tested `drop_target`; the syscalls need a root
  process and are not unit-tested. The later `sudo ip neigh` / `sudo ip addr`
  shell-outs run as the dropped user, hence passwordless sudo on Linux. macOS
  is untouched (never privileged). All shell-outs made while serving go through
  `tokio::process` — none block a runtime worker.
- **One lease, derived from the host IP** (`dhcp::derive_client_ip`: host's
  /24, last octet 100, or 101 when the host is .100; `--client-ip` overrides,
  `validate_client_ip` refuses anything outside the /24). A REQUEST for another
  address is NAKed (`request_verdict`), one addressed to another server
  (option 54) ignored, and non-Ethernet / `hlen != 6` clients dropped.
- **One client, enforced by MAC** (`dhcp::accept_client`, sequenced by
  `dhcp::disposition`). The lease above is the address contract; this is the
  identity one: the first DISCOVER — or first REQUEST this server actually
  answers — locks in that hardware address as the active client for the
  process's lifetime, and a later request from a *different* MAC is ignored
  outright — no OFFER/ACK/NAK, just a `warn!` rate-limited via
  `served::warn_rate_limited` — so a second device on the link cannot steal
  the lease, the ARP pin, or the MAC handed to TFTP's raw-frame sender. A
  retransmission from the active MAC is unaffected (`accept_client` is
  idempotent for it). The message type is settled *before* the lock is
  touched: a DHCPINFORM/DECLINE/RELEASE, or a REQUEST addressed to another
  server (option 54), takes no lock no matter who sends it, so an unrelated
  host on the link cannot lock out the real target with a packet netbootd
  would never have answered (#166). A NAKed REQUEST does take it — a NAK is a
  reply. There is no lease timer and no in-process reset — `paniolo netboot
  start`/`stop` restarts the process per boot session, so a new client means
  restarting netbootd.
- **TFTP is gated to the leased client and bounded in concurrency**
  (`tftp::run`). Every RRQ/WRQ whose source IP is not the one DHCP leases on
  this link (`client_ip`, threaded from `main` into `tftp::serve`) is dropped
  silently. Concurrent transfers are capped by a `MAX_TRANSFERS`-permit
  (currently 4) `tokio::sync::Semaphore`: a permit is acquired with
  `try_acquire_owned` before a transfer task is spawned and moved *into* that
  task, so it releases whenever the task stops running — normal completion,
  a read/send error, or `abort()` when a repeated RRQ from the same TID
  replaces it — with no separate bookkeeping needed. An RRQ that finds every
  slot taken is dropped rather than queued (TFTP has no "try again shortly"),
  so a flood of unique-source RRQs cannot grow tasks, sockets, or open files
  without bound; a rate-limited `warn!` covers a sustained flood of either
  rejection.
- **Hardened against the far end of the link.** TFTP streams blocks from the
  file (`seek` + bounded read; nothing read whole), gives each retransmit
  attempt a fixed `Instant` deadline (junk cannot re-arm it), replaces an
  in-flight transfer on a repeated RRQ from the same TID (`abort` the old
  task), and caps `blksize` at 1468 when replies go out as raw frames
  (`cap_blksize`); `frame::build_udp_frame` refuses a payload the 16-bit
  length fields cannot describe. HTTP: 10 s head/body-drain timeout, 64
  concurrent connections (`Semaphore`), HTTP/1.0 defaults to close, and a body
  is exactly the announced `Content-Length` (grown file → cut, shrunk file →
  connection dropped). Peer-supplied names are passed through
  `served::loggable` (printable ASCII, capped) before they reach the log.
  SIGTERM shuts down cleanly like Ctrl-C.
- **Privilege-separated `/dev/bpf` on macOS.** The macOS raw-frame send path
  (the Sequoia workaround) needs a BPF descriptor, which only root can open.
  Rather than run the daemon as root, a tiny **setuid-root** helper —
  `netbootd-bpf-helper` — opens `/dev/bpfN` **write-only**, binds it
  (`BIOCSETIF`), sets `BIOCSHDRCMPLT`, installs a reject-all filter
  (`BIOCSETF`, `ret #0`), and passes the fd back over a `socketpair` via
  `SCM_RIGHTS` (`src/handoff.rs`), then exits. netbootd itself runs
  **unprivileged** and only `write(2)`s frames to the fd
  (`src/bpf.rs::BpfSender::from_handoff`); the fd cannot capture (`read(2)`
  fails — no `FREAD` — and the filter accepts nothing) and is `FD_CLOEXEC` so
  the daemon's `sudo arp`/`ifconfig` children never inherit it. The helper is
  the *only* component that runs as root; `paniolo setup` installs it setuid
  4755 (the one-time sudo), and it gates itself rather than relying on the
  mode: it refuses any caller whose real uid is not the owner of the directory
  it lives in (`handoff::caller_allowed` — the installing user, or root) and
  refuses the default-route interface (`handoff::interface_refused`, looked
  up by `route::default_route_interface` — `/sbin/route` by absolute path with
  an empty environment). If the helper is missing, not setuid, or refuses,
  netbootd logs its exit status and stderr and falls back to the kernel
  `send_to` path (broken on macOS 15+). `setup` will not setuid a symlink or a
  file the invoking user does not own.
- **Primary-NIC guard.** `netcfg::is_primary_interface` mirrors the Python
  guard; `main()` refuses to start, and `monitor_interface` refuses to enforce,
  on the default-route interface.
- **Layout.** `src/lib.rs` exposes `frame` (frame builder, unit-tested),
  `handoff` (BPF open + fd passing + the helper's caller/interface policy,
  unit-tested) and, on macOS, `route` (the default-route lookup) so both the
  `netbootd` and `netbootd-bpf-helper` binaries share them. The daemon-only
  modules are `dhcp`, `tftp`, `http`, `served` (shared path resolution + log
  sanitizer), `pin` (interface pinning), `privdrop` (the Linux root drop),
  `netcfg` (ARP pin + IP monitor shell-outs) and `bpf`. On Linux netbootd uses
  the kernel send path (no BPF), matching the Python behavior.
- **`paniolo netboot start` checks liveness.** After spawning, the CLI watches
  the child for ~2 s (`try_wait` + the named-process probe) and only then
  writes the state file; an early exit fails `start` with the last 20 log
  lines instead. It also refuses to start a second target on an interface
  another target's live netbootd already serves (`state::running_netboots`,
  one netboot per interface), and `netif::configure_interface` refuses a /24
  any other interface on the host already holds (one subnet per link:
  `model::netboot_subnet_clash` is the lab-file side, refused by `netboot
  set` for the link being edited and reported by `doctor` as `CONFLICT`; the
  live side lists the host's addresses). `target show` always prints the netboot host IP, `(default)`
  when unset.

## hidrig (USB HID injector)

The `hidrig/` directory holds a Rust host CLI/daemon for a DIY USB HID injector: the
**dual-board "dumb pipe"** KB2040 rig. The CircuitPython 9.x firmware it drives is a
custom-hardware design that lives in a separate repo, paniolo-hardware
(https://github.com/curtisgalloway/paniolo-hardware, `hidrig-kb2040/firmware/`) —
`hidrig/` here is the host side only. This is only one of many HID injection devices
paniolo can use.

### Architecture

```
[control host]
  |-- USB-CDC (hidrig writes binary HID frames) --> [Control KB2040]
                                                      |-- I2C1 (GP10 SDA / GP19 SCL,
                                                      |   addr 0x41, 4.7k pull-ups) -->
                                                    [Target KB2040]
                                                      |-- USB HID --> [target / DUT]
```

The host composes HID reports (`src/compose.rs`) and writes binary frames to the
**control** board's data CDC endpoint; the control board relays `0x01` HID
frames verbatim over I2C1 to the **target** board, which calls `send_report` —
neither board parses HID semantics (the "dumb pipe", `docs/dev/hid-dual-board-design.md`).
The target board's USB faces the DUT as a device-mode HID keyboard + absolute
mouse (and is DUT-powered, so it reboots with the DUT); the control board is
independently host-powered. The command vocabulary (`type`/`key`/`moveabs`/…)
is the device-independent **HID serial protocol v1** (`docs/dev/hid-serial-protocol.md`),
but it is the *external* interface only — `hidrig` consumes it and composes; the
line protocol never reaches a wire. `hidrig` (`src/main.rs`, `src/compose.rs`,
`src/proto.rs`) is the host client; the reference firmware
(`hidrig-kb2040/firmware/control/`, `hidrig-kb2040/firmware/target/` in
paniolo-hardware) is the board side. The retired single-board "smart" firmware
(`hidrig-kb2040/firmware/single-board/{boot,code,config}.py` in paniolo-hardware,
line protocol + `adafruit_hid`) is kept there for a future dumb single-board on the
same composition.


### USB identity (paniolo-hardware `hidrig-kb2040/firmware/target/boot.py`)

In normal (HID-only) operation the DUT must see a plain keyboard + mouse, so
the target board's boot.py disables the CIRCUITPY drive, the CDC REPL, and
MIDI. The mode lives in NVM byte 0 — `0` = HID-only, anything else (including
erased `0xFF`, so a fresh board boots editable) = dev mode with CIRCUITPY +
REPL + HID. **Tap the BOOT button (GP11)** while running and code.py flips the
flag and resets, toggling dev ↔ HID-only with no jumper; grounding **D2** at
reset forces dev mode regardless of the flag, as a hardware fallback for a
wedged code.py. Do firmware edits on a dev machine, not the DUT. boot.py only
re-runs on hard reset. The status NeoPixel (core `neopixel_write`, no /lib
dependency): blue = up, waiting for the controller over I2C; green blip = a
frame arrived; red blip = `send_report` failed (DUT not enumerated yet). The
retired single-board `boot.py` (paniolo-hardware `hidrig-kb2040/firmware/single-board/`)
used the D2 jumper alone and a different colour code (blinking red = waiting for
enumeration, solid red = last command failed).

### paniolo integration

`paniolo hid set -t <target> --cmd "hidrig -d <uart>"` stores an opaque
command prefix in the lab file's `[targets.<name>.hid]` channel (mirroring the
generic power hooks; no device-specific code in `cli/`). `paniolo hid send -t
<target> <args...>` shell-quotes and appends the args and runs the result via
`sh -c` on the channel's host (transparent SSH dispatch via
`ChannelKind::Hid`). `paniolo doctor` probes absolute-path helpers with
`test -e`.

### Host testing tool (`hidrig/host/`)

`hid_seize_reports.c` is a macOS IOKit utility that opens the injector's HID
interface with `kIOHIDOptionsTypeSeizeDevice`, preventing any keystroke from
reaching the focused application. It registers an input report callback and
prints hex dumps of every keyboard and mouse report. Use it to verify the full
pipeline end-to-end without a target:

```bash
cd hidrig/host && make
sudo ./hid_seize_reports   # grant Input Monitoring in System Settings first
```

Run `hidrig -d <adapter> type/key/move/click/scroll ...` in a second terminal
and read the reports. The tool prints the 156-byte report descriptor on first
device match, so you can verify the HID descriptor matches expectations.

VID/PID are 0x239A/0x8106 (KB2040 running CircuitPython). The built binary is
gitignored; re-run `make` after cloning.

### Negative number arguments (`move`, `scroll`)

clap treats a token starting with `-` as a potential option flag; the `dx`/
`dy`/`amount` args use `allow_hyphen_values` so `hidrig move 50 -30` and
`hidrig scroll -3` work without a `--` separator (same for `paniolo hid send`,
whose trailing args allow hyphen values — keep `-t` before them).

## Runtime paths

The runtime base `/tmp/paniolo-<uid>/` is private: created 0700 and, when it
already exists, required to be a real directory owned by the current user
with no group/other bits — a too-open base we own is tightened, a symlink or
another owner's directory is refused (`platform::ensure_private_dir`, one copy
per daemon crate). The discovery-file *readers* (`daemon_port`, `daemon_pid`,
`list_discovered`) apply the same check before trusting anything beneath it,
so a planted `daemon.json` can never supply a port or a pid; `paniolo daemons`
warns when the base fails the check. The ControlMaster socket dir
(`cli/src/ssh.rs`, the same path on macOS) goes through the same check.
Daemon stderr logs and serialcap's capture files are created 0600.

| Purpose | Path |
|---|---|
| Lab file (all config) | `~/.config/paniolo/lab.toml` (or `--lab`/`PANIOLO_LAB`) |
| Helper durable state | `~/.config/paniolo/helpers/<name>/` (`PANIOLO_STATE_DIR`) |
| Netboot daemon state | `~/.local/share/paniolo/<name>/netboot.json` |
| Combined netboot log | `~/.local/share/paniolo/<name>/netboot.log` |
| hdmicap discovery file | `/tmp/paniolo-<uid>/hdmicap/<target>/daemon.json` (`{pid, port, token}`; owner-only) |
| hdmicap advisory lock | `/tmp/paniolo-<uid>/hdmicap/<target>/daemon.lock` |
| hdmicap stderr log | `/tmp/paniolo-<uid>/hdmicap/<target>/daemon.log` (truncated on each CLI-spawned start) |
| serialcap discovery file | `/tmp/paniolo-<uid>/serialcap/<target>/daemon.json` (`{pid, port, token, interfaces:[{name, device, baud}]}`; owner-only) |
| serialcap advisory lock | `/tmp/paniolo-<uid>/serialcap/<target>/daemon.lock` |
| serialcap stderr log | `/tmp/paniolo-<uid>/serialcap/<target>/daemon.log` (truncated on each CLI-spawned start) |
| serialcap capture log | `/tmp/paniolo-<uid>/serialcap/<target>/capture/<name>/serial.jsonl(.1..)` (rotated JSONL, per interface) |
| serialcap pending line | `/tmp/paniolo-<uid>/serialcap/<target>/capture/<name>/pending.json` (current unterminated line) |
| hid daemon discovery file | `/tmp/paniolo-<uid>/hid/<target>/daemon.json` (`{pid, port, token, device, …}`; channel name, any injector) |
| hid daemon advisory lock | `/tmp/paniolo-<uid>/hid/<target>/daemon.lock` |

The per-target capture daemons (hdmicap/serialcap/hid) add the `<target>`
segment so multiple targets capture concurrently on one host; host-singleton
daemons (zigplug/cambrionix/netbootd) have no `<target>` segment. The runtime
base honors `$PANIOLO_RUNTIME_BASE` (default `/tmp`).

## Source code constraints

- **No hardcoded network addresses, URLs, or hostnames.** All site-specific
  values go in config files under `~/.config/paniolo/` and are populated via
  setup commands. Error messages must be generic. The same rule extends past
  code to docs, fixtures, and captured output — see
  [Never commit private infrastructure](#never-commit-private-infrastructure).
- **No new dependencies without discussion.** Keep each crate's dependency set
  lean and justify any new crate in review. (The `zigplug` Zigbee helper is the
  one remaining Python component — its deps live in `zigplug/pyproject.toml`.)
- **Rust is formatted with `rustfmt` and linted with `clippy`.** CI runs
  `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` per crate
  (every crate in the Makefile `CRATES` list has its own job) — keep both
  clean before pushing.
  Run `make fmt` to format every crate. The `zigplug` Python helper is formatted
  with `pyink` at line-length 88 (`zigplug/pyproject.toml`).
- **Don't call a reference an "oracle".** Name what it actually is:
  `reference` (the general term), `expected` vs `actual` in tests and
  assertion messages, `known-good` for a build or board trusted because it
  has worked, `baseline` for a prior version of our own output, and
  `ground truth` only when the reference cannot itself be wrong (a
  human-verified screen reading, a measured value) rather than another
  implementation that could share the bug. Applies to prose, identifiers,
  test names, and commit messages.
- **`paniolo setup` builds the native components from the source tree** when
  run from a clone — `make install` (which invokes the *installed* CLI)
  resolves the checkout by walking up from the cwd (`setup::find_repo_root`).
  Outside a checkout (a packaged install: Homebrew, .deb, tarball), it runs
  the platform-finish steps only (`setup::run_packaged`): setuid the
  installed `netbootd-bpf-helper` on macOS, group membership on Linux — no
  builds. `--rust-only` still requires a clone and errors clearly without
  one. It does the steps needing nothing beyond cargo — the crate builds, the
  bundled-skills copy, the stale-copy cleanup — and skips whatever needs sudo
  or a second toolchain: the OCR helper and zigplug everywhere, the setuid
  bpf-helper on macOS, the dialout/video group check on Linux. That split is
  `setup::SourceStep::on_the_fast_path`, and it is not documentation: every
  skippable step in `setup::run` is gated on `source_steps()`, and the message
  the flag ends on is derived from the same list, so the two cannot disagree
  (#207). Adding a step means adding a `SourceStep` variant, which the
  compiler then forces you to classify.

## Remote control pattern

```bash
ssh control-mac "paniolo netboot set -t target-machine --interface en3 --tftp-root ~/pxe"
ssh control-mac "paniolo power set -t target-machine \
  --cycle-cmd /Users/you/.config/paniolo/scripts/power-cycle-target-machine.sh"
ssh control-mac "paniolo netboot start target-machine"
TFTP_ROOT=$(ssh control-mac "paniolo netboot tftp-root target-machine")
scp kernel.img control-mac:"${TFTP_ROOT}/kernel_2712.img"
ssh control-mac "paniolo netboot logs -f target-machine"
op run --env-file .env -- ssh control-mac "paniolo power-cycle target-machine"
ssh control-mac "paniolo netboot stop target-machine"
```

## Adding a new subsystem

**Adding support for new power-switching hardware is not a subsystem** — it's
a standalone helper binary wired in via the generic power hooks. Follow
[docs/dev/adding-power-helpers.md](docs/dev/adding-power-helpers.md) (hook contract,
helper CLI conventions, Rust/Python skeletons, verification ladder, PR
checklist); `cambrionix/`, `zigplug/`, and `shellyplug/` (the
simplest one — a stateless HTTP one-shot) are the exemplars.

For a genuine new subsystem (a channel with its own commands/daemon), in the
Rust `cli/` crate:

1. Add a module `cli/src/<subsystem>.rs` for its logic.
2. Add a clap subcommand group + its handlers in `cli/src/main.rs`.
3. Add the channel's config fields to the data model (`cli/src/model.rs`) and
   surgical lab-file editing (`cli/src/labfile.rs`) so they round-trip.
4. If it's a daemon with a PID, add its state/discovery handling alongside the
   others (`cli/src/state.rs`, `cli/src/daemons.rs`).
5. Hand-update the usage skill (`skills/paniolo/SKILL.md`) and update this
   file and `docs/` (`paniolo skill` only lists/prints the bundled skills —
   it generates nothing).

A new **crate** (helper or otherwise) additionally needs a CI job and a
`scripts/ci-local.sh` line — see the CI-coverage rule in "Before opening a PR".
`scripts/ci-coverage-check.sh` fails the build until both exist.

A new **helper binary** must also be registered everywhere the helper set is
named, or it builds green in CI and then silently never reaches users:

- `Makefile` — `CRATES`
- `cli/src/setup.rs` — `HELPER_CRATES` (what `paniolo setup` installs from a
  source clone)
- `.github/workflows/release.yml` — the `HELPERS` env list **and** the
  rust-cache `workspaces` block (what the released `.deb`/tarball ship;
  v0.1.13 shipped without the `amt` helper because this list was missed)
- `packaging/nfpm.yaml` — the helper list in the package description

`scripts/ci-coverage-check.sh` (run by the `coverage` CI job) fails the build
when the `Makefile`, release `HELPERS`, or `HELPER_CRATES` lists omit a crate,
so the mechanical ones can't drift again; the release smoke test also verifies
every `HELPERS` binary landed in the installed `.deb`.

## Platform support

Paniolo runs on three host platforms. (Network RFB video sources are
platform-independent Rust and build and test on all three in CI; the
JetKVM-through-RFB path was hardware-verified from a macOS control host only.)

| Platform | Status | CI | Release artifacts |
| --- | --- | --- | --- |
| macOS (Apple Silicon) | Supported | `macos` job in `ci.yml` | Homebrew tap (universal tarball, poured as an `all` bottle) |
| Linux (Debian/Ubuntu) | Supported | the `ubuntu-latest` jobs in `ci.yml` | `.deb`, tarball and Homebrew bottle from `release.yml` |
| Windows (x86_64-msvc) | Supported; power and video hardware-verified | `windows` job in `ci.yml` | portable zip + winget |

All ten crates build, lint clean under `clippy -D warnings`, and pass their
unit tests on all three. The `windows` CI job runs the same fmt/clippy/test
triple as the Linux jobs, because a Unix-only CI cannot tell you whether the
Windows `#[cfg]` arm still compiles.

**Hardware-verified on Windows** (the Windows bench host, 2026-08-28, against a
Shelly Plug and an Openterface KVM-GO):

- **power** — `shellyplug` on/off/cycle, each confirmed by read-back.
- **video** — `hdmicap` enumerates the capture device by its symbolic-link id
  and captures native 3840x2160 frames through the Media Foundation backend;
  daemon start, `shot` and `stop` all work.
- **hid** — `ch9329` over a COM port reports `target_connected=true` and its
  mouse moves wake the attached machine, which is how the first (black) capture
  turned into a real frame. Keystroke delivery is confirmed by that wake rather
  than by the CH9329's LED read-back, which stayed `false`.

**What is still NOT verified on Windows:**

- **netbootd falls back to `send_to`.** The `/dev/bpf` raw-frame sender and the
  `SCM_RIGHTS` fd handoff are Unix-only, exactly as on Linux. Whether DHCP/TFTP
  netboot works against a real target on Windows is untested, and `netif` has no
  Windows implementation to configure the interface with.
- **hidrig has no console bridge.** The PTY the `serial` channel points its
  `device =` at has no Windows analogue — ConPTY is not a substitute, since it
  exposes no filesystem node another process can open. HID and control are
  unaffected.
- **OCR is absent.** Neither Apple Vision nor Tesseract has a Windows
  counterpart wired up. `Windows.Media.Ocr` is the in-box candidate, and the
  `windows` crate is already a hdmicap dependency for capture.
- **`paniolo doctor` cannot probe a Windows control host.** Probes run natively
  when the host is local, but the SSH path still renders them as POSIX shell
  (`sh -c 'test -e …'`), which a PowerShell host cannot run. Everything else
  dispatches fine (see **Dispatching to a Windows control host** below); doctor
  is the one remaining verb that speaks shell over the wire rather than pure
  paniolo.

### Dispatching to a Windows control host

`paniolo <cmd> -t <target>` works when the target's host is Windows. The
mechanism is worth knowing, because the thing that used to break it is a trap
anywhere the remote shell is not ours to choose.

Dispatch does three things over SSH: ship a lab slice, re-exec `paniolo` against
it, and clean the slice up. Only the middle one was ever shell-safe.

- **Shipping and cleanup now go over SFTP** (`ssh::sftp_put` / `ssh::sftp_rm`).
  They used to be shell: `f=$(mktemp …) && cat > "$f" && printf %s "$f"` and
  `rm -f`. On a PowerShell host the first is not a command at all, and the
  second fails with *"parameter 'f' is ambiguous. Possible matches include:
  -Filter -Force."* SFTP is a protocol, so it behaves identically whatever the
  far side runs, and it reuses the session's ControlMaster so it costs no extra
  handshake.
- **The re-exec was already fine.** `remote_command` quotes each argument
  POSIX-style, and PowerShell reads `'C:\Users\curti\lab.toml'` as the same
  literal a POSIX shell would. Verified directly. The `VAR='v' cmd` env-prefix
  form is *not* portable — but the dispatch path passes no env, so it never
  appears there.
- **The slice path is deliberately relative** (`.paniolo-lab-<pid>-<ns>.toml`).
  SFTP reports a Windows home as `/C:/Users/name`, an SFTP-protocol path no
  native Windows program can open, so an absolute path from `pwd` is unusable as
  a `--lab` argument. Both platforms start an SSH session in the user's home,
  which is also SFTP's default directory, so a relative name resolves on both.

Verified end to end against the Windows bench host: `paniolo hid send -t <target> info` returned
real CH9329 state, `hid send … move` drove the KVM, `video show` read the
channel, and the slice was cleaned up. Unix dispatch to `waldo` is unchanged.

**What still speaks shell over the wire:** `doctor`. Its probes execute natively
against a local host but render to `sh -c` for a remote one, so probing a
Windows control host fails. Narrowing that to pure paniolo verbs — the remote
side running `paniolo doctor` rather than the near side shipping shell — is the
obvious fix and is not done.

### Writing portable code here

The POSIX primitives paniolo depends on live behind `cli/src/platform.rs`, with
one implementation per platform and a single documented contract each:
`current_uid`, `runtime_root`, `ensure_private_dir`, `make_executable`,
`pid_alive`, `signal_pid` / `try_signal_pid` / `terminate_pid`, `is_superuser`,
`detach`, `exec_replace`. **Reach for those rather than `libc` or
`std::os::unix` at a call site** — that is the rule the port established, and
the reason it is one small module instead of `#[cfg]` scattered through twenty
files.

A trimmed copy of the module is duplicated into hdmicap, serialcap, ch9329 and
hidrig. That is deliberate and matches how `daemon.rs` is already duplicated:
these are standalone cargo projects with no crate between them. **Keep the five
copies in sync.**

Two semantic differences are worth knowing before you rely on them:

- **Windows has no signals.** Both `Signal::Term` and `Signal::Kill` land on
  `TerminateProcess`, so a Windows daemon never runs its graceful-shutdown path
  (discovery-file removal, the 300 ms grace) and may leave a stale discovery
  file for the next `pid_alive` probe to reap.
- **Windows has no uid.** The runtime namespace uses an FNV hash of `%USERNAME%`
  — stable per user, meaningless on its own, and never used for an
  authorization decision. The 0700-plus-ownership check on the runtime dir
  becomes "exists and is a directory", because the path sits inside the user's
  own profile whose inherited ACL already excludes other non-admin users.

### Testing the platform split

A `#[cfg]` that compiles is not a `#[cfg]` that works, and the tests have to
know the difference. `paniolo doctor` shipped in the first cut of the Windows
port depending on `sh -c` — a binary Windows does not have — and CI was green,
because the doctor tests asserted on the **text** of the generated shell script
rather than running it. A string-shape test passes identically on every
platform; it can only tell you what a command *would* say, never whether it can
launch.

So: **anything that spawns a process, touches the filesystem, or resolves a
name gets a test that executes it.** The ones that matter live in
`cli/src/platform.rs` (`pid_alive` against a real child, `shell_command`
running a command and reading its exit code back, `ensure_private_dir`
creating then revalidating), `cli/src/doctor.rs` (every `Probe` variant run
natively via `run_local`), and `cli/src/daemons.rs` (`find_binary` resolving a
bare name to a real file through `$PATH`).

Two bugs were found by writing exactly those tests, one of them serious:

- **Non-positive pids were unguarded on Unix.** `kill()` reads 0 as "every
  process in my group" and -1 as "every process I may signal", so a zero or
  corrupt pid in a discovery file made `pid_alive` answer *running* and would
  have made `signal_pid(.., Kill)` take down paniolo and the shell that
  launched it. `is_real_pid` now guards every entry point, in all five copies
  of the module, with a test in each so the guard cannot quietly go missing.
- **Opaque lab-file commands ran through `sh -c`.** Power hooks and the hid
  `cmd` are user-written strings that need a shell; `platform::shell_command`
  now supplies `sh` on Unix and `cmd.exe` on Windows.

`doctor` no longer needs a shell locally at all. Probes are a `Probe` enum with
two views — `run_local()` executes natively, `to_posix()` renders the POSIX
script still used for the SSH hop to a Unix control host — so the local path
and the remote path cannot drift apart silently.

### Windows packaging

The Windows artifact is a **portable zip**, not an MSI — paniolo is a CLI plus
nine helpers, so the Windows analogue of the Homebrew tap and the `.deb` is a
tarball of binaries, not an installer. (Contrast `~/src/oh-brother`, which needs
WiX + a Burn bundle because it is a GUI app bootstrapping a WebView2 runtime.)

The layout is flat, because Windows has no bin/libexec split to hang helpers
off — the exe's own directory *is* the install prefix:

```
paniolo\
  paniolo.exe          <- the only thing that goes on PATH
  libexec\
    hdmicap.exe  serialcap.exe  netbootd.exe  hidrig.exe  …  winocr.exe
  share\paniolo\skills\
    paniolo\SKILL.md  kvm-puppeting\SKILL.md  control-host\SKILL.md
```

`daemons::exe_relative_dirs` adds `<exe dir>\libexec` on Windows, and
`daemons::find_binary` tries `<name>.exe` before the bare name.
`skills::skills_dirs_for_exe` likewise adds `<exe dir>\share\paniolo\skills`
there — the same `share/paniolo/skills` every other install keeps under its
prefix, with the exe's directory as the prefix. Through v0.3.0 the zip staged
only exes and the CLI only looked one level up, so `paniolo skill` on a
Windows install listed nothing; the first `/release-train` dry run
(2026-09-10) caught it because that was the first time anything ran the zip.
`package-windows` now stages the skills and smoke-tests the expanded zip
(`--help`, `paniolo skill` from outside the tree, every helper's `--help`,
winocr decoding a tiny PNG) the way the Linux and macOS jobs already did
theirs. `paniolo setup` on Windows does nothing but verify the helper layout:
there is no setuid bit, no `dialout` group, and no OCR helper to build.

Signing reuses oh-brother's Azure Artifact Signing setup — same account, same
OIDC identity, same action. Three differences from that pipeline: the **exes are
signed before zipping** (a zip carries no Authenticode signature, and SmartScreen
judges the exe), it is **ten files rather than three** (one glob, no Burn engine
detach/reattach — that dance is MSI-bundle-specific), and the RFC3161 timestamp
is asserted in a verify step for the same reason as there: Artifact Signing
certificates live 72 hours, so an untimestamped signature passes on release day
and dies in the field three days later.

`release.yml` gains `package-windows` (build → sign → verify → zip) and a
`winget` job. Both no-op until their credentials exist: signing skips while
`vars.AZURE_SIGNING_ACCOUNT` is unset, winget while `secrets.WINGET_TOKEN` is.
**The first winget version must be submitted by hand** (`wingetcreate` or
`komac`) — the action only updates a package that already exists.

### Developing against Windows

The Windows bench host (`lab-host-1` in examples; its real name, address and
checkout path are private infrastructure and live outside this repo) is reached over SSH, which lands
in PowerShell 7. Note `$HOME` there is the Windows account's `C:\Users\…`,
a different account name than on the Mac, and the repo is cloned under it.
`BRIK_HOST=<ssh host> BRIK_DEST=<checkout, forward slashes>
scripts/sync-brik.sh <crate>…` pushes sources there; both variables are
required, with no defaults, and the script excludes `target/`, which is
gigabytes and stalls the transfer. The MSVC linker resolves without a
Developer Prompt.

A local `cargo check --target x86_64-pc-windows-msvc` catches most `#[cfg]`
mistakes without leaving macOS, but **only for crates with no C dependency** —
anything pulling `ring` (i.e. anything with TLS) fails at its build script for
want of Windows headers. Those need the real host.

Per-subsystem behavior:

- **OCR backend.**
  - *macOS:* Apple Vision (`visionocr.swift`, compiled by `paniolo setup`).
  - *Linux:* Tesseract (`ocr/linuxocr`, copied by `paniolo setup`; requires the
    `tesseract-ocr` system package).
  - *Windows:* none. A port would supply a third binary behind the same
    stdin-PNG → stdout-text interface that both current backends expose via
    `PANIOLO_VISIONOCR`.
- **Netboot privileges.**
  - *macOS:* 14+ allows DHCP (port 67) and TFTP (port 69) rootless; `sudo` is
    used only for interface config (`ifconfig`).
  - *Linux:* both ports require root, so `paniolo netboot start` auto-prepends
    `sudo` when spawning `netbootd`, and `ip addr add` uses sudo too. netbootd
    drops back to the invoking user (`SUDO_UID`/`SUDO_GID`) as soon as its
    listeners are bound, so its own later `sudo ip neigh` / `sudo ip addr`
    calls run unprivileged too. With passwordless sudoers this is transparent;
    otherwise sudo prompts (and, inside the daemon, fails).
  - *Windows:* neither port is privileged — Windows has no low-port
    restriction — so `netbootd` is spawned directly with no `sudo` prefix.
- **Interface management** (`cli/src/netif.rs`, `netif::configure_interface()` /
  `restore_interface()`).
  - *macOS:* `networksetup -setmanual <service> <ip> <mask> <router>` (router
    = the host IP; the four-argument form is required) + `ifconfig`, which is
    what makes the address live — a `networksetup` failure is a warning.
  - *Linux:* iproute2 — `ip addr replace` first, then `ip addr del` of any
    other IPv4 address (parsed from `ip -4 -o addr show dev <iface>`), then
    `ip link set up`; a failed assignment never leaves the link addressless.
    `restore_interface` still flushes with `ip addr flush dev <iface>`.
  - *Windows:* not implemented.
  - Every mode change (`mode_*`, `down_hard`) and `netboot::start` refuse the
    interface carrying the system default route (`is_primary_interface`; the
    pure decision is `is_default_route_interface`, unit-tested).
- **ARP pinning** (netbootd).
  - *macOS:* `arp -s`.
  - *Linux:* `ip neigh replace ... nud permanent`.
  - *Windows:* not implemented.
- **Raw-frame sender** (netbootd).
  - *macOS:* BPF sender over `/dev/bpf*` ioctls; netbootd receives the `/dev/bpf`
    descriptor from the setuid `netbootd-bpf-helper` and stays unprivileged (see
    the **netbootd** section).
  - *Linux:* no BPF path — the ioctls don't exist; the server falls back to a
    normal `sendto()` with retry.
  - *Windows:* not implemented.
- **Interface listing** (`list_usb_ethernet_interfaces()`).
  - *macOS:* `networksetup`.
  - *Linux:* sysfs — `/sys/class/net/` (type, carrier).
  - *Windows:* not implemented.
- **Serial device paths** (`serial::list_devices()`, `cli/src/serial.rs`).
  - *macOS:* scans `/dev` for `tty.usbserial-*` and `tty.usbmodem*` nodes.
  - *Linux:* one entry per physical port, named by its stable `/dev/serial`
    symlink — `by-id` preferred (names the adapter; what lab files typically
    use), `by-path` as the fallback (port-derived; the only stable name for
    adapters without a serial number), picked by `preferred_alias()`; raw
    `/dev/ttyUSB*`/`ttyACM*` only if no symlinks exist. Store a stable symlink
    path in target configs so serial interfaces survive USB adapter
    re-enumeration. The serialcap `--interface` parser accepts these paths
    (colons in a by-path name are not confused with the optional `:SENSE`
    suffix because only the known signal names `cts`, `dsr`, `dcd`, `ri` are
    treated as the sense suffix).
  - *Windows:* `serialport::available_ports()` returns the OS's `COM<n>`
    names. There is no by-path analogue, so a lab file pinned to `COM7` is
    less stable than a Linux by-id path — the number can move when an adapter
    is re-enumerated.
- **hdmicap capture + build deps.**
  - *macOS:* our own AVFoundation layer (`hdmicap/src/capture_avf.m`); no extra
    system packages beyond the Xcode toolchain.
  - *Linux:* V4L2 with a raw-MJPEG tee. Building requires `build-essential
    pkg-config libclang-dev clang` (V4L2 bindgen via `v4l2-sys-mit`) plus `cmake
    nasm` (the `turbojpeg` crate builds a vendored libjpeg-turbo — Debian's
    system libturbojpeg is too old for its pkg-config path, and the crate's
    `require-simd` default makes nasm mandatory on x86-64). `make install` fails
    early with a hint if any are missing (`check-deps` in the Makefile);
    `paniolo setup` prints a reminder. `CaptureStream::next` hands back the
    whole mmap'd arena buffer, sized once at the format's worst-case
    `sizeimage` — for MJPG that is the *uncompressed* frame, 4,147,200 bytes at
    1080p — so `frame()` must trim it to the `Metadata.bytesused` the driver
    actually filled (the V4L2 twin of the Media Foundation stride lesson
    below). Skipping the trim is invisible to every consumer, because JPEG
    decoders stop at EOI: the preview rendered correctly while shipping a
    216 KB frame inside a 4.1 MB part, 94.8% zeros.
  - *Windows:* our own Media Foundation layer (`hdmicap/src/capture_mf.rs`),
    delivering NV12 so it reuses the macOS pixel path. It enumerates the
    device's *native* media types and selects one explicitly — the Windows form
    of the AVFoundation lesson below, and the reason an Openterface captures at
    its real 3840x2160 instead of a rescaled 1080p. MJPEG-only devices are not
    yet supported. Media Foundation samples can pad each row to a stride wider
    than the frame's visible width, so `frame()` gets the real per-sample pitch
    from `IMF2DBuffer::Lock2D` when the buffer supports it, falling back to the
    selected media type's `MF_MT_DEFAULT_STRIDE` (read once in `open()`) and
    finally to `width`; `pixel::compact_nv12` then strips the padding into
    tight Y/CbCr planes before anything downstream indexes by `width`. A
    negative pitch (bottom-up) is treated as an explicit unsupported case.

## Removed: `usbhub` (per-port USB hub power)

paniolo shipped a `usbhub` helper that switched VBUS on individual ports of
off-the-shelf hubs via USB hub-class requests — the uhubctl mechanism, in pure
Rust. It was removed in full. Do not rebuild it without reading this.

Why it went:

- **The `learn` procedure was unusable.** Hubs do not report which port maps to
  which silkscreen number, and chips routinely claim per-port switching with no
  VBUS MOSFETs behind it. So a profile could only be built by a human physically
  watching a device lose power, port by port, through a resumable wizard. That is
  a lot of ceremony to configure one power hook, and it had to be redone per hub
  model.
- **The hardware is hard to get and unreliable.** Hubs with genuinely switchable
  per-port VBUS are a shrinking niche, and the ones that work are not
  consistently available.
- **Port state did not survive the hub losing power.** Power-cycling the hub
  turned every port back on, so the helper's state could silently disagree with
  reality.

It was also the only power helper with device-specific support inside `cli/`
(`usbhub_profiles.rs`, a `USBHUB_LIBRARY_PATH` special case in
`daemons::helper_env`, and a bundled profile library in the `.deb`). That
contradicted the rule in **Source code constraints** that device knowledge lives
in helper binaries behind the generic hooks — so removing it took a carve-out
out of the core as well as a crate out of the tree.

**What to use instead:** `shellyplug` (Shelly Gen2+ over local HTTP RPC),
`zigplug` (Zigbee), `amt` (Intel AMT/vPro, with true power-state readback),
`cambrionix` (Cambrionix hub ports), or a relay driven through the generic
hooks — which is what the CI rack actually uses. If per-port USB power is ever
needed again, the git history has the implementation; the mechanism was never
the problem.

## Known limitations / gotchas

- **Drive through paniolo, not around its devices.** The agent-facing rule
  (`skills/paniolo/SKILL.md`): never reconfigure or open a paniolo-managed device
  by hand — `ifconfig`/`ip`/`ethtool` on the netboot interface, `screen`/`tio` on
  a serial port, `kill` on a daemon. A background daemon (netbootd/serialcap/
  hdmicap/hid) holds the device and tracks state; touching it directly desyncs
  that state (a stray interface address silently changes what `netif status`
  reports; opening the serial port collides with the exclusive serialcap daemon).
  Use `netif mode …`, `serial log`/`send`, `daemons stop`.
- **`netif mode off`/`link` toggle the host IP, not the carrier; `netif
  down-hard` drops the carrier.** `mode link` assigns just the host IP (no
  daemon); `mode off` releases it. Neither admin-downs the interface, and `netif
  status` reports `carrier` independently of `mode` — a NIC with **Wake-on-LAN**
  enabled keeps the PHY energized when the interface is down, so `carrier` can
  read `up` in `off` mode. `netif down-hard` is the hard down (for testing
  link-drop *detection*): `mode off` + disable WoL (`ethtool -s <iface> wol d`,
  Linux) + admin-down (`ip link set down` / `ifconfig down`). `mode link`/`netboot`
  bring it back up; WoL stays off until re-enabled or the adapter is replugged.
  (macOS WoL is a system-wide `pmset womp` pref, so `down-hard` relies on
  admin-down there.) Heads-up: `del_host_ip` releases the macOS static IP via
  `networksetup -setdhcp` (an `ifconfig` delete won't unset a `-setmanual` IP)
  *and then* `ifconfig <iface> inet <ip> -alias` (so an alias left by
  `ifconfig` can't outlive the service reset), so `mode off` from `link` mode
  actually clears the address on macOS.
- **Interface configuration requires root.** `netif::configure_interface()`
  needs NOPASSWD sudo (`ifconfig`/`networksetup` on macOS, `ip` on Linux).
- **SSH PATH.** Non-interactive SSH shells often lack `/opt/homebrew/bin`;
  paniolo probes the Homebrew paths on macOS and `/usr/sbin`+`/sbin` on Linux
  when resolving helper binaries.
- **hdmicap device identity.** Capture devices have a stable, port-derived id
  (AVFoundation `uniqueID` on macOS, `/dev/v4l/by-path` symlink on Linux) shown
  by `hdmicap devices` / `paniolo video devices`. Prefer the id in lab files —
  identical dongles (MS2109s ship without USB serials) are indistinguishable by
  name. A name substring matching more than one device is a hard error listing
  the candidates' ids; with several non-built-in captures (e.g. MS2109 + Razer
  Kiyo), `paniolo configure` lists the id alternatives as comments.
- **macOS capture is our own AVFoundation layer** (`hdmicap/src/capture_avf.m`,
  C ABI consumed by `capture.rs`), replacing nokhwa + a vendored bindings fork.
  Two hard-won rules live in it: (1) never set
  `activeVideoMin/MaxFrameDuration` — MS2109-class HDMI sticks throw
  NSException from those KVC paths (the bug the old vendor patch existed for);
  (2) `activeFormat` alone is ignored — the session's default preset scales
  output to 1080p-class, so native resolution requires explicit
  `kCVPixelBufferWidth/HeightKey` in the output's `videoSettings`
  (`AVCaptureSessionPresetInputPriority` is iOS-only). Note the macOS UVC
  stack decodes MJPEG before AVFoundation — raw-MJPEG passthrough (the Linux
  tee) is impossible on macOS; frames arrive as NV12 ('420v', video-range)
  and RGB materializes lazily per request.
- **Capture daemons are per-target; singleton daemons are per-host.** Discovery,
  lock, and stderr log live under `${PANIOLO_RUNTIME_BASE:-/tmp}/paniolo-<uid>/`
  — the base is deliberately env-independent (NOT `$TMPDIR`, which macOS varies
  per environment so a running daemon was invisible from other shells; NOT
  `$XDG_RUNTIME_DIR`, which systemd deletes when the user's last session ends,
  breaking daemons that outlive their SSH session). The capture daemons
  (serialcap/hdmicap/hid) are **per target** — each gets its own daemon dir
  `<base>/paniolo-<uid>/<daemon>/<target>/`, so multiple targets capture
  concurrently on one host (multiple hdmicap = multiple capture devices). The
  host-singleton daemons (zigplug/cambrionix/netbootd) stay **one per host** at
  `<base>/paniolo-<uid>/<daemon>/` with no `<target>` segment.
- **Helper `stop` commands authenticate shutdown.** `serialcap stop`,
  `hdmicap stop`, `ch9329 stop`, and `hidrig stop` call their daemon's
  token-protected `POST /stop` instead of signaling a discovery-file PID,
  which may have been recycled; zigplug's `stop` does the same over its own
  HTTP API. Older daemons without the endpoint require `paniolo daemons stop
  <name>` (which checks the process identity before signaling) before
  restarting.
- **Daemon shutdown hard-exits.** Both hdmicap (`/preview` MJPEG) and serialcap
  (`/stream` WebSocket) serve infinite responses, so a plain axum graceful
  shutdown would block on them forever. On SIGTERM each daemon removes its
  discovery file, gives a 300 ms grace, then `std::process::exit(0)`. The OS
  releases the capture device / serial port on exit.
- **Serial ports are exclusive.** Only one of `tio`/`screen`/serialcap can hold
  a port at a time. `paniolo serial watch` and `paniolo serial connect` conflict
  on the same device — use one or the other.
- **A pty is opened without a line rate.** The `serialport` crate applies baud on
  macOS with the `IOSSIOSPEED` ioctl, which returns ENOTTY ("Not a typewriter") on
  a pseudo-terminal, so serialcap detects a pty by path and opens it at baud 0 —
  the crate's documented pty path, and a pty has no line rate to set anyway. See
  `open_baud()`/`is_pty()` in `serialcap/src/serial_io.rs`. **Do not extend this to
  Linux**: there a rate of 0 is written to `c_ospeed` as `B0`, meaning *hang up*.
  This is what lets serialcap attach to a VM console (`utmctl attach`, qemu
  `-serial pty`) and what lets `evals/serial_loopback.py` run on macOS.
- **OCR character confusions on small console fonts.** Both visionocr and linuxocr
  2×-upscale and black-pad before recognition, but thin terminal fonts still
  produce confusions (`1`↔`l`↔`I`, `2`↔`Z`, colon spacing in IPv6). Accuracy
  improves markedly on larger boot-screen text. Do not change the target's console
  font to work around this — the font is relied upon by other agents (see OCR section).
  On macOS, visionocr uses `VNRecognizeTextRequest` `.accurate`, measured better
  than or equal to `.fast` on every captured frame, console text included.
- **macOS Local Network privacy gate.** On macOS (Sequoia+), a freshly built,
  non-Apple-signed helper that reaches a LAN host (e.g. `shellyplug`) fails with
  `No route to host` (EHOSTUNREACH) until the launching app is granted Local
  Network access (System Settings → Privacy & Security → Local Network). iTerm's
  detached server can need an iTerm restart for the grant to take; loopback
  daemons and Apple-signed `curl` are exempt. The tell: it reaches the internet
  but not a same-subnet host. See docs/power.md (shellyplug gotchas).
- **Homebrew tap upgrades.** The tap formula re-pins on each release. A
  same-version change to install logic won't `brew upgrade` — use `brew
  reinstall`, or bump the formula `revision`. (After a local `make rust`, the
  `~/.cargo/bin` paniolo can also be shadowed on PATH by the Homebrew keg.)
- **Reproducing CI locally.** GitHub's Linux jobs are easiest to mirror with
  `scripts/ci-local.sh` in a Lima VM; it copies the working tree to the VM's own
  disk first, because building on the shared virtiofs/9p mount fails (setuptools'
  editable `egg-info` can't update timestamps there, and cargo would clobber the
  host `target/`).
