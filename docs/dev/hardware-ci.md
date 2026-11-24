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

> **Status:** design, not implemented. Nothing in this document runs yet.
>
> **Scope:** panio's *own* CI, testing panio against real KVMs, muxes and relays.
> This is different from [ci-integration/](ci-integration/design.md), which is about
> letting other CI systems (LAVA, botanist) drive boards through panio.

## Start here: the first step (about 5 minutes, today)

Do this before anything else, because it has to be in place before a runner exists:

1. Open the repo's **Settings → Actions → General**.
2. Under **Approval for running fork pull request workflows from contributors**,
   choose **Require approval for all external contributors**.
3. Save.

That is the whole first step. The rest of phase 1 is in [§9](#9-phased-rollout).

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
and private repositories. GitHub does not bill minutes for jobs on your own
machine. Two things the free personal plan does *not* give you, and why they
don't matter here:

- **Runner groups** (restricting which repos may use a runner) are an
  organization feature. A runner registered to a single repository is only
  usable by that repository anyway.
- **Required reviewers on environments** for private repos need a paid plan.
  This repo is public, so environments with branch rules are available, but
  this design doesn't depend on them (see [§5](#5-triggers-and-security)).

The real constraint isn't billing, it is that **the repo is public**. GitHub's
own guidance is that self-hosted runners on public repos are dangerous, because
a fork pull request can run arbitrary code on them. [§5](#5-triggers-and-security)
is about making that impossible rather than merely unlikely.

## 3. Shape of the system

```
                 GitHub (schedule / workflow_dispatch / push to main or v* tag)
                                     │
                                     ▼
┌──────────────────────────── lab LAN (isolated) ────────────────────────────┐
│                                                                            │
│  bench1 (runner + control host)                                            │
│   ├─ actions-runner (user panio-ci, label panio-hw)                        │
│   ├─ job-started hook  ← refuses any job that isn't ours (§5.3)            │
│   ├─ panio + helpers, built from the commit under test                     │
│   └─ lab config outside the repo (~panio-ci/lab/)                          │
│        │USB            │USB/LAN           │USB             │LAN            │
│        ▼               ▼                  ▼                ▼               │
│   USB mux (KVM-Go,  JetKVM            relay board     smart plug          │
│   Mini-KVM, or      (HID, video)      (target power)  (KVM power,         │
│   usbtoggle)            │                  │           for recovery)       │
│        │ switched USB   │ HID + HDMI       │ power                        │
│        ▼                ▼                  ▼                              │
│   target-machine ("witness": a small Linux box)                            │
│        ▲                                                                   │
│        └──── SSH over wired LAN: the independent check channel ────────────│
└────────────────────────────────────────────────────────────────────────────┘
```

The key idea is the **witness**: the target is a Linux machine that bench1 can
reach over SSH on a plain wired link that **does not pass through any device
under test**. Every test acts through panio, then asks the witness what really
happened.

## 4. Hardware list and rough costs

Prices are rough US street prices as of November 2026. Check before buying.
"Have it?" is for you to fill in; phase 1 is designed to need as little new
hardware as possible.

### Phase 1 (minimum)

| Item | Role | Rough cost | Have it? |
|---|---|---|---|
| Runner box: Raspberry Pi 5 (4–8 GB) with PSU, case, 64 GB+ storage | Runs the Actions runner and panio | $100–130 | |
| *or* a used/low-end x86 mini PC (Intel N100 class) | Same; faster Rust builds | $150–200 | |
| Witness target: any spare Pi 4/5 or old PC running Linux | Reports outcomes over SSH | $0–80 | |
| One USB mux panio already supports: Openterface **KVM-Go** (hardware-verified) or a USB sharing switch + DSD relay (`usbtoggle`) | Device under test for the USB round trip | $0 if owned; $30–120 | |
| Small USB flash drive, dedicated to CI | Payload that moves between host and target | $8 | |
| Ethernet cables, a 5-port switch | Lab LAN and witness link | $0–25 | |

**Phase 1 total: $0 if you reuse what you have; about $150–300 buying new.**

### Phase 2 additions

| Item | Role | Rough cost |
|---|---|---|
| JetKVM (on **stable** firmware) | HID and video tests; would have caught #415 | $70–90 |
| Openterface Mini-KVM | Covers #417 once a helper exists; until then, a test that proves `usb` refuses it | $100–140 |
| DSD Tech SH-UR04A 4-channel USB relay | Target power on/off test | $15–20 |
| Wi-Fi/LAN smart plug (Shelly or Kasa, both panio-supported) | Power-cycle a wedged KVM during recovery | $15–25 |

### Phase 3 additions

| Item | Role | Rough cost |
|---|---|---|
| A second JetKVM, on the **dev** firmware channel | Early warning for upstream firmware changes ([§8](#8-firmware-channels)) | $70–90 |
| Cheap USB HDMI capture dongle (optional) | Second opinion on video tests | $15 |

## 5. Triggers and security

### 5.1 Threat

On a public repo, any GitHub user can open a fork PR that edits a workflow to
say `runs-on: [self-hosted, panio-hw]`. For a `pull_request` event, GitHub runs
the workflow file *from the PR*. Without controls, that is arbitrary code on a
machine sitting on your LAN with USB access to KVMs.

### 5.2 Triggers the hardware workflow accepts

`.github/workflows/hardware.yml` (the only workflow that may name the runner
label) uses exactly these triggers, and never `pull_request` or
`pull_request_target`:

| Trigger | When | Who can cause it |
|---|---|---|
| `schedule` | Nightly, for example 03:17 Pacific | Runs the workflow from `main` only |
| `workflow_dispatch` | On demand, on `main` or any branch **in this repo** (for example a PR branch like #428's, before merging) | Only users with write access; forks cannot dispatch here |
| `push` to tags `v*` | When a release tag is created by `cut-release.yml` | Write access only |

A fork PR can never trigger any of these. Testing a PR on hardware is a
deliberate act: dispatch the workflow on that PR's branch.

One gotcha: GitHub only offers **Run workflow** for a workflow that already
exists on the default branch, so `hardware.yml` has to merge to `main` before
it can be dispatched on a PR branch.

### 5.3 Layers of defense

Each layer alone would stop the common case. Together, a fork PR has to defeat
all of them, and the last two live where a PR cannot reach.

1. **Fork PR approval setting** (the first step above). Fork PR workflows
   don't run until you click approve. Rule for yourself: never approve a fork
   run whose diff touches `.github/`.
2. **Unique runner label.** The runner carries the label `panio-hw`. Jobs
   asking for `ubuntu-latest` can never land on it.
3. **A lint in regular CI** (`scripts/ci-hw-guard.sh`, run by an always-run
   job in `ci.yml`). It fails if any workflow other than `hardware.yml`
   mentions `self-hosted` or `panio-hw`, or if `hardware.yml` has any trigger
   besides the three above. This catches *our own* mistakes, such as a future
   edit adding `pull_request`.
4. **Job-started hook on the runner box** (the real backstop). The Actions
   runner runs the script named by `ACTIONS_RUNNER_HOOK_JOB_STARTED` before
   every job; a non-zero exit fails the job before any workflow step runs. The
   script lives on bench1, outside the repo, so no PR can change it. It
   refuses the job unless **all** of these hold:
   - `GITHUB_REPOSITORY` is `curtisgalloway/panio`
   - `GITHUB_EVENT_NAME` is `schedule`, `workflow_dispatch`, or `push`
   - for `push`: `GITHUB_REF` is `refs/heads/main` or `refs/tags/v*`
   - for `workflow_dispatch`: `GITHUB_TRIGGERING_ACTOR` is `curtisgalloway`
   - `GITHUB_WORKFLOW_REF` names `.github/workflows/hardware.yml`

   It logs each refusal to a local file so a probe is visible.
5. **Least privilege on the box.**
   - The runner runs as an unprivileged `panio-ci` user with no sudo. USB
     access comes from udev rules and the `dialout`/`plugdev` groups, not root.
   - `hardware.yml` sets `permissions: contents: read`. No repository secrets
     are used; device credentials (the JetKVM password) live in a file on
     bench1 readable only by `panio-ci`.
   - bench1 sits on its own LAN segment or VLAN. Its firewall allows outbound
     HTTPS (GitHub) and the lab devices, and nothing else on the home network.
   - The runner is configured `--ephemeral` and re-registered by a small
     systemd service after each job, so no workspace state survives between
     jobs. (Phase 2. In phase 1, a plain persistent runner is fine.)

### 5.4 Public logs

Workflow logs and artifacts on a public repo are public. The
[Never commit private infrastructure](https://github.com/curtisgalloway/panio/blob/main/AGENTS.md#never-commit-private-infrastructure)
rule applies to everything a hardware job prints:

- The lab's real device paths, serials, addresses and hostnames stay in the
  config under `~panio-ci/lab/` and are referred to by role (`jetkvm-stable`,
  `mux`, `witness`).
- Test scripts print roles, never raw paths, and register any value read
  from config with `::add-mask::` before using it.
- Screenshots are only uploaded when they show the witness's test pattern,
  which contains nothing but a nonce.

## 6. How each test verifies the outcome

Rules every test follows:

- **Act through panio, check through the witness.** Never use panio to check
  panio's own work.
- **Use a fresh nonce per run** (a random 16-hex string), so a stale result
  from an earlier run can't pass.
- **Check both directions** where there are two (attach-target *and*
  attach-host).
- **One test, one script** under `scripts/hw/`, exit 0 for pass, 1 for product
  failure, 77 for "device not available" (the automake skip convention, see
  [§7](#7-flaky-and-offline-devices)).

| Test | Phase | Action through panio | Independent check | Would it catch |
|---|---|---|---|---|
| **USB round trip** | 1 | Write `nonce` to a file on the CI drive while it's host-side; `usb attach-target` | Witness: the drive appears in `lsblk` by its label within 15 s, and the file holds `nonce`. bench1: the drive is **gone** from its own `lsblk`. Then the witness writes `nonce2`; `usb attach-host`; bench1 reads `nonce2` and the witness no longer sees the drive | #417 |
| **USB state readback** | 1 | `usb state` after each switch | Must agree with what the witness saw; a disagreement is a failure even if both commands exited 0 | #417 |
| **Unsupported device refuses** | 2 | `usb attach-target` on the Mini-KVM, while no helper supports it | Must exit non-zero with an "unsupported" error; exit 0 fails the test. Replaced by the round trip once a helper lands | #417 |
| **Keyboard** | 2 | `hid send type <nonce>` via JetKVM | Witness runs a small evdev logger (python-evdev, as a systemd unit) on the KVM's HID gadget and reports the key sequence; it must spell `nonce` | — |
| **Mouse move** | 2 | `hid send move` and `hid send moveabs` | Witness evdev log shows `REL_X`/`REL_Y` (or `ABS_*`) events with the right signs | — |
| **Scroll** | 2 | `hid send scroll -200`, `scroll 3`, `scroll 0` | Witness evdev log shows `REL_WHEEL` events with the right sign and the right total, and nothing for 0 | #415 |
| **Power** | 2 | Relay off, then on | Witness stops answering ping within 10 s of off; SSH answers within a set boot budget (for example 90 s) after on | — |
| **Video** | 3 | `video` capture plus OCR | Witness shows `nonce` in a large font on its console (`setfont` + `tty1`); OCR of the capture must find it | — |
| **Firmware version recorded** | 2 | `panio … doctor` / JetKVM `/version` | Not a pass/fail check; written to the run summary ([§8](#8-firmware-channels)) | — |

Each job takes a [panio lease](../leases.md) on the target first, so a nightly
run can't collide with you using the bench by hand, and a hand session can't
corrupt a run. If the lease is held, the job reports "bench busy" (exit 77)
instead of failing.

## 7. Flaky and offline devices

The goal is that a red result always means "panio is broken" and never "a cable
wiggled".

**Three outcomes, not two.** Each test script returns pass (0), fail (1) or
skip (77). The job summary shows a table per device, with skips listed in
their own column so they can't hide.

**Preflight.** The first step pings every device and runs `panio doctor`. A
device that is offline marks its tests skip, not fail, and the run continues
with what's left.

**One recovery, one retry.** If a test fails or a device doesn't respond:

1. Power-cycle that device with its smart plug (KVMs) or relay (target), and
   wait for it to come back (up to 90 s).
2. Rerun that one test once.
3. If it now passes, it's reported as **flaky**, not pass. If it fails again,
   it's a fail.

**Escalation, so problems don't rot.**

- A device skipped on **2 consecutive nightly runs** → the workflow opens (or
  updates) an issue titled `lab: <role> offline`, labelled `lab`.
- A test reported flaky **3 times in 7 days** → an issue `lab: <test> flaky`.
- Neither blocks anything; they show up in the weekly triage.

**Runner offline.** If bench1 is down, the nightly job just sits queued (and
GitHub cancels it after 24 h). A tiny watchdog job on `ubuntu-latest`, on its
own schedule, runs `gh run list --workflow hardware.yml` and fails if there's
been no completed hardware run in 36 h, which emails you. Separately, GitHub
removes a self-hosted runner that has been offline for 14 days; if that
happens, re-register it.

**Timeouts.** Every test has a hard timeout (default 120 s) and the job has
`timeout-minutes: 30`, so a wedged USB device can't hold the runner forever.

## 8. Firmware channels

Panio has to work with firmware that's already shipped *and* get early warning
about firmware that's coming. #415 was exactly that: a dev-channel change
that hadn't shipped yet.

| Device | Channel | Policy | Failure means |
|---|---|---|---|
| `jetkvm-stable` | Latest stable release, updated by hand after reading the release notes | Blocking for releases | A real regression in panio, or a stable firmware break that needs a fix before the next release |
| `jetkvm-dev` (phase 3) | JetKVM's dev update channel, auto-updating | Non-blocking: shows as a warning in the summary | An upcoming break. The workflow opens an issue labelled `firmware-dev`, with the old and new firmware versions |
| Openterface KVM-Go / Mini-KVM | Pinned at whatever version is installed; updated by hand | Blocking | Panio regression (firmware is pinned) |

Every run writes the firmware version of every device into its summary. When a
test goes from pass to fail, the summary shows whether firmware changed since
the last pass, which tells you immediately whether to look at panio's diff or
the device's changelog.

Until there's a second JetKVM, the one unit stays on stable. Upstream firmware
changes keep being caught the way #415 was, by reading upstream changes during
the weekly triage.

## 9. Phased rollout

Estimates are hands-on time for you. "Claude" steps are ones a cloud session
can write as a PR for you to review (review time is included in the estimate).

### Phase 1: one real test, on hardware you have (about 3 hours, $0–25)

Goal: the USB round trip test runs nightly and on demand, and the runner can't
be used by a fork.

| # | Step | Who | Time |
|---|---|---|---|
| 1.1 | Set fork PR approval to **all external contributors** (the [first step](#start-here-the-first-step-about-5-minutes-today)) | You | 5 min |
| 1.2 | Pick the runner box (an existing Linux machine or Pi is fine) and the witness (any spare Linux box), and cable the USB mux and witness LAN link | You | 30 min |
| 1.3 | On the runner box: create user `panio-ci`, install the Actions runner (**Settings → Actions → Runners → New self-hosted runner**), give it the label `panio-hw`, install it as a service | You | 20 min |
| 1.4 | Install the job-started hook from [§5.3](#53-layers-of-defense) and test that it refuses: dispatch a throwaway workflow from a different workflow file and confirm the hook rejects it | Claude writes the script; you install it | 25 min |
| 1.5 | Witness setup: SSH key from `panio-ci@bench1`, a CI-only flash drive labelled `PANIOCI` | You | 20 min |
| 1.6 | `scripts/hw/usb-roundtrip.sh` and `scripts/hw/lib.sh` (nonce, witness SSH, skip/fail codes) | Claude | 20 min review |
| 1.7 | `.github/workflows/hardware.yml` (dispatch + nightly; builds `cli` and the mux helper from the commit under test) and `scripts/ci-hw-guard.sh` with its `ci.yml` job | Claude | 20 min review |
| 1.8 | Merge, then **Run workflow** by hand once and read the summary | You | 15 min |
| 1.9 | Deliberately break it: unplug the mux and confirm the run reports skip, not pass | You | 10 min |

**Done when:** a manual run passes, an unplugged mux shows as skip, and a test
dispatch from a non-hardware workflow is refused by the hook.

### Phase 2: JetKVM, power, and reliability (about 6–8 hours over a week, $100–250)

| # | Step | Who | Time |
|---|---|---|---|
| 2.1 | Buy and cable a JetKVM (stable firmware), relay, and smart plug | You | 1 h |
| 2.2 | Witness evdev logger as a systemd unit, plus its install script | Claude | 30 min review, 20 min install |
| 2.3 | Keyboard, mouse and scroll tests | Claude | 30 min review |
| 2.4 | Power test | Claude | 15 min review |
| 2.5 | Preflight, recovery-and-retry, flaky reporting, firmware versions in the summary | Claude | 45 min review |
| 2.6 | Auto-filed `lab:` issues and the hosted watchdog job | Claude | 30 min review |
| 2.7 | Lease around each run | Claude | 15 min review |
| 2.8 | Ephemeral runner + re-register service; firewall/VLAN for bench1 | You (Claude drafts the unit file and rules) | 1.5 h |
| 2.9 | Mini-KVM "refuses cleanly" test (while #417 is open) | Claude | 15 min review |
| 2.10 | Run PR #428's branch on the real JetKVM via dispatch (stable firmware exercises the old JSON-RPC path, so this proves no regression; phase 3.5 tests the new path) | You | 15 min |

**Done when:** a week of nightly runs is green or every red is a real bug, and
#428 has been run on a real JetKVM.

### Phase 3: firmware channels and release check (about 4–5 hours, $70–110)

| # | Step | Who | Time |
|---|---|---|---|
| 3.1 | Second JetKVM on the dev channel | You | 45 min |
| 3.2 | Device matrix in `hardware.yml` with blocking/non-blocking per channel; `firmware-dev` issue filing | Claude | 30 min review |
| 3.3 | Run on `v*` tag push; add "hardware run green on this SHA" to the release checklist in `RELEASE-TRAIN.md` (advisory, not a hard gate in `release.yml`) | Claude | 20 min review |
| 3.4 | Video + OCR test | Claude | 30 min review, 30 min tuning |
| 3.5 | Re-test #428 on the dev-channel JetKVM, which exercises the new binary `hidrpc` path for real | You | 15 min |

### Later, when needed

- Mini-KVM round trip, once a helper implements the mux (#417).
- Netboot (`netbootd`) test: witness netboots a known image, reports its kernel version.
- Serial console test through `serialcap`.

## 10. Decisions and open questions

| # | Question | Proposed answer |
|---|---|---|
| D1 | Run hardware tests on every PR? | **No.** Nightly, on release tags, and on demand. A PR that touches a hardware helper can be dispatched by hand. |
| D2 | Make the hardware run a hard release gate in `release.yml`? | **Not yet.** Advisory first (phase 3.3); revisit after a month of stable nightly runs. A flaky bench shouldn't block a release. |
| D3 | Pi or x86 for the runner? | Either. A Pi 5 is cheaper and quieter; an N100 mini PC builds Rust roughly 2–3x faster. Phase 1 uses whatever is already on hand. |
| D4 | Build panio on the runner, or download the CI artifact? | **Build on the runner** in phase 1 (simplest, about 5–10 min on a Pi 5 with a warm cache). Revisit if nightly runs get slow. |
| Q1 | Can the existing bench double as the runner, or does CI need its own? | Shared is fine with leases in place (§6). A dedicated bench avoids "bench busy" skips. Your call. |
| Q2 | Which mux is the phase 1 device: KVM-Go or `usbtoggle`? | Whichever is already cabled. |
