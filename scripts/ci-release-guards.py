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

"""Prove release.yml publishes only on a pushed v* tag (#338).

`workflow_dispatch` is documented as artifacts-only, but a dispatch against a
tag sets `github.ref_type == 'tag'`, so a guard written on the ref alone lets
a manual run attach assets to a Release, bump the tap, refresh apt, and
submit to winget.

This script reads every job's `if:` and `needs:` out of the workflow,
evaluates the guards with a small GitHub-expression interpreter against each
trigger the workflow can see, follows `needs` the way the runner does (a job
whose dependency did not run is skipped), and compares the set of jobs that
would run with the expected set. Any job not in BUILD_JOBS is treated as
publishing, so a new publishing job is covered without editing this file.

Stdlib only: the CI runner and a dev Mac both have python3, neither is
guaranteed PyYAML, and the workflow layout here is regular enough to read by
indentation.
"""

import re
import sys

WORKFLOW = ".github/workflows/release.yml"

# Jobs that build and upload workflow artifacts. Everything else publishes.
BUILD_JOBS = {"profile-pins", "package", "package-windows", "package-macos"}

# Publishing jobs that also skip prerelease tags (v1.2.3-rc.1).
STABLE_ONLY = {"winget"}

# Each package job must finish before anything is published.
PACKAGE_JOBS = {"package", "package-windows", "package-macos"}


def ctx(event, ref):
    """The `github` context for one trigger."""
    if ref.startswith("refs/tags/"):
        ref_type, ref_name = "tag", ref[len("refs/tags/") :]
    else:
        ref_type, ref_name = "branch", ref[len("refs/heads/") :]
    return {
        "event_name": event,
        "ref": ref,
        "ref_type": ref_type,
        "ref_name": ref_name,
    }


# (label, github context, is a publishing trigger, is a prerelease)
SCENARIOS = [
    ("dispatch on a branch", ctx("workflow_dispatch", "refs/heads/main"), False, False),
    (
        "dispatch on a stable tag",
        ctx("workflow_dispatch", "refs/tags/v1.2.3"),
        False,
        False,
    ),
    (
        "dispatch on a prerelease tag",
        ctx("workflow_dispatch", "refs/tags/v1.2.3-rc.1"),
        False,
        True,
    ),
    ("push of a stable tag", ctx("push", "refs/tags/v1.2.3"), True, False),
    ("push of a prerelease tag", ctx("push", "refs/tags/v1.2.3-rc.1"), True, True),
]


# --- GitHub expression interpreter (the subset these guards use) ------------

TOKEN = re.compile(
    r"\s*(?:(?P<str>'(?:[^']|'')*')|(?P<op>==|!=|&&|\|\||[!(),])"
    r"|(?P<num>\d+)|(?P<ident>[A-Za-z_][A-Za-z0-9_.-]*))"
)


def tokenize(src):
    pos, out = 0, []
    src = src.strip()
    while pos < len(src):
        m = TOKEN.match(src, pos)
        if not m or m.end() == pos:
            raise ValueError(f"cannot parse expression at {src[pos:]!r}")
        pos = m.end()
        kind = m.lastgroup
        text = m.group(kind)
        if kind == "str":
            out.append(("str", text[1:-1].replace("''", "'")))
        elif kind == "num":
            out.append(("str", text))
        else:
            out.append((kind, text))
    return out


def _norm(v):
    # GitHub compares strings case-insensitively.
    return v.lower() if isinstance(v, str) else v


FUNCS = {
    "startswith": lambda a, b: _norm(str(a)).startswith(_norm(str(b))),
    "endswith": lambda a, b: _norm(str(a)).endswith(_norm(str(b))),
    "contains": lambda a, b: _norm(str(b)) in _norm(str(a)),
    "success": lambda: True,
    "always": lambda: True,
    "failure": lambda: False,
    "cancelled": lambda: False,
}


class Parser:
    """or > and > equality > unary (!) > primary, as in GitHub's grammar."""

    def __init__(self, tokens, context):
        self.toks, self.i, self.ctx = tokens, 0, context
        self.status_fn = False

    def peek(self):
        return self.toks[self.i] if self.i < len(self.toks) else (None, None)

    def take(self, text=None):
        tok = self.peek()
        if text is not None and tok[1] != text:
            raise ValueError(f"expected {text!r}, got {tok[1]!r}")
        self.i += 1
        return tok

    def parse(self):
        v = self.or_()
        if self.i != len(self.toks):
            raise ValueError(f"trailing tokens: {self.toks[self.i:]}")
        return v

    def or_(self):
        v = self.and_()
        while self.peek()[1] == "||":
            self.take()
            r = self.and_()
            v = v or r
        return v

    def and_(self):
        v = self.eq()
        while self.peek()[1] == "&&":
            self.take()
            r = self.eq()
            v = v and r
        return v

    def eq(self):
        v = self.unary()
        while self.peek()[1] in ("==", "!="):
            op = self.take()[1]
            r = self.unary()
            v = (_norm(v) == _norm(r)) if op == "==" else (_norm(v) != _norm(r))
        return v

    def unary(self):
        if self.peek()[1] == "!":
            self.take()
            return not self.unary()
        return self.primary()

    def primary(self):
        kind, text = self.take()
        if text == "(" and kind == "op":
            v = self.or_()
            self.take(")")
            return v
        if kind == "str":
            return text
        if kind != "ident":
            raise ValueError(f"unexpected token {text!r}")
        if self.peek()[1] == "(":
            self.take("(")
            args = []
            while self.peek()[1] != ")":
                args.append(self.or_())
                if self.peek()[1] == ",":
                    self.take()
            self.take(")")
            name = text.lower()
            if name not in FUNCS:
                raise ValueError(f"unsupported function {text}()")
            if name in ("success", "always", "failure", "cancelled"):
                self.status_fn = True
            return FUNCS[name](*args)
        if text in ("true", "false"):
            return text == "true"
        root, _, field = text.partition(".")
        if root != "github":
            raise ValueError(f"guard reads {text}; only the github context is modeled")
        if field not in self.ctx:
            raise ValueError(f"github.{field} is not modeled in the scenarios")
        return self.ctx[field]


def evaluate(expr, context):
    """Return (value, uses_status_function)."""
    expr = expr.strip()
    if expr.startswith("${{") and expr.endswith("}}"):
        expr = expr[3:-2]
    p = Parser(tokenize(expr), context)
    return bool(p.parse()), p.status_fn


# --- workflow reader ---------------------------------------------------------


def read_jobs(path):
    """{job id: {"if": str | None, "needs": [ids]}} from a workflow file."""
    jobs, cur, in_jobs, needs_list = {}, None, False, False
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if re.match(r"^jobs:\s*$", line):
                in_jobs = True
                continue
            if not in_jobs:
                continue
            if re.match(r"^\S", line):
                break
            m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
            if m:
                cur = jobs.setdefault(m.group(1), {"if": None, "needs": []})
                needs_list = False
                continue
            if cur is None:
                continue
            m = re.match(r"^    if:\s*(.+?)\s*$", line)
            if m:
                val = m.group(1)
                if len(val) >= 2 and val[0] == val[-1] and val[0] in "\"'":
                    val = val[1:-1]
                cur["if"] = val
                needs_list = False
                continue
            m = re.match(r"^    needs:\s*(.*?)\s*$", line)
            if m:
                val = m.group(1)
                if val.startswith("["):
                    cur["needs"] = [
                        n.strip() for n in val.strip("[]").split(",") if n.strip()
                    ]
                elif val:
                    cur["needs"] = [val]
                needs_list = not val
                continue
            if needs_list:
                m = re.match(r"^      -\s*([A-Za-z0-9_-]+)\s*$", line)
                if m:
                    cur["needs"].append(m.group(1))
                    continue
            if re.match(r"^    \S", line):
                needs_list = False
    return jobs


def runs(jobs, context):
    """Set of jobs the runner would start for this context."""
    ran, decided = set(), {}

    def decide(job):
        if job in decided:
            return decided[job]
        spec = jobs[job]
        deps_ok = all(decide(d) for d in spec["needs"])
        if spec["if"] is None:
            ok = deps_ok
        else:
            val, status_fn = evaluate(spec["if"], context)
            # Without a status function GitHub prepends success(): a job whose
            # dependency was skipped is skipped too.
            ok = val and (deps_ok or status_fn)
        decided[job] = ok
        if ok:
            ran.add(job)
        return ok

    for job in jobs:
        decide(job)
    return ran


def expected(jobs, publishing_trigger, prerelease):
    want = set()
    for job in jobs:
        if job in BUILD_JOBS:
            want.add(job)
        elif publishing_trigger and not (prerelease and job in STABLE_ONLY):
            want.add(job)
    return want


def check(path):
    jobs = read_jobs(path)
    errors = []
    missing = (BUILD_JOBS | STABLE_ONLY) - set(jobs)
    if missing:
        errors.append(f"{path}: expected jobs not found: {sorted(missing)}")
    publishing = sorted(set(jobs) - BUILD_JOBS)
    if not publishing:
        errors.append(f"{path}: found no publishing jobs; is the reader broken?")

    # Publishing must wait for every package job, directly or transitively.
    def ancestors(job, seen=None):
        seen = set() if seen is None else seen
        for d in jobs[job]["needs"]:
            if d in jobs and d not in seen:
                seen.add(d)
                ancestors(d, seen)
        return seen

    for job in publishing:
        lacking = PACKAGE_JOBS - ancestors(job)
        if lacking:
            errors.append(f"{job}: does not wait for {sorted(lacking)}")
        if jobs[job]["if"] is None:
            errors.append(f"{job}: publishing job has no if: guard")

    for label, context, pub, pre in SCENARIOS:
        try:
            got = runs(jobs, context)
        except ValueError as e:
            errors.append(f"{label}: {e}")
            continue
        want = expected(jobs, pub, pre)
        for job in sorted(got - want):
            errors.append(f"{label}: {job} would run (guard: {jobs[job]['if']})")
        for job in sorted(want - got):
            errors.append(f"{label}: {job} would be skipped (guard: {jobs[job]['if']})")
        print(f"  {label}: runs {', '.join(sorted(got))}")
    return errors


def self_test():
    """The interpreter must reject the pre-#338 guard, or the check is toothless."""
    old = {
        "package": {"if": None, "needs": []},
        "package-windows": {"if": None, "needs": []},
        "package-macos": {"if": None, "needs": []},
        "profile-pins": {"if": None, "needs": []},
        "release": {
            "if": "github.ref_type == 'tag'",
            "needs": ["package", "package-windows", "package-macos"],
        },
        "winget": {
            "if": "${{ github.ref_type == 'tag' && !contains(github.ref_name, '-') }}",
            "needs": ["release"],
        },
    }
    got = runs(old, SCENARIOS[1][1])
    assert {"release", "winget"} <= got, f"self-test: old guards not caught: {got}"
    assert evaluate("!contains('v1-rc', '-')", {})[0] is False
    assert evaluate("startsWith('refs/tags/V1', 'refs/tags/v')", {})[0] is True


def main(argv):
    path = argv[1] if len(argv) > 1 else WORKFLOW
    self_test()
    print(f"{path}:")
    errors = check(path)
    if errors:
        print(f"\nFAIL: {len(errors)} release guard problem(s):")
        for e in errors:
            print(f"  {e}")
        print(
            "\nEvery job outside BUILD_JOBS publishes and must be guarded with"
            "\n  github.event_name == 'push' && github.ref_type == 'tag'"
            " && startsWith(github.ref, 'refs/tags/v')"
            "\n(plus !contains(github.ref_name, '-') for stable-only channels)."
        )
        return 1
    print("\nRelease guards OK: only a pushed v* tag publishes.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
