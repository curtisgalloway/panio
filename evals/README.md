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

# panio agent-eval suite

Runnable fixtures for the eval design in
[`docs/dev/agent-evals.md`](../docs/dev/agent-evals.md): can a *naive* agent go from a
plain-language goal to the correct panio command via the CLI's self-describing
surface (`panio --help` → `panio skill` → docs), without inventing commands?

**No hardware.** Every scenario is either **T1** (config-only — the agent really
runs panio against a throwaway lab file, graded by inspecting the resulting
`lab.toml`) or **T0** (stated-commands — the agent says what it would run, graded
by an LLM judge). Stdlib-only **Python 3.11+** (uses `tomllib`); no
`pip`/`uv`/network needed. Agent mode also needs the `claude` CLI on PATH.

> On this machine the stock `python3` is 3.9 — use `python3.12` (or any
> 3.11+). The examples below do.

## Status (2026-06) — what's built and verified

**Built:** 33 scenarios (c1–c8 config, r1–r9 runtime, s1–s11 serial, m1–m5 meta —
r8 power-state, m3 discover, m4 helper, m5 setup added to cover the last few CLI
groups; r9 tests the bare link up/down via `netif mode link`/`off`, the
Wake-on-LAN carrier wrinkle, and `netif down-hard`, which many USB adapters ignore); the scripted T1 grader + LLM-judge; a `run.py --check` **drift guard**
(every scenario reference + the T1-safe allowlist is validated against the live
`--help` surface, so a renamed subcommand or dropped flag fails loudly instead of
silently rotting the references); the `run.py` orchestrator (sandbox, three
discovery conditions, reference/agent modes); `serial_loopback.py`. Two agent
CLIs are wired: `claude` (`-p`, prompt on stdin) and `agy` (`-p`, prompt as
argument). The judge supports both and injects panio's real `--help` tree as
the COMMAND REFERENCE so invent-resistance is graded against ground truth; it
also evaluates each scenario trap explicitly and applies a concrete weighted
pass rule (not a holistic "looks adequate"). The T1 graders for c3/c7 assert the
full power-hook wiring (shellyplug on/off/state; `adb -s <serial> reboot`), not
just the headline command.

**Verified runs (all unstaged; results overwrite per mode+condition):**
- Reference golden self-test — every T1 scenario PASSes (`run.py --all --reference`).
- **agy, cold** — full T1 set **9/9** (c1–c8, s1); **r-series 7/7** (claude-judged);
  T0 sample s2/s4/m2 **3/3**. agy did genuine cold discovery (help/skill probing).
- Not yet run: remaining T0 serial (s3, s5–s10) with an agent; the **warm** and
  **preloaded** conditions (i.e. the discovery-delta experiment is not done yet);
  a cross-model judge across the whole suite.

**Runner hardening (from real failures):** T0 scenarios get a "you may probe
`--help`/`skill` but don't execute the operational commands" guard (an agent that
ran `netif mode ffx` blocked on `sudo`); per-scenario 360 s timeout that's caught
(one hang ≠ dead batch); `stdin=DEVNULL`; per-scenario `try/except` + incremental
result save; a `daemons stop --all` sweep after each agent run (scoped to the
sandbox's own `PANIO_RUNTIME_BASE`, so it never touches the operator's
daemons).

**Known platform limits:** `--isolation home` needs valid creds (macOS Keychain `.credentials.json`
can be stale → 401). agy bypasses the PATH shim, so its T1 allowlist isn't
enforced (graded on `lab.toml` outcome). See `docs/dev/agent-evals.md` §3.1, §6.2, §8.

## Layout

```
evals/
  run.py                 orchestrator (sandbox + condition + run + grade)
  serial_loopback.py     Linux: execute the 'operating serial' scenarios for real
  scenarios/*.toml       33 scenarios — c1–c8 config, r1–r9 runtime,
                         s1–s11 serial, m1–m5 meta
  fixtures/*.toml        seed lab files for scenarios that start from a state
  graders/
    t1_config.py         scripted grader: lab.toml assertions + T1-safe allowlist
    judge.py             builds the LLM-judge prompt (and can call a judge)
    drift.py             the --check drift guard: scenario expectations vs the
                         live CLI surface
  results/               written per run (gitignored)
```

## Quick start

```bash
cd evals

# what's here ( * = core set )
python3.12 run.py --list

# drift guard: every scenario reference + the T1-safe allowlist must match the
# live CLI surface. Run this first (and in CI) — it's fast and needs no agent.
python3.12 run.py --check

# golden self-test: run each T1 scenario's reference commands, grade the lab.
# Every T1 scenario must PASS here — this validates the fixtures + grader.
python3.12 run.py --all --reference

# grade one lab by hand
python3.12 graders/t1_config.py /path/to/lab.toml scenarios/c1_provision_target.toml
```

## Running a real agent

`run.py` drives `claude` headless (`claude -p`, prompt on stdin) in a sandbox:

```bash
# config scenario, agent must discover + use panio from cold:
python3.12 run.py --scenario c1 --condition cold --agent claude --isolation home

# runtime scenario, warm condition, with an LLM judge:
python3.12 run.py --scenario r1 --condition warm --agent claude --judge-cmd "claude -p"
```

**Other agents.** `--agent agy` drives the `agy` CLI (`agy -p "<goal>"`; it takes
the prompt as the flag argument, and runs shell tools headlessly without a skip
flag). Two caveats: agy re-derives `PATH` for its shell tool, so it **bypasses
the logging shim** — the `lab.toml` outcome is still graded, but the T1-safe
allowlist check is not enforced for agy; and use `--isolation light` (a sandbox
`HOME` breaks agy's own auth). The `--judge-cmd` path currently assumes a
stdin-reading judge (works with `claude -p`), so T0 judging isn't wired for
`agy -p` (prompt-as-argument) yet.

The runner, per scenario:
1. makes a temp sandbox: throwaway `lab.toml`, a `panio` **shim** that logs
   every invoked argv (for the T1-safe allowlist check) then execs the real CLI,
   and per-condition skill state;
2. runs the reference commands or the agent (goal piped on stdin, `cwd` = sandbox
   project dir, `PANIO_LAB` = sandbox lab);
3. grades: `t1_config` for T1, the judge prompt for T0;
4. writes a summary to `results/` and leaves the sandbox for inspection on
   failure.

### Discovery conditions (`--condition`)

| Condition | How it's realized | Tests |
|---|---|---|
| `cold` | nothing pushed; the agent must pull guidance via `panio --help`/`panio skill` | the self-describing surface |
| `warm` | the `panio` skill is copied into the sandbox `cwd`'s `.claude/skills/` so it's registered | the "invoke the skill" decision + usage |
| `preloaded` | the full `SKILL.md` is injected with `--append-system-prompt` | usage only (ceiling) |

### ⚠️ Run a clean agent

Results are only meaningful if the agent doesn't already know panio from its
environment. The maintainer's own machine is the most contaminated possible
(user `CLAUDE.md`, auto-memory naming panio + bench hosts, prior sessions).
Use `--isolation home` to run with a sandbox `HOME` (no user `CLAUDE.md`/memory).
`--isolation light` (default) keeps your `HOME` for auth and only skips user
*settings* — fine for smoke-testing the harness, **not** for real Cold numbers
(your user memory loads, and if it names panio the agent isn't naive).

**Auth under `home` isolation.** The runner symlinks `~/.claude/.credentials.json`
into the sandbox `HOME` for the duration of the run (a token refresh lands in
the real file; the link is removed when the run ends, so a kept sandbox holds
no credential copy), but on a macOS setup that authenticates via the
**Keychain** that file can be stale — the agent then dies with
`401 Invalid authentication credentials` and runs nothing. If you hit that,
either export a valid `ANTHROPIC_API_KEY` (it's passed through to the agent) or
mint a fresh token into the sandbox home before the run. See
`docs/dev/agent-evals.md` §3.1 and §8.

## Serial — the operating workhorse

Driving the serial console is most of how agents use panio, so it has a
dedicated cluster (`s1`–`s11`): `s1` is config (T1, scripted); `s2`–`s11` are
*operating* scenarios — start capture, tail, poll new output with `--since`,
send a command and read its response, pace a slow polled console, pick one of
two interfaces with `-i`, re-read an exact `--from/--to --json` range, port
exclusivity, DTR reset/hard-off, read-back-after-stop, and rebooting over the
console (`serial send "reboot"`) as distinct from a DTR reset.

The operating scenarios are judge-graded as stated commands by default (they
need a real port). The ones with a `[loopback]` table (marked `exec` in
`--list`) can also be **executed for real on Linux** via `serial_loopback.py`,
which opens a PTY, plays a fake DUT on the far end (banner + command responses),
drives `serial watch`/`send`/`log`, and asserts on the captured log:

```bash
python3.12 serial_loopback.py            # all exec serial scenarios
python3.12 serial_loopback.py s4 s2      # by id
```

> **Runs on macOS and Linux.** macOS needed serialcap to learn to open a pty
> without setting a line rate (see `open_baud()` in `serialcap/src/serial_io.rs`);
> before that every scenario here reported **SKIP**. Verified 6/6 PASS on
> macOS 26.6 / Apple Silicon.

## Adding a scenario

Drop a `scenarios/<id>.toml` (see any existing one). T1 scenarios set
`grader.type = "t1_config"` with `[[grader.assert]]` blocks (path + one of
`equals`/`contains`/`matches`/`exists`); path syntax supports
`targets.x.serial[name=bmc].baud`. Keep the `reference` list executable for T1
(it's the golden self-test) and descriptive for T0. Keep references in sync with
the CLI surface — they're the judge's source of truth. Run `python3.12 run.py
--check` after editing: it validates every `panio …` reference line (subcommand
paths + flags) and the T1-safe allowlist against the live `--help` tree. It skips
multi-line prose references (m1/m2/s8-style) and honors shell quoting and
pass-through args (`helper <name> …`), so only genuine drift trips it.
