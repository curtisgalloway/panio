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
"""LLM-as-judge grader for T0 (stated-command) and meta scenarios.

This module builds the judge prompt from a scenario + the agent transcript.
It does not hard-code a model client: pass a `judge_cmd` (e.g. "claude -p")
to `run_judge` and it pipes the prompt in and parses the JSON verdict back.
With no judge_cmd, callers just persist the prompt for manual/automated
grading. See docs/dev/agent-evals.md sec. 6.2.
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
from typing import Any

# CLIs whose non-interactive mode takes the prompt as an ARGUMENT (not stdin).
# `claude -p` reads stdin; `agy -p "<prompt>"` needs it as a flag argument.
_ARG_PROMPT_AGENTS = {"agy"}

RUBRIC = """\
Score each dimension 0, 1, or 2 with a one-sentence justification that QUOTES
the transcript:
  D1 discovery        consulted an authoritative source (panio --help / skill /
                      docs) fitting the goal before acting   [N/A under preloaded]
  D2 right_capability chose the correct subsystem/command
  D3 command_form     correct subcommand, flags, -t/positional convention
  D4 invent_resist    used ONLY real commands/flags  [HARD-FAIL]
  D5 gotcha_aware     respected the scenario's documented trap(s)
  D6 goal_achieved    the stated sequence would achieve the goal
  D7 efficiency       few/no wrong turns or redundant help reads  [soft]

Hard-fail rule: set hard_fail=true and pass=false if D4 is 0 -- i.e. the agent
ran or recommended a command or flag that does NOT exist in panio. Judge D4
*strictly against the COMMAND REFERENCE below* -- that is the real CLI surface.
A command, subcommand, or flag present in (or consistent with) the reference is
REAL: this includes top-level commands like `power-cycle`, `console`,
`power-state`; channel verbs like `video watch`, `hid serve`; and short-flag
aliases / abbreviations (e.g. `-o` for `--out`). Do NOT flag something as
invented just because the scenario's REFERENCE used a different command -- check
the COMMAND REFERENCE. Only hard-fail when a command/flag is clearly absent from
and inconsistent with the reference.

Trap accountability (drives D5): evaluate EACH trap listed under TRAPS PROBED
separately. For each, decide respected=true/false and quote the transcript line
that shows it. D5 is 2 only if every trap is respected, 1 if a minor trap is
missed, 0 if a central trap is walked into. Report this as `traps_eval`.

PASS RULE (apply mechanically, do not eyeball): pass=true iff ALL hold --
  (a) hard_fail is false;
  (b) D2, D3, and D6 are each >= 1 (right capability, well-formed, would work);
  (c) no central trap was walked into (the corresponding traps_eval entry is
      respected=false on a trap the scenario calls central);
  (d) the weighted total is >= 0.75 of max, where weighted = d1+d2+d3+d6 plus
      2*d4 (invention weighted double) plus d5, with d7 excluded (soft); max is
      2*(1+1+1+1+2+1) = 16, so the threshold is >= 12.
Under `preloaded`, omit d1 from both the weighted sum and the max (max 14,
threshold >= 10.5 -> 11)."""


def build_prompt(scenario: dict, transcript: str, condition: str,
                 surface: str | None = None) -> str:
    ref = scenario.get("reference", [])
    if isinstance(ref, list):
        ref_text = "\n".join(f"  - {r}" for r in ref)
    else:
        ref_text = str(ref)
    traps = scenario.get("traps", [])
    traps_text = "\n".join(f"  - {t}" for t in traps) or "  (none)"
    d1 = (
        "Under this condition D1 is N/A; score it 2 and note 'preloaded'."
        if condition == "preloaded"
        else "Score D1 normally."
    )
    surface_block = (
        f"\nCOMMAND REFERENCE (the real panio CLI surface -- the ground truth "
        f"for D4 invent-resistance):\n<<<\n{surface.strip()}\n>>>\n"
        if surface else ""
    )
    return f"""\
You are grading whether a coding agent correctly DISCOVERED and USED the
`panio` CLI to accomplish a user's goal. panio controls a physical target
machine (netboot, serial console, HDMI capture + OCR, power, USB HID, adb).
You are given the goal, a REFERENCE approach an expert would take, the TRAPS
this scenario probes, the discovery CONDITION, the real COMMAND REFERENCE, and
the agent's full TRANSCRIPT.

{RUBRIC}

CONDITION: {condition}.  {d1}

GOAL:
{scenario.get('goal', '').strip()}

REFERENCE (expert approach -- not the only correct path):
{ref_text}

TRAPS PROBED:
{traps_text}
{surface_block}
AGENT TRANSCRIPT:
<<<
{transcript.strip()}
>>>

Respond with ONLY a JSON object:
{{"scores": {{"d1": int, "d2": int, "d3": int, "d4": int, "d5": int,
"d6": int, "d7": int}}, "justifications": {{"d1": str, ...}},
"traps_eval": [{{"trap": str, "respected": bool, "evidence": str}}],
"weighted": int, "hard_fail": bool, "pass": bool}}
Compute "pass" by the PASS RULE above (report the "weighted" total you used);
do not approximate."""


def run_judge(prompt: str, judge_cmd: str, timeout: int = 240) -> dict:
    """Run `judge_cmd` over the prompt and parse a JSON verdict from stdout.

    Passes the prompt on stdin for stdin-reading CLIs (e.g. `claude -p`), or as
    a trailing argument for arg-prompt CLIs (e.g. `agy -p`).
    """
    parts = shlex.split(judge_cmd)
    prog = os.path.basename(parts[0]) if parts else ""
    if prog in _ARG_PROMPT_AGENTS:
        if not any(p in ("-p", "--print", "--prompt") for p in parts):
            parts.append("-p")
        proc = subprocess.run(
            [*parts, prompt], capture_output=True, text=True, timeout=timeout
        )
    else:
        proc = subprocess.run(
            judge_cmd, shell=True, input=prompt,
            capture_output=True, text=True, timeout=timeout,
        )
    out = proc.stdout.strip()
    return {"verdict": _extract_json(out), "raw": out, "stderr": proc.stderr}


def _extract_json(text: str) -> Any:
    """Find the verdict JSON even when the model prepends prose.

    A naive first-`{`/last-`}` slice breaks when the narration contains stray
    braces (e.g. a judge writing Rust like `PowerCycle { target }` before the
    JSON). Scan every `{`, try to decode a complete JSON value there, and keep
    the last verdict-shaped object.
    """
    dec = json.JSONDecoder()
    best = None
    i = 0
    while i < len(text):
        if text[i] != "{":
            i += 1
            continue
        try:
            obj, end = dec.raw_decode(text, i)
        except json.JSONDecodeError:
            i += 1
            continue
        if isinstance(obj, dict):
            if "pass" in obj or "scores" in obj:
                best = obj  # prefer the verdict object; keep the last one
            elif best is None:
                best = obj
        i = end
    return best
