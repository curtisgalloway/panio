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

> **Status:** design, revision 3. Nothing in this document runs yet, and nothing
> in it authorizes deploying a runner. The acceptance gates are proposed and
> haven't been executed. The real network layout, hook behavior, panio lease
> semantics and release credentials still need to be checked on the bench.
>
> **Scope:** panio's *own* CI, testing panio against real KVMs, muxes and relays.
> This is different from [ci-integration/](ci-integration/design.md), which is about
> letting other CI systems (LAVA, botanist) drive boards through panio.
>
> **History.** Revision 2 applied the first security review on PR #429
> (SEC-01 to SEC-10) and moved the runner to a private control repository.
> Revision 3 applies the follow-up review (R2-1 to R2-7). Its main changes:
> an explicit approval manifest, crash-safe cleanup, a separate execution user,
> a separate reporting repository, and stated egress and update policies.
> [§12](#12-review-findings-and-where-they-are-addressed) maps every finding to
> the section that addresses it.

## Trust model in one paragraph

Phase 1 is **trusted-code execution with containment**, not isolation from
hostile code. Only commits you have explicitly approved run on the bench, and
the bench is contained so that a mistake or a compromised dependency can't
reach your home network or any valuable secret. Candidate code runs as a
separate user from the runner and the harness, but it still shares the kernel,
the hardware and the internet connection with them. Treat everything that
follows with that model in mind.

## Start here: the first step

The private control repository
[`curtisgalloway/panio-hardware-ci`](https://github.com/curtisgalloway/panio-hardware-ci)
exists (created November 25, 2026), with read-only workflow permissions. The
next step is 1.2 in [§10](#10-phased-rollout): drafting the bench workflow,
harness and approval manifest in that repository.

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
storage in *private* repositories count against the Free plan's monthly
allowance (2,000 minutes and 500 MB at the time of writing). The small jobs in
this design use a few minutes a day.

The Free plan doesn't give a personal account three things, none of which the
design depends on:

- **Runner groups** (an organization feature). A runner registered to one
  repository is only offered that repository's jobs.
- **Environments with protection rules** on private repos.
- **Branch protection and rulesets** on private repos. Instead, write access is
  limited by having no collaborators at all
  ([§5.2](#52-where-authority-lives)).

The constraint that shapes the design isn't billing. A self-hosted runner
attached to a **public** repository can be reached by workflows from fork pull
requests, and fork approval, runner labels and workflow lints are not
authorization boundaries. So **public panio gets no self-hosted runner at all.**

## 3. Architecture

```
 public: curtisgalloway/panio          private: panio-hardware-ci (bench control)
 ┌────────────────────────┐            ┌────────────────────────────────────────┐
 │ source code only       │ approved   │ hardware.yml (schedule/dispatch)       │
 │ no self-hosted runner  │ SHA only ─▶│ harness + verdict logic                │
 │ no hardware workflow   │ (read-only │ approved/candidates.txt  (manifest)    │
 └────────────────────────┘  clone)    │ writer: Curtis only, no collaborators  │
            ▲                          └──────────────┬─────────────────────────┘
            │ Issues: write only                      │ jobs for label panio-hw
 ┌──────────┴──────────────────┐                      │
 │ private: panio-hardware-    │◀── Actions: read ────┤ (phase 2: reads results)
 │ report (hosted runners only)│                      │
 │ reporter.yml, App key       │                      │
 └─────────────────────────────┘                      │
┌──────────────────────── isolated bench network ─────▼───────────────────────────┐
│ bench1                                                                          │
│  ├─ root: systemd units, precheck, admission records, dirty marker, verifier    │
│  ├─ panio-ci: runner listener + harness (holds runner credentials, witness key) │
│  ├─ panio-job: candidate build and every candidate panio process, each in a     │
│  │   per-run systemd slice; owns the USB device permissions                     │
│  │    │USB            │USB/LAN          │USB             │LAN                   │
│  │  USB mux        JetKVM (ph. 2)    relay board     metering smart plug (ph. 2)│
│  │    │switched USB   │HID + HDMI       │power ──────────▶ (in series)           │
│  │    ▼               ▼                 ▼                                       │
│  │  target-machine ("witness"): fixed SSH verbs only                            │
│  │    ▲                                                                         │
│  └────┴── wired LAN link, not through any device under test                     │
│  Blocked by upstream rules: home LAN, router and device management (IPv4, IPv6) │
│  Allowed out: HTTPS to the internet (accepted residual risk, §5.5)              │
└─────────────────────────────────────────────────────────────────────────────────┘
```

Unchanged from earlier revisions: the independent **witness**, outcome checks
in **both directions** with a **fresh nonce**, and a **low-cost first phase**.

## 4. Hardware list and rough costs

Prices are rough US street prices as of November 2026; check before buying.

### Phase 1 (minimum)

| Item | Role | Rough cost |
|---|---|---|
| Runner box: Raspberry Pi 5 (4–8 GB) with PSU, case, 64 GB+ storage, *or* a low-end x86 mini PC (Intel N100 class) | Runs the listener, harness and candidate build. Dedicated to the bench | $0 if owned; $100–200 |
| Witness target: any spare Pi 4/5 or old PC running Linux | Reports outcomes over restricted SSH | $0–80 |
| One USB mux panio already supports: Openterface **KVM-Go** (hardware-verified) or a USB sharing switch with a DSD relay (`usbtoggle`) | Device under test for the USB round trip | $0 if owned |
| Small USB flash drive, CI only | Payload that moves between host and witness | $8 |
| Isolated network segment: a VLAN with firewall rules on gear you have, **or** a small travel router with its own firewall, plus explicit deny rules on the upstream router | Network containment ([§5.5](#55-network-containment-and-egress)) | $0 with capable gear; $30–40 otherwise |

**Phase 1 total: $0–25 if you already have the box, witness, mux and a way to
isolate the bench; add $30–40 for a travel router if not.** Without isolation,
don't register a runner. Run the harness by hand instead.

### Phase 2 additions

| Item | Role | Rough cost |
|---|---|---|
| JetKVM (on **stable** firmware) | HID and video tests; would have caught #415 | $70–90 |
| Openterface Mini-KVM | Covers #417 once a helper exists; until then, a test that proves `usb` refuses it | $100–140 |
| DSD Tech SH-UR04A 4-channel USB relay | Switches the target's power | $15–20 |
| **Metering** smart plug (a Shelly Plug model that reports watts; panio-supported) in series with the target's supply | Independent observation of electrical power ([§6](#6-how-each-test-verifies-the-outcome)) | $20–30 |
| Second (non-metering) smart plug, Shelly or Kasa | Power-cycle a wedged KVM during recovery | $15–25 |
| Second boot medium for bench1 (or a second SD card) | Disposable, re-imaged execution environment ([§5.6](#56-execution-users-scopes-and-state)) | $10–20 |

### Phase 3 additions

| Item | Role | Rough cost |
|---|---|---|
| A second JetKVM, on the **dev** firmware channel | Early warning for upstream firmware changes ([§9](#9-firmware-channels)) | $70–90 |
| Cheap USB HDMI capture dongle (optional) | Second opinion on video tests | $15 |

## 5. Security design

### 5.1 Threats

1. **Untrusted workflows reaching the bench.** Answered by the private
   repository (§5.2).
2. **Code that was never really reviewed.** Being an ancestor of `main` isn't
   the same as having been reviewed. A commit can arrive by direct push, by an
   admin bypass, or as an intermediate commit in a merge (§5.3).
3. **Bypassing admission on the box.** Anything the runner user can write, a
   job can change (§5.4).
4. **Reaching the home network, or exfiltrating data** (§5.5).
5. **Candidate code interfering with the runner or the harness** (§5.6, §5.7).
6. **A crash leaving hardware in an unknown state** that a person or the next
   job then trusts (§7).
7. **Leaking lab details or misusing the reporting credential** (§5.8).

### 5.2 Where authority lives

| | Public `panio` | Private `panio-hardware-ci` | Private `panio-hardware-report` (phase 2) |
|---|---|---|---|
| Self-hosted runner | **None** | Registered here only, label `panio-hw` | **None**; hosted runners only |
| Workflows | No hardware workflows | `hardware.yml` (bench), `watchdog.yml` (hosted) | `reporter.yml` (hosted) |
| Triggers | n/a | `schedule`, `workflow_dispatch` only | `schedule` only |
| Who can write | Unchanged | **You only**: no collaborators, no deploy keys, no Apps with Contents write | **You only**, same rule |
| Credentials it holds | Unchanged | None beyond the default `GITHUB_TOKEN` (`contents: read`) | GitHub App key (Issues: write on public panio), and a read-only token for the bench repo's run results |

**Who is trusted (stated outright).** Everyone who can write to
`panio-hardware-ci` is trusted to define what runs on the bench. Today that is
only you. The Free plan can't protect a private repository's branches, so the
control is that there are no other writers. Adding a collaborator, deploy key
or App with write access to either private repository is a security change
that needs this document revisited first. The hook additionally requires that
the workflow commit (`GITHUB_SHA`) is the current head of the private `main`,
so an old or side-branch workflow can't be replayed.

**Release tags.** The bench workflow has no `push` trigger. Tags in public
panio are created by `cut-release.yml` with the `RELEASE_TAG_DEPLOY_KEY`
deploy key. A deploy-key push *does* trigger workflows in public panio (a push
made with the ordinary `GITHUB_TOKEN` doesn't), but tags can't trigger anything
in the private repo. Testing a tag means adding the tag's commit to the
approval manifest (§5.3), like any other commit.

### 5.3 Approval manifest and admission

**What counts as approved.** A file in the private repo,
`approved/candidates.txt`, lists every public panio commit that may run on the
bench. Each entry is one line:

```
<40-hex public panio SHA>  <YYYY-MM-DD>  <who approved>  <why, e.g. "main after #428">
```

Only an entry in this file approves a commit. Being reachable from public
`main` is checked too, but only as a sanity check, never as approval. Adding a
line is a reviewed act: you (or a cloud session, as a PR to the private repo
that you merge) add the exact SHA after deciding that it's what you mean to
run. It may be a merge commit on `main`, a tag's commit, or anything else you
choose.

The nightly run doesn't follow moving `main`. It tests the **most recently
added** manifest entry. If you don't add entries, the nightly keeps retesting
the same commit. That still catches bench and firmware drift, but it won't
cover new code until you approve it. This is a deliberate trade:
convenience is given up so that review means something.

**Resolve once, bind everywhere.** The job-started hook resolves the candidate
SHA exactly once and writes a root-owned **admission record**,
`/run/bench/admission/<run-id>-<attempt>.json` (mode 0640, root:panio-ci). The
record holds the run id, attempt, workflow commit, candidate SHA, lease token
and admission time. After that:

- the harness checks out the candidate **by that SHA** (`git fetch origin
  <sha>` then `git checkout --detach <sha>`) and verifies `git rev-parse HEAD`
  equals it before building;
- `bench-exec` (§5.6) refuses to start any candidate process whose run id
  doesn't match a live admission record;
- the run summary is stamped with the candidate SHA from the record, not from
  anything the job computed.

Nothing resolves `main` again after admission, so `main` moving mid-run can't
change what is tested (N20).

**Admission checks.** The hook denies by default; any missing or malformed
value is a refusal. It admits a job only if all of these hold:

| Check | Required value |
|---|---|
| `GITHUB_REPOSITORY` | `curtisgalloway/panio-hardware-ci` |
| `GITHUB_WORKFLOW_REF` | `curtisgalloway/panio-hardware-ci/.github/workflows/hardware.yml@refs/heads/main` (exact) |
| `GITHUB_EVENT_NAME` | `schedule` or `workflow_dispatch` |
| `GITHUB_REF` | `refs/heads/main` |
| `GITHUB_SHA` | equals the current head of the private `main` (`git ls-remote`) |
| `GITHUB_ACTOR`, `GITHUB_TRIGGERING_ACTOR` | both `curtisgalloway`, which covers re-runs |
| Candidate SHA | `inputs.sha` from `$GITHUB_EVENT_PATH` (dispatch) or the newest manifest entry (schedule). Must be **listed in `approved/candidates.txt` at `GITHUB_SHA`**, be 40 hex characters, and be reachable from public `main` in a fresh clone the hook makes itself |
| Dirty marker and quarantine flag | both absent ([§7](#7-leases-cleanup-and-crash-recovery)) |
| Lease | free, and acquired by the hook for this run |

Refusals are logged through a root helper, so the job can't erase the record.

### 5.4 Protecting the hook and the service, and keeping the runner current

- **Ownership.** The hook, its policy, the systemd units, the helpers in
  `/usr/local/sbin`, the runner install directory and every parent directory
  are owned by `root` and not writable by `panio-ci` or `panio-job`. `panio-ci`
  can write only the runner's `_work` and `_diag` directories.
- **Environment from systemd.** The `ACTIONS_RUNNER_HOOK_JOB_STARTED` and
  `..._COMPLETED` variables come from `Environment=` in a root-owned unit. The
  runner's own `.env` is root-owned and empty.
- **Startup checks fail closed.** `ExecStartPre=+/usr/local/sbin/bench-precheck`
  runs as root and refuses to start the listener unless the hook, policy and
  runner binaries match SHA-256 hashes pinned in a root-owned file; the
  ownership above holds; the quarantine flag and dirty marker are absent (or
  crash reconciliation succeeds, §7); the egress canaries fail as they should
  (§5.5); and the runner version is within the update policy below.
- **Two narrow sudo rules for `panio-ci`:** `bench-exec` (§5.6) and
  `bench-state` (§7). Both validate their arguments against a live admission
  record. There's no other sudo.

**Runner updates.** Registering with `--disableupdate` stops the runner from
rewriting its own (hash-pinned) binaries. GitHub requires a runner with
automatic updates disabled to be updated within 30 days of a new runner
release, and may stop sending jobs sooner if an update is security-critical.
So, as policy:

| | |
|---|---|
| Owner | You |
| Monitoring | `bench-precheck` and a daily root timer compare the installed version with the newest `actions/runner` release (an HTTPS fetch, which egress allows) and write a warning to the run summary once a newer release is 7 days old. Phase 2's watchdog (§8) catches a runner that GitHub has stopped sending jobs to |
| Deadline | Within 14 days of a new release; the same week for a release GitHub marks as a security fix. Never past 30 days |
| Procedure | Stop the listener → download the release tarball → verify its SHA-256 against the release page → install as root → update the pinned hashes → rerun the local refusal tests (N02–N05) with the listener stopped → start the listener → one manual dispatch |
| Time | About 30 minutes per update |

### 5.5 Network containment and egress

**What is enforced:**

- bench1, the witness and the lab devices sit on their own segment. **NAT
  alone isn't containment.** The router upstream of the segment needs explicit
  rules that drop connections from the segment to the home LAN prefixes, to
  the router's own management addresses, and to every other device's
  management interface, over both IPv4 and IPv6. The simplest reliable choice
  is no IPv6 on the segment; if IPv6 stays on, block ULA (`fc00::/7`) and the
  home prefix as well.
- bench1's own nftables rules (root-owned) allow only the lab subnet, DNS to the
  segment's resolver, and TCP 443 to addresses that aren't private, link-local
  or ULA. Everything else is dropped, IPv4 and IPv6.

**What is not enforced: egress destinations on the internet.** Allowing TCP 443
to any public address is **LAN containment, not destination-restricted
egress**. Code on the bench can send data to any HTTPS server on the internet.
Phase 1 accepts this as residual risk, on these conditions:

- **No valuable secrets live on the bench.** What is there, all lab-only: the
  runner registration credential (good only for the private repo's jobs, which
  carry no secrets); the JetKVM's local password; the witness SSH key, which
  only reaches the restricted witness agent; and lab device identifiers, which
  are private by project rule but low in value. No GitHub token with write
  access, no cloud credentials, no personal SSH keys, no App keys. The
  reporter's credentials never reach the bench (§5.8).
- Revisit this if the bench ever needs a valuable secret. The alternative is an
  allowlisting HTTPS proxy on the segment's router, covering the GitHub Actions
  endpoints listed in GitHub's self-hosted runner reference plus crates.io and
  its static CDN for builds. That's more maintenance, so it's listed as an
  option in [§11](#11-decisions-and-open-questions), not planned.

**Testing with canaries that really answer.** A probe against an address that
happens to be offline proves nothing. The canaries are real listeners:

- On the home LAN: a tiny TCP listener on a machine you control (for example
  `nc -lk 8099`), reachable over both IPv4 and IPv6. **Control check:** reach
  it from another home machine first, so you know it's up. Then the bench
  must fail to reach it.
- The router's management interface (HTTP/HTTPS/SSH ports): reachable from
  the home LAN, unreachable from the bench, over IPv4 and IPv6.
- `bench-precheck` repeats the bench-side half of these probes on every
  listener start and refuses to start if any one connects.

### 5.6 Execution users, scopes and state

**Two users, so candidate code doesn't share a UID with the runner.**

| User | Runs | Can read | Can't |
|---|---|---|---|
| `panio-ci` | The runner listener, the hooks, the **harness** | Runner credentials (`.credentials`, `.runner`), the witness SSH key, admission records | Access USB devices; write anything root-owned |
| `panio-job` | The candidate **build** and **every candidate process** (`panio` and its helpers) | The candidate checkout and build output; the lab devices it needs, through udev groups | Read `panio-ci`'s files (runner credentials, witness key), signal `panio-ci` processes, or write anything root-owned |

The harness starts candidate work only through `sudo -n
/usr/local/sbin/bench-exec <run-id> <verb> …`. That root helper:

- checks the run id against a live admission record;
- accepts a fixed set of verbs (`build`, `panio <args>`) with validated
  arguments;
- runs the process as `panio-job` with `systemd-run --uid=panio-job
  --slice=bench-job-<run-id>.slice`, with a CPU, memory and runtime limit, no
  new privileges, and a private `/tmp`;
- returns its exit code and output to the harness.

So every candidate process lives in one cgroup per run, separate from the
listener and the harness.

**Cleanup from outside the job's scope.** Cleanup never "kills all `panio-ci`
processes"; that would kill the listener and the hook itself. Instead the
root-run verifier stops `bench-job-<run-id>.slice`, then checks from outside it
that the slice is empty, that `panio-job` owns no processes anywhere, and that
`panio-job`'s home directory and the workspace match their allowlists. Any
difference means quarantine.

**What this doesn't solve (stated plainly):**

- Phase 1 is **trusted-code execution with containment**. A hostile candidate
  still controls the hardware it is given, can use the internet (§5.5), and
  shares a kernel with everything else.
- The harness, as `panio-ci`, can read the runner credentials, and so can
  anything that compromises the harness. Phase 2's single-use registration
  shortens how long they're valid, but doesn't stop them being exposed while
  a job runs.
- Separate UIDs block casual interference. They don't stop kernel exploits.

**Persistent state.** Phase 1 keeps one persistent registration and relies on
the verifier and quarantine above. Phase 2 gives each job genuinely fresh
execution state: a just-in-time, single-use registration inside a VM or
container whose root filesystem is recreated from a known image for every job
(on a Pi, an overlay root discarded on reboot), with USB devices passed in per
job.

### 5.7 Trusted harness, untrusted candidate

- **The harness and verdict logic live in the private repo** and run as
  `panio-ci`. Nothing from the candidate checkout decides a verdict.
- **The candidate supplies binaries only.** They are built with `cargo build
  --locked` via `bench-exec build`, as `panio-job`.
- **Witness SSH is narrow.** The witness's `authorized_keys` entry uses
  `restrict,command="/usr/local/bin/witness-agent"`. The agent accepts only
  `drive-list`, `drive-mount`, `drive-unmount`, `nonce-read`,
  `nonce-write <hex>`, and later `evdev-capture <seconds>`, validates their
  arguments, and does nothing else. The key belongs to `panio-ci`, so
  candidate code can't use it.
- **Bench-side drive access** also goes through a root helper with fixed verbs
  (`mount`, `unmount`, `nonce-read`, `nonce-write`) that accepts only the
  CI drive's recorded identity (§6).

### 5.8 Results, logs and publication

- **Masking isn't a confidentiality boundary.** Raw logs and artifacts stay
  private in `panio-hardware-ci`.
- **Phase 1: publishing is manual.** The run summary is a short structured
  table (test, verdict, device role, firmware version, candidate SHA) with no
  paths, serials or addresses.
- **Phase 2: a separate reporting repository.** A repository secret isn't
  limited to the workflow that's meant to use it: any workflow in that
  repository can reference it. So the issue-writing credential doesn't live in
  `panio-hardware-ci` at all. It lives in a second private repo,
  `panio-hardware-report`, that has **no self-hosted runner and no other
  workflows**.
  - **Credentials there:** a GitHub App key with **Issues: write** on public
    panio only (no contents, workflow or administration permissions), and a
    fine-grained read-only token with **Actions: read** on
    `panio-hardware-ci`. The bench never holds either.
  - **Trigger:** `reporter.yml` runs on a schedule (every 2 hours) and *pulls*
    completed runs. Nothing in the bench repo can dispatch it.
  - **Binding:** it accepts a run only if the workflow path is
    `.github/workflows/hardware.yml`, the repository, event and conclusion are
    as expected, and the artifact's embedded run id, attempt and candidate SHA
    match the run's own API metadata. A `(run id, attempt)` pair it has
    already processed is skipped; the processed list is kept as a marker in
    each issue body and a state file. That defeats replay.
  - **Handling the data:** the artifact is parsed with a JSON parser against a
    fixed schema (bounded size, enumerated test names and verdicts, no
    free-text fields). Values reach the GitHub API only as structured fields,
    never interpolated into shell commands or `${{ }}` expressions. No
    candidate code is fetched or run. Issues use fixed templates, are
    deduplicated by a stable key, and are rate-limited (at most three writes
    per run).
  - Its writers are trusted the same way as the bench repo's: you only.

## 6. How each test verifies the outcome

Rules every test follows:

- **Act through panio, check through the witness.** Never use panio to check
  panio's own work.
- **Both the command and the outcome must be right.** A command that exits
  non-zero is a **fail**, even if the hardware did the right thing: that's a
  CLI regression. A command that exits 0 without the physical outcome is a
  **fail** too.
- **Fresh nonce per run** (128 random bits, hex).
- **Both directions** wherever there are two.
- **Four verdicts:** `pass`, `fail` (product failure), `infra-error` (bench or
  harness problem), `skip` (device not available).

### Phase 1: USB round trip (would have caught #417)

**Device identity.** At setup, the harness records the CI drive's identity:
USB vendor and product IDs, USB serial number, and filesystem UUID. Every
"appears" and "disappears" check below matches that full identity, not the
label alone. The identity stays in the private lab config and never appears in
the published summary.

1. **Setup.** Lease held, dirty marker set (§7). The mux is on the host side,
   verified as in step 4. The drive is visible on bench1 and not on the
   witness.
2. bench1 mounts the drive, writes `nonce1`, then **flushes and unmounts** it.
3. `panio usb attach-target` must **exit 0**.
4. **Independent checks:**
   - the drive (by identity) **disappears** from bench1 within 10 s;
   - the drive **appears** on the witness within 15 s;
   - the witness mounts it, and `nonce-read` returns exactly `nonce1`;
   - `panio usb state` exits 0 and reads `target`.
5. The witness writes `nonce2`, then **flushes and unmounts** the drive
   (`drive-unmount`) before anything switches back.
6. `panio usb attach-host` must exit 0. Then the mirror image of step 4: gone
   from the witness, present on bench1, the file holds exactly `nonce2`, and
   `usb state` reads `host`.

| Situation | Verdict |
|---|---|
| Every command exits 0 and every check in both directions holds | pass |
| Exit 0 but the drive didn't move, or `usb state` disagrees with the witness (N07) | **fail** |
| Non-zero exit even though the drive moved correctly (N23) | **fail** (CLI regression) |
| Old nonce, missing file, wrong device identity, or only one direction done (N08) | fail; never pass |
| Witness unreachable, or the drive isn't visible anywhere at setup | infra-error |

### Later tests

| Test | Phase | Action through panio | Independent check | Would catch |
|---|---|---|---|---|
| **Unsupported device refuses** | 2 | `usb attach-target` on the Mini-KVM while no helper supports it | Must exit non-zero with an "unsupported" error. Exit 0 is a fail. Replaced by the round trip once a helper lands | #417 |
| **Keyboard** | 2 | `hid send type <nonce>` via JetKVM | `witness-agent evdev-capture` on the KVM's HID gadget; the key sequence must spell `nonce` | — |
| **Mouse move** | 2 | `hid send move`, `hid send moveabs` | evdev shows `REL_X`/`REL_Y` (or `ABS_*`) with the right signs | — |
| **Scroll** | 2 | `hid send scroll -200`, `scroll 3`, `scroll 0` | evdev shows `REL_WHEEL` with the right sign and total, and nothing for 0 | #415 |
| **Power** | 2 | Relay off, then on (commands must exit 0) | See below | — |
| **Video** | 3 | `video` capture plus OCR | Witness shows `nonce` in a large font on `tty1`; OCR of the capture must find it | — |
| **Firmware recorded** | 2 | `doctor` / JetKVM `/version` | Not a verdict; written to the summary ([§9](#9-firmware-channels)) | — |

**Power: three separate things, kept separate.**

| Signal | What it shows | Role in the verdict |
|---|---|---|
| Commanded state (what panio asked for) and the relay's readback | That the relay *thinks* it switched | Required to match, but never enough alone |
| **Metered power** from the metering plug in series with the target's supply | That electrical power actually stopped and resumed | **Required:** below 0.5 W for at least 5 s after off, and back above the target's idle draw after on |
| Witness reachability and `boot_id` | That the target went down and **rebooted** | Supporting evidence only. A changed `boot_id` proves a reboot, not that power was ever interrupted (a watchdog reset also changes it) |

Pass needs all of these: commands exit 0, the readback matches, metered power
drops and returns, and the witness comes back with a new `boot_id`. A metered
drop without a readback match, or the reverse, is a fail. Ping loss alone
never counts as power loss.

## 7. Leases, cleanup and crash recovery

Phase 1, all of it. Hooks and `if: always()` steps are best effort: a power
loss, kernel crash or forced kill can skip both. So correctness never depends
on them.

**Dirty marker.** Before the first actuation of a run, the harness calls `sudo
-n /usr/local/sbin/bench-state begin <run-id>`, which writes a root-owned
marker on persistent storage (`/var/lib/bench/dirty`, fsynced) naming the run,
the devices it may touch and their expected resting states. Only `bench-state
clear <run-id>` removes it, and only after the **root verifier** has
independently confirmed the resting state: mux on the host side, CI drive
present on bench1 with its full identity and absent from the witness, target
power on (phase 2), no held HID keys, and an empty job slice.

**Startup reconciliation.** At every boot and every listener start,
`bench-precheck` looks for the marker. If it's there, it runs the same
cleanup-and-verify sequence as root. If that succeeds, it clears the marker
and continues. If it fails, or can't run, it sets **quarantine**, and the
listener doesn't start.

**Leases, fencing and handoff:**

- **Who holds it.** The job-started hook acquires the panio lease on the bench
  target as holder `bench-ci`, with a TTL of the job's timeout plus a margin
  (45 minutes). It writes the token into the admission record (§5.3), which
  is how it's handed to the harness.
- **Renewal.** The harness renews every 5 minutes. If a renewal fails, it stops
  actuating and goes to cleanup.
- **Fencing.** Every actuating command carries the token, and must be
  rejected if the token has expired or been superseded. That way a stuck old
  run can't act after a new holder takes over. **This is a requirement on
  panio's lease implementation**, and step 1.6 validates it (does `panio lease`
  support TTL, renewal and token checks on every actuating verb?). It isn't a
  claim that the current implementation falls short; if a piece is missing,
  it becomes a panio issue before phase 1 runs unattended.
- **Release.** Only `bench-state clear`, after verification, releases the
  lease. **An uncertain bench is never released to manual users.** If
  reconciliation fails, the lease is re-held by `bench-ci` with no expiry and
  the bench is quarantined until you inspect it and clear both by hand.
- **Busy bench.** If you hold the lease for manual work, the hook refuses the
  job with "bench busy" and nothing actuates (N10).

**Timeouts.** Each test step has a hard timeout (default 120 s), the job has
`timeout-minutes: 30`, and the job slice has its own runtime limit.

## 8. Flaky and offline devices

- **Preflight.** The witness and every device are checked first. An
  unavailable device marks its tests `skip`.
- **All-skipped isn't green.** A run where every test for a device, or the
  whole run, was skipped, or where the witness was absent, reports **missing
  coverage** and fails the workflow (N09).
- **Bounded recovery (phase 2).** On a failure or a device that doesn't
  respond, power-cycle that device once, wait up to 90 s, and rerun that one
  test once. A pass on the retry is reported as **flaky**, never as pass.
  There are no further retries.
- **Escalation (phase 2, through the reporter):** a device skipped on 2
  consecutive nightly runs gets an issue `lab: <role> offline`; a test flaky 3
  times in 7 days gets `lab: <test> flaky`.
- **Runner offline or stuck (phase 2).** `watchdog.yml`, on a hosted runner in
  the bench repo, fails (which emails you) if there's been no successful,
  not-all-skipped run in 36 hours. That covers an offline runner, a runner
  GitHub stopped sending jobs to for being out of date, a stuck queue,
  cancelled runs and quarantine (N17).

## 9. Firmware channels

| Device | Channel | Policy | A failure means |
|---|---|---|---|
| `jetkvm-stable` | Latest stable release, updated by hand after reading the release notes | Counts toward the advisory release check | A panio regression, or a stable firmware break that needs a fix before the next release |
| `jetkvm-dev` (phase 3) | JetKVM's dev update channel, auto-updating | Non-blocking; a warning in the summary | An upcoming break; the reporter files `firmware-dev` with old and new versions |
| Openterface KVM-Go / Mini-KVM | Pinned at the installed version; updated by hand | Counts toward the advisory release check | A panio regression (firmware is pinned) |

Every run records every device's firmware version. When a test goes from pass
to fail, the summary says whether firmware changed since the last pass.

## 10. Phased rollout

Estimates are your hands-on time. "Claude" steps are ones a cloud session can
write as a PR to the private repo for you to review and merge; the review is
included in the estimate. Each step must pass the listed gates
([§13](#13-negative-acceptance-gates)) before the next step starts.

### Phase 1: one real test, contained (about 6–10 hours, provisional; $0–25)

The range is wide because isolation depends on what your router can do.

| # | Step | Who | Time | Gates |
|---|---|---|---|---|
| 1.1 | Private `panio-hardware-ci` created with read-only workflow permissions (**done** Nov 25) | You | — | — |
| 1.2 | Add `hardware.yml` (dispatch only, **no schedule yet**), the harness, `approved/candidates.txt` with one entry, and the admission policy | Claude drafts; you review and merge | 45 min | — |
| 1.3 | Isolate the bench network with explicit upstream deny rules; set up the home-LAN and router canaries and confirm they answer from the home LAN | You | 1–3 h | N06 |
| 1.4 | On bench1: create `panio-ci` and `panio-job`, udev rules (devices go to `panio-job` only), nftables rules | You (Claude drafts the files) | 45 min | N06 |
| 1.5 | Witness: `witness-agent` behind a forced-command key; record the CI drive's identity | You (Claude drafts the agent) | 30 min | — |
| 1.6 | Install the root-owned hook, `bench-precheck`, `bench-exec`, `bench-state`, the verifier, `bench-quarantine` and the systemd units. **With the listener stopped**, run the local refusal tests using crafted environments and event files. Check that panio's lease supports TTL, renewal and token fencing | You (Claude drafts all scripts and a local test runner) | 1.5 h | N02–N05, N14, N19, N22 |
| 1.7 | Register the runner to the **private repo only** with `--disableupdate` and label `panio-hw`; start it while you watch. Run the GitHub-side refusal tests before any candidate code runs | You | 45 min | N01, N03, N04, N19 |
| 1.8 | First real dispatch on the approved SHA. Then the negative outcome tests: a stub helper that exits 0 without switching, one that switches but exits non-zero, the witness unplugged, the lease held, a cancel mid-switch, and a **hard power-off of bench1 mid-switch** followed by reboot | You | 1.5 h | N07–N11, N13, N20, N21, N23 |
| 1.9 | Add the nightly schedule, only once every gate above has passed | You | 5 min | all phase 1 gates |

**Stop rule:** any failed security gate, or uncertain hardware state after
cleanup, means stop the listener and quarantine until it's understood.

### Phase 2: JetKVM, power, reporting, disposable execution (about 9–13 hours over two weeks, $130–300)

| # | Step | Who | Time | Gates |
|---|---|---|---|---|
| 2.1 | Buy and cable the JetKVM (stable), relay, metering plug and recovery plug | You | 1 h | — |
| 2.2 | `evdev-capture`; keyboard, mouse and scroll tests | Claude | 45 min review, 20 min install | — |
| 2.3 | Power test with commanded, readback and metered signals | Claude | 30 min review | N24 |
| 2.4 | Bounded recovery, flaky verdict, firmware versions | Claude | 30 min review | — |
| 2.5 | Disposable execution: just-in-time registration in a VM/container (or overlay root) rebuilt per job | You (Claude drafts) | 2–4 h | N14, N18 |
| 2.6 | Create private `panio-hardware-report`; the GitHub App (Issues: write, public panio only); a read-only Actions token; `reporter.yml` | You create the repo, App and token (30 min); Claude writes the workflow | 1.5 h | N12, N15, N16, N25 |
| 2.7 | `watchdog.yml` | Claude | 15 min review | N17 |
| 2.8 | Mini-KVM "refuses cleanly" test while #417 is open | Claude | 15 min review | — |

### Phase 3: firmware channels, video, advisory release check (about 4–5 hours, $70–110)

| # | Step | Who | Time |
|---|---|---|---|
| 3.1 | Second JetKVM on the dev channel | You | 45 min |
| 3.2 | Device matrix with per-channel policy; `firmware-dev` issues via the reporter | Claude | 30 min review |
| 3.3 | Video + OCR test | Claude | 30 min review, 30 min tuning |
| 3.4 | Advisory release check in `RELEASE-TRAIN.md`: "the release commit is in the manifest and its hardware run is green on stable devices". Not a gate in `release.yml` | Claude | 20 min review |
| 3.5 | Once #428 merges, approve that commit and confirm the dev-channel JetKVM exercises the new binary `hidrpc` scroll path | You | 15 min |

### Ongoing

- Runner updates per §5.4: about 30 minutes, within 14 days of each release.
- Approving new commits: about 2 minutes per manifest entry.

## 11. Decisions and open questions

| # | Question | Decision |
|---|---|---|
| D1 | Run hardware tests on every PR? | **No.** |
| D2 | Where does the runner register? | The private `panio-hardware-ci` only. |
| D3 | What approves a commit? | An entry in `approved/candidates.txt` in the private repo. Being an ancestor of `main` is a sanity check, not approval. |
| D4 | Who is trusted to change what runs? | Writers of the two private repos: only you. Adding anyone means revisiting this document. |
| D5 | Unrestricted HTTPS egress? | **Accepted** as residual risk in phase 1, with no valuable secrets on the bench. An allowlisting proxy is the option if that changes. |
| D6 | Pre-merge testing (e.g. a PR branch like #428's)? | **Deferred.** It needs disposable execution (phase 2), a manifest entry naming the exact SHA, and acceptance that unreviewed code can drive the hardware during that run. |
| D7 | Hard release gate in `release.yml`? | **No.** Advisory first; revisit after a month of stable nightly runs. |
| D8 | Pi or x86 for bench1? | Either. An N100 mini PC builds Rust about 2–3x faster and makes phase 2's VM-per-job easier. |
| Q1 | Can your existing bench double as the CI bench? | Only if it can move onto the isolated segment and be dedicated to CI. |
| Q2 | Which mux is the phase 1 device: KVM-Go or `usbtoggle`? | Whichever is already cabled. |
| Q3 | Put the harness on a second machine, apart from the candidate? | Not planned. It's the only full defense against a hostile candidate; revisit with D6. |

## 12. Review findings and where they are addressed

### First review (revision 1 → 2)

| Finding | Summary | Addressed in |
|---|---|---|
| SEC-01 | Hook installed after registration | §10 steps 1.6 → 1.7; listener won't start without the hook (§5.4) |
| SEC-02 | Network containment deferred | §5.5 and step 1.3, before any job |
| SEC-03 | Approval, labels and lint aren't boundaries | §2, §5.2: private repo; public panio has no runner |
| SEC-04 | Hook not immutable; admission too loose | §5.3, §5.4, §5.7 |
| SEC-05 | "Ephemeral" isn't the same as wiped | §5.6 |
| SEC-06 | Read-only token vs. automatic issues | §5.8 |
| SEC-07 | Masking isn't confidentiality | §5.8 |
| SEC-08 | Leases and cancellation cleanup came too late | §7 |
| SEC-09 | Harness in the candidate checkout; shared host | §5.6, §5.7 |
| SEC-10 | Trigger and hook mismatch; tag credentials | §5.2 |

### Follow-up review (revision 2 → 3)

| Finding | Summary | Addressed in |
|---|---|---|
| R2-1 (high) | Being an ancestor isn't review; the SHA must be bound once | §5.3: approval manifest, admission record, checkout by exact SHA; §5.2 trust statement; gates N19, N20 |
| R2-2 (high) | Cleanup can't depend on hooks; lease handoff | §7: persistent dirty marker, startup reconciliation, lease TTL, renewal, fencing and no release while uncertain; gates N21, N22 |
| R2-3 (high) | Reporter secret scope | §5.8: separate hosted-only `panio-hardware-report`, pull model, run binding, replay rejection, no shell interpolation; gate N25 |
| R2-4 (medium) | Public HTTPS is an exfiltration path; NAT isn't containment | §5.5: explicit upstream deny rules, egress accepted as residual risk with no valuable secrets, canaries that really answer; D5 |
| R2-5 (medium) | `--disableupdate` needs a policy | §5.4: owner, monitoring, deadline, procedure |
| R2-6 (medium) | Same-UID interference; indiscriminate cleanup | §5.6: separate `panio-job` user, per-run slices via `bench-exec`, verification from outside the slice; trust model stated up front |
| R2-7 (medium) | Verdicts need the command result; unmount, identity, power | §6: exit codes required, witness-side unmount, full device identity, metered power separate from the readback, `boot_id` as supporting evidence only; gates N23, N24 |

## 13. Negative acceptance gates

Each gate is a deliberate attempt to make the wrong thing happen, with the
required result. "Step" is where in [§10](#10-phased-rollout) it is first
exercised. None has been executed yet.

| Gate | Attempt | Required result | Step |
|---|---|---|---|
| N01 | A public fork workflow asks for `panio-hw` | No runner assigned | 1.7 |
| N02 | Hook missing or altered | Service refuses to start | 1.6 |
| N03 | Wrong workflow, event, ref, or a workflow commit that isn't the private `main` head | Rejected before any workflow step | 1.6, 1.7 |
| N04 | Re-run triggered by another actor | No candidate code executes | 1.6, 1.7 |
| N05 | A job tries to modify the hook, policy, units, helpers or runner files | Denied by file ownership | 1.6 |
| N06 | Connect to a **live** home-LAN canary or router management, over IPv4 and IPv6 | Blocked from the bench; reachable from the home LAN as a control | 1.3, 1.4 |
| N07 | panio exits 0 without the USB moving | Product failure | 1.8 |
| N08 | Stale nonce, wrong device identity, or incomplete round trip | Never a pass | 1.8 |
| N09 | Witness absent, or everything skipped | Missing coverage; the run fails | 1.8 |
| N10 | Lease already held | No device actuation | 1.8 |
| N11 | Cancel during actuation | Verified cleanup, or quarantine | 1.8 |
| N12 | Any bench credential tries to change a public panio issue | Denied | 1.8, 2.6 |
| N13 | Sensitive diagnostic data in the output | Stays in private logs; not in the summary | 1.8 |
| N14 | Unexpected persistent state between jobs | Next execution blocked | 1.6, 2.5 |
| N15 | Malformed, oversized or replayed reporter input | Rejected or skipped; no command execution | 2.6 |
| N16 | Reporter credential tries contents, workflow or admin changes | Denied by its permissions | 2.6 |
| N17 | Runner offline, out of date, queued, cancelled, or an all-skipped run | Watchdog reports missing successful coverage | 2.7 |
| N18 | State planted in the disposable environment | Absent from the next job; otherwise quarantine | 2.5 |
| N19 | Dispatch a SHA that is on public `main` but **not in the manifest** (for example a direct-pushed or intermediate commit) | Refused at admission | 1.6, 1.7 |
| N20 | Public `main` moves between admission and checkout | The run builds and reports the admitted SHA; `HEAD` check passes only for that SHA | 1.8 |
| N21 | Hard power-off of bench1 mid-actuation, then reboot | Dirty marker forces reconciliation before any job; on failure, quarantine; the lease isn't released to manual users | 1.8 |
| N22 | Actuate with an expired or superseded lease token | Rejected by panio (fencing) | 1.6 |
| N23 | Helper switches the USB correctly but exits non-zero | Fail (CLI regression) | 1.8 |
| N24 | Relay readback says off, but the metering plug still shows power (or the reverse) | Fail; `boot_id` change alone never passes | 2.3 |
| N25 | A workflow other than `reporter.yml`, or anything on the bench, tries to use the reporter credential | Impossible: the credential exists only in `panio-hardware-report`, which has no other workflows and no self-hosted runner | 2.6 |

## 14. References

- [Adding self-hosted runners](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/add-runners)
- [Self-hosted runners reference](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)
- [Managing GitHub Actions settings for a repository](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/managing-github-actions-settings-for-a-repository)
- [Running scripts before or after a job](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/run-scripts)
- [Variables reference](https://docs.github.com/en/actions/reference/workflows-and-actions/variables)
- [`GITHUB_TOKEN`](https://docs.github.com/en/actions/concepts/security/github_token)
- [Secrets](https://docs.github.com/en/actions/concepts/security/secrets)
- [Secure use reference](https://docs.github.com/en/actions/reference/security/secure-use)
- [Create an issue (REST)](https://docs.github.com/en/rest/issues/issues#create-an-issue)
