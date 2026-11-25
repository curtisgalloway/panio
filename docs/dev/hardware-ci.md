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

> **Status:** design, revision 4. Nothing in this document runs yet, and nothing
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
> Revision 3 applied the follow-up review (R2-1 to R2-7): an explicit approval
> manifest, crash-safe cleanup, a separate execution user, a separate reporting
> repository, and stated egress and update policies. Revision 4 applies the
> revision 3 review (R3-a to R3-d) and a consensus review with a second model
> (C1 to C16). Its main changes: one privileged admission entry point, a
> dedicated bench behind a single host gate with an explicit maintenance
> handoff, a durable fence that doesn't depend on lease expiry,
> candidate-built helpers called by absolute path, synchronous process
> execution, a supervised listener-off round trip before the runner is
> registered, and revised estimates and costs.
> [§12](#12-review-findings-and-where-they-are-addressed) maps every finding to
> the section that addresses it.

## Trust model in one paragraph

Phase 1 is **trusted-code execution with containment**, not isolation from
hostile code. Only commits you have explicitly approved run on the bench, and
the bench is contained so that a mistake or a compromised dependency can't
reach your home network or any valuable secret. Candidate code runs as a
separate user from the runner and the harness, but it still shares the kernel,
the hardware and the internet connection with them. The bench is **dedicated
to CI**: people use it only through an explicit maintenance handoff (§7),
never alongside a run. Treat everything that follows with that model in mind.

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
from an independent vantage point catches this class of bug. It catches a
specific instance only on the device that has it: phase 1's round trip runs on
a supported mux, so it detects false-success switching there, not #417 itself,
which needs the Mini-KVM ([§6](#6-how-each-test-verifies-the-outcome)).

## 2. Can this run on the free GitHub plan?

**Yes.** Self-hosted runners are free on every plan, including Free, for public
and private repositories, and GitHub doesn't bill minutes for jobs on your own
machine. Hosted jobs (the watchdog and reporter in phase 2) and artifact
storage in *private* repositories count against the Free plan's included
allowance. Check the account's current included allowance and remaining usage
before enabling scheduled hosted jobs; the reporter runs daily (§5.8) to keep
that use small, and artifacts have bounded size and retention.

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
 │ private: panio-hardware-    │◀── Actions + Contents: read (phase 2) ─┤
 │ report (hosted runners only)│                      │
 │ reporter.yml, App key       │                      │
 └─────────────────────────────┘                      │
┌──────────────────────── isolated bench network ─────▼───────────────────────────┐
│ bench1 (dedicated to CI)                                                        │
│  ├─ root: systemd units, bench-state (host gate: admission, maintenance,        │
│  │   dirty marker, quarantine, verifier), bench-exec, read-only deploy key      │
│  ├─ panio-ci: runner listener + harness (holds runner credentials, witness key) │
│  ├─ panio-job: candidate build and every panio process that touches the         │
│  │   fixture (CI and maintenance), one fixed HOME, per-attempt systemd slice;   │
│  │   the only user with USB device permissions                                  │
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
| Sacrificial media, CI only: a **microSD card** for the KVM-Go (it switches its onboard microSD reader) or a small USB flash drive for a sharing switch | Payload that moves between host and witness | $8 |
| Isolated network segment: a VLAN with firewall rules on gear you have, **or** a small travel router with its own firewall, plus explicit deny rules on the upstream router | Network containment ([§5.5](#55-network-containment-and-egress)) | $0 with capable gear; $30–40 otherwise |

**Phase 1 total: $0–25 if you already own the box, witness, supported mux and
isolation gear. About $330 if you already own a supported mux but none of the
rest** (box, witness, travel router and media). A buyer with no supported mux
also pays for one and, for the sharing-switch option, for the relay. The
travel-router figure assumes the upstream router can enforce the deny rules in
§5.5. Without isolation, don't register a runner. Run the harness by hand
instead.

### Phase 2 additions

| Item | Role | Rough cost |
|---|---|---|
| JetKVM (on **stable** firmware) | HID and video tests; would have caught #415 | $70–90 |
| Openterface Mini-KVM | Covers #417 once a helper exists; until then, a test that proves `usb` refuses it | $100–140 |
| DSD Tech SH-UR04A 4-channel USB relay | Switches the target's power | $15–20 |
| **Metering** smart plug (a Shelly Plug model that reports watts; panio-supported) in series with the target's supply | Independent observation of electrical power ([§6](#6-how-each-test-verifies-the-outcome)) | $20–30 |
| Second (non-metering) smart plug, Shelly or Kasa | Power-cycle a wedged KVM during recovery | $15–25 |
| Second boot medium for bench1 (or a second SD card) | Disposable, re-imaged execution environment ([§5.6](#56-execution-users-scopes-and-state)) | $10–20 |

**Phase 2 total: $230–325 if all six are bought.** Each is optional on its own;
price the ones you need.

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
8. **A green result that tested something else**: an installed helper instead
   of the candidate's, or an exit status read before the command finished
   (§5.6, §5.7).

### 5.2 Where authority lives

| | Public `panio` | Private `panio-hardware-ci` | Private `panio-hardware-report` (phase 2) |
|---|---|---|---|
| Self-hosted runner | **None** | Registered here only, label `panio-hw` | **None**; hosted runners only |
| Workflows | No hardware workflows | `hardware.yml` (bench), `watchdog.yml` (hosted) | `reporter.yml` (hosted) |
| Triggers | n/a | `schedule`, `workflow_dispatch` only | `schedule` only |
| Who can write | Unchanged | **You only**: no collaborators, no deploy keys with write access, no Apps with Contents write | **You only**, same rule |
| Credentials it holds | Unchanged | None beyond the default `GITHUB_TOKEN` (`contents: read`). One **read-only deploy key** for this repo is held by root on bench1 (§5.3) | GitHub App key (Issues: write on public panio), and a read-only token (Actions: read, Contents: read) for the bench repo |

**Who is trusted (stated outright).** Everyone who can write to
`panio-hardware-ci` is trusted to define what runs on the bench. Today that is
only you. The Free plan can't protect a private repository's branches, so the
control is that there are no other writers. Adding a collaborator, deploy key
with write access, or App with write access to either private repository is a
security change that needs this document revisited first. Admission
additionally requires that the workflow commit (`GITHUB_SHA`) is the current
head of the private `main`, so an old or side-branch workflow can't be
replayed. The runner account (`panio-ci`) supplies the hook's inputs and is
trusted in phase 1; root ownership of the admission record doesn't
authenticate those inputs, it only stops the job from changing them later.

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

**Which entry runs.** Every run, scheduled or dispatched, tests the **last
entry** of `approved/candidates.txt` at `GITHUB_SHA`, in file order (not by
date). Blank lines and lines starting with `#` are ignored. A malformed last
entry is a refusal, never a fallback to an earlier line. There is no SHA
input on dispatch: to retest an older approved commit, append it again (a
reviewed commit, about 2 minutes). So the candidate is always derivable from
the control commit alone, which the reporter relies on (§5.8).

The nightly run doesn't follow moving `main`. If you don't add entries, it
keeps retesting the same commit. That still catches bench and firmware drift,
but it won't cover new code until you approve it. This is a deliberate trade:
convenience is given up so that review means something.

**One privileged entry point.** The job-started hook runs as `panio-ci`, so it
can't create root-owned state itself. It calls `sudo -n /usr/local/sbin/bench-state
admit`, the **only** operation that creates an admission. Under one host lock,
`bench-state admit`:

1. refuses if any admission is active, or if maintenance (§7), the dirty
   marker or the quarantine flag is set;
2. fetches private `main` once into a root-owned bare mirror. Only
   `bench-state admit` uses the root-held **read-only deploy key** for
   `panio-hardware-ci`. It fetches over SSH to `ssh.github.com` on TCP 443
   (which the egress policy allows, §5.5), with the host key pinned for that
   endpoint. That one fetched ref is used both to check that
   `GITHUB_SHA` is the head of `main` and to read the manifest at
   `GITHUB_SHA`, so there is no separate `ls-remote` to race. The key's
   exposure is read access to that repository and its history; no candidate
   process ever receives a repository credential;
3. runs the admission checks below;
4. writes a root-owned **admission record**,
   `/run/bench/admission/<run-id>-<attempt>.json` (mode 0640,
   root:panio-ci), holding run id, attempt, control SHA (`GITHUB_SHA`),
   candidate SHA and admission time, and creates the attempt's directories
   (§5.6).

Refusal logging goes through the same helper and works before any admission
exists, so the job can't erase the record. After admission:

- `bench-exec build` (§5.6) checks out the candidate **by that SHA** into a
  fresh per-attempt directory and verifies `git rev-parse HEAD` equals it
  before building;
- every CI job operation after admission (`bench-exec` and the `bench-state`
  job verbs) requires the exact active run id and attempt. The runner's sudo
  rules authorize only admission, refusal logging and the listed CI job
  operations. The operator's maintenance commands and the root service's
  recovery commands have separate entry points (§7);
- the run summary is stamped with both SHAs from the record, not from
  anything the job computed.

Nothing resolves `main` again after admission, so `main` moving mid-run can't
change what is tested (N20).

**Admission checks.** `bench-state admit` denies by default; any missing or
malformed value is a refusal. It admits a job only if all of these hold:

| Check | Required value |
|---|---|
| `GITHUB_REPOSITORY` | `curtisgalloway/panio-hardware-ci` |
| `GITHUB_WORKFLOW_REF` | `curtisgalloway/panio-hardware-ci/.github/workflows/hardware.yml@refs/heads/main` (exact) |
| `GITHUB_EVENT_NAME` | `schedule` or `workflow_dispatch` |
| `GITHUB_REF` | `refs/heads/main` |
| `GITHUB_SHA` | equals the head of private `main` in the fetch above |
| `GITHUB_ACTOR`, `GITHUB_TRIGGERING_ACTOR` | both `curtisgalloway`, which covers re-runs |
| Candidate SHA | the last entry of `approved/candidates.txt` at `GITHUB_SHA` (rules above): 40 hex characters, and reachable from public `main` in the public mirror (§5.6) |
| Active admission, maintenance, dirty marker, quarantine | all absent ([§7](#7-leases-cleanup-and-crash-recovery)) |

### 5.4 Protecting the hook and the service, and keeping the runner current

- **Ownership.** The hook, its policy, the systemd units, the helpers in
  `/usr/local/sbin`, the runner install directory and every parent directory
  are owned by `root` and not writable by `panio-ci` or `panio-job`. `panio-ci`
  can write only the runner's `_work` and `_diag` directories and the public
  mirror (§5.6).
- **Environment from systemd.** The `ACTIONS_RUNNER_HOOK_JOB_STARTED` and
  `..._COMPLETED` variables come from `Environment=` in a root-owned unit. The
  runner's own `.env` is root-owned and empty.
- **Startup checks fail closed.** `ExecStartPre=+/usr/local/sbin/bench-precheck`
  runs as root and refuses to start the listener unless the hook, policy and
  runner binaries match SHA-256 hashes pinned in a root-owned file; the
  ownership above holds; maintenance, the quarantine flag and the dirty marker
  are absent (or crash reconciliation succeeds, §7); the egress canaries fail
  as they should (§5.5); and the runner version is within the update policy
  below.
- **Three narrow sudo rules for `panio-ci`:** `bench-state admit` (§5.3), the
  other `bench-state` verbs (§7, including the fixed drive subcommands of
  §5.7), and `bench-exec` (§5.6). Everything except `admit` and refusal
  logging requires a live CI admission record. There's no other sudo for
  `panio-ci`. Maintenance and recovery have their own entry points (§7).

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
- bench1's root-owned nftables rules allow DNS to the segment resolver, SSH
  to the witness, and the configured fixture endpoints and ports. They deny
  other same-segment traffic, home-LAN prefixes and router management
  endpoints before allowing outbound TCP 443 to public internet addresses.
  All other traffic is dropped. These rules apply to each enabled address
  family.

**What is not enforced: egress destinations on the internet.** Allowing TCP 443
to any public address is **LAN containment, not destination-restricted
egress**. Code on the bench can send data to any HTTPS server on the internet.
Phase 1 accepts this as residual risk, on these conditions:

- **No valuable secrets live on the bench.** What is there, all lab-only: the
  runner registration credential (good only for the private repo's jobs, which
  carry no secrets); the root-held read-only deploy key for the private control
  repo (read access to the harness and manifest); the JetKVM's local password;
  the witness SSH key, which only reaches the restricted witness agent; and lab
  device identifiers, which are private by project rule but low in value. No
  GitHub token with write access, no cloud credentials, no personal SSH keys,
  no App keys. The reporter's credentials never reach the bench (§5.8).
- Revisit this if the bench ever needs a valuable secret. The alternative is an
  allowlisting HTTPS proxy on the segment's router, covering the GitHub Actions
  endpoints listed in GitHub's self-hosted runner reference plus crates.io and
  its static CDN for builds. That's more maintenance, so it's listed as an
  option in [§11](#11-decisions-and-open-questions), not planned.

**Testing the two boundaries separately, with canaries that really answer.** A
probe against an address that happens to be offline proves nothing, and a
probe that bench1's own firewall already blocks says nothing about the
upstream rule. So:

- **Canaries.** On the home LAN, a known-live **HTTPS** listener on a machine
  you control (TCP 443 exercises the port allowed for public egress; the
  destination rules still apply), reachable over IPv4 and, where IPv6 is on,
  IPv6; and the router's
  management interface (HTTP/HTTPS/SSH). **Control check:** reach each one
  from another home machine first, so you know it's up.
- **Upstream boundary.** Probe the canaries from an ordinary device on the
  bench segment that doesn't have bench1's host firewall (a laptop on the
  bench port will do). Each must fail.
- **Host boundary.** Probe the same canaries from bench1. Each must fail,
  and bench1's firewall counters or logs must show the host rules dropped
  it, since a timeout alone doesn't say which layer blocked it.
- **Same-segment paths.** Check that the traffic the tests need works
  (bench1 to the witness's SSH port) and that management interfaces of other
  devices on the segment are not reachable from bench1 except the lab
  devices it drives.
- **IPv6.** Record how IPv6 was tested, or how it was shown to be disabled on
  the segment.
- `bench-precheck` repeats the bench1-side probes on every listener start.
  Precheck refuses startup if a forbidden probe connects or a required probe
  fails.

### 5.6 Execution users, scopes and state

**Two users, so candidate code doesn't share a UID with the runner.**

| User | Runs | Can read | Can't |
|---|---|---|---|
| `panio-ci` | The runner listener, the hooks, the **harness** | Runner credentials (`.credentials`, `.runner`), the witness SSH key, admission records | Access USB devices; write anything root-owned |
| `panio-job` | The candidate **build** and **every panio process that touches the fixture**, from CI runs and from maintenance (§7), with one fixed `HOME` | The candidate checkout and build output; the lab devices, through udev groups. It is the **only** user with device permissions; the operator's own account has none | Read `panio-ci`'s files (runner credentials, witness key), signal `panio-ci` processes, or write anything root-owned |

The fixed `HOME` gives all fixture panio invocations one lease store. Verbs
within an attempt share that attempt's `PANIO_RUNTIME_BASE`. Physical exclusivity between CI
and people is owned by the host gate (§7), not by panio leases. Where a test
exercises panio leases, acquire, renew, enforce and release all happen through
`panio-job`, started by the harness via `bench-exec`; the hook doesn't acquire
a panio lease.

**Process execution.** The harness starts candidate work only through `sudo -n
/usr/local/sbin/bench-exec <run-id> <attempt> <verb> …`. That root helper:

- for CI callers, checks the run id and attempt against the live admission
  record (the maintenance path, §7, has its own entry point);
- accepts a fixed set of verbs (`build`, `panio <args>`) with validated
  arguments;
- runs the process as `panio-job` with `systemd-run --wait --pipe --collect
  --uid=panio-job --slice=bench-job-<run-id>-<attempt>.slice`, keeping the
  default `KillMode=control-group`, with CPU, memory, disk and runtime limits
  and no new privileges;
- sets `PANIO_RUNTIME_BASE` to the attempt's runtime directory (below), and
  doesn't rely on `PrivateTmp` for panio's runtime state;
- waits for the program, returns its output and its **actual** exit status, and
  classifies a launch failure, a timeout and a signal death explicitly, so none
  is mistaken for a result.

**One-shot verbs in phase 1.** `--wait` with `KillMode=control-group` means
anything a verb leaves behind is killed when it exits. Phase 1's USB round
trip needs no daemon across verbs (the CH9329 path opens the serial port
directly when no daemon owns it), so this is what's wanted. A later phase that
needs a daemon across verbs runs it as its own named service in the attempt
slice, and teardown stops and verifies it.

**Independent deadline.** A root timer started by `bench-state admit` revokes
the admission and drains the attempt's slice at the job timeout plus a margin,
even if the listener has gone away.

**Per-attempt state.**

| Path | Owner | Holds |
|---|---|---|
| Public panio bare mirror | `panio-ci` (fetches it), readable by `panio-job` and root | Source for the candidate checkout and the reachability check |
| Private control bare mirror | root | The manifest and `main` head for admission (§5.3) |
| Bounded disk-backed scratch, per attempt | `panio-job` | A fresh checkout, `CARGO_HOME` and `CARGO_TARGET_DIR` |
| `/run/bench/job/<run-id>-<attempt>` (0700) | `panio-job` | Only small runtime and discovery files (`PANIO_RUNTIME_BASE`). `/run` is memory-backed, so builds never go here |

The Rust toolchain is installed read-only. `bench-exec build` runs `cargo
fetch --locked` and then `cargo build --locked` in the attempt's scratch, so no
crate cache carries over from an earlier candidate. The supervised run (step
1.7) records the cold build time and peak disk use, which set the scratch
limit and the build timeout.

**Device-free build.** `bench-exec build` runs with `PrivateDevices=yes`, with
no fixture device descriptors and no device-owning daemon running. That makes
the build device-free for the USB fixture. It isn't device-free for
network-controlled fixtures (JetKVM, smart plugs) until the build also has a
network restriction; phase 2 adds one before relying on that claim.

**Cleanup from outside the job's scope.** Cleanup never "kills all `panio-ci`
processes"; that would kill the listener and the hook itself. Instead the
root-run verifier stops `bench-job-<run-id>-<attempt>.slice`, then checks from
outside it that the slice is empty, that `panio-job` owns no processes
anywhere, and that `panio-job`'s home directory and the workspace match their
allowlists. panio's own state paths under that home are on the allowlist, but
their contents are treated as job-owned data, never as evidence. Known-good
lease cleanup runs as `panio-job` with the fixed `HOME`; root's own cleanup
uses its own record and the systemd unit identity, since lease retirement can
signal processes. Any difference means quarantine.

**What this doesn't solve (stated plainly):**

- Phase 1 is **trusted-code execution with containment**. A hostile candidate
  still controls the hardware it is given, can use the internet (§5.5), and
  shares a kernel with everything else.
- The harness, as `panio-ci`, can read the runner credentials, and so can
  anything that compromises the harness.
- Separate UIDs block casual interference. They don't stop kernel exploits.
- Concurrent manual use of a CI bench is out of scope for phase 1. Supporting
  it needs a lease authority shared across users, which is a larger project
  (§11 Q4).

**Persistent state.** Phase 1 keeps one persistent registration and relies on
the verifier and quarantine above. Disposable execution (a VM or container,
or on a Pi an overlay root discarded on reboot, rebuilt per job with USB
devices passed in) is phase 2. **Just-in-time, single-use registration is
deferred** within it: GitHub's repository JIT registration needs a credential
with repository **Administration: write**, which must not live on the bench
or in the guest. It waits until an external trusted provisioner owns that
credential and the host owns the persistent fence; only the one-use
configuration then enters the guest.

### 5.7 Trusted harness, untrusted candidate

- **The harness and verdict logic live in the private repo** and run as
  `panio-ci`. Nothing from the candidate checkout decides a verdict.
- **The candidate supplies binaries only.** `bench-exec build` builds, as
  `panio-job` and with each crate's own lockfile, the CLI **and every helper
  in the fixture's actuation chain** (for example `ch9329` for a KVM-Go, or
  `usbtoggle` and the relay helper it calls for a sharing switch).
- **Helpers by absolute path.** panio puts its installed helper directories on
  `PATH` ahead of anything the harness sets, so a fresh build alone doesn't
  guarantee the candidate's helper runs. The private test configuration
  therefore names the run-specific binaries by **absolute path**, including a
  nested relay helper. A missing candidate helper fails setup (N29). The
  known-good recovery binaries and their configuration are installed
  separately and are never used for a verdict.
- **Witness SSH is narrow.** The witness's `authorized_keys` entry uses
  `restrict,command="/usr/local/bin/witness-agent"`. The agent accepts only
  `drive-list`, `drive-mount`, `drive-unmount`, `nonce-read`,
  `nonce-write <hex>`, and later `evdev-capture <seconds>` and
  `show-nonce <hex>` (phase 3), validates their arguments, and does nothing
  else. The key belongs to `panio-ci`, so candidate code can't use it.
- **Bench-side drive access** goes through fixed subcommands of
  `bench-state` (`drive-mount`, `drive-unmount`, `nonce-read`,
  `nonce-write`) that accept only the CI media's recorded identity (§6). CI
  callers need the live admission; the maintenance holder reaches them
  through the gated maintenance path (§7).

### 5.8 Results, logs and publication

- **Masking isn't a confidentiality boundary.** Raw logs and artifacts stay
  private in `panio-hardware-ci`, with bounded size and a short retention.
- **Phase 1: publishing is manual.** The run summary is a short structured
  table (test, verdict, device role, firmware version, control SHA, candidate
  SHA) with no paths, serials or addresses.
- **Phase 2: a separate reporting repository.** A repository secret isn't
  limited to the workflow that's meant to use it: any workflow in that
  repository can reference it. So the issue-writing credential doesn't live in
  `panio-hardware-ci` at all. It lives in a second private repo,
  `panio-hardware-report`, that has **no self-hosted runner and no other
  workflows**. That separation protects the credential from the bench; on the
  reporting side, the control is the repository's writer and workflow
  inventory, which N25 audits.
  - **Credentials there:** a GitHub App key with **Issues: write** on public
    panio only (no contents, workflow or administration permissions), and a
    fine-grained read-only token with **Actions: read** and **Contents: read**
    on `panio-hardware-ci`. The bench never holds either.
  - **Trigger:** `reporter.yml` runs on a schedule (**daily**) and *pulls*
    completed runs. Nothing in the bench repo can dispatch it.
  - **Binding, two SHAs validated separately.** The run's metadata identifies
    the **control** commit, not the public candidate. So the reporter binds
    the repository, workflow path (`.github/workflows/hardware.yml`), ref,
    event, run id, attempt and **control SHA** to GitHub's metadata for the
    run; selects exactly one expected result artifact for that attempt; and
    **derives the candidate SHA itself** from the manifest at the control SHA
    (§5.3), rejecting an artifact whose candidate differs.
  - **Replay and retries.** Publication is serialized. Each intended issue
    operation has a durable key made of run id, attempt and a stable finding
    key, kept as a marker in the issue body. On retry, the reporter
    reconciles each operation against its marker and performs only the
    outstanding ones, within the publication limit. A completed publication
    replayed makes zero new writes.
  - **Handling the data:** the artifact is parsed with a JSON parser against a
    fixed schema (bounded size, enumerated test names and verdicts, no
    free-text fields). Values reach the GitHub API only as structured fields,
    never interpolated into shell commands or `${{ }}` expressions. No
    candidate code is fetched or run. Issues use fixed templates, are
    deduplicated by a stable key, and are rate-limited (at most three writes
    per run).
  - Its writers are trusted the same way as the bench repo's: you only. A
    repository secret is available to any workflow a trusted writer later
    adds there, so an unexpected writer or workflow fails N25's audit.

## 6. How each test verifies the outcome

Rules every test follows:

- **Act through panio, check through the witness.** Never use panio to check
  panio's own work.
- **Both the command and the outcome must be right.** Require the exit status
  specified by the test and the independent outcome it specifies. An
  actuation expected to succeed must exit zero: a non-zero exit is a **fail**
  even if the hardware did the right thing (a CLI regression), and exit 0
  without the physical outcome is a **fail** too. The unsupported-device test
  must return its documented refusal.
- **Fresh nonce per run** (128 random bits, hex), and a distinct nonce per
  direction.
- **Both directions** wherever there are two.
- **Four verdicts:** `pass`, `fail` (product failure), `infra-error` (bench or
  harness problem), `skip` (device not available). A healthy observation that
  contradicts the command is `fail`. Losing the witness, or being unable to
  establish the media's identity, during a transition is `infra-error`, and it
  fails coverage. It is never reported as a product regression.

### Phase 1: USB round trip (detects false-success switching, the #417 class)

This runs on the phase 1 mux (KVM-Go or a sharing switch). It detects a
helper that reports a switch that didn't happen, on that device. It does
**not** cover the Mini-KVM, whose coverage starts with its own test in phase
2.

**Media identity.** At setup, the harness enrolls the identity of the CI media
that is actually observable on **both** endpoints: for a USB drive, vendor and
product IDs, USB serial number and filesystem UUID; for KVM-Go microSD, what
its reader exposes on each side plus the filesystem UUID. Every "appears" and
"disappears" check below matches that full identity, not the label alone.
Automount is disabled on both bench1 and the witness. The identity stays in
the private lab config and never appears in the published summary.

1. **Setup.** CI admission or operator maintenance held, dirty marker set
   (§7). The mux is on the host side, verified by enrolled identity on bench1
   and absence on the witness.
2. bench1 mounts the media, writes `nonce1`, then **flushes and unmounts** it.
3. `panio usb attach-target` must **exit 0**.
4. **Independent checks:**
   - the media (by identity) **disappears** from bench1 within 10 s;
   - the media **appears** on the witness within 15 s;
   - the witness mounts it, and `nonce-read` returns exactly `nonce1`;
   - `panio usb state` exits 0 and reads `target`.
5. The witness writes `nonce2`, then **flushes and unmounts** the media
   (`drive-unmount`) before anything switches back.
6. `panio usb attach-host` must exit 0. Then the mirror image of step 4: gone
   from the witness, present on bench1, and `usb state` reads `host`. After
   confirming the enrolled media has returned, bench1 mounts it, reads
   exactly `nonce2`, unmounts it and **confirms the final unmount** before
   the run ends.

| Situation | Verdict |
|---|---|
| Every command exits 0 and every check in both directions holds | pass |
| Exit 0 but the media didn't move, or `usb state` disagrees with a healthy witness (N07) | **fail** |
| Non-zero exit even though the media moved correctly (N23) | **fail** (CLI regression) |
| Old nonce, missing file, wrong identity on a healthy witness, or only one direction done (N08) | fail; never pass |
| Witness lost during a transition, media identity can't be established, or media not visible anywhere at setup (N31) | infra-error; fails coverage |

If a recovery retry follows (phase 2, §8), the original failure's evidence is
kept. A physical recovery pass never erases a CLI regression.

### Later tests

| Test | Phase | Action through panio | Independent check | Would catch |
|---|---|---|---|---|
| **Unsupported device refuses** | 2 | `usb attach-target` on the Mini-KVM while no helper supports it | Must exit non-zero with an "unsupported" error. Exit 0 is a fail. Replaced by the round trip once a helper lands | #417 |
| **Keyboard** | 2 | `hid send type <nonce>` via JetKVM | `witness-agent evdev-capture` on the KVM's HID gadget; the key sequence must spell `nonce` | — |
| **Mouse move** | 2 | `hid send move`, `hid send moveabs` | evdev shows `REL_X`/`REL_Y` (or `ABS_*`) with the right signs | — |
| **Scroll** | 2 | `hid send scroll -200`, `scroll 3`, `scroll 0` | evdev shows `REL_WHEEL` with the right sign and total, and nothing for 0 | #415 |
| **Power** | 2 | Relay off, then on (commands must exit 0) | See below | — |
| **Video** | 3 | `video` capture plus OCR | `witness-agent show-nonce <hex>` puts the fresh nonce on `tty1` in a large font; OCR of the capture must find it, and a stale frame must fail | — |
| **Firmware recorded** | 2 | `doctor` / JetKVM `/version` | Not a verdict; written to the summary ([§9](#9-firmware-channels)) | — |

**Power: three separate things, kept separate.**

| Signal | What it shows | Role in the verdict |
|---|---|---|
| Commanded state (what panio asked for) and the relay's readback | That the relay *thinks* it switched | Required to match, but never enough alone |
| **Metered power** from the metering plug in series with the target's supply | That electrical power actually stopped and resumed | **Required:** below 0.5 W for at least 5 s after off, and back above the target's idle draw after on (thresholds calibrated in step 2.3) |
| Witness reachability and `boot_id` | That the target went down and **rebooted** | Supporting evidence only. A changed `boot_id` proves a reboot, not that power was ever interrupted (a watchdog reset also changes it) |

Pass needs all of these: commands exit 0, the readback matches, metered power
drops and returns, and the witness comes back with a new `boot_id`. A metered
drop without a readback match, or the reverse, is a fail. Ping loss alone
never counts as power loss.

## 7. Leases, cleanup and crash recovery

Phase 1, all of it. Hooks and `if: always()` steps are best effort: a power
loss, kernel crash or forced kill can skip both. So correctness never depends
on them.

**The host gate.** `bench-state` (root) is the single authority over who may
touch the fixture. It holds one host lock, and under it keeps four durable
facts on persistent storage under `/var/lib/bench/`: the active admission's
identity, **maintenance** ownership, the **dirty marker**, and the
**quarantine** flag. Every change to them is fsynced, including the parent
directory, on creation and on removal. An admission, maintenance, a dirty
marker or quarantine each blocks a new admission and a new maintenance
session. None of them depends on a panio lease, and **lease expiry never
clears any of them**. That is the durable fence: panio caps a lease at 24
hours and supports preemption, so a lease can't keep anyone off an uncertain
bench; the host gate does.

**Dirty marker.** `bench-state` writes the marker, naming the attempt, the
devices it may touch and their expected resting states, **before granting
device access to any candidate process**, the build included. Only
`bench-state clear` removes it, and only after the root verifier has
independently confirmed the resting state.

**Resting state** (what the verifier checks): mux on the host side; the CI
media present on bench1 with its full identity, unmounted, and absent from the
witness; for `usbtoggle`, the relay contact **open** (an interrupted
`usbtoggle` press can leave it closed) as well as the media position; target
power on (phase 2); no held HID keys; an empty attempt slice and no
`panio-job` processes.

**Recovery and handoff, serialized under the host lock:**

1. revoke new launches for the old attempt;
2. stop and drain every process from the old attempt;
3. safely unmount the media on **both** endpoints;
4. restore the fixture with the known-good tooling (§5.7);
5. verify the resting state;
6. finish panio lease retirement, as `panio-job` (§5.6);
7. clear the marker and the admission.

An incomplete transition leaves the bench fenced across reboot, lease expiry
and listener downtime. If any step fails, or can't run, `bench-state` sets
**quarantine**, and only you clear it by hand after inspecting the bench.

**Startup reconciliation.** At boot and listener startup, the root service
(`bench-precheck`) reconciles the durable admission identity and the dirty
marker under the host lock, including when `/run` is empty. It revokes and
drains an abandoned admission. If the marker is present, it performs the full
recovery sequence above. It retires the abandoned identity only after
verifying a safe handoff. An incomplete reconciliation keeps the fence
(quarantine), and the listener doesn't start. Reconciliation preserves
maintenance ownership and quarantine.

**Maintenance handoff.** People use the bench only through `sudo bench-state
maintenance begin` and `… end`, and a gated command path that runs panio as
`panio-job` while maintenance is held. There is no unrestricted login as
`panio-job` and no `sudo -u panio-job` outside maintenance.

- `maintenance begin` and `admit` are mutually exclusive under the host lock.
  `begin` refuses while an admission, a dirty marker or quarantine exists, so
  you wait for a run to finish and be verified. While maintenance is held,
  admission refuses with "bench busy" and nothing actuates (N10).
- While maintenance is held, the operator's gated path can run the
  supervised harness, including its build, candidate and fixed drive verbs.
  It uses the reviewed control and candidate SHAs, fresh bounded scratch, the
  same launch limits and the same dirty-marker ordering as CI. Only the
  maintenance holder can invoke it. It creates no CI admission. Step 1.7 uses
  it.
- Maintenance ownership persists across reboot.
- `maintenance end` drains your processes and runs the same root
  resting-state verification as a CI run. If verification fails, the fence
  stays (maintenance remains set, or quarantine), and CI stays out.

**panio leases.** Where a test exercises panio's own leases, the harness
acquires the lease as `panio-job` via `bench-exec` with a TTL of the job's
timeout plus a margin (45 minutes), renews it every 5 minutes, and stops
actuating if a renewal fails. **Fencing** (every actuating command carries the
token and is rejected if the token has expired or been superseded) is a
requirement on panio's lease implementation, tested within the one account
(N22). If a piece is missing, it becomes a panio issue before phase 1 runs
unattended. The lease is evidence that panio works, not the bench's
exclusivity mechanism.

**Timeouts.** Each hardware step has a hard timeout (default 120 s), budgeted
separately from the cold build (§5.6); the job has `timeout-minutes: 30`; the
attempt slice has its own runtime limit; and the independent root deadline
(§5.6) drains the attempt if the listener is gone.

## 8. Flaky and offline devices

- **Preflight.** The witness and every device are checked first. An
  unavailable device marks its tests `skip`.
- **All-skipped isn't green.** A run where every test for a device, or the
  whole run, was skipped, or where the witness was absent, reports **missing
  coverage** and fails the workflow (N09).
- **Bounded recovery (phase 2).** On a failure or a device that doesn't
  respond, power-cycle that device once, wait up to 90 s, and rerun that one
  test once. A pass on the retry is reported as **flaky**, never as pass, and
  the first attempt's evidence is kept. There are no further retries.
- **Escalation (phase 2, through the reporter):** a device skipped on 2
  consecutive nightly runs gets an issue `lab: <role> offline`; a test flaky 3
  times in 7 days gets `lab: <test> flaky`.
- **Runner offline or stuck (phase 2).** `watchdog.yml`, on a hosted runner in
  the bench repo, tracks **last completed coverage** separately from **last
  passing result**. Last completed coverage advances when the required
  observations yield a product verdict, including a confirmed product
  failure. Last passing result advances only on a pass. Missing required
  coverage, including witness loss or unestablishable identity during a
  transition, leaves the coverage clock unchanged. It fails (which emails
  you) after 36 hours without completed coverage: an offline runner, a runner
  GitHub stopped sending jobs to for being out of date, a stuck queue,
  cancelled runs, quarantine, or runs that lose coverage mid-transition
  (N17).

## 9. Firmware channels

| Device | Channel | Policy | A failure means |
|---|---|---|---|
| `jetkvm-stable` | Latest stable release, updated by hand after reading the release notes | Counts toward the advisory release check | A panio regression, or a stable firmware break that needs a fix before the next release |
| `jetkvm-dev` (phase 3) | JetKVM's dev update channel, auto-updating | Non-blocking; a warning in the summary | An upcoming break; the reporter files `firmware-dev` with old and new versions |
| Openterface KVM-Go / Mini-KVM | Pinned at the installed version; updated by hand | Counts toward the advisory release check | A panio regression (firmware is pinned) |

Every run records every device's firmware version. When a test goes from pass
to fail, the summary says whether firmware changed since the last pass.

## 10. Phased rollout

Estimates are your hands-on time, and they include the time to run each
step's gates. "Claude" steps are ones a cloud session can write as a PR to the
private repo for you to review and merge; the review is included in the
estimate. Each step must pass the listed gates
([§13](#13-negative-acceptance-gates)) before the next step starts.

### Phase 1: one contained hardware test, about 10–20 hours hands-on

The rows total 10¼–18¾ hours, rounded in the heading. Step 1.1 is complete
with zero time remaining. Hardware costs are $0–25 with the listed gear
already owned, or about $330 with only the supported mux owned, subject to the
upstream-router condition in §4. The range is wide because isolation depends
on what your router can do. The
estimate assumes a supported fixture you already own, a dedicated Linux
bench, and no panio product change. The order answers the cheapest decisive
questions first: a supervised, listener-off round trip (1.7) proves the media
can be identified on both ends, the candidate helper moves it, and known-good
recovery restores it, before any runner is registered.

| # | Step | Who | Time | Gates |
|---|---|---|---|---|
| 1.1 | Private `panio-hardware-ci` created with read-only workflow permissions (**done** Nov 25; its settings are verified in 1.8) | You | — | — |
| 1.2 | Add `hardware.yml` (dispatch only, **no schedule yet**), the harness with candidate helper paths, `approved/candidates.txt` with one entry, and the admission policy | Claude drafts; you review and merge | 1–2 h | — |
| 1.3 | Isolate the bench network with explicit upstream deny rules; set up the HTTPS and router canaries, confirm they answer from the home LAN, and probe them from an ordinary device on the bench segment | You | 1–3 h | N06 |
| 1.4 | bench1 base install. Install the OS, systemd and nftables. Create `panio-ci` and `panio-job`, grant fixture devices only to `panio-job` (udev rules), install the read-only toolchain and deploy key, and install the known-good panio binary and fixture helpers. Smoke-test those binaries as `panio-job` without actuating the fixture. Step 1.7 performs the first recovery | You (Claude drafts the files) | 1.5–2.5 h | N06 |
| 1.5 | Witness: `witness-agent` behind a forced-command key. Disable automount on both endpoints and prepare the private media identity record; the enrolled identity is confirmed in both positions during gated maintenance in 1.7 | You (Claude drafts the agent) | 45–90 min | N06 |
| 1.6 | Install the root-owned hook, `bench-precheck`, `bench-exec`, `bench-state` (admission, maintenance, marker, quarantine, verifier, drive subcommands, deadline) and the systemd units. **With the listener stopped**, run the local refusal tests using crafted environments and event files, including other-actor cases, and the one-account lease test | You (Claude drafts all scripts and a local test runner) | 2–4 h | N02–N05, N14, N19, N22, N26 |
| 1.7 | **Supervised round trip, listener off.** Enter through `maintenance begin`. First perform fixture recovery with the known-good tools while no candidate workspace exists, and require the independent verifier to confirm the resting state. Then run the harness by hand against the approved SHA, record cold-build time and peak disk, and `maintenance end` | You | 1–1.5 h | N07, N08, N23, N29, N31 |
| 1.8 | Register the runner to the **private repo only** with `--disableupdate` and label `panio-hw`; verify from GitHub settings that it's attached to that repo only and that the repo has no other writers. Run hardware-free dispatches, positive and negative | You | 45–60 min | N01, N03, N19, N26 |
| 1.9 | First unattended dispatch on the approved SHA, then the drills: a stub helper that exits 0 without switching, the witness unplugged, maintenance held, a cancel mid-switch, a **hard power-off of bench1 mid-switch** followed by reboot, and an interrupted recovery | You | 2–3 h | N07, N09, N10, N11, N13, N20, N21, N27, N28 |
| 1.10 | Reconcile the gate inventory, add the nightly schedule, and check which entry it selects. Only once every gate above has passed | You | 15 min | all phase 1 gates |

**Stop rule:** any failed security gate, or uncertain hardware state after
cleanup, means stop the listener and quarantine until it's understood.

### Phase 2: JetKVM, power, reporting, disposable execution (about 10–16 hours hands-on over two weeks if all listed steps proceed, including disposable execution; $230–325 if everything is bought)

| # | Step | Who | Time | Gates |
|---|---|---|---|---|
| 2.1 | Buy and cable the JetKVM (stable), relay, metering plug and recovery plug | You | 1 h | — |
| 2.2 | `evdev-capture`; keyboard, mouse and scroll tests; a network restriction on the build | Claude | 45 min review, 20 min install | — |
| 2.3 | Power test with commanded, readback and metered signals; calibrate meter cadence and the on/off thresholds | Claude drafts; you calibrate | 1–2 h | N24 |
| 2.4 | Bounded recovery, flaky verdict, firmware versions | Claude | 30 min review | — |
| 2.5 | Disposable execution (VM, container or overlay root) rebuilt per job, with the fence kept on the host; JIT registration only once an external provisioner owns its credential (§5.6) | You (Claude drafts) | 4–8 h | N14, N18 |
| 2.6 | Create private `panio-hardware-report`; the GitHub App (Issues: write, public panio only); a read-only token (Actions and Contents: read); `reporter.yml` with SHA binding and idempotent publishing | You create the repo, App and token (30 min); Claude writes the workflow | 2–3 h | N12, N15, N16, N25, N30 |
| 2.7 | `watchdog.yml` | Claude | 15 min review | N17 |
| 2.8 | Mini-KVM "refuses cleanly" test while #417 is open | Claude | 15 min review | — |

### Phase 3: firmware channels, video, advisory release check (about 3½–4½ hours hands-on, $70–110)

| # | Step | Who | Time |
|---|---|---|---|
| 3.1 | Second JetKVM on the dev channel | You | 45 min |
| 3.2 | Device matrix with per-channel policy; `firmware-dev` issues via the reporter | Claude | 30 min review |
| 3.3 | Video + OCR test, including the witness's `show-nonce` verb and a stale-frame check | Claude drafts; you tune | 1.5–2.5 h |
| 3.4 | Advisory release check in `RELEASE-TRAIN.md`: "the release commit is in the manifest and its hardware run is green on stable devices". Not a gate in `release.yml` | Claude | 20 min review |
| 3.5 | Once #428 merges, approve that commit and confirm the dev-channel JetKVM exercises the new binary `hidrpc` scroll path | You | 15 min |

### Ongoing

- Runner updates per §5.4: about 30 minutes, within 14 days of each release.
- Approving new commits: about 2 minutes per manifest entry, once the
  candidate's review has already happened.

## 11. Decisions and open questions

| # | Question | Decision |
|---|---|---|
| D1 | Run hardware tests on every PR? | **No.** |
| D2 | Where does the runner register? | The private `panio-hardware-ci` only. |
| D3 | What approves a commit? | An entry in `approved/candidates.txt` in the private repo. The last entry runs. Being an ancestor of `main` is a sanity check, not approval. |
| D4 | Who is trusted to change what runs? | Writers of the two private repos: only you. Adding anyone means revisiting this document. |
| D5 | Unrestricted HTTPS egress? | **Accepted** as residual risk in phase 1, with no valuable secrets on the bench. An allowlisting proxy is the option if that changes. |
| D6 | Pre-merge testing (e.g. a PR branch like #428's)? | **Deferred.** It needs disposable execution (phase 2), a manifest entry naming the exact SHA, and acceptance that unreviewed code can drive the hardware during that run. |
| D7 | Hard release gate in `release.yml`? | **No.** Advisory first; revisit after a month of stable nightly runs. |
| D8 | Pi or x86 for bench1? | Either. An N100 mini PC builds Rust about 2–3x faster and makes phase 2's VM-per-job easier. |
| D9 | How do people and CI share the bench? | They don't, concurrently. The bench is dedicated to CI; people use it through the `bench-state` maintenance handoff (§7). |
| Q1 | Can your existing bench double as the CI bench? | Only if it can move onto the isolated segment and be dedicated to CI, with manual work through maintenance. |
| Q2 | Which mux is the phase 1 device: KVM-Go or `usbtoggle`? | Whichever is already cabled. |
| Q3 | Put the harness on a second machine, apart from the candidate? | Not planned. It's the only full defense against a hostile candidate; revisit with D6. |
| Q4 | Concurrent manual use of a CI bench? | Out of scope. It needs a lease authority shared across Unix users, and a budget, before N10 or N22 could cover it. |

## 12. Review findings and where they are addressed

### First review (revision 1 → 2)

| Finding | Summary | Addressed in |
|---|---|---|
| SEC-01 | Hook installed after registration | §10 steps 1.6 → 1.8; listener won't start without the hook (§5.4) |
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
| R2-2 (high) | Cleanup can't depend on hooks; lease handoff | §7: persistent dirty marker, startup reconciliation; revised in revision 4 (host gate instead of lease handoff). N21, N27 and N28 cover crash recovery and handoff. N22 checks panio's token within one account |
| R2-3 (high) | Reporter secret scope | §5.8: separate hosted-only `panio-hardware-report`, pull model, run binding, replay rejection, no shell interpolation. N25 audits credential placement, writers, workflows and runner inventory. N15 and N30 cover input handling, binding and replay |
| R2-4 (medium) | Public HTTPS is an exfiltration path; NAT isn't containment | §5.5: explicit upstream deny rules, egress accepted as residual risk with no valuable secrets, canaries that really answer; D5 |
| R2-5 (medium) | `--disableupdate` needs a policy | §5.4: owner, monitoring, deadline, procedure |
| R2-6 (medium) | Same-UID interference; indiscriminate cleanup | §5.6: separate `panio-job` user, per-attempt slices via `bench-exec`, verification from outside the slice; trust model stated up front |
| R2-7 (medium) | Verdicts need the command result; unmount, identity, power | §6: exit codes required, witness-side unmount, full device identity, metered power separate from the readback, `boot_id` as supporting evidence only; gates N23, N24 |

### Revision 3 review and consensus review (revision 3 → 4)

R3-a to R3-d come from the design review posted on PR #429 at revision 3. C1
to C16 are the change list agreed in a consensus review with a second model,
which also covered R3-a to R3-d. Neither review executed any gate.

| Finding | Summary | Addressed in |
|---|---|---|
| R3-a, C1 | Admission couldn't bootstrap: the hook runs as `panio-ci`, and both sudo helpers needed an existing record. Recovery had the same gap after reboot | §5.3: `bench-state admit` as the only creator; §5.4: three sudo rules; §7: recovery from the root service with an empty `/run`; gate N26 |
| C2 | Admission needs authenticated read access to the private repo | §5.3: root-held read-only deploy key, pinned host key, one fetch for head and manifest; §5.2, §5.5 |
| C3 | Candidate selection must be derivable from the control commit | §5.3: last manifest entry, parsing rules, no dispatch SHA input |
| C4 | Workspace, crate cache and build placement | §5.6: per-attempt disk scratch, `/run` for small files only, `cargo fetch --locked`, build without USB fixture access in phase 1 |
| C5 | A green result could exercise an installed helper | §5.7: build every helper in the chain, absolute paths, known-good recovery binaries kept separate; gate N29 |
| C6 | `systemd-run` is asynchronous; `PrivateTmp` splits runtime dirs | §5.6: `--wait --pipe --collect`, `KillMode=control-group`, one-shot verbs in phase 1, `PANIO_RUNTIME_BASE` per attempt, independent deadline; gate N29 |
| R3-c, C7 | panio's lease store is per Unix user, so CI, candidate and manual callers shared no lease | §5.6, §7: dedicated bench, one host gate, all fixture panio as `panio-job` with one `HOME`, maintenance handoff; D9, Q4; gates N10, N22, N27 |
| R3-d, C8 | A finite lease can expire during a long crash | §7: durable host fence independent of lease expiry, marker before any device-capable process, serialized recovery, relay-open check; gates N21, N27, N28 |
| R3-b, C9 | Reporter compared the candidate SHA with run metadata, which names the control commit | §5.8: control SHA bound to metadata, candidate derived from the manifest, Contents: read, idempotent publishing, daily schedule; gates N25, N30 |
| C10 | Phase 1 overstated #417 coverage; KVM-Go media; witness loss isn't a product failure | §1, §4, §6: false-success class on the named fixture, microSD media, two-endpoint identity, automount off, final unmount, `infra-error`; gate N31 |
| C11 | Canaries could pass without testing the upstream rule | §5.5: HTTPS canary, ordinary-device probe, same-segment and IPv6 checks; §8 watchdog coverage vs. pass |
| C12 | Gate inventory couldn't run as scheduled | §13: N04 local only, N12 moved to 2.6, N10, N19, N22 and N25 redefined, N26–N31 added; §10 gate columns |
| C13 | JIT registration needs an Administration: write credential | §5.6: deferred until an external provisioner owns it; step 2.5 |
| C14 | Estimates were low; missing base-install step; registration came before a supervised run; recovery was tested before its prerequisites existed | §10: revised times and phase totals that match their rows, step 1.4 includes the base install, new step 1.7 performs the first verified recovery |
| C15 | Cost headlines | §2, §4: phase 1 owned/not-owned cases, phase 2 total, no quoted allowance figure |
| C16 | Bookkeeping | This table, §13, status line |
| Document check | The second model then checked revision 4 against C1–C16 and found places where the text didn't yet implement them: the supervised run had no authority under maintenance, an admission could be stranded before its marker, steps 1.4–1.5 came before their prerequisites, N06 and N26 could pass for the wrong reason, the watchdog counted lost coverage, and the reporter's idempotency key didn't match its write limit | §5.3, §5.4 (CI-scoped admission, SSH on 443 for the deploy key); §5.5 (layer evidence, required probes); §6 (exit rule, setup and final unmount); §7 (maintenance harness path, admission reconciliation); §8 (coverage clock); §5.8 (per-issue keys); §10 steps 1.4–1.5; gates N06, N10, N17, N26, N28, N30 |

## 13. Negative acceptance gates

Each gate is a deliberate attempt to make the wrong thing happen, with the
required result. "Step" lists every rollout step in [§10](#10-phased-rollout)
that exercises the gate. Each step must pass its assigned portion before
rollout continues. Gates that need another GitHub actor run locally with crafted
inputs: never add a writer, or attach a runner to public panio, to exercise a
gate. None has been executed yet.

| Gate | Attempt | Required result | Step |
|---|---|---|---|
| N01 | A public fork workflow asks for `panio-hw` | No runner assigned (verified from GitHub settings: the runner is attached to the private repo only) | 1.8 |
| N02 | Hook missing or altered | Service refuses to start | 1.6 |
| N03 | Wrong workflow, event, ref, or a workflow commit that isn't the private `main` head | Rejected before any workflow step | 1.6, 1.8 |
| N04 | Re-run triggered by another actor (crafted event, run locally) | No candidate code executes | 1.6 |
| N05 | A job tries to modify the hook, policy, units, helpers or runner files | Denied by file ownership | 1.6 |
| N06 | 1.3: confirm the HTTPS and router canaries answer from the home LAN, then probe them from an ordinary device on the bench segment (upstream denials). 1.4: probe from bench1, with host firewall counters or logs, and probe prohibited same-segment management paths (host denials). 1.5: use the required witness SSH path. For each applicable probe, test IPv4 and IPv6, or record evidence that IPv6 is disabled | Both enforcement layers are demonstrated, prohibited paths are blocked, and required witness traffic works | 1.3, 1.4, 1.5 |
| N07 | panio exits 0 without the USB moving | Product failure | 1.7, 1.9 |
| N08 | Stale nonce, wrong media identity, or incomplete round trip | Never a pass | 1.7 |
| N09 | Witness absent, or everything skipped | Missing coverage; the run fails | 1.9 |
| N10 | Request admission while maintenance is held | "Bench busy"; no device actuation | 1.9 |
| N11 | Cancel during actuation | Verified cleanup, or quarantine | 1.9 |
| N12 | Any bench credential tries to change a public panio issue | Denied | 2.6 |
| N13 | Sensitive diagnostic data in the output | Stays in private logs; not in the summary | 1.9 |
| N14 | Unexpected persistent state between jobs | Next execution blocked | 1.6, 2.5 |
| N15 | Malformed, oversized or replayed reporter input | Rejected or skipped; no command execution | 2.6 |
| N16 | Reporter credential tries contents, workflow or admin changes | Denied by its permissions | 2.6 |
| N17 | Runner offline, out of date, queued, cancelled, an all-skipped run, or a completed run with required coverage lost mid-transition | Watchdog reports missing completed coverage | 2.7 |
| N18 | State planted in the disposable environment | Absent from the next job; otherwise quarantine | 2.5 |
| N19 | In a local admission test, supply a reachable public SHA (for example a direct-pushed or intermediate commit) different from the selected manifest entry through a legacy `sha` event field; then an empty manifest, and a malformed final non-comment entry. In the hardware-free GitHub dispatch, compare the recorded candidate with the manifest | The supplied SHA never becomes the admitted candidate; an empty or malformed manifest refuses admission, with no fallback to an earlier line; the recorded candidate equals the final entry at the admitted control SHA | 1.6, 1.8 |
| N20 | Public `main` moves between admission and checkout | The run builds and reports the admitted SHA; `HEAD` check passes only for that SHA | 1.9 |
| N21 | Hard power-off of bench1 mid-actuation, then reboot | Dirty marker forces reconciliation before any job or maintenance; on failure, quarantine | 1.9 |
| N22 | Actuate as `panio-job` with an expired or superseded lease token | Rejected by panio (fencing) | 1.6 |
| N23 | Helper switches the USB correctly but exits non-zero | Fail (CLI regression) | 1.7 |
| N24 | Relay readback says off, but the metering plug still shows power (or the reverse) | Fail; `boot_id` change alone never passes | 2.3 |
| N25 | Audit where the issue-writing credential exists, and the reporting repository's runners, writers and workflows | The credential exists only in the reporting repository; the repository has no self-hosted runner; its writers and workflows match the approved inventory. An unexpected writer or workflow fails the audit. (N12 separately checks that bench credentials can't write public issues) | 2.6 |
| N26 | In 1.6, independent local cases, each from an empty `/run` and a clean durable gate: private-read access unavailable with otherwise valid inputs; before admission, `bench-exec`, each CI drive verb, and refusal logging; a valid case that admits, then a second admission and a job verb carrying a stale attempt. In 1.8, the positive case again by hardware-free GitHub dispatch | Without private read, admission refuses, creates no active record and grants no device access. Before admission, `bench-exec` and the drive verbs refuse while refusal logging succeeds. The valid case admits once and rejects the second admission and the stale attempt. 1.8 shows exactly one admission | 1.6, 1.8 |
| N27 | With distinct operator and CI identities: hold maintenance and request CI, then the reverse; repeat after a panio lease has expired with a dirty marker set; leave a maintenance process running or the media mounted and request `maintenance end`; reboot once with maintenance set | Only the authorized holder reaches the fixture; CI stays out until cleanup and verification pass; maintenance survives the reboot | 1.9 |
| N28 | Interrupt the first device-capable process, then interrupt recovery before the final clear; restart with no admission record. Also reboot after admission but before the dirty marker is created, and check that the abandoned admission is retired safely | Mount state verified on both endpoints, relay open where applicable, and either a verified handoff or continued quarantine | 1.9 |
| N29 | Make an installed helper available while withholding the candidate-built one; run a one-shot verb with a delayed non-zero exit and a leftover child | Setup fails without the candidate helper; the harness gets the actual status, the unit is empty afterward, and the next verb works | 1.7 |
| N30 | Give the reporter a valid control/candidate pair with different SHAs, then another approved candidate, an earlier attempt's artifact, and a retry after publication. Also interrupt publication after the first of two intended issues and retry | Only the intended pair is accepted, and it's published once; the retry completes the second issue without duplicating the first | 2.6 |
| N31 | Lose the witness during observation, and separately present a stable contradicting hardware observation | Both lose green status: `infra-error` and `fail` respectively | 1.7 |

## 14. References

- [Adding self-hosted runners](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/add-runners)
- [Self-hosted runners reference](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)
- [Managing GitHub Actions settings for a repository](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/managing-github-actions-settings-for-a-repository)
- [Running scripts before or after a job](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/run-scripts)
- [Just-in-time runner configuration (REST)](https://docs.github.com/en/rest/actions/self-hosted-runners#create-configuration-for-a-just-in-time-runner-for-a-repository)
- [Workflow runs (REST)](https://docs.github.com/en/rest/actions/workflow-runs)
- [GitHub Actions billing](https://docs.github.com/en/billing/concepts/product-billing/github-actions)
- [Variables reference](https://docs.github.com/en/actions/reference/workflows-and-actions/variables)
- [`GITHUB_TOKEN`](https://docs.github.com/en/actions/concepts/security/github_token)
- [Secrets](https://docs.github.com/en/actions/concepts/security/secrets)
- [Secure use reference](https://docs.github.com/en/actions/reference/security/secure-use)
- [Create an issue (REST)](https://docs.github.com/en/rest/issues/issues#create-an-issue)
- [`systemd-run`](https://www.freedesktop.org/software/systemd/man/latest/systemd-run.html)
- [panio target leases](../leases.md)
- [panio USB switching](../usb.md)
