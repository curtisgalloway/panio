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
"""Drift check: do the eval suite's references still match the live CLI?

The whole suite leans on the scenario `reference` lists being *real* panio
commands — they are the golden T1 self-test and the judge's ground truth for D4
(invent-resistance). If the CLI renames a subcommand or drops a flag, those
references silently rot: the judge keeps "verifying" against a stale surface and
the golden self-test starts exercising the wrong path. This module turns "the
references match the CLI" into a deterministic check that fails loudly.

It parses the recursive `--help` surface (the same text the judge gets as its
COMMAND REFERENCE) into a map of `path -> {flags, subcommands}`, then asserts:

  * every `panio <subcommand-path>` in every scenario's `reference` exists;
  * every `--flag`/`-x` on those lines is valid for that subcommand-path;
  * every group/action in the T1-safe allowlist (graders/t1_config.SAFE) is a
    real group/subcommand.

Run standalone: `python3 graders/drift.py <surface.txt>`, or via the runner:
`python3 run.py --check`.
"""

from __future__ import annotations

import re
import shlex
import sys
from pathlib import Path

# Flags clap exposes on every subcommand; valid regardless of path.
GLOBAL_FLAGS = {"--lab", "--help", "-h", "--version", "-V"}

_FLAG_LONG = re.compile(r"--[a-z][a-z0-9-]*")
# a lone short flag: -t, -o, -V — not "-3" (a negative number) or "--x".
_FLAG_SHORT = re.compile(r"(?<![\w-])-[a-zA-Z](?![\w-])")


def parse_surface(text: str) -> dict[tuple[str, ...], dict[str, set[str]]]:
    """Parse the recursive --help dump into {path: {flags, subs}}.

    The surface is a concatenation of blocks, each headed by a literal
    `$ panio <path> --help` line, followed by clap's standard
    Usage/Arguments/Options/Commands sections.
    """
    paths: dict[tuple[str, ...], dict[str, set[str]]] = {}
    for chunk in text.split("$ panio "):
        first, _, body = chunk.partition("\n")
        first = first.strip()
        if not first.endswith("--help"):
            continue
        path_str = first[: -len("--help")].strip()
        path = tuple(path_str.split())
        flags: set[str] = set()
        subs: set[str] = set()
        variadic = False
        section: str | None = None
        for line in body.splitlines():
            s = line.strip()
            if s.startswith("Usage:"):
                # A trailing `[ARGS]...` means this command passes its tail
                # through verbatim (e.g. `helper <name> <helper-args>`), so any
                # flags after it are opaque, not panio flags.
                variadic = "..." in s
                section = None
                continue
            if s in ("Options:", "Commands:"):
                section = "opt" if s == "Options:" else "cmd"
                continue
            # A new column-0, non-empty line (Arguments:/another header) always
            # ends the current section.
            if s and not line.startswith((" ", "\t")):
                section = None
                continue
            if section == "opt":
                # clap's *long* help puts each option on its own line with the
                # description (or a whitespace-only line, for undocumented opts)
                # below it — so a blank line does NOT end the Options block.
                flags.update(_FLAG_LONG.findall(line))
                flags.update(_FLAG_SHORT.findall(line))
            elif section == "cmd":
                # Commands are listed contiguously; a blank line ends the block
                # (guards against a wrapped description being read as a subcmd).
                if not s:
                    section = None
                    continue
                m = re.match(r"\s+([a-z][a-z0-9-]+)", line)
                if m and m.group(1) != "help":
                    subs.add(m.group(1))
        paths[path] = {"flags": flags, "subs": subs, "variadic": variadic}
    return paths


def _match_path(lead: list[str], paths: dict) -> tuple[str, ...] | None:
    """Longest known command-path that prefixes the leading non-flag tokens."""
    for n in range(len(lead), 0, -1):
        cand = tuple(lead[:n])
        if cand in paths:
            return cand
    return () if () in paths else None


def check_line(line: str, paths: dict) -> list[str]:
    """Validate one `reference` line against the parsed surface.

    Non-`panio` lines (shell like `cp ...`, prose) are skipped. Returns a
    list of human-readable problems (empty == clean).
    """
    # strip a trailing ` # comment` (whitespace-hash), not a `#` inside a token.
    line = re.split(r"\s#", line, maxsplit=1)[0].strip()
    try:
        # shlex respects quoting, so a `-d` inside `"shellyplug -d ... cycle"`
        # is one token, not a stray panio short flag.
        toks = shlex.split(line)
    except ValueError:
        toks = line.split()
    if not toks or toks[0] != "panio":
        return []
    rest = toks[1:]
    lead: list[str] = []
    for t in rest:
        if t.startswith("-"):
            break
        lead.append(t)
    path = _match_path(lead, paths)
    if path is None:
        return [f"unknown command: `panio {lead[0]}`"]
    problems: list[str] = []
    # If the matched path is a *group* (has subcommands) and a leading token
    # remains, that token had to be a subcommand — and since `_match_path`
    # already extends to the longest *known* path, an unconsumed token here is
    # an invented sub/top-level command (every real subcommand has its own help
    # block, so it would have been matched). Leaf commands take positionals, so
    # leftover tokens there are fine.
    subs = paths.get(path, {}).get("subs", set())
    if subs and len(lead) > len(path):
        nxt = lead[len(path)]
        where_g = f"`panio {' '.join(path)}`" if path else "panio"
        problems.append(f"unknown subcommand `{nxt}` of {where_g}")
        return problems
    if paths.get(path, {}).get("variadic"):
        # trailing args are passed through verbatim — their flags aren't ours.
        return problems
    valid = paths.get(path, {}).get("flags", set()) | GLOBAL_FLAGS
    where = f"`panio {' '.join(path)}`" if path else "`panio`"
    for t in rest:
        if t.startswith("--"):
            fl = t.split("=", 1)[0]
            if fl not in valid:
                problems.append(f"unknown flag {fl} for {where}")
        elif t.startswith("-") and len(t) > 1 and not t[1:].lstrip("-")[:1].isdigit():
            if t not in valid:
                problems.append(f"unknown short flag {t} for {where}")
    return problems


def check_scenarios(scenarios: dict, paths: dict) -> list[dict]:
    """Check every scenario's reference list. Returns per-scenario problems."""
    out = []
    for sid, sc in scenarios.items():
        ref = sc.get("reference", [])
        if not isinstance(ref, list):
            ref = [ref]
        problems = []
        for entry in ref:
            # A command reference is a single-line element (`panio serial
            # watch dut`). A multi-line element is a PROSE block (the meta/
            # discovery scenarios describe the ideal answer in sentences that
            # legitimately contain the word "panio") — not a command list, so
            # don't try to parse it.
            lines = [ln for ln in str(entry).splitlines() if ln.strip()]
            if len(lines) != 1:
                continue
            problems += check_line(lines[0], paths)
        if problems:
            out.append({"id": sid, "problems": problems})
    return out


def check_allowlist(safe: dict, paths: dict) -> list[str]:
    """Every group/action in t1_config.SAFE must be a real group/subcommand."""
    problems = []
    for group, actions in safe.items():
        if (group,) not in paths:
            problems.append(f"allowlist group `{group}` is not a real panio command")
            continue
        if not actions:
            continue
        real = paths[(group,)]["subs"]
        for action in actions:
            if action not in real and (group, action) not in paths:
                problems.append(
                    f"allowlist action `{group} {action}` is not a real subcommand"
                )
    return problems


def run_check(scenarios: dict, surface: str, safe: dict) -> dict:
    """Top-level: parse surface, check scenarios + allowlist, return a report."""
    paths = parse_surface(surface)
    scen = check_scenarios(scenarios, paths)
    allow = check_allowlist(safe, paths)
    ok = not scen and not allow
    return {"ok": ok, "paths": len(paths), "scenarios": scen, "allowlist": allow}


def _main(argv: list[str]) -> int:
    if len(argv) < 2:
        print("usage: drift.py <surface.txt> [scenarios_dir]", file=sys.stderr)
        return 2
    import tomllib

    surface = Path(argv[1]).read_text()
    scen_dir = Path(argv[2]) if len(argv) > 2 else Path(__file__).parent.parent / "scenarios"
    scenarios = {}
    for f in sorted(scen_dir.glob("*.toml")):
        with open(f, "rb") as fh:
            sc = tomllib.load(fh)
        scenarios[sc["id"]] = sc
    sys.path.insert(0, str(Path(__file__).parent))
    from t1_config import SAFE  # noqa: E402

    report = run_check(scenarios, surface, SAFE)
    print(f"parsed {report['paths']} command paths from the surface")
    for s in report["scenarios"]:
        print(f"  [{s['id']}] reference drift:")
        for p in s["problems"]:
            print(f"      - {p}")
    for p in report["allowlist"]:
        print(f"  [allowlist] {p}")
    print("OK — references match the live CLI" if report["ok"] else "DRIFT DETECTED")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv))
