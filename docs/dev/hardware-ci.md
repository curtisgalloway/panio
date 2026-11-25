<!--
Copyright 2026 The Panio Authors

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

# Hardware-in-the-loop CI for panio (design)

> **Status:** design, revision 2. Nothing in this document runs yet, and nothing
> in it authorizes deploying a runner. Acceptance gates are proposed, not
> executed. The real network layout, hook behavior and release credentials
> still have to be checked on the bench.
>
> **Scope:** panio's *own* CI, testing panio against real KVMs, muxes and relays.
> This is different from [ci-integration/](ci-integration/design.md), which is about
> letting other CI systems (LAVA, botanist) drive boards through panio.
>
> **Revision 2** applies the security design review on PR #429 (findings
> SEC-01 to SEC-10). The main change: the runner belongs to a **separate private
> repository**, and public panio supplies reviewed source at a fixed commit, not
> workflow authority. [§12](#12-review-findings-and-where-they-are-addressed)
> maps each finding to the section that addresses it.

## Start here: the first step (about 10 minutes, today)

This step registers no runner and touches no hardware:

1. Create a **private** repository named `panio-hardware-ci` under your
   account (**github.com/new**, Private, add a README).
2. In its **Settings → Actions → General**, leave Actions enabled. Under
   **Workflow permissions**, choose **Read repository contents and packages
   permissions**. Save.

That's the whole first step. Phase 1 continues in [§10](#10-phased-rollout).

---

## 1. Why

Two recent bugs passed every test in CI and still broke on real hardware:

| Issue | What the tests saw | What the hardware did |
|---|---|---|
| [#417](https://github.com/curtisgalloway/panio/issues/417) | `panio usb attach-target` exits 0 and prints `target` | The Openterface Mini-KVM's USB-A port never switched; the drive stayed on the control host |
| [#415](https://github.com/curtisgalloway/panio/issues/415) / PR #428 | Scroll works against the fake JetKVM device | JetKVM firmware 0.5.11-dev removed the `wheelReport` JSON-RPC, so scroll fails; the fix was tested only against a simulated device |

Both have the same shape: **panio's own report of success was trusted, and the
real outcome was never checked.** Simulated devices can only encode what we
already believe about the hardware. A hardware job that checks the *outcome*
from an independent vantage point catches both.

## 2. Can this run on the free GitHub plan?

**Yes.** Self-hosted runners are free on every plan, including Free, for public
and private repositories, and GitHub doesn't bill minutes for jobs on your own
machine. Hosted jobs (the watchdog and reporter in phase 2) and artifact
storage in a *private* repository count against the Free plan's monthly
allowance (2,000 minutes and 500 MB at the time of writing). The small jobs in
this design use a few minutes a day.

What the Free plan doesn't give a personal account, and why it doesn't matter
here:

- **Runner groups** are an organization feature. A runner registered to one
  repository is only offered that repository's jobs.
- **Environments with protection rules** aren't available for private repos on
  Free. Nothing here depends on them.

The constraint that shapes the design isn't billing. A self-hosted runner
attached to a **public** repository can be reached by workflows from fork pull
requests, and fork approval, runner labels and a workflow lint are not
authorization boundaries (an approved fork workflow can still ask for any
label). So **public panio gets no self-hosted runner at all.**

## 3. Architecture

```
 public: curtisgalloway/panio                private: panio-hardware-ci
 ┌──────────────────────────┐               ┌─────────────────────────────────┐
 │ source code only         │  reviewed SHA │ hardware.yml (schedule/dispatch)│
 │ no self-hosted runner    │──────────────▶│ harness + verdict logic         │
 │ no hardware workflow     │  (read-only   │ admission policy (reviewed)     │
 └──────────────────────────┘   clone)      │ maintainer: Curtis only         │
                                            └───────────────┬─────────────────┘
                                                            │ jobs for label panio-hw
┌─────────────────────── isolated bench network ────────────▼──────────────────┐
│ bench1                                                                       │
│  ├─ systemd unit (root-owned): ExecStartPre safety checks, then the listener  │
│  ├─ job-started hook (root-owned, hash-pinned): admission policy (§5.3)      │
│  ├─ runner user panio-ci: no sudo except one quarantine command               │
│  └─ builds panio from the approved SHA; runs the harness from the private repo│
│     │USB             │USB/LAN          │USB            │LAN                  │
│  USB mux          JetKVM (phase 2)   relay board     smart plug (phase 2)    │
│     │switched USB    │HID + HDMI       │power                                │
│     ▼                ▼                 ▼                                     │
│  target-machine ("witness"): answers a fixed set of SSH commands only        │
│     ▲                                                                        │
│     └──── wired LAN link, not through any device under test ─────────────────│
│                                                                              │
│  Blocked: home LAN and management addresses (IPv4 and IPv6).                 │
│  Allowed out: HTTPS to the internet only.                                    │
└──────────────────────────────────────────────────────────────────────────────┘
```

Ideas that carry over from revision 1:

- **The witness.** The target is a small Linux machine that bench1 reaches
  over SSH on a plain wired link that doesn't pass through any device under
  test. Every test acts through panio, then asks the witness what really
  happened.
- **Outcome checks in both directions** with a fresh nonce each run.
- **A low-cost first phase** on hardware you already have.

What changed:

- **Workflow authority moves to the private repo.** Its workflows, harness
  and admission policy are trusted code that only you maintain. Public panio
  is just a source of commits.
- **Phase 1 runs only reviewed `main` commits.** Testing a PR branch before it
  merges (PR #428's case) and running on release tags are deferred until
  explicit admission for them is designed ([§11](#11-decisions-and-open-questions)).
- **Isolation, the protected hook, leases and cleanup are all in phase 1,
  before any job runs.**

## 4. Hardware list and rough costs

Prices are rough US street prices as of November 2026; check before buying.

### Phase 1 (minimum)

| Item | Role | Rough cost |
|---|---|---|
| Runner box: Raspberry Pi 5 (4–8 GB) with PSU, case, 64 GB+ storage, *or* a low-end x86 mini PC (Intel N100 class) | Runs the listener, builds panio, runs the harness. Dedicated to the bench | $0 if owned; $100–200 |
| Witness target: any spare Pi 4/5 or old PC running Linux | Reports outcomes over restricted SSH | $0–80 |
| One USB mux panio already supports: Openterface **KVM-Go** (hardware-verified) or a USB sharing switch with a DSD relay (`usbtoggle`) | Device under test for the USB round trip | $0 if owned |
| Small USB flash drive, CI only | Payload that moves between host and witness | $8 |
| Isolated network segment: a VLAN on gear you have, **or** a small travel router that puts the bench behind its own NAT and firewall | Network containment ([§5.5](#55-network-containment)) | $0 with a VLAN-capable router; $30–40 otherwise |

**Phase 1 total: $0–25 if you already have a box, witness, mux and a way to
isolate the bench; add $30–40 for a travel router if not.** Without isolation,
don't register a runner: run the harness by hand on the bench instead.

### Phase 2 additions

| Item | Role | Rough cost |
|---|---|---|
| JetKVM (on **stable** firmware) | HID and video tests; would have caught #415 | $70–90 |
| Openterface Mini-KVM | Covers #417 once a helper exists; until then, a test that proves `usb` refuses it | $100–140 |
| DSD Tech SH-UR04A 4-channel USB relay | Target power test | $15–20 |
| Wi-Fi/LAN smart plug (Shelly or Kasa, both panio-supported) | Power-cycle a wedged KVM during recovery | $15–25 |
| Second boot medium for bench1 (or a second SD card) | Disposable, re-imaged execution environment ([§5.6](#56-execution-state)) | $10–20 |

### Phase 3 additions

| Item | Role | Rough cost |
|---|---|---|
| A second JetKVM, on the **dev** firmware channel | Early warning for upstream firmware changes ([§9](#9-firmware-channels)) | $70–90 |
| Cheap USB HDMI capture dongle (optional) | Second opinion on video tests | $15 |

## 5. Security design

### 5.1 Threats

1. **Untrusted workflows reaching the bench.** On a public repo, a fork PR
   can ask for a self-hosted runner label. Addressed by putting the runner on
   a private repo that only you maintain (§5.2).
2. **Bypassing admission on the box.** A job runs as the runner user. Anything
   that user can write (hook script, runner `.env`, service files, the runner
   binary itself), a job can change for the *next* job (§5.4).
3. **Reaching the home network from the bench.** Code on bench1 is LAN-adjacent
   to everything else you own (§5.5).
4. **Untrusted code inside a trusted workflow.** Even a reviewed `main` commit
   runs build scripts (`build.rs`), dependencies and freshly built binaries
   that nobody has audited line by line. A trusted workflow doesn't make them
   trustworthy (§5.7).
5. **Leaking lab details.** Logs, artifacts and issues can expose device
   paths, serials and addresses ([AGENTS.md: never commit private
   infrastructure](https://github.com/curtisgalloway/panio/blob/main/AGENTS.md#never-commit-private-infrastructure)) (§5.8).

### 5.2 Where authority lives

| | Public `panio` | Private `panio-hardware-ci` |
|---|---|---|
| Self-hosted runner | **None.** Nothing to target | Registered here only, label `panio-hw` |
| Hardware workflows | None | `hardware.yml` (bench), later `reporter.yml` and `watchdog.yml` (hosted) |
| Triggers | n/a | `schedule` (nightly) and `workflow_dispatch`. **No** `push`, `pull_request`, `pull_request_target`, `workflow_run` on the bench workflow |
| Who can change workflows or policy | n/a | You only; no other collaborators |
| What it takes from the other repo | n/a | A read-only clone of public panio at one approved commit |
| Credentials | Unchanged | Bench jobs get `GITHUB_TOKEN` with `contents: read` on the **private** repo only. No token that can touch public panio ever reaches the bench |

**Approved source commit.** In phase 1 a candidate is a 40-hex commit that is
reachable from public `main` (`git merge-base --is-ancestor <sha>
origin/main` in a fresh clone). The nightly run uses `main`'s current head;
a manual dispatch takes `sha` as an input and is held to the same rule. So
only code that has already gone through review on public panio runs on the
bench.

**Release tags (SEC-10).** Revision 1 mixed a tag-only `push` trigger with a
hook that also allowed `main` pushes. Both are gone: the bench workflow has no
`push` trigger. For later: tags here come from `cut-release.yml`, which pushes
with the `RELEASE_TAG_DEPLOY_KEY` deploy key. A deploy-key push *does* trigger
workflows, unlike a push made with the ordinary `GITHUB_TOKEN`, which doesn't.
That only matters inside public panio, though; tags can't trigger anything in
the private repo. A tag-driven hardware run would be designed as "the private
repo's nightly also tests the newest `v*` tag", with the same ancestry rule,
and only after phase 3 ([§11](#11-decisions-and-open-questions)).

### 5.3 Admission policy (job-started hook)

The runner runs the script named by `ACTIONS_RUNNER_HOOK_JOB_STARTED` before
every job, and a non-zero exit fails the job before any workflow step runs.
The hook **denies by default**. A missing or malformed variable is a refusal.
It admits a job only if all of the following hold:

| Check | Required value |
|---|---|
| `GITHUB_REPOSITORY` | `curtisgalloway/panio-hardware-ci` |
| `GITHUB_WORKFLOW_REF` | `curtisgalloway/panio-hardware-ci/.github/workflows/hardware.yml@refs/heads/main` (exact) |
| `GITHUB_EVENT_NAME` | `schedule` or `workflow_dispatch` |
| `GITHUB_REF` | `refs/heads/main` |
| `GITHUB_ACTOR` and `GITHUB_TRIGGERING_ACTOR` | both `curtisgalloway`. This covers re-runs, where the triggering actor is whoever clicked **Re-run** |
| `GITHUB_SHA` (the private repo's workflow commit) | on the policy's list of reviewed workflow commits, or reachable from the private `main` |
| Candidate SHA (from `inputs.sha` in `$GITHUB_EVENT_PATH`, or public `main` for `schedule`) | 40 hex characters and an ancestor of public `main`, checked in a fresh clone the hook makes itself |
| Quarantine flag | absent ([§5.6](#56-execution-state)) |
| Lease | the bench lease is free and acquired by the hook ([§7](#7-leases-cancellation-and-cleanup)) |

Each refusal goes to a root-owned log (via the quarantine helper, below), so
probes leave a trace the job can't erase.

### 5.4 Protecting the hook and the service (fail closed)

A hook that merely lives outside the checkout isn't protected: the runner user
could rewrite it, or drop the variable that names it, between jobs. So:

- **Ownership.** The hook, its policy file, and every parent directory are
  owned by `root` and not writable by `panio-ci`. So are the runner's install
  directory and its config files, except the `_work` and `_diag` directories
  the listener needs to write.
- **No self-update.** Register with `--disableupdate`, so the runner never
  rewrites its own binaries. Upgrades are a manual step you do as root.
- **Environment from systemd, not `.env`.** `ACTIONS_RUNNER_HOOK_JOB_STARTED`
  and `ACTIONS_RUNNER_HOOK_JOB_COMPLETED` are set with `Environment=` in a
  root-owned systemd unit. The runner's own `.env` file is root-owned and
  empty.
- **Startup checks.** The unit's `ExecStartPre=+/usr/local/sbin/bench-precheck`
  runs as root before the listener starts and refuses to start it unless:
  the hook and policy match SHA-256 hashes pinned in a root-owned file; the
  ownership and modes above hold; the quarantine flag is absent; and the
  isolation probes in §5.5 fail as they should. A missing or altered hook
  therefore means no listener, not a listener without a hook.
- **One root command.** `panio-ci` has no general sudo. One sudoers line lets
  it run `/usr/local/sbin/bench-quarantine <reason>`, which only writes the
  root-owned quarantine flag and log, and stops the runner service. Only you
  clear the flag, by hand.

**Residual risk:** the listener's registration credentials (`.credentials`,
`.runner`) have to be readable by `panio-ci`, so a malicious job could copy
them. With a private repo that only lets someone receive *this repo's* jobs,
which carry no secrets. Phase 2's just-in-time, single-use registration
removes even that ([§5.6](#56-execution-state)).

### 5.5 Network containment

Containment is in phase 1 and comes **before** registration:

- bench1, the witness and the lab devices sit on their own segment: a VLAN, or
  behind a travel router with its own NAT and firewall.
- **Upstream (router) rules:** the segment can't open connections to the home
  LAN or to any device's management interface, over IPv4 or IPv6. The simplest
  dependable choice is turning IPv6 off on the segment; if it stays on, block
  ULA (`fc00::/7`) and the home prefix too.
- **Host rules on bench1 (nftables, root-owned):** outbound is allowed only to
  the lab subnet, DNS to the segment's resolver, and TCP 443 to addresses that
  aren't private, link-local or ULA. Everything else is dropped in both IPv4
  and IPv6. The GitHub Actions runner only needs HTTPS out.
- **Probes:** `bench-precheck` tries a short TCP connect to a configured
  trusted-LAN address over IPv4 and over IPv6, plus the home router's
  management port. Any success blocks the listener from starting.

If this isolation isn't available, **don't register a runner.** Run the
harness by hand on the bench instead; it's the same code without GitHub in the
loop.

### 5.6 Execution state

"Ephemeral" registration means one job per registration, not a wiped machine.
The design doesn't claim isolation it doesn't have:

- **Phase 1 (persistent, reviewed code only).** One long-lived registration on
  a dedicated, contained bench. The job-completed hook removes the workspace,
  kills leftover `panio-ci` processes, and returns hardware to a known state
  ([§7](#7-leases-cancellation-and-cleanup)). The job-started hook then checks
  for *unexpected* state: files in `panio-ci`'s home outside an allowlist,
  surviving processes, new cron/systemd user units, changed `authorized_keys`,
  a non-empty workspace. Any of these runs `bench-quarantine`, so the next job
  can't run. This is acceptable only because phase 1 runs nothing but commits
  already reviewed on public `main`.
- **Phase 2 (disposable).** Each job gets genuinely fresh execution state: a
  just-in-time, single-use runner registration inside a throwaway VM or
  container whose root filesystem is recreated from a known image for every
  job (or, on a Pi, an overlay root that is discarded on reboot). USB devices
  are passed through per job. Only then does the doc call the runner
  ephemeral.

### 5.7 Trusted harness, untrusted candidate

- **The harness and verdict logic live in the private repo** (trusted control
  plane): test sequencing, nonce generation, witness queries, pass/fail rules
  and summaries. Nothing from the candidate checkout (scripts, test fixtures,
  `scripts/hw/` in public panio) decides a verdict.
- **The candidate only supplies binaries.** The workflow builds panio and the
  needed helpers from the approved SHA with `cargo build --locked`, as
  `panio-ci`, and the harness invokes those binaries as the thing under test.
- **Witness SSH is narrow.** The witness's `authorized_keys` entry for bench1
  uses `restrict,command="/usr/local/bin/witness-agent"`. The agent accepts a
  fixed set of verbs (`drive-list`, `nonce-read`, `nonce-write <hex>`,
  `drive-flush`, later `evdev-capture <seconds>`), validates their arguments,
  and does nothing else. No shell, no forwarding.

**Residual risk, stated plainly:** the candidate's build scripts and binaries
run on the same host, as the same user, as the harness. A malicious candidate
could tamper with the harness's process or results while the job runs.
Independent *physical* observation (the witness) protects against panio being
**wrong**. It doesn't protect against panio being **hostile**. Phase 1
accepts this because candidates are reviewed `main` commits. Phase 2's
disposable environment limits how long such tampering can persist, not whether
it can happen in that job. Fully separating them (harness on a second machine
that drives a candidate-only box) is noted in [§11](#11-decisions-and-open-questions)
and not planned.

### 5.8 Results, logs and publication

- **Masking isn't a confidentiality boundary.** Raw logs and artifacts stay in
  the private repository, visible only to you.
- **Phase 1: publishing is manual.** The run summary is a short structured
  table (test, verdict, device role, firmware version) with no paths, serials
  or addresses. You decide what, if anything, goes to public panio.
- **Phase 2: a separate hosted reporter.** `reporter.yml` runs on
  `ubuntu-latest` in the private repo after a bench run completes. It reads
  only the sanitized summary artifact, validates it against a fixed JSON
  schema (bounded size, enumerated test names and verdicts, no free-text
  fields), and files or updates issues on public panio using **fixed
  templates**, with deduplication by a stable key and a rate limit (for
  example at most three issue writes per run).
  - It authenticates as a **GitHub App** installed only on public panio with
    **Issues: write** and nothing else (no contents, workflows or
    administration). Its private key is a secret in the private repo,
    referenced only by `reporter.yml`, which never runs on the self-hosted
    runner. The bench never sees it.

## 6. How each test verifies the outcome

Rules every test follows:

- **Act through panio, check through the witness.** Never use panio to check
  panio's own work.
- **Fresh nonce per run** (128 random bits, hex), so a stale result from an
  earlier run can't pass.
- **Both directions** wherever there are two.
- **Four outcomes:** `pass`, `fail` (product failure), `infra-error` (bench or
  harness problem), and `skip` (device not available). Results are reported
  per test, in the harness's own structured output.

### Phase 1: USB round trip (would have caught #417)

1. **Setup.** Lease held. Mux on the host side (verified as in step 4). The CI
   drive (filesystem label `PANIOCI`) is visible on bench1 and not on the
   witness.
2. bench1 writes `nonce1` to a file on the drive, then **flushes and
   unmounts** it (`sync`, `umount`) before switching.
3. `panio usb attach-target`. Record its exit code and output.
4. **Independent checks:**
   - the drive **disappears** from bench1 (`lsblk` by label) within 10 s;
   - the drive **appears** on the witness (`witness-agent drive-list`) within
     15 s;
   - `witness-agent nonce-read` returns exactly `nonce1`;
   - `panio usb state` reads `target`.
5. The witness writes `nonce2` and flushes (`nonce-write`, `drive-flush`).
6. `panio usb attach-host`, then the mirror image of step 4: gone from the
   witness, present on bench1, file holds exactly `nonce2`, `usb state` reads
   `host`.

Verdict rules:

| Situation | Verdict |
|---|---|
| Every check in both directions holds | pass |
| panio exits 0 but the drive didn't move, or `usb state` disagrees with the witness (N07) | **fail**, even though every command "succeeded" |
| Old nonce, missing file, or only one direction completed (N08) | fail; never pass |
| Witness unreachable, or the drive isn't visible anywhere at setup | infra-error, not pass |

### Later tests

| Test | Phase | Action through panio | Independent check | Would catch |
|---|---|---|---|---|
| **Unsupported device refuses** | 2 | `usb attach-target` on the Mini-KVM while no helper supports it | Must exit non-zero with an "unsupported" error. Exit 0 is a fail. Replaced by the round trip once a helper lands | #417 |
| **Keyboard** | 2 | `hid send type <nonce>` via JetKVM | `witness-agent evdev-capture` on the KVM's HID gadget; key sequence must spell `nonce` | — |
| **Mouse move** | 2 | `hid send move` and `hid send moveabs` | evdev shows `REL_X`/`REL_Y` (or `ABS_*`) with the right signs | — |
| **Scroll** | 2 | `hid send scroll -200`, `scroll 3`, `scroll 0` | evdev shows `REL_WHEEL` with the right sign and total, and nothing for 0 | #415 |
| **Power** | 2 | Relay off, then on | Requires **two** signals, because ping loss alone isn't proof of power loss: the witness stops answering **and** a hardware signal agrees (the relay helper's readback, plus the witness's USB gadget or HDMI signal dropping on the KVM). After power-on, the witness's `/proc/sys/kernel/random/boot_id` must have changed | — |
| **Video** | 3 | `video` capture plus OCR | Witness shows `nonce` in a large font on `tty1`; OCR of the capture must find it | — |
| **Firmware recorded** | 2 | `doctor` / JetKVM `/version` | Not a verdict; written to the summary ([§9](#9-firmware-channels)) | — |

## 7. Leases, cancellation and cleanup

All phase 1.

- **Exclusive lease.** The job-started hook acquires a [panio
  lease](../leases.md) on the bench target before admitting the job, and
  every actuation the harness performs carries the lease token. If the lease
  is held (you are using the bench by hand), the job is refused with "bench
  busy" and nothing is actuated (N10).
- **Cleanup on every exit**, including cancellation and timeout: a workflow
  step with `if: always()` and, as a backstop, the job-completed hook. Both run
  the same idempotent cleanup:
  1. flush and unmount the CI drive wherever it is mounted;
  2. return the mux to the host side;
  3. (phase 2) restore target power to on and release any held HID keys;
  4. **verify** the result with the same independent checks as the test;
  5. release the lease.

  If verification fails or can't run, cleanup runs `bench-quarantine`. The
  bench then accepts no further jobs until you inspect it (N11).
- **Timeouts.** Each test step has a hard timeout (default 120 s), and the job
  has `timeout-minutes: 30`.

## 8. Flaky and offline devices

The goal is that a red result always means "panio is broken" and never "a cable
wiggled", and that a silent bench never looks green.

- **Preflight.** The first step checks the witness and every device (`panio
  doctor`, reachability). An unavailable device marks its tests `skip`.
- **All-skipped is not green.** A run where every test for a device (or the
  whole run) was skipped, or where the witness was absent, is reported as
  **missing coverage** and fails the workflow (N09).
- **Bounded recovery (phase 2).** On a failure or a non-responsive device:
  power-cycle that device once with its smart plug or relay, wait up to 90 s,
  and rerun that one test once. A pass on the retry is reported as
  **flaky**, never as pass. A second failure is a fail. There are no further
  retries.
- **Escalation (phase 2, through the reporter):** a device skipped on 2
  consecutive nightly runs → issue `lab: <role> offline`; a test flaky 3 times
  in 7 days → issue `lab: <test> flaky`. Phase 1 shows these in the summary
  only.
- **Runner offline or stuck (phase 2).** `watchdog.yml` runs on
  `ubuntu-latest` in the private repo every 12 hours. It uses `gh run list
  --workflow hardware.yml` and fails (which emails you) if there's been no
  **successful, not-all-skipped** run in 36 hours. That covers an offline
  runner, a job stuck in the queue (GitHub cancels queued jobs after 24 h),
  cancelled runs and quarantine (N17). Separately, GitHub removes a
  persistent self-hosted runner that has been offline for 14 days.

## 9. Firmware channels

Panio has to work with firmware that has already shipped *and* get early
warning about firmware that's coming. #415 was the second kind.

| Device | Channel | Policy | A failure means |
|---|---|---|---|
| `jetkvm-stable` | Latest stable release, updated by hand after reading the release notes | Counts toward the advisory release check | A panio regression, or a stable firmware break that needs a fix before the next release |
| `jetkvm-dev` (phase 3) | JetKVM's dev update channel, auto-updating | Non-blocking; a warning in the summary | An upcoming break; the reporter files `firmware-dev` with old and new versions |
| Openterface KVM-Go / Mini-KVM | Pinned at the installed version; updated by hand | Counts toward the advisory release check | A panio regression (firmware is pinned) |

Every run records every device's firmware version. When a test goes from pass
to fail, the summary says whether firmware changed since the last pass, which
tells you whether to look at panio's diff or the device's changelog.

## 10. Phased rollout

Estimates are your hands-on time. "Claude" steps are ones a cloud session can
write as a PR for you to review; the review is included in the estimate. Each
step lists the acceptance gates ([§13](#13-negative-acceptance-gates)) it must
pass before the next step starts.

### Phase 1: one real test, contained (about 4–8 hours, provisional; $0–25)

The range is wide because network isolation depends on what your router can
do.

| # | Step | Who | Time | Gates |
|---|---|---|---|---|
| 1.1 | Create private `panio-hardware-ci`; set read-only workflow permissions (the [first step](#start-here-the-first-step-about-10-minutes-today)). No runner | You | 10 min | — |
| 1.2 | Add `hardware.yml` (dispatch + schedule, **schedule commented out**), the harness, the admission policy file and the source-commit rule to the private repo | Claude drafts; you review and commit | 45 min | — |
| 1.3 | Isolate the bench network: VLAN or travel router; upstream rules; IPv6 off or blocked | You | 1–3 h | N06 |
| 1.4 | On bench1: create `panio-ci` (no sudo), udev rules for just the mux and drive, nftables egress rules | You (Claude drafts the files) | 45 min | N06 |
| 1.5 | Witness: `witness-agent` behind a forced-command SSH key; CI drive labelled `PANIOCI` | You (Claude drafts the agent) | 30 min | — |
| 1.6 | Install the root-owned hook, `bench-precheck`, `bench-quarantine`, the systemd unit and lease/cleanup scripts. With the **listener stopped**, run the refusal tests locally by feeding the hook crafted environments and event files | You (Claude drafts all scripts and a local test runner) | 1 h | N02, N03, N04, N05, N14 |
| 1.7 | Register the runner to the **private repo only** with `--disableupdate` and label `panio-hw`; start it while you watch. Run the negative admission tests from GitHub (wrong workflow, wrong actor on re-run, non-`main` SHA) before any candidate code | You | 45 min | N01, N03, N04 |
| 1.8 | First real dispatch on a recent `main` commit; read the summary. Then the negative outcome tests: run once with the mux helper replaced by a stub that exits 0 without switching, once with the witness unplugged, once with the lease held, and once cancelled mid-switch | You | 1 h | N07, N08, N09, N10, N11, N13 |
| 1.9 | Uncomment the nightly schedule, only once every gate above passed | You | 5 min | all phase 1 gates |

**Stop rule:** any failed security gate, or uncertain hardware state after
cleanup, means stop the listener and quarantine until it's understood.

**Done when:** a manual run on `main` passes, every phase 1 gate has been
exercised and recorded, and the nightly schedule is on.

### Phase 2: JetKVM, power, reporting, disposable execution (about 8–12 hours over two weeks, $110–270)

| # | Step | Who | Time | Gates |
|---|---|---|---|---|
| 2.1 | Buy and cable a JetKVM (stable), relay and smart plug; extend udev rules and the lab config | You | 1 h | — |
| 2.2 | `witness-agent evdev-capture`; keyboard, mouse and scroll tests | Claude | 45 min review, 20 min install | — |
| 2.3 | Power test with two independent signals | Claude | 20 min review | — |
| 2.4 | Bounded recovery, flaky verdict, firmware versions in the summary | Claude | 30 min review | — |
| 2.5 | Disposable execution: just-in-time registration in a VM/container (or overlay root) rebuilt per job | You (Claude drafts) | 2–4 h | N14, N18 |
| 2.6 | GitHub App (Issues: write, public panio only); `reporter.yml` with schema validation, templates, dedupe and rate limit | You create the App (20 min); Claude writes the workflow | 1 h | N12, N15, N16 |
| 2.7 | `watchdog.yml` | Claude | 15 min review | N17 |
| 2.8 | Mini-KVM "refuses cleanly" test while #417 is open | Claude | 15 min review | — |

### Phase 3: firmware channels, video, advisory release check (about 4–5 hours, $70–110)

| # | Step | Who | Time |
|---|---|---|---|
| 3.1 | Second JetKVM on the dev channel | You | 45 min |
| 3.2 | Device matrix with per-channel policy; `firmware-dev` issues via the reporter | Claude | 30 min review |
| 3.3 | Video + OCR test | Claude | 30 min review, 30 min tuning |
| 3.4 | Advisory release check: `RELEASE-TRAIN.md` gains "latest nightly on the release commit is green on stable devices". Not a gate in `release.yml` | Claude | 20 min review |
| 3.5 | Once #428 merges, confirm the dev-channel JetKVM exercises the new binary `hidrpc` scroll path | You | 15 min |

### Later, when needed

- Mini-KVM round trip, once a helper implements the mux (#417).
- Pre-merge (PR branch) and tag-triggered runs, after their admission is
  designed ([§11](#11-decisions-and-open-questions)).
- Netboot (`netbootd`) and serial console (`serialcap`) tests.

## 11. Decisions and open questions

| # | Question | Proposed answer |
|---|---|---|
| D1 | Run hardware tests on every PR? | **No.** |
| D2 | Where does the runner register? | **The private `panio-hardware-ci` repo only.** Public panio has no self-hosted runner. |
| D3 | Which code runs in phase 1? | Only commits reachable from public `main`. |
| D4 | Pre-merge testing (e.g. a PR branch like #428's)? | **Deferred.** Needs an admission design for unreviewed code: at minimum a disposable environment (phase 2), a per-run approval naming the exact SHA, and acceptance that hardware can be driven maliciously during that run. |
| D5 | Tag-triggered runs? | **Deferred** to after phase 3, as "nightly also tests the newest `v*` tag" in the private repo (§5.2). |
| D6 | Hard release gate in `release.yml`? | **No.** Advisory first; revisit after a month of stable nightly runs. |
| D7 | Pi or x86 for bench1? | Either. A Pi 5 is cheaper and quieter; an N100 mini PC builds Rust about 2–3x faster and makes phase 2's VM-per-job easier. |
| Q1 | Can your existing bench double as the CI bench? | Only if it can be moved onto the isolated segment and dedicated to `panio-ci`. Otherwise use a second box. |
| Q2 | Which mux is the phase 1 device: KVM-Go or `usbtoggle`? | Whichever is already cabled. |
| Q3 | Separate the harness host from the candidate host? | Not planned. It's the only way to defend verdicts against a hostile candidate (§5.7). Revisit if pre-merge testing (D4) is wanted. |

## 12. Review findings and where they are addressed

| Finding | Summary | Addressed in |
|---|---|---|
| SEC-01 | Hook installed after registration | §10 steps 1.6 → 1.7: protected hook and local refusal tests before registration; listener won't start without it (§5.4) |
| SEC-02 | Network containment deferred | §5.5 and step 1.3, in phase 1 before any job; IPv4 and IPv6; "no isolation, no runner" |
| SEC-03 | Approval, labels and lint aren't boundaries | §2, §5.2: runner moved to a private repo; public panio has none |
| SEC-04 | Hook not immutable; admission too loose | §5.3 (full checks, deny by default, re-run actor, approved SHA), §5.4 (ownership, `--disableupdate`, systemd environment, hash-pinned precheck), §5.7 (candidate build is untrusted) |
| SEC-05 | Ephemeral ≠ wiped | §5.6: persistent phase 1 limited to reviewed code with cleanup and quarantine; real per-job reset in phase 2 |
| SEC-06 | Read-only token vs. automatic issues | §5.8: manual in phase 1; hosted reporter with an Issues-only GitHub App in phase 2; bench never holds it |
| SEC-07 | Masking isn't confidentiality | §5.8: raw logs private; only sanitized structured summaries published |
| SEC-08 | Leases and cancellation cleanup late | §7, in phase 1 |
| SEC-09 | Harness in candidate checkout; shared host | §5.7: harness and verdicts in the private repo, restricted witness SSH, residual risk documented |
| SEC-10 | Trigger and hook mismatch; tag credentials | §5.2: no `push` trigger; deploy-key note; tags deferred |

## 13. Negative acceptance gates

Each gate is a deliberate attempt to make the wrong thing happen, with the
required result. "Step" is where in [§10](#10-phased-rollout) it is first
exercised. All are proposed, not yet executed.

| Gate | Attempt | Required result | Step |
|---|---|---|---|
| N01 | A public fork workflow asks for `panio-hw` | No runner assigned (public panio has none; the job sits queued) | 1.7 |
| N02 | Hook missing or altered | Service refuses to start | 1.6 |
| N03 | Wrong workflow, event or ref | Rejected before any workflow step | 1.6, 1.7 |
| N04 | Candidate SHA not on public `main`, or a re-run by another actor | No candidate code executes | 1.6, 1.7 |
| N05 | A job tries to modify the hook, policy, unit or runner files | Denied by file ownership | 1.6 |
| N06 | Connect to the trusted LAN over IPv4 or IPv6 | Blocked | 1.3, 1.4 |
| N07 | panio exits 0 without the USB moving | Product failure | 1.8 |
| N08 | Stale nonce or incomplete round trip | Never a pass | 1.8 |
| N09 | Witness absent, or everything skipped | Missing coverage; the run fails | 1.8 |
| N10 | Lease already held | No device actuation | 1.8 |
| N11 | Cancel during actuation | Verified cleanup, or quarantine | 1.8 |
| N12 | Bench token tries to change a public panio issue | Denied (token is private-repo, read-only) | 1.8, 2.6 |
| N13 | Sensitive diagnostic data in output | Stays in private logs; not in the published summary | 1.8 |
| N14 | Unexpected persistent state between jobs | Next execution blocked (quarantine) | 1.6, 2.5 |
| N15 | Malformed, oversized or replayed reporter input | Rejected or deduplicated; no command execution | 2.6 |
| N16 | Reporter credential tries contents, workflow or admin changes | Denied by App permissions | 2.6 |
| N17 | Runner offline, queued, cancelled, or an all-skipped run | Watchdog reports missing successful coverage | 2.7 |
| N18 | State planted in the disposable environment | Absent from the next job; otherwise quarantine | 2.5 |

## 14. References

- [Adding self-hosted runners](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/add-runners)
- [Self-hosted runners reference](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)
- [Managing GitHub Actions settings for a repository](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/managing-github-actions-settings-for-a-repository)
- [Running scripts before or after a job](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/run-scripts)
- [Variables reference](https://docs.github.com/en/actions/reference/workflows-and-actions/variables)
- [`GITHUB_TOKEN`](https://docs.github.com/en/actions/concepts/security/github_token)
- [Create an issue (REST)](https://docs.github.com/en/rest/issues/issues#create-an-issue)
