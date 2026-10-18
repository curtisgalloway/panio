# Serial agent benchmark — does panio beat YOLO and `fx serial`?

A hardware-in-the-loop, head-to-head benchmark: when an AI coding agent has to
drive a target's serial console during low-level bring-up, does **panio**
produce better task outcomes than the alternatives the agent would otherwise
reach for — improvising its own serial access ("YOLO"), or using the idiomatic
ecosystem tool (`fx serial`)?

This is a different question from [`agent-evals.md`](agent-evals.md), and the two
are complementary:

- **`agent-evals.md`** asks *"can a naive agent discover and correctly form
  panio commands?"* — independent variable is the **discovery condition**
  (Cold/Warm/Preloaded), no hardware, no comparison to non-panio approaches.
- **This doc** asks *"does using panio yield better serial-task outcomes than
  the alternatives?"* — independent variable is the **approach** (YOLO /
  `fx serial` / panio), on a **real board**, across **two agent harnesses**.

Because we are measuring the *value of the tool when used* — not whether the
agent can find it — the panio arm here is run with panio guidance available
(the Warm/Preloaded end of the other doc's axis). Discoverability is the other
suite's job; this one assumes each approach is in hand and asks which wins.

---

## 1. The hypothesis

panio is better for agents managing a serial connection because the agent does
not have to stand up one-off connections repeatedly, can inspect the *history* of
serial output, and can still send and receive interactively. The claim
decomposes into three legs of unequal strength:

| Leg | Claim | panio mechanism | Strength |
|---|---|---|---|
| **L1 — persistence** | No repeated one-off connections; survives a flaky link | Per-target daemon owns the port exclusively, **reconnects on loss** | Strong |
| **L2 — history** | Can look back at output that already streamed by | On-disk JSONL capture (`seq` + `ts_ms`), **recording since before the agent looked**; `serial log --from/--to/--since` | **Strongest** |
| **L3 — interactivity** | Can send and receive in a structured way | `serial send` → daemon `/input`, then poll `serial log` | **Weakest** |

L2 is strongest because the advantage is not "scrollback" — it is that the bytes
which arrived *before the agent decided to connect* are already captured. L3 is
weakest because panio's `send` is fire-and-forget (no request/response
correlation): the agent still sends-then-polls, which a disciplined improviser
can replicate, and the send→poll round trip can even make panio **slower** on
tight-timing tasks. A credible benchmark is built so L3 can tie or lose; a test
that makes panio win everything proves only that it was rigged.

---

## 2. What is under test — the approach ladder

Three arms, a monotone capability ladder. Each rung answers one question.

| Arm | The agent has… | Question it answers |
|---|---|---|
| **A0 — YOLO** | a shell + the device path + baud, **no serial guidance** | Can the agent improvise adequate serial access from scratch? |
| **A1 — `fx serial`** | the idiomatic Fuchsia tool, run *at its best* (always `--output-file`) | Does the purpose-built ecosystem tool beat improvising? |
| **A2 — panio** | the `panio` CLI + daemon already watching + skill pullable | Does panio's persistent-capture architecture beat the idiomatic tool? |

The A1→A2 delta is the real product claim. A1 is steel-manned: we always pass
`--output-file`, because bare `fx serial` would be a strawman (see §9 for why
`--output-file` is the fairest configuration).

> A previously-considered "documented-recipe" arm (best raw-Unix logging hygiene)
> was **dropped**: verification (§9) showed `fx serial --output-file` already
> *is* that capability — a flat session logfile with no pre-capture, structure,
> or reconnect — so it occupies that rung as a real, less-riggable baseline.

---

## 3. Instrument — real board, Fuchsia bring-up, serial-only regime

The board is a real **Raspberry Pi 5** (or vim3) in a Fuchsia bring-up, driven
over its USB-serial console. We chose real hardware over a PTY simulator
deliberately: L1's reconnect/exclusive-ownership advantage is a
*hardware-flakiness* story (hot-unplug, a degrading USB cable), and a simulator
underweights exactly the leg we most want to measure.

Two constraints make the real board tractable and fair:

- **Serial-only regime (required).** Every task lives *before the network/ffx
  link is up* — early boot, a boot hang, a device with no working network. The
  moment the device is reachable over the network a Fuchsia agent will reach for
  `ffx log` and route around serial entirely, and we would no longer be testing
  serial tooling. Pre-network is also the *realistic* bring-up regime where
  serial is the only window.
- **Nonce-injected ground truth.** A real boot log is not deterministic, so we do
  not grade on it. Each trial injects a unique per-trial **nonce** into the boot
  console output (a u-boot `echo`, or a kernel-cmdline print baked into the
  netbooted boot image — panio already serves the boot artifacts). "Report the
  boot token" then has an exact, machine-checkable answer the agent cannot get
  from training data, while the surrounding serial behavior stays fully real.

Board reset and disruption are both **automated** (see §6):

- **Reset between trials:** the pi5's power is a Zigbee plug via `zigplug`, so a
  clean power-cycle is one command.
- **Programmable hot-unplug:** cutting and restoring the USB-serial adapter's
  VBUS would be a scripted hot-unplug that exercises panio's reconnect loop and
  kills a YOLO/`fx serial` reader, and a cycle between *every* trial would force
  clean re-enumeration so no leftover port handle leaks a spurious "resource
  busy" into the next arm. **(No shipped helper does per-port VBUS any more —
  see AGENTS.md on why `usbhub` was removed. A switchable-port hub or a relay in
  the adapter's supply would be needed.)**

---

## 4. Harnesses — Claude Code and Antigravity (`agy`)

Two agent harnesses, run as a factor:

| Harness | Headless invocation | Version observed |
|---|---|---|
| **Claude Code** | `claude -p <prompt>` (+ `--output-format stream-json` for the trace) | — |
| **Antigravity** | `agy -p <prompt>` (`--print`) | `agy` 1.0.10, `/opt/homebrew/bin/agy` |

Running both turns this into a **generalization test**: if panio helps in both,
the claim is robust; if only in one, the effect was partly harness-specific
scaffolding.

**Confound to state, not eliminate.** The two harnesses run different base models
(Claude vs. Gemini-family), so "harness" is really "agent *product* = scaffolding
+ model," bundled. That is acceptable — the real-world question is "should I tell
people to use panio regardless of which agent they run." Report results as
"product X benefits more from panio," never "harness X is worse at serial."
Record the exact `--model` per run for reproducibility.

### 4.1 Enforce arms by environment + context, never by tool gating

The arms do **not** differ in *which tools the agent may call* — every arm needs
shell/terminal access (to run pyserial, `socat`/`fx serial`, or `panio`). What
differs is:

- **Environment** — is `panio` on PATH with its daemon already watching? Is
  `fx` configured against the device? Set this up in the shell *before* launching
  the agent.
- **Context** — is panio's skill/memory/guidance present (A2) or absent
  (A0/A1)?

**PATH caveat (verified, 2026-08-20 pilot #2):** `agy`'s command shell
re-sources the user's login profile, so a PATH prefix injected into `agy`'s own
environment gets *re-shadowed* by profile entries (`/opt/homebrew/bin` won over
an injected shim dir, and the subject silently ran a different `panio` than
intended). To pin which binary an arm sees, don't rely on an exported PATH —
use a shim whose directory the profile itself puts first, or remove/rename the
competing binary for the run, and have the harness log `which <tool>` per
trial as a check.

This matters because the harnesses are asymmetric on tool control: `claude -p`
has fine-grained `--allowedTools`/`--disallowedTools`, but `agy` exposes only the
all-or-nothing `--dangerously-skip-permissions` (no per-tool allowlist). If you
enforced arms via Claude's tool gating you would be using a *different mechanism
per harness* and reintroduce the comparability problem. **Rule: hold the
permission mechanism constant — skip-permissions in both — and vary only
environment and context.** Use each harness's auto-approve purely to stop
permission prompts from hanging the headless run.

### 4.2 The clean-agent rule (contamination is the #1 threat)

Same rule as [`agent-evals.md` §3.1](agent-evals.md): the A0/A1 arms are
worthless if the agent already knows panio from its environment. The
maintainer's own workstation — with the panio checkout, the user `CLAUDE.md`,
and auto-memory that names panio and the bench hosts — is the *most*
contaminated environment possible.

- **Claude Code:** run with `--isolation home` (sandbox `HOME`, no user
  `CLAUDE.md`/memory); fresh project dir that is **not** the panio checkout; no
  prior session history.
- **Antigravity:** the mirror checks — no panio plugin enabled
  (`agy plugin list`), no panio `AGENTS.md`/guidance inside any `--add-dir`
  workspace for the A0/A1 arms. **(To verify: exactly what ambient context `agy`
  auto-loads — workspace `AGENTS.md`? enabled plugins?)**
- **Training-data leakage** caveat applies (panio is public): absolute A0
  numbers read with that caveat, but the **arm deltas** stay valid.

---

## 5. Task battery

Each task isolates one mechanism, has a machine-checkable ground truth, and a
**pre-registered** predicted outcome (committing predictions up front is what
makes this a test rather than a demo).

| Task | Mechanism | Ground truth | A0 YOLO | A1 `fx serial` | A2 panio |
|---|---|---|---|---|---|
| **T-pre — pre-arrival nonce** | L2 capture-before-look | nonce printed during a boot that predates the task | likely miss | **structurally cannot** (no capture before attach) | **win** (daemon already logged it) |
| **T-async — delayed token** | L1/L2 continuous listen | token at boot + ~8 s, no prompt | miss if one-shot read | win *iff* attached in time | win |
| **T-scroll — deep scrollback** | L2 structured history | needle near line ~1200 of a 5000-line flood, asked later | only if it logged to a file | grep flat file (clumsy, only if `--output-file` set up front) | win (`log --from/--to`) |
| **T-drop — hot-unplug + reconnect** | L1 resilience | token emitted right after a scripted VBUS drop/restore | reader dies, stays dead | socat dies, logfile freezes | **win** (supervisor reconnects) |
| **T-timer — autoboot window** | L3 interactivity (falsifier) | send a keypress within a ~2 s interrupt window to unlock a secret | possible | possible (but pty TUI to drive live) | **may lose** (send→poll latency) |

T-timer is kept precisely because it can falsify L3. The pure
command/response task was *dropped* — it is the tie case, and a real board tests
it worst (a PTY simulator would be the better instrument for it).

---

## 6. Automation harness

One scripted trial = one `(harness, arm, task, trial-index)` cell:

```
for trial in cells (randomized arm order — see §8):
    zigplug off ; zigplug on              # clean board reset
    inject per-trial nonce into boot artifacts
    set up arm environment:
        A0: shell + device path; NO panio, NO fx config, clean context
        A1: `fx` configured; always use `fx serial --output-file <log>`
        A2: `panio serial watch <target>` already running; skill pullable
    launch agent headless:
        claude -p  <task> --output-format stream-json  --dangerously-skip-permissions
        agy    -p  <task> --log-file <trace>           --dangerously-skip-permissions
    (T-drop only) at a log-line trigger, cut then restore the adapter's VBUS
    capture: stdout (final answer)  +  trace (tool-call events)
    grade: final answer vs. nonce  →  pass/fail
    scrape: behavioral metrics from the trace
```

Two harness-specific capture notes:

- **Grading is symmetric** — the final answer is on stdout for both; compare to
  the nonce.
- **Behavioral metrics are asymmetric** — the tool-call trace lives in
  `--output-format stream-json` (Claude Code) vs. the `--log-file` (agy). One
  parser per harness; mechanical but real work.
- **Timeouts:** `agy --print-timeout` defaults to **5 min**. Set it generously
  and *uniformly* across arms/harnesses — a tight timeout differentially kills
  the *slower* arms (YOLO/`fx serial` must power-cycle and watch a full boot,
  while panio answers from already-captured output), which would measure "the
  harness gave up," not "the agent failed." Track wall-clock separately.

---

## 7. Metrics

- **Primary — task success** (binary, vs. the injected nonce). Headline.
- **Behavioral** (scraped from the trace):
  - count of `busy` / `resource busy` errors,
  - tool **thrash** (number of distinct approaches/tools tried),
  - hung or timed-out commands; whether it resorted to a power-cycle to recover,
  - **A1-specific:** did the agent successfully *drive the `fx serial` TUI* —
    allocate the pty, dodge the `select` menu, detach cleanly — or hang / leave a
    zombie `socat` holding the port? This is itself a failure mode and is where
    the two harnesses may diverge most.
- **Efficiency** (soft, model-verbosity-confounded): tool-call count, wall-clock,
  tokens.

Report **rates over N trials**, not single pass/fail. Treat success-rate and
thrash as the headline; efficiency as secondary.

---

## 8. Trial structure & threats to validity

- **N ≥ 5–8 per cell.** The diagnostic effects are near-binary (e.g. T-pre ~5/5
  for panio vs. ~0/5 for `fx serial`); lean on **effect size**, not elegant
  p-values. Reserve power-worry for the one close comparison (A1 vs. A2 on
  T-scroll).
- **Randomize arm order across trials.** A flaky cable that degrades over a
  session must not correlate with arm — otherwise resilience is confounded with
  luck. (Randomizing also *handicaps* panio, since reconnect looks better on a
  worse cable, which keeps us honest.)
- **Pre-register predictions** (§5, and §10) before running.
- **Confounds already covered:** contamination (§4.2), harness=product+model
  (§4), timeout-as-killer (§6), training leakage (§4.2), send→poll latency as a
  *predicted panio loss* on T-timer (§1, §5).

---

## 9. Baseline characterization — verified

### 9.1 `fx serial` is a thin, interactive `socat` wrapper

Read from the Fuchsia tree at `tools/devshell/serial` (fuchsia-vim3 checkout;
current implementation as of this writing — re-check against the tree under test):

- **The whole tool is one `exec`:** `socat -,...,escape=0x0f "file:${DEVICE}",...`
  (`serial:140`) — it attaches your **terminal** (`-`) to the device file, escape
  char Ctrl-O to exit (`serial:22,129`). There is **no daemon and no
  persistence**; the connection lives only for that foreground command.
- **`--output-file`** maps to socat's `-R` (`serial:64-67`): a **flat raw byte
  dump** of the device→host stream — no timestamps, no sequence numbers, no
  structure — and it captures **only while attached**, from the moment the flag
  is passed.
- **No pre-attach capture** (no daemon → output before launch is unrecoverable),
  **no structured history** (best case is `grep` over the flat file), **no
  reconnect** (socat exits on device loss; on hot-unplug it dies like a raw
  `cat`), **exclusive open**, and an interactive **`select` menu** when multiple
  ttyUSB devices exist (`serial:96-102`) that an agent must avoid by passing an
  explicit path.
- **It is a TUI, not a service.** Because socat attaches to the controlling
  terminal, an agent cannot "send a command, then query the log" as two discrete
  acts — it must allocate a pty, inject keystrokes into the live stream, parse
  inline, and send Ctrl-O to detach. *That pty-wrangling is the agent pain panio
  removes.*

Conclusion: `fx serial`, though purpose-built, was built for **a human at a
terminal**, so it shares every weakness panio targets. `--output-file` is its
single capture affordance and equals the dropped "documented-recipe" rung — hence
A1 = `fx serial --output-file`, steel-manned.

### 9.2 `agy` (Antigravity) invocation surface

- `agy -p` / `--print` — single non-interactive prompt, prints the response
  (mirrors `claude -p`). Final answer on stdout → grading.
- `--print-timeout` (default 5 min), `--model`, `--add-dir` (workspace),
  `--dangerously-skip-permissions` (all-or-nothing — **no per-tool
  allowlist**), `--sandbox` (terminal restrictions, likely too blunt for this
  suite), `plugin` subcommand.
- **`--log-file` is NOT the tool-call trace** (verified on `agy` 1.1.6,
  2026-08-20 pilot): it captures the CLI server's debug log (HTTP helper
  noise), with no tool-call events. The real transcript is written to
  `~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/
  transcript.jsonl` (and `transcript_full.jsonl` with full outputs) — JSONL
  events with `type` (`USER_INPUT`/`PLANNER_RESPONSE`/`GENERIC`),
  `tool_calls[].name`+`args.CommandLine`, and per-step timestamps; command
  outputs land in the following `GENERIC` step's `content`. Find the
  conversation id by mtime under `brain/` after the run. The metrics parser
  should target that file, not `--log-file`.

---

## 10. Pre-registered predictions

Committed before the first run. Panio is expected to **lose or tie** where the
hypothesis is weak — that is the point.

| Task | A0 YOLO | A1 `fx serial` | A2 panio | Decisive delta |
|---|---|---|---|---|
| T-pre | fail | **fail** (structural) | **pass** | A1→A2 (L2) |
| T-async | mostly fail | mixed | pass | A0→A2 (L1/L2) |
| T-scroll | mixed | pass-ish | pass | A1→A2 close (L2) |
| T-drop | fail | fail | **pass** | A1→A2 (L1) |
| T-timer | mixed | mixed | **tie / lose** | falsifier (L3) |

If the results match this table, panio's win is **architectural** (L1+L2) and
honestly bounded (L3). If A2 also wins T-timer, re-examine for a harness
artifact. If A1 ties A2 on T-pre/T-drop, the hypothesis's strong legs are wrong.

---

## 11. Open items to verify before building

1. **`agy` ambient context** — does it auto-load a workspace `AGENTS.md` or
   enabled plugins that could smuggle panio knowledge into A0/A1?
   (`agy plugin list`; inspect `--add-dir` behavior.) *Partially answered
   (2026-08-20 pilot, agy 1.1.6): no plugins imported by default, and the only
   auto-loaded skills are agy's own builtins under
   `~/.gemini/antigravity-cli/builtin/skills/` (no panio leak). Whether a
   workspace `AGENTS.md` is auto-loaded is still unverified (the pilot
   workspace was empty).*
2. **`agy` trace format** — ~~confirm `--log-file` contains the tool-call
   events~~ **Resolved (2026-08-20 pilot): it does not** — see §9.2; parse
   `brain/<conversation-id>/.system_generated/logs/transcript.jsonl` instead.
3. **Bench wiring** — is the USB-serial adapter on a switchable RSH hub port so
   VBUS hot-unplug (T-drop) is scriptable?
4. **Nonce injection path** — the exact hook for a per-trial nonce in the
   Fuchsia/pi5 boot (u-boot `echo` vs. kernel-cmdline print in the netbooted
   image).
5. **`fx serial` re-check** — confirm the implementation in the *tree under test*
   matches §9.1 (line numbers from the fuchsia-vim3 checkout).
