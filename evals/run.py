#!/usr/bin/env python3
# Copyright 2026 Curtis Galloway
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
"""Runner for the panio agent-eval suite.

Sets up an isolated sandbox (throwaway lab file, a logging `panio` shim,
per-condition skill state), runs either the scenario's golden REFERENCE
commands or a real agent, then grades the result. Stdlib only.

Examples:
  python3 run.py --list
  python3 run.py --scenario c1 --reference            # golden self-test (T1)
  python3 run.py --all --reference                     # validate every T1 reference
  python3 run.py --scenario c1 --condition cold --agent claude
  python3 run.py --scenario r1 --condition warm --agent claude --judge-cmd "claude -p"

See docs/dev/agent-evals.md for the design.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
SKILL_MD = REPO / "skills" / "panio" / "SKILL.md"
sys.path.insert(0, str(HERE))
from graders import drift as drift_mod  # noqa: E402
from graders import judge as judge_mod  # noqa: E402
from graders import t1_config  # noqa: E402


# --- scenario loading --------------------------------------------------------


def load_scenarios() -> dict[str, dict]:
    out = {}
    for path in sorted((HERE / "scenarios").glob("*.toml")):
        with open(path, "rb") as fh:
            sc = tomllib.load(fh)
        sc["_path"] = str(path)
        out[sc["id"]] = sc
    return out


# --- sandbox + condition setup ----------------------------------------------


def real_panio() -> str:
    p = shutil.which("panio")
    if not p:
        sys.exit("error: `panio` not found on PATH (install it first)")
    return p


def make_sandbox(scenario: dict, real: str) -> dict:
    sb = Path(tempfile.mkdtemp(prefix=f"panio-eval-{scenario['id']}-"))
    (sb / "bin").mkdir()
    (sb / "project").mkdir()
    (sb / "home").mkdir()
    lab = sb / "lab.toml"
    argv_log = sb / "argv.log"

    # logging shim: records argv (tab-separated), then execs the real panio.
    shim = sb / "bin" / "panio"
    shim.write_text(
        "#!/bin/sh\n"
        "{ for a in \"$@\"; do printf '%s\\t' \"$a\"; done; printf '\\n'; } "
        '>> "$PANIO_ARGV_LOG"\n'
        f'exec "{real}" "$@"\n'
    )
    shim.chmod(0o755)

    # seed the lab: a fixture, or a fresh empty lab.
    seed = scenario.get("seed", "")
    if seed:
        shutil.copy(HERE / "fixtures" / seed, lab)
    else:
        proc = subprocess.run([real, "init", "--lab", str(lab)],
                              capture_output=True, text=True)
        if proc.returncode != 0:
            shutil.rmtree(sb, ignore_errors=True)
            raise RuntimeError(
                f"panio init failed (rc={proc.returncode}): "
                f"{(proc.stderr or proc.stdout).strip()}")

    return {"dir": sb, "lab": lab, "argv_log": argv_log}


CREDENTIALS_REL = Path(".claude") / ".credentials.json"


def base_env(sb: dict, isolation: str) -> dict:
    env = dict(os.environ)
    env["PATH"] = f"{sb['dir'] / 'bin'}:{env.get('PATH', '')}"
    env["PANIO_LAB"] = str(sb["lab"])
    env["PANIO_ARGV_LOG"] = str(sb["argv_log"])
    # Scope panio's runtime dir (daemon pids/sockets) to the sandbox in every
    # mode, so the agent's daemons — and the post-run `daemons stop --all`
    # sweep — never see the operator's real /tmp/panio-<uid>.
    runtime = sb["dir"] / "runtime"
    runtime.mkdir(parents=True, exist_ok=True)
    env["PANIO_RUNTIME_BASE"] = str(runtime)
    if isolation == "home":
        home = sb["dir"] / "home"
        env["HOME"] = str(home)
        # Carry credentials so the agent can still authenticate — as a symlink,
        # not a copy: a token refresh by the agent then lands in the real file,
        # and no credential copy is left behind in a kept sandbox. Removed by
        # `remove_credential_link` when the run ends.
        cred = Path.home() / CREDENTIALS_REL
        if cred.exists():
            link = home / CREDENTIALS_REL
            link.parent.mkdir(parents=True, exist_ok=True)
            if link.is_symlink() or link.exists():
                link.unlink()
            link.symlink_to(cred)
    return env


def remove_credential_link(sb: dict) -> None:
    """Drop the sandbox's credential entry, whatever it has become. A symlink
    is simply unlinked; a regular file means the agent replaced the link (e.g.
    a write-to-temp-and-rename refresh) — remove that too, since a kept sandbox
    must never hold a credential copy, and say that the refresh did not reach
    the real file."""
    path = sb["dir"] / "home" / CREDENTIALS_REL
    if path.is_symlink():
        path.unlink()
    elif path.exists():
        path.unlink()
        print(f"warning: {path} was a file, not the credential symlink — removed; "
              "a token refreshed during the run did not reach "
              f"~/{CREDENTIALS_REL}", file=sys.stderr)


def setup_condition(scenario: dict, sb: dict, condition: str) -> dict:
    """Returns extra agent args + system-prompt text for the condition."""
    extra_args: list[str] = []
    if condition == "preloaded":
        extra_args += ["--append-system-prompt", SKILL_MD.read_text()]
    elif condition == "warm":
        # register the panio skill as a project skill in the agent's cwd.
        dst = sb["dir"] / "project" / ".claude" / "skills" / "panio"
        dst.mkdir(parents=True, exist_ok=True)
        shutil.copy(SKILL_MD, dst / "SKILL.md")
    # cold: nothing pushed; guidance is pullable via `panio skill`.
    return {"extra_args": extra_args}


# --- execution modes ---------------------------------------------------------


def run_reference(scenario: dict, sb: dict, env: dict) -> dict:
    """Execute the scenario's golden reference commands (deterministic)."""
    log = []
    for cmd in scenario.get("reference", []):
        proc = subprocess.run(
            cmd, shell=True, env=env, capture_output=True, text=True,
            cwd=sb["dir"] / "project",
        )
        log.append(
            {"cmd": cmd, "rc": proc.returncode,
             "out": proc.stdout, "err": proc.stderr}
        )
    transcript = "\n".join(
        f"$ {e['cmd']}\n{e['out']}{e['err']}".rstrip() for e in log
    )
    return {"transcript": transcript, "exec_log": log}


AGENT_TIMEOUT = 360  # per-scenario wall clock; a hang here is usually a blocked sudo


def run_agent(scenario: dict, sb: dict, env: dict, agent: str,
              cond_args: list[str], goal: str) -> dict:
    cwd = sb["dir"] / "project"
    try:
        if agent == "agy":
            # agy's `-p` takes the prompt as the flag argument (not stdin).
            # Headless print mode auto-runs shell tools (no skip flag needed).
            # stdin=DEVNULL so a stray `sudo` gets EOF and fails fast.
            proc = subprocess.run(
                ["agy", "-p", goal], env=env, capture_output=True, text=True,
                cwd=cwd, stdin=subprocess.DEVNULL, timeout=AGENT_TIMEOUT,
            )
            return {"transcript": proc.stdout, "raw_stdout": proc.stdout,
                    "rc": proc.returncode, "stderr": proc.stderr}
        if agent != "claude":
            # generic: treat `agent` as a command; goal piped on stdin.
            proc = subprocess.run(
                agent, shell=True, input=goal, env=env,
                capture_output=True, text=True, cwd=cwd, timeout=AGENT_TIMEOUT,
            )
            return {"transcript": proc.stdout + proc.stderr,
                    "rc": proc.returncode}

        tools = ["Bash", "Read", "Grep", "Glob"]
        if (sb["dir"] / "project" / ".claude" / "skills").exists():
            tools.append("Skill")
        cmd = [
            "claude", "-p",
            "--output-format", "json",
            "--permission-mode", "bypassPermissions",
            *cond_args,
            "--allowedTools", *tools,
        ]
        proc = subprocess.run(
            cmd, input=goal, env=env, capture_output=True, text=True,
            cwd=cwd, timeout=AGENT_TIMEOUT,
        )
        transcript = _claude_transcript(proc.stdout) or proc.stdout
        return {"transcript": transcript, "raw_stdout": proc.stdout,
                "rc": proc.returncode, "stderr": proc.stderr}
    except subprocess.TimeoutExpired:
        return {"transcript": f"(agent timed out after {AGENT_TIMEOUT}s — it "
                "likely tried to execute a command that blocked, e.g. a sudo "
                "password prompt)", "rc": -1, "timeout": True}


def _claude_transcript(stdout: str) -> str | None:
    try:
        obj = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    if isinstance(obj, dict) and "result" in obj:
        return str(obj["result"])
    return None


# --- grading -----------------------------------------------------------------

_SURFACE_CACHE: str | None = None


def command_surface(real: str, force: bool = False) -> str:
    """Build (and cache) the real panio CLI surface: the recursive --help
    tree. Injected into the judge prompt so invent-resistance (D4) is graded
    against ground truth, not the judge's guesses about what commands exist.

    `force=True` ignores any cache and re-walks the live CLI — used by `--check`
    so drift is always measured against the installed binary, never a stale dump.
    The on-disk cache is also ignored when the panio binary is newer than it
    (a rebuild since the last walk), so a judge never grades against the
    previous build's surface.
    """
    global _SURFACE_CACHE
    if not force and _SURFACE_CACHE is not None:
        return _SURFACE_CACHE
    cache = HERE / ".panio_surface.txt"
    if not force and surface_cache_fresh(cache, real):
        _SURFACE_CACHE = cache.read_text()
        return _SURFACE_CACHE
    seen: set[tuple] = set()
    chunks: list[str] = []

    def walk(path: list[str]) -> None:
        key = tuple(path)
        if key in seen:
            return
        seen.add(key)
        r = subprocess.run([real, *path, "--help"], capture_output=True, text=True)
        txt = r.stdout or r.stderr
        chunks.append(f"$ panio {' '.join(path)} --help\n{txt}".rstrip())
        subs, in_cmds = [], False
        for line in txt.splitlines():
            s = line.strip()
            if s == "Commands:":
                in_cmds = True
                continue
            if in_cmds:
                if not s:
                    break
                m = re.match(r"\s+([a-z][a-z0-9-]*)\b", line)
                if m and m.group(1) != "help":
                    subs.append(m.group(1))
        for sub in subs:
            walk(path + [sub])

    walk([])
    _SURFACE_CACHE = "\n\n".join(chunks)
    cache.write_text(_SURFACE_CACHE)
    return _SURFACE_CACHE


def surface_cache_fresh(cache: Path, real: str) -> bool:
    """A cached --help dump is trustworthy only if it postdates the binary it
    was walked from (`panio --version` is constant across builds, so mtime
    is the signal)."""
    try:
        return cache.stat().st_mtime >= Path(real).stat().st_mtime
    except OSError:
        return False


def grade(scenario: dict, sb: dict, exec_result: dict, condition: str,
          judge_cmd: str | None, surface: str | None = None) -> dict:
    gtype = scenario.get("grader", {}).get("type", "judge")
    if gtype == "t1_config":
        return {"type": "t1_config",
                **t1_config.grade(str(sb["lab"]), scenario, str(sb["argv_log"]))}
    prompt = judge_mod.build_prompt(scenario, exec_result["transcript"],
                                    condition, surface=surface)
    (sb["dir"] / "judge_prompt.txt").write_text(prompt)
    if judge_cmd:
        res = judge_mod.run_judge(prompt, judge_cmd)
        verdict = res["verdict"] or {}
        return {"type": "judge", "passed": bool(verdict.get("pass")),
                "verdict": verdict, "raw": res["raw"]}
    return {"type": "judge", "passed": None,
            "note": "no --judge-cmd; prompt saved to judge_prompt.txt"}


# --- orchestration -----------------------------------------------------------


def run_one(scenario: dict, condition: str, mode: str, agent: str,
            isolation: str, judge_cmd: str | None, keep: bool) -> dict:
    real = real_panio()
    sb = make_sandbox(scenario, real)
    env = base_env(sb, isolation)
    try:
        return _run_one(scenario, sb, env, real, condition, mode, agent,
                        judge_cmd, keep)
    finally:
        remove_credential_link(sb)


def _run_one(scenario: dict, sb: dict, env: dict, real: str, condition: str,
             mode: str, agent: str, judge_cmd: str | None, keep: bool) -> dict:
    cond = setup_condition(scenario, sb, condition)
    if mode == "reference":
        exec_result = run_reference(scenario, sb, env)
    else:
        goal = scenario["goal"]
        # T0 is a *stated-command* tier: the agent may probe `--help`/`skill`
        # to discover commands, but must not execute the operational ones
        # (which need hardware/sudo and would block in this no-hardware sandbox).
        if scenario.get("tier") == "t0":
            goal = goal.rstrip() + (
                "\n\nNote: you may inspect `panio --help` and `panio skill` "
                "to discover commands, but do NOT execute the operational "
                "commands (netboot/netif/power/video/`serial watch`/etc.) — "
                "just list the exact sequence you would run.")
        exec_result = run_agent(scenario, sb, env, agent, cond["extra_args"], goal)
        # Safety sweep: a "list the commands" scenario shouldn't start daemons,
        # but an over-eager agent might. Don't leave a stray holding a port.
        # `env` carries the sandbox PANIO_RUNTIME_BASE, so this only ever
        # sees daemons the agent started — never the operator's.
        subprocess.run([real, "daemons", "stop", "--all"], env=env,
                       capture_output=True)
    if exec_result.get("timeout"):
        result = {"type": "timeout", "passed": False,
                  "reason": f"agent timed out after {AGENT_TIMEOUT}s"}
    else:
        gtype = scenario.get("grader", {}).get("type", "judge")
        surface = command_surface(real) if (gtype == "judge" and judge_cmd) else None
        result = grade(scenario, sb, exec_result, condition, judge_cmd, surface)
    (sb["dir"] / "transcript.txt").write_text(exec_result["transcript"])
    out = {
        "id": scenario["id"], "title": scenario.get("title", ""),
        "tier": scenario.get("tier"), "condition": condition, "mode": mode,
        "sandbox": str(sb["dir"]), "grade": result,
    }
    if not keep and result.get("passed"):
        shutil.rmtree(sb["dir"], ignore_errors=True)
        out["sandbox"] = "(removed; passed)"
    return out


def run_drift_check(scenarios: dict) -> int:
    """Validate every scenario reference + the T1-safe allowlist against the
    *live* CLI surface. The whole suite trusts these references (golden
    self-test + the judge's D4 ground truth), so this catches a renamed
    subcommand or dropped flag before it silently rots the eval."""
    real = real_panio()
    surface = command_surface(real, force=True)  # always re-walk the live CLI
    report = drift_mod.run_check(scenarios, surface, t1_config.SAFE)
    print(f"drift check: parsed {report['paths']} command paths from the live CLI")
    for s in report["scenarios"]:
        print(f"  [{s['id']}] reference drift:")
        for p in s["problems"]:
            print(f"      - {p}")
    for p in report["allowlist"]:
        print(f"  [allowlist] {p}")
    if report["ok"]:
        print("OK — all references and the T1-safe allowlist match the live CLI")
        return 0
    print("DRIFT DETECTED — update the references/allowlist to match the CLI")
    return 1


def fmt(out: dict) -> str:
    g = out["grade"]
    p = g.get("passed")
    tag = "PASS" if p else ("FAIL" if p is False else "????")
    extra = g.get("reason") or g.get("note") or ""
    return f"  [{tag}] {out['id']:4} {out['title'][:48]:48} {extra}"


def main() -> int:
    ap = argparse.ArgumentParser(description="panio agent-eval runner")
    ap.add_argument("--scenario", action="append", default=[],
                    help="scenario id (repeatable)")
    ap.add_argument("--all", action="store_true", help="run every scenario")
    ap.add_argument("--tier", choices=["t0", "t1"], help="filter by tier")
    ap.add_argument("--condition", default="cold",
                    choices=["cold", "warm", "preloaded"])
    ap.add_argument("--reference", action="store_true",
                    help="run golden reference commands instead of an agent")
    ap.add_argument("--agent", default="claude",
                    help="agent command (default: claude)")
    ap.add_argument("--isolation", default="light",
                    choices=["home", "light", "none"])
    ap.add_argument("--judge-cmd", default=None,
                    help="command to grade T0 scenarios (e.g. 'claude -p')")
    ap.add_argument("--keep", action="store_true",
                    help="keep sandbox dirs even on pass")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--check", action="store_true",
                    help="check every scenario reference + the T1-safe allowlist "
                         "against the live CLI surface; exit non-zero on drift")
    args = ap.parse_args()

    scenarios = load_scenarios()

    if args.check:
        return run_drift_check(scenarios)

    if args.list:
        for sid, sc in scenarios.items():
            core = "*" if sc.get("core") else " "
            ex = "exec" if "loopback" in sc else "    "
            print(f"{core} {sid:4} [{sc.get('tier')}] "
                  f"{sc.get('grader', {}).get('type', '?'):10} {ex} "
                  f"{sc.get('title', '')}")
        return 0

    if args.all:
        chosen = list(scenarios.values())
    elif args.scenario:
        unknown = [s for s in args.scenario if s not in scenarios]
        if unknown:
            ap.error(f"unknown scenario id(s): {', '.join(unknown)} "
                     "(see --list)")
        chosen = [scenarios[s] for s in args.scenario]
    else:
        ap.error("pass --scenario ID, --all, or --list")
    if args.tier:
        chosen = [s for s in chosen if s.get("tier") == args.tier]

    mode = "reference" if args.reference else "agent"
    if mode == "reference":
        chosen = [s for s in chosen if s.get("grader", {}).get("type") == "t1_config"]
        if not chosen:
            print("no t1_config scenarios match (reference mode is T1-only)")
            return 0

    results = []
    print(f"running {len(chosen)} scenario(s) [{mode}, condition={args.condition}]")
    out_dir = HERE / "results"
    out_dir.mkdir(exist_ok=True)
    summary = out_dir / f"run-{mode}-{args.condition}.json"
    for sc in chosen:
        try:
            out = run_one(sc, args.condition, mode, args.agent, args.isolation,
                          args.judge_cmd, args.keep or mode == "agent")
        except Exception as exc:  # one scenario's failure must not kill the batch
            out = {"id": sc.get("id"), "title": sc.get("title", ""),
                   "tier": sc.get("tier"), "condition": args.condition,
                   "mode": mode, "sandbox": "(error)",
                   "grade": {"passed": False,
                             "reason": f"runner error: {type(exc).__name__}: {exc}"}}
        results.append(out)
        print(fmt(out))
        summary.write_text(json.dumps(results, indent=2))  # incremental save
    npass = sum(1 for r in results if r["grade"].get("passed") is True)
    ngraded = sum(1 for r in results if r["grade"].get("passed") is not None)
    print(f"\n{npass}/{ngraded} graded scenarios passed  (summary: {summary})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
