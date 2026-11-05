# Copyright 2026 The Panio Authors
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

"""A container job's steps must not use bash-only shell syntax.

A job with `container:` gets `sh -e {0}` as its default shell, and in the
Debian images this repo builds in, /bin/sh is dash. dash has no `pipefail`
and exits 2 on `set -o pipefail` with "Illegal option -o pipefail".

This is a release-blocking class of bug rather than an ordinary one, because
`release.yml` runs only on a tag push: pull-request CI never executes it, and
the release train builds the .deb by replaying the profile's recipe by hand
rather than by running the workflow. So nothing exercises these steps until
a tag is pushed, and by then the tag exists. That is exactly what happened on
2026-09-19 -- the v0.4.0 tag build failed on both Linux arches, no GitHub
Release was produced, and the tag had to be moved.

A step that genuinely needs bash may have it, by declaring `shell: bash`
(the Debian images do ship bash); this test only rejects bash syntax in a
step that has not asked for bash.
"""

import pathlib
import re

import pytest
import yaml

ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOWS = sorted((ROOT / ".github" / "workflows").glob("*.yml"))

# (pattern, what to say). Only constructs that dash actually rejects or
# silently mis-parses; this is a guard, not a style checker.
BASH_ONLY = [
    (re.compile(r"\[\["), "[[ ]] test (dash: not found)"),
    (re.compile(r"<<<"), "<<< here-string (dash: unsupported)"),
    (re.compile(r"\$\{[A-Za-z_][A-Za-z0-9_]*:\d"), "${VAR:offset} slicing (dash: bad substitution)"),
]

# `set -o <name>` is checked by name, not by shape. dash has -o options and
# takes most of the POSIX ones, so rejecting the whole form would fail a step
# for writing `set -o errexit`, which dash runs happily. Verified against dash
# 0.5.12: errexit, nounset, noglob, xtrace and verbose are all accepted, and
# pipefail is the one that exits 2 with "Illegal option". Anything not on this
# list is reported rather than assumed fine, so a bash-only option nobody
# thought of still trips the guard.
DASH_SET_O = frozenset(
    """allexport errexit ignoreeof monitor noclobber noexec noglob nolog notify
       nounset verbose vi xtrace emacs""".split()
)
SET_O = re.compile(r"\bset\s+(?:-[a-z]+\s+)*-[a-z]*o\s+([A-Za-z_-]+)")


COMMENT = re.compile(r"^\s*#")


def code_only(run: str) -> str:
    """`run` with whole-line comments dropped.

    A comment that discusses the banned syntax -- including the one above the
    step this guard was written for -- is prose, not something dash will ever
    execute, and failing CI over it would only teach people to stop explaining
    themselves. A `#` inside a quoted string on a code line is not handled;
    this is a guard, not a shell parser.
    """
    return "\n".join(line for line in run.splitlines() if not COMMENT.match(line))


def container_steps():
    """(workflow, job, step name, run block) for every sh-shelled container step."""
    found = []
    for path in WORKFLOWS:
        doc = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        for job_name, job in (doc.get("jobs") or {}).items():
            if not isinstance(job, dict) or not job.get("container"):
                continue
            # A job-level default shell applies to every step that has none.
            job_shell = ((job.get("defaults") or {}).get("run") or {}).get("shell")
            if job_shell:
                continue
            for i, step in enumerate(job.get("steps") or []):
                if not isinstance(step, dict) or "run" not in step:
                    continue
                if step.get("shell"):
                    continue
                name = step.get("name") or f"step {i}"
                found.append((path.name, job_name, name, step["run"]))
    return found


def bash_only_hits(run: str):
    """Every reason `run` would not survive dash. The matcher, in one place."""
    code = code_only(run)
    hits = [why for pattern, why in BASH_ONLY if pattern.search(code)]
    for name in SET_O.findall(code):
        if name not in DASH_SET_O:
            hits.append(f"set -o {name} (dash does not accept it)")
    return hits


# (snippet, should it be reported). The `set -o` rows are the point: an
# earlier version of this guard rejected the form itself and would have
# failed a step for `set -o errexit`, which dash accepts. Every "allow" row
# here was run through real dash before being written down.
MATCHER_CASES = [
    ("set -eu", False),
    ("set -euo pipefail", True),
    ("set -o pipefail", True),
    ("set -o errexit", False),
    ("set -o nounset", False),
    ("set -o noglob", False),
    ("set -o xtrace", False),
    ("set -e\nset -o verbose", False),
    ("set -o histexpand", True),
    ("set -o posix", True),
    ("if [[ -f x ]]; then :; fi", True),
    ("cat <<< hi", True),
    ("echo ${V:0:3}", True),
    ("# set -o pipefail is what broke the release\nset -eu", False),
    ("echo ok", False),
]


@pytest.mark.parametrize("snippet,reported", MATCHER_CASES)
def test_matcher_reports_only_what_dash_rejects(snippet, reported):
    hits = bash_only_hits(snippet)
    assert bool(hits) == reported, f"{snippet!r} -> {hits}"


def test_there_are_container_steps_to_check():
    """A guard that silently checks nothing is worse than no guard."""
    steps = container_steps()
    assert steps, "no container-job steps found -- has the workflow layout changed?"


@pytest.mark.parametrize(
    "workflow,job,step,run",
    container_steps(),
    ids=lambda v: v if isinstance(v, str) and len(v) < 40 else "",
)
def test_container_step_avoids_bash_only_syntax(workflow, job, step, run):
    hits = bash_only_hits(run)
    assert not hits, (
        f"{workflow}: job '{job}', step '{step}' runs under dash "
        f"(container job, no `shell:`) but uses: {', '.join(hits)}. "
        f"Use POSIX syntax, or add `shell: bash` to the step."
    )


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__]))
