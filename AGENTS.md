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

## How these instructions are organized

This root file holds the rules that apply to every change: privacy, the PR
checklist, releases, source constraints, and a map of the repo. Each component
directory has its own `AGENTS.md` with that component's internals (module
layout, protocols, hardware notes, gotchas).

**Before editing files in a directory that has an `AGENTS.md`, read it.** Not
every agent harness loads nested `AGENTS.md` files automatically, so do not
assume it is already in your context.

Per-OS behavior and the Windows port are in
[docs/dev/platform-support.md](docs/dev/platform-support.md). When adding
internals documentation, put it in the component's `AGENTS.md`, not here.

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
   - `AGENTS.md` — the root file and the `AGENTS.md` of each component
     touched (module layout, command descriptions, architecture notes)
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

8. **Tag the PR with the issue it fixes, in a form GitHub parses.** A PR
   that fixes an issue says so with a closing keyword immediately followed
   by the number, on its own line in the PR body: `Fixes #238` or
   `Closes #238` (one line per issue). GitHub then links the PR and closes
   the issue when the PR merges, squash merges included. Put the same line
   in the commit message, and put the number in the PR title too
   (`ch9329: drain startup echo on Linux (#238)`), so it survives in
   `git log --oneline`.

   - **Only for a full fix.** A PR that fixes part of an issue, works around
     it, or adds diagnostics writes `Part of #238`, which links without
     closing. Then it comments on the issue saying what is now done and what
     is still missing. The issue stays open until its own ask is done.
   - **Prose does not count.** "Fixes the autodetect failure in #81" and
     "Related to #238" link nothing and close nothing: the keyword must be
     directly followed by `#N`.
   - **Check after the merge.** `gh issue view <N> --json state` should
     say `CLOSED`. If it does not, the reference was wrong; fix it by
     closing the issue with a comment that names the merged PR.

   **Why:** PR #84 ("Fixes the autodetect failure in #81") and PR #275
   ("Related to #238") both merged without GitHub recording that they
   addressed those issues, so both issues sat open for weeks with no
   pointer to the work. The next person to triage them had to rediscover
   from `git log` what had already been done (2026-10-16).

9. **Push, open the PR, and merge only when asked.** Get the branch ready by
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

Current capabilities (one line each; see the linked docs):
- Netboot: DHCP + TFTP + HTTP, PXE and HTTP Boot over a direct USB-Ethernet link (`paniolo netboot`, docs/netboot.md).
- HDMI/USB video capture via the hdmicap daemon (`paniolo video`, docs/video.md).
- Serial console with per-target daemon and rolling capture logs (`paniolo serial`, `serial log -i <name>`).
- Combined video+serial web dashboard and KVM (`paniolo console`, hdmicap `GET /`).
- Network video via RFB, e.g. a JetKVM (`paniolo video set --device rfb+hid:`, docs/video.md).
- On-device OCR of the captured screen (`paniolo video read`).
- USB HID keyboard/mouse injection via a generic helper hook (`paniolo hid send`, docs/hid.md).
- Named target actions for non-power controls (`paniolo action list|run|set|rm`, docs/actions.md).
- Switchable USB media on the Openterface KVM-Go (`paniolo usb attach-host|attach-target|state`, docs/usb.md).
- Power control via DTR, generic hooks, and named levels (`paniolo power`, `power-cycle`, `power-state`, docs/power.md).

Full detail: docs/dev/architecture.md#capabilities

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
  A lab file can pin one per channel (`port` on `video`, `hid`, and any one
  `[[serial]]` interface; `--port` on `video set`/`hid set`/`serial add|set`,
  0 clears) so a standing `ssh -L` reaches the dashboard across restarts.
  Every start path honors it (`watch`, `console`, `daemons restart`, the hid
  daemon's `serve --port N`), the dispatch slice carries it, and validation
  refuses interfaces that disagree or two daemons claiming one port on a host.
- **Daemon APIs are authenticated.** Every request to a daemon (serialcap,
  hdmicap, hidrig, ch9329) must carry the token it publishes in its owner-only
  discovery file, and must name a loopback `Host`/`Origin`; the layer is each
  daemon's `src/auth.rs`, byte-identical across the four. The URL handed to a
  browser carries `?token=`, but **`video preview` is the only command that
  prints a token-bearing URL**; every other command prints the token-free
  `http://127.0.0.1:<port>` because output lands in agent transcripts and CI
  logs (#196). A daemon started by an older paniolo has no token; a named
  `paniolo daemons restart` replaces it.
- **Netboot is rust-engine only** (netbootd); the pure-Python DHCP/TFTP engine
  exists only in the legacy tree.
- **Helpers live off PATH** in the private libexec dir
  (`~/.local/libexec/paniolo/bin`): only `paniolo` itself installs to
  `~/.cargo/bin`. `daemons::find_binary` resolves libexec → PATH → legacy
  `~/.cargo/bin`; hook commands (`*_cmd`, hid `cmd`) run via `sh -c` with
  libexec prepended to PATH, so lab files keep referencing helpers by bare
  name. `paniolo helper [NAME] [ARGS…]` lists or runs them directly.
  The helper name and its args are one trailing positional, so everything
  after the name — `--help`, a helper's own `-t`/`-i` — goes to the helper
  (`paniolo helper serialcap --help`). Placement flags go before the name:
  `--host <labhost>`, or `-t <target> [--channel <kind> [-i <iface>]]`, which
  resolve a host (`dispatch::helper_host`) and re-exec `paniolo helper --
  NAME ARGS…` there without shipping a lab slice (`dispatch::run_on_host`;
  `helper` reads no lab). A channel the target lacks is refused rather than
  falling back to the default host (#300). Listing helpers and reading
  bundled helper help do not open hardware or start a daemon.
- **Plugin roots** (`PANIOLO_PLUGIN_PATH`, #303): a PATH-syntax list of
  install prefixes that each add `<root>/libexec/paniolo/bin` and
  `<root>/share/paniolo/skills`, searched after every bundled dir, so a plugin
  adds helpers and skills but never shadows a bundled one. The top-level
  `--plugin-path` flag and a host's lab `plugin_path` set the same variable
  (including on re-exec to a remote host); `doctor` prints a `plugins` line
  per root.
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
  not act on an implicit target. `hid send`, `adb run`, `adb input`, `action
  run`, and the `usb` verbs also take `-t` only, because their positional tail is the
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
- **Env forwarding to a control host**: a fixed allowlist (`ssh::FORWARDED_ENV`
  — today just `AMT_PASSWORD`) is carried across a non-interactive dispatch
  (`run`/`run_passthrough`/`run_stdout_to`) over the remote command's stdin, so
  it never appears in the control host's `ps` output; `run_interactive` (`serial
  connect`, `adb shell`) forwards nothing — its stdin is the user's own
  terminal. See
  [distributed-control.md](docs/distributed-control.md#env-forwarding-to-a-control-host).
- **`paniolo daemons`** is the unified daemon inventory: discovery-file
  daemons under `/tmp/paniolo-<uid>/`, netbootd via its state files, and stray
  helper processes. `daemons stop [NAME…|--all] [--force]` and `daemons
  restart [NAME…|--all|--stale]` act on them (a `NAME` is `serialcap` or
  `'serialcap[pi5]'`). A daemon is **stale** only on a proven content
  difference from the binary on disk; missing identity is "unknown", never a
  restart recommendation. A daemon that outlived its discovery file is an
  untracked daemon; `video watch|stop` and `serial watch|stop` reap it before
  starting a replacement.

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
pre-created — on every helper invocation (hooks, `paniolo helper`, daemon
spawns). Per-target capture daemons (serialcap/hdmicap/hid) add a `/<target>`
segment; host-singleton helpers use the base form. The base honors
`$PANIOLO_RUNTIME_BASE`. Contract for new helpers:
docs/dev/adding-power-helpers.md.

Full internals, including the cli/src module map: cli/AGENTS.md

## Module layout

Each directory below has an `AGENTS.md` with its internals; read it before
editing there.

| Directory | Purpose | Internals |
| --- | --- | --- |
| `cli/` | The `paniolo` control plane: lab file, dispatch, orchestration, device glue (Rust) | cli/AGENTS.md |
| `hdmicap/` | Warm-stream HDMI capture daemon, RFB client/server, dashboard (Rust) | hdmicap/AGENTS.md |
| `serialcap/` | Serial console daemon with timestamped capture logs (Rust) | serialcap/AGENTS.md |
| `netbootd/` | Single-binary DHCP + TFTP + HTTP netboot engine (Rust) | netbootd/AGENTS.md |
| `hidrig/` | USB HID injector host CLI and daemon for the dual-board KB2040 rig (Rust) | hidrig/AGENTS.md |
| `ch9329/` | HID helper for WCH CH9329 UART-to-USB-HID bridges (Openterface, NanoKVM-USB) (Rust) | ch9329/AGENTS.md |
| `jetkvm/` | HID and H.264-to-RFB video helper for the JetKVM network KVM (Go) | jetkvm/AGENTS.md |
| `cambrionix/` | Power helper for Cambrionix USB hub ports | cambrionix/AGENTS.md |
| `shellyplug/` | Power helper for Shelly Gen2+ smart plugs over local HTTP RPC | shellyplug/AGENTS.md |
| `amt/` | Power helper for Intel AMT (vPro) machines over WS-Management | amt/AGENTS.md |
| `secret/` | Shared library resolving a helper's secrets from env, file, or command | secret/AGENTS.md |
| `zigplug/` | Power helper for Zigbee smart plugs via a CC2652 coordinator (Python) | zigplug/AGENTS.md |
| `ocr/` | OCR helpers: Apple Vision on macOS, Tesseract on Linux | ocr/AGENTS.md |

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
| Plugin helpers / skills | `<root>/libexec/paniolo/bin/`, `<root>/share/paniolo/skills/` for each root in `PANIOLO_PLUGIN_PATH` (or `--plugin-path`), searched after the bundled dirs |
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

A new **crate** also gets its own `AGENTS.md` for its internals (module
layout, protocols, gotchas), plus a row in the Module layout table above.

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

A `#[cfg]` that compiles is not a `#[cfg]` that works: a string-shape test
passes identically on every platform and never shows whether a command can
launch. Anything that spawns a process, touches the filesystem, or resolves a
name gets a test that executes it.

Windows dispatch, packaging, developing against the Windows bench host, and
per-subsystem OS behavior: docs/dev/platform-support.md

## Removed: `usbhub` (per-port USB hub power)

The `usbhub` helper was removed in full; do not rebuild it without reading
notes/usbhub-removed.md. Use `shellyplug`, `zigplug`, `amt`, `cambrionix`, or
a relay driven through the generic power hooks instead.

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
- **Component gotchas live with the component.** Capture-device identity and
  the macOS capture layer are in hdmicap/AGENTS.md, the pty line-rate rule is
  in serialcap/AGENTS.md, and OCR character confusions are in ocr/AGENTS.md.
- **Do not change the target's console font to improve OCR** — other agents
  rely on it (see ocr/AGENTS.md).
