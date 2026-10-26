# Release train profile: panio

Derived from commit acb12ce on 2026-09-16. Executed by the `release-train`
skill (public-skills, `plugins/dev-tools/skills/release-train`); kept honest by
its `profile_check.py` against the `## Sources` table below. Read `AGENTS.md`
"Cutting a release" first: everything there still holds, this file only adds
what the train needs to build, install and run each channel before the tag.

The first dry run (2026-09-10) PASSed the homebrew, deb and source arms and
FAILed the windows arm (the zip shipped no skills; fixed in #197, which also
added the zip smoke step to `package-windows`), so every arm below except
windows is verified once by the train; windows is verified by that fix's
bench-host run and by CI's new smoke step.

## Project

- cli: `panio`
- version source: the tag (`${GITHUB_REF_NAME#v}` in the release workflow); every `Cargo.toml` stays `0.1.0`. `panio --version` prints the tag because each package build exports `PANIO_VERSION=X.Y.Z` (`cli/src/main.rs`); every arm below exports it the same way, and a build without it prints `0.1.0 (unversioned dev build)`
- tag format: `vX.Y.Z`; annotated; subject `vX.Y.Z: <lowercase one-line summary>`, optional body
- main branch: `main`; protected: PR required, required checks `cli`, `serialcap`, `netbootd`, `hdmicap`, `macos`; auto-merge off; tag ruleset "Protect release tags"
- release workflow: `.github/workflows/release.yml`
- ci workflow: `.github/workflows/ci.yml`; lint helpers `scripts/ci-actions-pinned.sh`, `scripts/ci-coverage-check.sh`, `scripts/ci-release-guards.py` (only a pushed `v*` tag publishes; `gh workflow run release.yml` against a branch or a tag builds artifacts and never publishes, #338)
- profile pins: `scripts/ci-profile-pins.sh` checks the Sources table below. `ci.yml` only warns when a row is stale; `release.yml`'s `profile-pins` job blocks every package build on it, so a stale row fails the tag push's workflow before anything is built. Run it in pre-flight and re-pin before tagging
- bump rules: conventional prefixes where present; this repo mostly writes `<crate>: <summary>`, so apply AGENTS.md: minor for a new command, channel, helper, verb, wire-protocol or daemon-lifecycle change, or a fix that changes what a target sees; patch for small fixes and docs; on the line, take the minor
- releaser identity: tag with the author's GitHub noreply form already in the history (`git log --format=%ae | sort -u`), passed as `-c user.email` on the tag command, never edited into config
- helpers: `hdmicap serialcap netbootd cambrionix dsdrelay dsdble phidget usbtoggle hidrig ch9329 shellyplug switchport amt` (the `HELPERS` line in the release workflow, and `CRATES` in `Makefile` minus the library crates, must agree; `scripts/ci-coverage-check.sh` enforces it). The Go helper `jetkvm` is deliberately NOT in that list yet: release packaging for it is milestone 1b, and the coverage script carries the written exemption. Until then it reaches users only through the source channel (`panio setup` with Go installed). Its video path needs `ffmpeg` at runtime (JetKVM video only; HID does not), and `hdmicap` gained the `tokio-tungstenite` and `futures-util` dependencies for its RFB client, so a channel smoke that builds `hdmicap` offline needs them vendored or fetchable
- library crates: `secret` (no binary; in `Makefile` `CRATES` and CI, never in `HELPERS`; it ships compiled into `amt`, so no channel stages it and the smoke contract has nothing to check for it)
- bundled skills: `skills/panio`, `skills/kvm-puppeting`, `skills/control-host`, `skills/managed-switch` (shipped to `share/panio/skills` in every channel)
- log dir: `logs/release-train/<run>/` (gitignored, persistent)

## Hosts

Roles only; reach recipes and addresses are in `RELEASE-TRAIN.local.md`
(gitignored, see "Never commit private infrastructure" in `AGENTS.md`).

| role | needed by | what it must have |
|---|---|---|
| local (macOS, Apple Silicon) | homebrew, source, archaeology | rustup toolchain with `aarch64-apple-darwin` and `x86_64-apple-darwin`, `swiftc`, `brew`, `uv`, `python3.12`, `gh` |
| linux-builder (aarch64 Linux VM) | deb | cargo, the apt build deps from `Makefile` check-deps, `dpkg-deb`, `gpg`, `apt-ftparchive`, `rsync`, passwordless sudo; `nfpm` is fetched per run |
| windows-bench (Windows 11, x86_64) | windows | cargo, PowerShell 7 as the ssh login shell, a checkout to sync into |

## Smoke contract

Run against the **installed** copy with `HOME` (or `USERPROFILE` and
`APPDATA`/`LOCALAPPDATA`) pointed at a fresh temp dir, from a working
directory outside the checkout so `panio skill` resolves the packaged copy
rather than the repo's `skills/`, and with `PANIO_PLUGIN_PATH` unset so no
plugin root (#303) adds entries to the S2/S3 lists.

| id | check | pass condition |
|---|---|---|
| S1 | `panio --help`, then `panio --version` | exit 0; lists `target`, `serial`, `video`, `hid`, `doctor`, `daemons`, `skill`, `helper`; `--version` prints exactly `panio X.Y.Z` |
| S2 | `panio skill` and `panio skill panio` | exit 0; lists `panio`, `kvm-puppeting`, `control-host`, `managed-switch`; the second prints a SKILL.md body |
| S3 | `panio helper` then `panio helper -- <h> --help` for each helper, `<h>` being the first field of each list line (a plugin-root entry adds `<TAB>plugin: <root>`; none appear with the variable unset) (since #300 everything after the helper name goes to the helper, so the `--` is optional; it stays here because a pre-#300 panio needs it, where clap otherwise answers for the `helper` verb and the binary never runs) | each exit 0 (a `--help` that returns 0 proves the shared libraries loaded), except `netbootd-bpf-helper` on macOS: the setuid helper takes no arguments and answers any call, `--help` included, with a usage line and exit 2 (`release.yml` asserts exactly that), which is its pass condition |
| S4 | `panio --lab $T/lab.toml init`, `target add smoke`, `target list`, `config show`, `target rm smoke`, `target list` | exit 0 each; `smoke` listed before `rm`, absent after |
| S5 | `panio --lab $T/lab.toml doctor` on that hardware-free lab, then `panio daemons list` | exit 0 or a clean "nothing configured" exit; never a panic or traceback |
| S6 | the platform's OCR helper answers: `visionocr` (macOS) and `winocr.exe` (Windows) decode the 8x8 PNG the release workflow's macOS smoke step uses and print the v1 envelope; `linuxocr --help` and `rapidocr --help` on Linux | exit 0, `engine` field matches |
| S7 | `python3.12 evals/run.py --check` with the installed `panio` first on `PATH` | exit 0 (the drift guard proves the shipped `--help` surface matches every documented scenario); n/a on Windows |

## Channels

Only the channels this repo ships. There is no crates.io or PyPI publish; the
from-source path is the `source` arm. winget is not an arm: `WINGET_TOKEN`
exists (2026-10-19), but the release job skips with a notice until the
hand-submitted first version (`CurtisGalloway.Panio`) is merged into
`microsoft/winget-pkgs`; after that it opens an update PR there on each tag.
v0.12.0 predates the skip and is not on winget: once the package exists,
`gh run rerun <that release run> --failed` publishes it.

The arms run in parallel and the release worktree is their **source only**.
Each arm stages into its own scratch directory outside the worktree — export
`RT_STAGE=$(mktemp -d)` per arm and put the keg layout, the tarball and its
sidecar, and the throwaway formula under it. The deb arm's scratch directory is
its VM-local copy on the builder, which only it uses; its staging tree has to
sit at `packaging/stage/*` inside that copy, because `packaging/nfpm.yaml`
hardcodes those `src:` paths.

Why it matters: the deb arm rsyncs the worktree to the builder mid-run, so
anything another arm has left in the worktree rides along. On the v0.3.1 train
the homebrew arm staged its keg and tarball in the worktree and the macOS
tarball ended up in the builder's copy. Nothing was corrupted that time, and
`rsync --delete` cannot corrupt the worktree — it only deletes at the
destination — but an arm's build inputs should not depend on what another arm
happens to have finished writing.

The sync carries `--delete-excluded`, not bare `--delete`. An `--exclude`d path
is *skipped* by `--delete`, so a plain exclude would leave last train's
`packaging/stage/*` and `dist/*` sitting on the builder to be globbed into the
next `.deb` by `nfpm`'s `src: packaging/stage/libexec/*` and into the next apt
pool by `build-apt-repo.sh`'s `ls "$DEBS_DIR"/*.deb`. `--delete-excluded`
removes them instead, so the builder's copy is the worktree plus this arm's own
output and nothing else. The one exception is the per-crate `target/` dirs,
which a `P` (protect) filter rule keeps across trains so the build is
incremental; the deb arm's build step spells out why a plain exclude is not
enough.

### homebrew

- kind: homebrew
- artifact: `panio-X.Y.Z-macos-universal.tar.gz` plus `.sha256`, and the same tree as a Homebrew bottle, `panio-X.Y.Z.all.bottle.tar.gz` plus `.sha256` (keg layout under `panio/X.Y.Z/`)
- workflow job: package-macos
- host: local
- build: in the worktree, with `PANIO_VERSION=X.Y.Z` exported, `cargo build --release --target <triple>` for `cli` and every helper, both Apple targets, `CARGO_PROFILE_RELEASE_STRIP=symbols`; `swiftc -O -target <arch>-apple-macos12.0 ocr/visionocr.swift` per slice; `lipo` everything including `netbootd-bpf-helper`; stage the keg layout (`bin/panio`, `libexec/bin/*`, `share/panio/skills/*`) under `$RT_STAGE`, never inside the worktree (see the rule above the channels); ad-hoc `codesign`; tarball and sidecar as the `package-macos` job builds them but written to `$RT_STAGE` rather than the job's `dist/` — that tarball inside the worktree is what rode into the builder on v0.3.1. The `swiftc` calls need explicit `-o $RT_STAGE/visionocr-<arch>` per slice, as the job has: without `-o` both slices write `visionocr` into the cwd and the second overwrites the first, leaving `lipo` one architecture to "combine". Use a persistent `CARGO_TARGET_DIR` outside the worktree so trains are incremental. Then the bottle, as the job's `Build Homebrew bottle` step does: copy the staged keg under `$RT_STAGE/bottle/panio/X.Y.Z/`, `tar -C $RT_STAGE/bottle -czf $RT_STAGE/panio-X.Y.Z.all.bottle.tar.gz panio`, sidecar with `shasum -a 256`; and a second copy of the same keg tarred as `panio-rt/X.Y.Z/` into `$RT_STAGE/panio-rt-X.Y.Z.all.bottle.tar.gz` for the install step below (Homebrew fetches `<formula name>-<version>.<tag>.bottle.tar.gz` from `root_url` and expects the keg inside to be named after the formula)
- install like a user: on macOS the tap's stable formula pours the `all` bottle, and it has to — a bottle-less formula sends Homebrew down its build-from-source preflight, whose Xcode minimum-version check refuses outright on a macOS newer than Homebrew's table (#225, seen on v0.3.1 and v0.4.0: "Your Xcode (26.6) ... is too outdated") — so the throwaway formula must pour too. Write it at `$RT_STAGE/panio-rt.rb` (not in the worktree, where it shows up as an untracked file during the publish phase): `keg_only "release-train dry run"`, `url "file://<tarball>"` with the real `sha256` (Homebrew requires a stable url), a `bottle do` block with `root_url "file://$RT_STAGE"` and `sha256 cellar: :any_skip_relocation, all: "<sha256 of panio-rt-X.Y.Z.all.bottle.tar.gz>"`, and an `install` block that is only `odie "must pour"`, so a fall-through to building from the tarball fails the arm instead of passing on the wrong path. `HOMEBREW_DEVELOPER=1 brew install --formula ./panio-rt.rb` (Homebrew 6 refuses a bare `.rb` outside a tap otherwise); the install log must contain `Pouring panio-rt-X.Y.Z.all.bottle.tar.gz`. keg-only means nothing links into the developer's `bin`
- smoke: S1..S7 against `$(brew --cellar)/panio-rt/X.Y.Z/bin/panio` (`brew --prefix panio-rt` cannot resolve a formula installed from a bare path)
- cleanup: `rm -rf "$RT_STAGE"` — a universal keg plus its tarball is hundreds of MB per train, and nothing else reaps it. Then `HOMEBREW_NO_AUTOREMOVE=1 brew uninstall panio-rt`, always. The variable is not optional: Homebrew 7 runs `autoremove` after every uninstall, and on 2026-09-13 a bare `brew uninstall panio-rt` swept four unrelated orphaned leaves (`rust`, `llvm@22`, `libgit2`, `libssh2`, ~2 GB) out of the shared Cellar
- caveats: the real tap formula (`curtisgalloway/homebrew-tap`) is re-pinned by the release workflow's `bump-tap` job, not exercised here; re-verify covers it. The Linux bottles (`panio-X.Y.Z.arm64_linux.bottle.tar.gz`, `x86_64_linux`) are built and layout-checked only by the `package` job in CI; no host in this train has Linuxbrew, so a Linux `brew install` pouring one is unverified by the train. Outside the train, the v0.5.1 `x86_64_linux` bottle (released under the old name, paniolo) was poured by hand on an x86_64 Linux machine on 2026-09-26 (Homebrew 7.0.6, `brew install curtisgalloway/tap/panio`: `INSTALL_RECEIPT.json` recorded `poured_from_bottle: true`, and `panio --version`, `panio skill` and `brew test panio` passed, with every helper in `libexec/bin`); that was one release on one arch, not a per-release check. The `arm64_linux` bottle has never been poured. Installing Homebrew on linux-builder (aarch64) would let this arm pour it every train

### deb

- kind: deb
- artifact: `panio_X.Y.Z_<arch>.deb` plus `.sha256`, via `packaging/nfpm.yaml`
- workflow job: package
- host: linux-builder
- build: rsync the worktree to a VM-local dir that only this arm uses (never build on a shared mount), with `--delete-excluded --filter='P target/' --exclude=target/ --exclude=/.venv/ --exclude=/.git/ --exclude=/dist/ --exclude=/logs/ --exclude=/packaging/stage/` — leading slashes anchor each pattern at the transfer root, since an unanchored `dist*` also matches `docs/distributed-control.md`. `target/` is deliberately unanchored: this repo has no workspace `target/`, only per-crate ones (cli/target, hdmicap/target and so on, gitignored), and the `P` (protect) filter is what keeps them warm on the builder — `--delete-excluded` deletes excluded paths at the destination too, so a bare `--exclude=target/` still cold-builds all nine crates every train (#227; verified with rsync 3.2.7 on the builder). `--delete-excluded` is what stops last train's staging and `.deb` surviving on the builder (see the rule above the channels); build `cli` and every helper `--release` with `PANIO_VERSION=X.Y.Z` exported (the builder needs `libdbus-1-dev` for `dsdble`, as the `package` job installs; the `.deb` declares `libdbus-1-3` at runtime); stage the gitignored staging tree under `packaging` (`stage/bin` with the CLI, `stage/libexec` with the helpers plus `ocr/linuxocr` and `ocr/rapidocr`) as the `package` job does; fetch `nfpm` at the workflow's `NFPM_VERSION` and verify its `checksums.txt`; `VERSION=X.Y.Z ARCH=<arch> nfpm package -f packaging/nfpm.yaml -p deb`
- install like a user: assemble a real repo with `packaging/scripts/build-apt-repo.sh <debs> <out> <fpr>` under a throwaway GPG key, install the public key under `/etc/apt/keyrings/panio-rt.asc`, write a deb822 `.sources` (`URIs: file:///<out>`, `Suites: stable`, `Components: main`, `Signed-By` that key), `apt-get update`, `apt-cache policy panio` shows X.Y.Z, `apt-get install panio=X.Y.Z`; record whether an older panio was present (upgrade path) or not (fresh path)
- smoke: S1..S7 against `/usr/bin/panio`; S3 also `/usr/libexec/panio/bin/linuxocr --help` and `rapidocr --help`; also `/usr/share/panio/skills/panio/SKILL.md` and `/usr/lib/tmpfiles.d/panio.conf` exist
- cleanup: `apt-get remove panio` unless it was installed before; remove the `.sources` file, the keyring, the throwaway key
- caveats: the builder is Ubuntu 24.04 (glibc 2.39), not the `debian:bookworm` (glibc 2.36) container CI builds in, so glibc-floor regressions are CI's to catch; an amd64 `.deb` is built only by CI. The builder mounts the host home **read-only**, so a script run inside the VM cannot write its log or verdict into the run dir. The script writes them under the VM's own home and the **control host pulls them** afterward (`scp <builder>:~/rt-<run>/verdict.json logs/release-train/<run>/`) — a copy started from inside the VM hits the same read-only mount one step later. The v0.3.1 apt re-verify arm learned this mid-run. The mount is not yet recorded in `RELEASE-TRAIN.local.md`; add it there when that file is next edited

### windows

- kind: zip
- artifact: `panio-X.Y.Z-x86_64-pc-windows-msvc.zip` plus `.sha256`
- workflow job: package-windows
- host: windows-bench
- build: sync sources to the bench host the way `scripts/sync-brik.sh` does (`tar` over ssh, excluding `target` and `.git`); on the host mirror the `package-windows` job's "Build every crate" step (cli, every helper, `ocr/winocr`, with `$env:PANIO_VERSION = "X.Y.Z"` set first), stage `panio\panio.exe`, `panio\libexec\*.exe`, and `panio\share\panio\skills\<name>\SKILL.md`; `Compress-Archive`; write the sidecar as the job does. Anything beyond a one-liner goes in a `.ps1` copied over and run with `pwsh -NoProfile -File`; PowerShell quoting over ssh is not worth fighting
- install like a user: `Expand-Archive` into a fresh temp dir; a new `pwsh` with that dir's `panio\` prepended to `PATH` and `USERPROFILE`, `APPDATA`, `LOCALAPPDATA` pointed at a fresh temp dir
- smoke: S1..S6 (S7 n/a); S6 is `libexec\winocr.exe --json <tiny png>`
- cleanup: nothing to undo; the zip and install dir stay under the host's build-logs dir
- caveats: signing is skipped locally (no Azure vars) so the exes are unsigned; netboot and OCR-in-panio are known-unsupported on Windows per docs/dev/platform-support.md; the bench host sleeps when idle, so the local file's liveness probe and wake step come first. Fallback when the host is unreachable: `gh workflow run release.yml --ref <release branch>`, download `packages-windows-x86_64`, inspect the layout, report PARTIAL (never dispatch in a dry run)

### source

- kind: source
- artifact: none (the README's from-source path: `cargo install --path cli` then `panio setup`; also what `brew install --HEAD panio` does)
- workflow job: none
- host: local
- build: `PANIO_VERSION=X.Y.Z cargo install --path cli --root $S/.cargo` from the worktree with `HOME=$S`, `CARGO_HOME` **and `RUSTUP_HOME` exported explicitly at their real paths** (with `HOME` overridden rustup otherwise looks under `$S/.rustup`, finds no toolchain, and cargo fails with "could not choose a version of cargo"; the registry cache is the other reason), a persistent `CARGO_TARGET_DIR`, and **`CARGO_INSTALL_ROOT=$S/.cargo`**: `panio setup` reinstalls the CLI itself with `cargo install --path cli --force` and no `--root` (`cli/src/setup.rs`), which without that variable overwrites the developer's real CLI (it did, 2026-09-10)
- install like a user: `HOME=$S panio setup --rust-only` from the worktree root (helpers land in `$S/.local/libexec/panio/bin`, and since #207 the bundled skills land in `$S/.local/share/panio/skills` on this path too); then by hand the OCR helper, which `--rust-only` skips because it needs a second toolchain: `swiftc -O -o $S/.local/libexec/panio/bin/visionocr ocr/visionocr.swift`; then zigplug, which `--rust-only` skips for the same reason (it needs uv) — `UV_TOOL_DIR=$S/uv UV_TOOL_BIN_DIR=$S/.local/libexec/panio/bin uv tool install ./zigplug` and `zigplug --help`; then jetkvm, which `--rust-only` also skips because it needs Go (`SourceStep::JetKvm` in `cli/src/setup.rs`) — `(cd jetkvm && go build -trimpath -o $S/.local/libexec/panio/bin/jetkvm .)` and `jetkvm version`. All three by-hand steps are load-bearing: until the v0.4.0 train the zigplug install ran on the fast path anyway (the block had no `will(...)` gate), so a recipe that omitted it still ended up with a working `zigplug` and nothing said otherwise
- smoke: S1..S7 against `$S/.cargo/bin/panio` with `HOME=$S`
- cleanup: nothing outside `$S`
- caveats: `panio setup` rebuilds the CLI with `cargo install --force` and inherits `PANIO_VERSION` from the environment, so keep it exported for that step too or S1 sees `0.1.0 (unversioned dev build)`

## Archaeology

- issue source: `gh issue list --state closed --limit 200 --json number,title,closedAt,body`
- fix commits: `git log --format='%h%x09%s%x09%b'` filtered for `Fixes #N` / `Closes #N` trailers first; loose `#N` matching mis-maps prose mentions (#144, #168, #140/#147 did on 2026-09-10); a fix commit with no issue still counts
- test locations: Rust `#[cfg(test)]` modules beside the code and `<crate>/tests/*.rs`; pytest in `ocr/tests` and `zigplug/tests`; `evals/tests`
- how to run: `cargo test --manifest-path <crate>/Cargo.toml` (own `CARGO_TARGET_DIR`, do not share with the arms); Python the way `.github/workflows/ci.yml` does; `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before committing; `uvx pyink` for Python
- prove-it-bites: `git checkout <fix>^ -- <fixed source files only>`, run, must fail; `git checkout HEAD -- <those files>`, must pass
- conventions: tests must execute the fixed path (spawn, parse, call), never assert on the text of source or scripts; American spelling; commit subject `test: pin #<n> <short bug>`
- state on 2026-09-10: all 27 closed issues already pinned in their fix commits; backlog is fix commits with no issue number

## Publish

- gate: `date`; the repo is public on GitHub, so the user-level business-hours push rule applies; ask before any push, tag, or PR
- steps: push the release branch; `gh pr create --base main` with only the archaeology commits; `gh pr checks --watch`; `gh pr merge --squash`; `git fetch origin && git checkout main && git pull --ff-only`; annotated tag on the merged head with the releaser identity above; `git push origin vX.Y.Z`; watch `release.yml` (`gh run watch`), then the dispatched `docs.yml` run (it rebuilds the apt pool from the newest 5 Releases; a red docs run means apt clients keep the previous version)
- re-verify github: `gh release view vX.Y.Z --json assets` lists 18 assets (2 `.deb`, 2 Linux `.tar.gz`, 3 `.bottle.tar.gz` (`all`, `arm64_linux`, `x86_64_linux`), macOS `.tar.gz`, Windows `.zip`, each with `.sha256`); download all, `shasum -a 256 -c` each against its sidecar (Perl `shasum`, which rejects a CRLF sidecar that some `sha256sum` builds accept, #243; the release job runs the same check before attaching)
- re-verify homebrew: the tap's `Formula/panio.rb` shows `version "X.Y.Z"`, the new tarball sha256s, and a `bottle do` block whose `root_url` is this release's download directory with `all`, `arm64_linux` and `x86_64_linux` digests matching the release's `.bottle.tar.gz.sha256` sidecars; `brew update && brew fetch panio` succeeds and fetches `panio-X.Y.Z.all.bottle.tar.gz` (a tarball fetch instead means the bottle block is missing and every macOS user with an Xcode older than Homebrew's table is back to #225); install the fetched bottle `panio-rt`-style (retar its keg as `panio-rt/X.Y.Z/`, pour it through a throwaway formula as the homebrew arm does) and run S1..S3
- re-verify apt: fetch with `curl -H "Cache-Control: no-cache"` and a cache-busting query (`?rt=<run>`), because the Pages CDN served a pre-publish `Packages` for at least 28 minutes after a rebuild on 2026-09-13 and a plain GET reported the newest release as missing; then `https://panio.dev/apt/dists/stable/InRelease` is signed and its `Packages` lists X.Y.Z; on linux-builder configure the `.sources` from `README.md`, `apt-get update`, `apt-cache policy panio` shows X.Y.Z, `apt-get install panio`, run S1..S3 (the builder's mount of the host home is read-only, so the log and verdict are written under the VM home and pulled by the control host, as in the deb arm's caveats)
- re-verify windows: download the zip, verify the sidecar, expand and run S1..S3 on windows-bench if reachable, else inspect the layout and report PARTIAL
- re-verify source: `cargo install --git https://github.com/curtisgalloway/panio --tag vX.Y.Z panio` into a temp root, run S1
- a re-verify failure never rolls back the tag; open an issue and report `PUBLISHED, unverified on <channel>`

## Sources

`profile_check.py` recomputes each blob id; a CHANGED row means the section it
feeds needs re-reading before `--update` re-pins it.

| path | blob | feeds |
|---|---|---|
| `.github/workflows/release.yml` | 38cd58a503b8 | Channels (every arm's build and staging), Publish |
| `.github/workflows/docs.yml` | 206e9ae532f2 | Publish: re-verify apt |
| `packaging/nfpm.yaml` | e3496d9710c6 | Channels: deb |
| `packaging/scripts/build-apt-repo.sh` | 391037a4ea4c | Channels: deb, install like a user |
| `Makefile` | f147f0ade7b0 | Project: helpers; Channels: source |
| `cli/src/setup.rs` | e6ed6816d4c8 | Channels: source |
| `cli/src/skills.rs` | 0cf3c3778373 | Smoke contract S2; Channels: homebrew, windows |
| `cli/src/daemons.rs` | 863b5e59da06 | Smoke contract S3 |
| `scripts/ci-coverage-check.sh` | 14c39a57f1c7 | Project: helpers |
| `scripts/sync-brik.sh` | 1920fb8f6241 | Channels: windows |
| `README.md` | ba8cbccb6a24 | Channels: source; Publish: re-verify apt |
| `AGENTS.md` | 9aff23ceb3df | Project: bump rules, tag format; Publish |
