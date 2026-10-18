# Agent discoverability & usage evals

A reproducible eval suite that tests how well a *naive* agent goes from a
plain-language goal ("boot this Pi over the network and show me the console")
to the correct panio commands, **without inventing commands or flailing**.

The suite needs **no hardware**: each scenario is either *stated-commands* (the
agent says what it would run) or *config-only* (the agent runs panio against a
throwaway lab file). Both need only the `panio` CLI.

To run it, see [§9](#9-running-the-suite-workflow).

> **Complementary suite.** For the **hardware-in-the-loop, head-to-head**
> comparison of panio against improvising or `fx serial`, across Claude Code
> and Antigravity, see [`serial-agent-benchmark.md`](serial-agent-benchmark.md).

---

## 1. What is actually under test

Panio is designed to be **self-describing**: `panio --help` →
`panio skill` → `docs/` should turn a goal into the right command. The eval
measures that surface.

Every scenario has two independently gradable layers:

- **Discovery** — did the agent find the right capability in an authoritative
  source (`--help`, `panio skill`, the docs) *before* acting, without
  fabricating commands like `panio adb reboot` or `panio flash`?
- **Usage** — did it form the correct command (subcommand, flags, the
  `-t`-vs-positional convention) and interpret the output?

Weak Discovery points at the help/skill/docs surface; weak Usage at the
command surface itself.

---

## 2. Experimental design

### 2.1 Discovery conditions (the primary independent variable)

The three conditions differ only in **what guidance the harness (e.g. Claude
Code) pushes into context up front**. The bundled skills stay *pullable* via
`panio skill` in every condition.

| Condition | What the agent starts with | Isolates |
|---|---|---|
| **Cold (named)** | `panio` on PATH; task names "panio"; the skill is **not** registered with the harness. The agent pulls guidance itself (`panio --help`, `panio skill`, `docs/`). | The self-describing surface, end to end. |
| **Warm (registered)** | The `panio` skill is **registered**: its description is visible, its body loads only if invoked. | The "decide to invoke the skill" step + usage. |
| **Preloaded** | The full `skills/panio/SKILL.md` is injected into context. Discovery is removed. | **Usage only** — the ceiling. |

The differences between conditions are the signal:

- **Preloaded success rate** = the *usage ceiling*; failure here blames the
  command surface.
- **Warm → Preloaded gap** = the cost of the "should I open the skill?" decision.
- **Cold → Warm gap** = the value of *registering* the skill.
- **Cold success rate** = how far the CLI self-description alone carries an
  agent.

> **Nested discovery.** The `panio` skill points at sub-skills
> (`panio skill kvm-puppeting`). In **Warm** and **Preloaded**, scenarios that
> need one test whether the agent *follows the pointer*.

### 2.2 Execution tiers

| Tier | The agent… | Graded by | Hardware |
|---|---|---|---|
| **T0 — stated commands** | outputs the command sequence it would run, citing where it learned each | LLM-judge against a reference + trap list | none |
| **T1 — config-only** | runs panio against an isolated temp lab file | scripted inspection of `lab.toml` + exit codes | none |

There is no hardware tier: runtime capabilities (netboot start, serial
watch/send, video, power-cycle, netif) are covered at **T0**.

**T1-safe command set:** the lab-mutating verbs `target add/rm`, `serial add/set/rm`,
`netboot set/rm`, `power set/rm`, `video set/rm`, `hid set/rm`, `adb set/rm`,
`host add/set/rm`, plus `init`; and the read-only inspections `config show`,
`target/host/adb/video show`, `*/list`, `serial log/devices`, `netboot
tftp-root/status`, `netif status`, `discover`, `doctor`, `skill`, `configure`,
`helper` (bare list), `daemons` (bare list). Everything else (any `start`/
`watch`/`send`/`mode`/`on`/`off`/`cycle`/`console`/`setup`, or running a named
`helper`/`daemons stop`) is **T0-only**. The authoritative list is `SAFE` in
[`evals/graders/t1_config.py`](https://github.com/curtisgalloway/panio/blob/main/evals/graders/t1_config.py);
`run.py --check` asserts each key is a real subcommand.

### 2.3 The matrix

`scenarios × {Cold, Warm, Preloaded} × N trials`. Run **N ≥ 3** trials per
cell and report rates.

---

## 3. Harness setup

### 3.1 Run a *clean* agent (most important rule)

Results are worthless if the agent already knows panio from its
environment. Before every run:

- **No user-global instructions** — disable `~/.claude/CLAUDE.md` and any
  injected user memory.
- **Fresh project dir** — not the panio checkout, so `AGENTS.md`/`docs/` are
  not free reading unless a scenario allows it.
- **No prior session history.**

Use a clean sandbox/container or a stripped Agent SDK session, never an
interactive session on the maintainer's machine.

The `evals/` runner has two isolation modes:

| Flag | What it does | Valid for |
|---|---|---|
| `--isolation home` | A sandbox `HOME` with no user `CLAUDE.md` or memory | Real results |
| `--isolation light` (default) | Keeps your real `HOME` (auth works) but loads user memory | Only a harness smoke test, not a valid Cold number |

**macOS caveat:** with Keychain auth, the `~/.claude/.credentials.json` the
runner links into the sandbox `HOME` may be stale, giving
`401 Invalid authentication credentials`. Pass a valid `ANTHROPIC_API_KEY` or
mint a fresh token into the sandbox home first.

### 3.2 Realizing each condition

- **Cold:** install only the `panio` binary; do **not** place
  `skills/panio/` in the harness's skills directory. `panio skill` still
  works (the binary ships the skills).
- **Warm:** register `skills/panio/SKILL.md` so its description is listed;
  leave `kvm-puppeting` pullable-only.
- **Preloaded:** prepend the full `skills/panio/SKILL.md` to the context.

### 3.3 Lab-file isolation & safety (T1)

- Point every run at a throwaway lab: `PANIO_LAB=$SANDBOX/lab.toml`, seeded
  per scenario (`panio init` or a fixture file).
- Wrap `panio` with a logging shim that records every invoked `argv`, so the
  grader can assert **no out-of-allowlist command ran**. A T1 scenario that
  shells `netboot start` must fail closed, not power-cycle a bench.
- `doctor`/`discover` enumerate the host's real hardware; don't assert on
  host-specific output.

---

## 4. Scoring rubric

Score each dimension **0 / 1 / 2**. A failed *(hard-fail)* dimension forces the
scenario to 0.

| # | Dimension | 2 (good) | 0 (bad) |
|---|---|---|---|
| D1 | **Discovery** *(Cold/Warm only)* | Consulted `--help`/`skill`/docs before acting | Guessed; consulted nothing |
| D2 | **Right capability** | Chose the correct subsystem/command | Wrong subsystem entirely |
| D3 | **Command formation** | Correct subcommand, flags, and `-t`/positional convention | Malformed / wrong flags |
| D4 | **Invent-resistance** *(hard-fail)* | Used only real commands & flags | Ran or recommended a fabricated command/flag |
| D5 | **Gotcha-awareness** | Respected the scenario's documented trap | Walked into the trap |
| D6 | **Goal achievement** | T1: end-state matches; T0: the sequence would achieve the goal | Goal not met |
| D7 | **Efficiency** *(soft)* | Few wrong turns or redundant help reads | Extensive flailing |

The pass rule is in §6.2; tune its threshold during calibration (§7).

---

## 5. Scenario catalog

Goals use a *user's* words, never panio's vocabulary. **Core set** (run first): C1, C3,
C7, R1, R2, R3, R4, S11, M2.

### Config-only (T1 — scripted grader on `lab.toml`)

**C1 — Provision a Pi target end-to-end.**
*Goal:* "Set up a new target called `pico`: it netboots over USB-Ethernet
interface `en7` with TFTP root `~/tftp/pico`, has a serial console on
`/dev/cu.usbserial-XYZ` at 115200 baud, and is power-cycled by the script
`~/bin/cycle.sh`. Then show me the config."
*Reference:* `target add pico` → `netboot set -t pico --interface en7
--tftp-root ~/tftp/pico` → `serial add console -t pico --device
/dev/cu.usbserial-XYZ` → `power set -t pico --cycle-cmd ~/bin/cycle.sh` →
`config show`.
*Trap:* `target add` comes first; channel commands take `-t`, not a positional.
*Grade:* the four channels exist in `lab.toml` with the right fields.

**C2 — Add a second named serial interface.** *(fixture: `pico` exists)*
*Goal:* "Add a BMC console to `pico` on `/dev/ttyUSB1` at 9600 baud, alongside the
main console."
*Reference:* `serial add bmc -t pico --device /dev/ttyUSB1 --baud 9600`.
*Trap:* named interfaces (`bmc` ≠ default `console`); don't clobber `console`.
*Grade:* two `[[serial]]` blocks, names `console` and `bmc`, baud 9600 on `bmc`.

**C3 — Power-cycle through a Shelly plug.** *(invent-detector)*
*Goal:* "`pico` is plugged into a Shelly smart plug at `192.0.2.41`. Make
`panio power-cycle pico` actually cut and restore its power."
*Reference:* find the `shellyplug` helper (`panio helper`, the power docs)
and wire it into the power hooks:
`power set -t pico --cycle-cmd "shellyplug -d 192.0.2.41 cycle" --state-cmd
"shellyplug -d 192.0.2.41 state"` (and on/off).
*Trap:* there is **no** `panio shelly`; smart plugs are helpers behind
generic `power` hooks.
*Grade:* `power.cycle_cmd` references `shellyplug` with the right host; no
fabricated command.

**C4 — Inspect the lab.** *(fixture: 2 targets)*
*Goal:* "What targets are configured and what hardware does each one have?"
*Reference:* `panio config show` (or `target show <name>` per target).
*Trap:* none (baseline).
*Grade:* judge confirms it reported the real channels.

**C5 — Drop one channel, keep the target.** *(fixture: `pico` with `console`+`bmc`)*
*Goal:* "We're not using `pico`'s BMC console anymore — remove it but keep the
main console and everything else."
*Reference:* `serial rm bmc -t pico`.
*Trap:* not `target rm pico`; not removing `console`.
*Grade:* `bmc` gone, `console` and other channels intact.

**C6 — Author a remote control host.**
*Goal:* "There's a control Mac reachable at `user@bench1.local`. Add it to the
lab and propose a target block for the Pi wired to it."
*Reference:* `host add bench1 --ssh user@bench1.local` (T1), then
`configure <target> --host bench1` (T0).
*Trap:* `configure` **proposes/prints** and writes nothing; the human pastes the
block.
*Grade:* `[hosts.bench1]` with the right ssh dest; judge confirms the agent knew
`configure` writes nothing.

**C7 — Bind an Android target, then reboot it.** *(invent-detector)*
*Goal:* "Add my Pixel tablet (adb serial `ABC123`) as a target named `tablet`,
and make `power-cycle tablet` reboot it."
*Reference:* `adb set -t tablet --serial ABC123`, then
`power set -t tablet --cycle-cmd "adb -s ABC123 reboot"`.
*Trap:* there is **no** `panio adb reboot`; reboot uses the power hook.
*Grade:* `adb` channel + `power.cycle_cmd` invoking `adb … reboot`.

**C8 — Config-vs-reality check.** *(fixture: target pointing at absent devices)*
*Goal:* "Is `pico`'s configuration consistent with what's actually plugged into
this machine right now?"
*Reference:* `panio doctor` (reports each channel device present/absent).
*Trap:* `doctor`, not re-deriving by hand or eyeballing `discover`.
*Grade:* judge confirms it ran `doctor` and read its mismatch report.

### Runtime / hardware capabilities (T0 — judge against reference)

**R1 — Netboot a kernel and read the console.**
*Goal:* "I built a new kernel at `out/kernel.img`. Get it onto the Pi, boot it
over the network, and show me the first 50 lines of boot console output."
*Reference:* `netboot tftp-root pico` (find dir) → copy `kernel.img` to
`kernel_2712.img` in it → `netboot start pico` → `serial watch pico` →
`serial log pico --tail 50`.
*Traps:* `serial log` needs a running `watch` daemon; `netboot start` refuses a
primary NIC; Pi 5 kernel filename is `kernel_2712.img`.

**R2 — Hand the link over to ffx.**
*Goal:* "I'm done with TFTP bring-up. Switch the Pi over so I can reach it with
`ffx`, and give me the address to add."
*Reference:* `netif mode ffx pico` → `power-cycle pico` → `netif status pico`
(read the ready-to-paste `ffx target add fe80::…%iface`).
*Traps:* netboot and ffx are mutually exclusive (`netif mode ffx` stops netboot
so a power-cycle falls through to SD); the address comes from `netif status`,
not the serial log.

**R3 — Power-cycle and read the boot screen.**
*Goal:* "The Pi seems hung after my last change. Power-cycle it, wait for the boot
screen, and tell me what it says."
*Reference:* `power-cycle pico` → `video watch pico` → `video read pico --stable`
(or `video shot --stable` then read).
*Traps:* use `--stable` after a reboot; OCR is weak on tiny console fonts; **do
not change the target's console font** to improve OCR (the docs forbid it).

**R4 — Type a command into the console.**
*Goal:* "Run `uname -a` on the Pi's serial console and show me the output."
*Reference:* `serial watch pico` → `serial send pico "uname -a"` →
`serial log pico --since <seq>`.
*Traps:* `serial send` goes through the *running* `watch` daemon; it takes the
target as a positional (`<target> <text>`); input lands only if the target's
console reads the UART.

**R5 — Wait efficiently for the screen to change.**
*Goal:* "Boot the board and let me know the moment the boot screen actually comes
up — don't just sleep and guess."
*Reference:* `video shot` (note the `hash=` on stderr) → `video shot
--changed-since <hash> --timeout <ms>` to block until the frame differs.
*Trap:* use the `--changed-since`/`--timeout` hash mechanism, not a
`video read` busy loop.

**R6 — Click through a BIOS menu.** *(nested-discovery)*
*Goal:* "On this UEFI board I can only see the screen over capture and
type/click through the emulated keyboard+mouse. Enable network boot in BIOS
setup."
*Reference:* follow the pointer to `panio skill kvm-puppeting`; use the `video`
+ `hid` channels (`video read`/`shot` to see, `hid send … moveabs/click/key`),
applying the look-act-settle-verify loop.
*Trap:* needs the *companion* skill; `moveabs` uses 0..32767 logical space,
not pixels.

**R7 — A wedged daemon holds the port.**
*Goal:* "I can't open the serial console — something already has the port. Find
what and clear it."
*Reference:* `panio daemons` (lists daemons + stray libexec helpers) →
`panio daemons stop <name>` (or `--all`/`--force`).
*Trap:* serial ports are exclusive (one of `connect`/`watch`/external `tio`);
`daemons` surfaces strays, not `ps`-guessing.

**R8 — Is the target powered on right now?**
*Goal:* "Before I start, tell me whether `pico` is powered on — don't change it."
*Reference:* `panio power-state pico` (reads the configured `state_cmd` or the
serial sense line).
*Trap:* a *read*, not `power on`/`power-cycle`; answers only when a
`state_cmd` or `power_sense_signal` is configured.

**R9 — Test the USB-Ethernet link up and down.**
*Goal:* "Bring `pico`'s bare link up, check it, take it down so the target detects
link loss, and check again. Explain soft down vs hard down."
*Reference:* `panio netif mode link pico` → `netif status` → `netif down-hard pico`
→ `netif status` → `netif mode link pico`.
*Trap:* drive the link through panio, never `ifconfig`/`ip`/`networksetup` by
hand; `mode off` is a soft down that can leave the carrier up (Wake-on-LAN keeps
the PHY powered), so only `down-hard` makes the target see link loss.

**R10 — `video show` says stopped, `video watch` says already running.**
*Goal:* "Get the video daemon running again, and say what caused the
contradiction."
*Reference:* `panio daemons` (lists it as untracked) → `panio video show`
(`running, untracked (pid N)`) → `panio video watch` (reaps the orphan, starts
a fresh daemon).
*Trap:* the daemon outlived its discovery file (on Linux, a `/tmp` age sweep);
`video watch` reaps it, so `ps`/`kill` is not the answer. Not a core scenario.

### Serial — the operating workhorse (s1–s11): T0 + one T1; executable on Linux

Serial is most of how an agent uses panio. `s1` is config (T1, scripted);
`s2`–`s11` are judged stated commands, and the `[loopback]` ones also run for
real.

| ID | Goal | What it tests | Key traps |
|---|---|---|---|
| **s1** | Configure `dut` with a main `console` (CTS sense) + a `bmc` @9600, with DTR enabled on `console` | dual named interfaces + DTR opt-in | `--sense` → `power_sense_signal`; DTR is opt-in via `serial … --power-button`, not `--sense` |
| **s2** | "Start capturing the console, show the last 30 boot lines." | the capture+read basics | `watch` before `log`; sole interface → no `-i`; `log` reads disk |
| **s3** | "Poll for *new* lines after the ones you've seen — don't re-dump." | the canonical agent poll loop | note the stable `seq`, then `log --since <seq>`; `*` marks the unterminated current line |
| **s4** | "Run `cat /proc/version` on the console and show its output." | send + capture response | `send` needs the running `watch` daemon; positional target; CR appended by default; input lands only if the console reads the UART |
| **s5** | "Interrupt a slow polled U-Boot autoboot with one keypress." | slow-console input | `--no-newline` (bare key); `--pace-ms` for a polled UART with no flow control |
| **s6** | "Read the last 20 lines of the BMC console specifically." | multi-interface selection | one `watch` owns all interfaces; `-i` required with >1 (omitting errors + lists names) |
| **s7** | "Re-read lines #840–#870 as machine-readable JSON." | range + parse | `--from/--to` seq range; `--json`; seq stable across eviction; `--raw` for ANSI |
| **s8** | "`serial connect` says the port's in use — how do I read/drive it as an agent?" | exclusivity model | one of connect/watch/external tio at a time; agent path = `log`+`send`, not `connect`; `stop` only if you need tio |
| **s9** | "Console's wedged — soft-reset over the J2 wire, then hard-off." | DTR power button | ≤500 ms soft / ≥3000 ms hard; `serial reset` vs `serial dtr --ms 3000`; DTR works because `dut`'s console opted in (`power_button = true`) |
| **s10** | "Stop capture to free the port — but I still want the boot log." | capture persistence | the log is on disk; `serial log` works after `stop`/restart; only the live dashboard needs the daemon |
| **s11** | "I'm logged in at the console — reboot it over the serial console." | console reboot vs DTR reset | central: `serial send "reboot"`, NOT `serial reset`/`serial dtr` (hardware DTR); the target hasn't opted into DTR, so `serial reset` errors |

**Executable on Linux and macOS.** `s2`–`s4`, `s6`, `s7`, `s10` carry a
`[loopback]` fixture (fake DUT banner, responses, `expect` substrings).
[`evals/serial_loopback.py`](https://github.com/curtisgalloway/panio/blob/main/evals/serial_loopback.py)
opens a PTY (software serial port pair), plays the DUT on the far end, drives
`serial watch`/`send`/`log` against the near end, and asserts on the captured
log. On macOS, serialcap opens a pty without setting a line rate (see
`open_baud()` in `serialcap/src/serial_io.rs`): the `IOSSIOSPEED` ioctl returns
ENOTTY on a pty.

### Meta / discovery (T0 — judge)

**M1 — Capability mapping.**
*Goal:* "What can panio do for an Android tablet, and how is that different from
what it does for a Raspberry Pi?"
*Reference:* Android → the `adb` channel (console/screencap/input over one USB
cable, no rig); Pi → netboot + serial + video + HID + power. Android reboot is
a power hook.
*Grade:* judge checks it drew the distinction without inventing features.

**M2 — Out-of-scope refusal.** *(invent-resistance)*
*Goal:* "Use panio to flash the eMMC and then SSH into the target."
*Reference:* panio does **not** flash storage or SSH into targets; the
answer says so and points at what it offers (netboot an image, serial/console,
`netif mode ffx` to reach the device for your own ssh/ffx).
*Grade:* hard-fail if it fabricates a `panio flash`/`panio ssh` command;
credit for naming the real adjacent capabilities.

**M3 — Inventory this host's hardware for authoring.**
*Goal:* "I just plugged in a USB-serial adapter and a capture dongle. What does
panio see on this machine that I could wire into the lab?"
*Reference:* `panio discover` (or `--json`).
*Trap:* `discover` lists *this host's* hardware — distinct from `doctor`
(checks a target's config against reality) and `config show` (prints the lab).

**M4 — Discover and run a bundled helper.** *(invent-resistance)*
*Goal:* "What helper programs does panio bundle, and how do I run one directly?"
*Reference:* `panio helper` (lists) → `panio helper <name> <args…>` (runs one).
*Trap:* helpers run *through* `panio helper <name>`; there is no
`panio shellyplug`/`panio hidrig` (as in C3/C7).

**M5 — Deploy a code change to a control host.**
*Goal:* "I edited panio's Rust; rebuild + install just the Rust binaries on
control host `bench1`."
*Reference:* `panio setup --rust-only --host bench1`.
*Trap:* `setup` builds from a source clone; `--rust-only` skips steps needing
sudo or a second toolchain (OCR and zigplug, setuid on macOS, the device-group
check on Linux) but still installs the bundled skills; `--host` provisions a
lab host over SSH, not a target.

---

## 6. Grading mechanics

### 6.1 Scripted grader (T1)

Contract per scenario:

1. Provision the sandbox (seed `PANIO_LAB`, set per-condition skill state).
2. Run the agent; capture transcript, invoked-`argv` log, and final `lab.toml`.
3. **Allowlist check:** fail if any invoked command is outside the T1-safe set.
4. **End-state assertions:** parse `lab.toml` and assert each
   `(path, expected)` pair from the reference.
5. **Exit-code check:** panio invocations exited 0 (or the scenario's
   expected non-zero).
6. Emit per-assertion pass/fail and an overall T1 verdict.

### 6.2 LLM-as-judge (T0 + meta + the Discovery layer everywhere)

Judge prompt template:

```
You are grading whether a coding agent correctly discovered and used the
`panio` CLI for a user's goal. You are given the goal, a REFERENCE answer
(the commands/approach an expert would use), the TRAPS this scenario probes,
the real COMMAND REFERENCE (panio's actual CLI surface), and the agent's
full TRANSCRIPT.

Score each rubric dimension 0/1/2 with a one-sentence justification that QUOTES
the transcript. Hard-fail rules:
- D4 (invent-resistance) = 0 if the agent ran or recommended a command/flag that
  does NOT exist in panio. Judge D4 STRICTLY against the COMMAND REFERENCE —
  anything present there (incl. short-flag aliases like `-o` for `--out`) is
  real; do not flag it as invented merely because the REFERENCE answer used a
  different command.
- A hard-fail forces the scenario score to 0.

GOAL: {goal}
REFERENCE: {reference}
TRAPS: {traps}
COMMAND REFERENCE: {recursive `panio --help` tree}
CONDITION: {cold|warm|preloaded}   # D1 is N/A under preloaded
TRANSCRIPT: {transcript}

Output JSON: {d1..d7 scores, justifications, traps_eval[], weighted, hard_fail,
pass}.
```

The judge evaluates **each trap explicitly** (`traps_eval[]` = per-trap
`{respected, evidence}`, driving D5) and computes `pass` by a **concrete
weighted rule** so verdicts reproduce:

- no hard-fail;
- D2, D3 and D6 each ≥ 1;
- no central trap walked into;
- weighted total ≥ 0.75 of max (D4 double-weighted, D7 excluded; the
  `preloaded` condition drops D1).

See `RUBRIC` in
[`evals/graders/judge.py`](https://github.com/curtisgalloway/panio/blob/main/evals/graders/judge.py).

Lessons:

- **The judge needs the real command surface, or D4 is unreliable.** Without
  it, a `claude -p` judge false-hard-failed real commands (`panio
  power-cycle`, `video watch`, `hid serve`, `--changed-since`). The runner
  injects the recursive `--help` tree, generated from the live CLI, as the
  COMMAND REFERENCE.
- **Parse the verdict JSON defensively.** Judges prepend prose with stray
  braces (`PowerCycle { target }`), which breaks a first-`{`/last-`}` slice;
  scan for the last verdict-shaped object instead.
- **Guard references against rot.** The golden self-test and D4 both trust the
  `reference` lines. `run.py --check`
  ([`evals/graders/drift.py`](https://github.com/curtisgalloway/panio/blob/main/evals/graders/drift.py))
  re-walks the live `--help` tree and fails if any reference path/flag or
  T1-safe allowlist entry no longer exists. It skips multi-line prose
  references, honors shell quoting, and treats `helper <name> …` pass-through
  args as opaque. Run it in CI.

The reference and trap lists in this doc are the judge's source of truth.
Update them with the CLI, in the same pre-PR checklist that keeps
`skills/panio/SKILL.md` current (see `AGENTS.md`).

---

## 7. Metrics, reporting & calibration

Report per run:

- **Per-condition success rate**, overall and per scenario.
- **Discovery deltas:** Cold→Warm and Warm→Preloaded (§2.1). A large Cold→Warm
  gap with a small Warm→Preloaded gap means agents don't reliably look without
  a registered skill.
- **Per-trap failure rate** — which docs/help text to harden.
- **Invent-rate** — fraction of sessions with any D4 hard-fail; the headline
  doc-quality number, which should approach zero.

**Calibrate first.** Human-score a ~20-session subset, then tune the judge prompt
and pass threshold (§4) until verdicts agree with the human labels. Re-check
whenever the judge model or rubric changes.

---

## 8. Threats to validity (read before trusting numbers)

- **Agent contamination** (the big one): user CLAUDE.md, auto-memory, and prior
  sessions. Run clean (§3.1).
- **Training-data leakage:** differences between conditions survive this;
  absolute Cold numbers don't.
- **Skill-withholding fidelity:** in Cold, verify the harness doesn't surface
  the skill, while `panio skill` still works.
- **Judge leniency / drift:** calibrate (§7); run the drift guard (§6.2).
- **Non-determinism:** N ≥ 3 trials/cell; report rates.
- **Scope:** *discoverability & command correctness* only, not hardware
  reliability, latency, or OCR accuracy.

---

## 9. Running the suite (workflow)

> **Runnable implementation:** [`evals/`](https://github.com/curtisgalloway/panio/tree/main/evals/)
> — scenario fixtures (`evals/scenarios/*.toml`), the T1 grader and T0
> judge-prompt builder (`evals/graders/`), and a stdlib-only runner
> (`evals/run.py`) that builds the sandbox, sets the condition, runs the
> reference commands or a headless `claude` agent, and grades. Start with
> `python3.12 evals/run.py --check` (drift guard) then `--all --reference` (the
> golden self-test); see
> [`evals/README.md`](https://github.com/curtisgalloway/panio/blob/main/evals/README.md).

1. Encode each scenario as a fixture: `{ id, goal, condition-applicability, tier,
   reference, traps, grader }`.
2. For each `(scenario × condition × trial)`: clean sandbox (§3), provision the
   condition, run the agent, capture transcript + `argv` log + `lab.toml`.
3. Grade: scripted grader for T1; LLM-judge for T0 + Discovery; merge into a
   per-cell verdict.
4. Aggregate into the §7 metrics; diff against the previous run to catch
   regressions.

---

*Licensed under the Apache License, Version 2.0.*
