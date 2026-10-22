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

"""Doc-only PR skipping: the classifier, the strip step, and ci.yml's shape.

A job skipped by `if:` counts as a passing required check, so the classifier
must fail open, and every skippable job must delete the doc-only files from its
checkout so that no build can quietly come to depend on one. The first two
groups of tests execute scripts/ci-doc-only.sh in throwaway git repos. The
last group reads the real ci.yml and fails when a new job forgets the pattern.
"""

import os
import pathlib
import subprocess

import pytest
import yaml

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci-doc-only.sh"
CI_YML = ROOT / ".github" / "workflows" / "ci.yml"

# Must match the ALWAYS-RUN list in the comment on ci.yml's `changes` job.
ALWAYS_RUN = {
    "coverage",
    "actions-pinned",
    "release-guards",
    "profile-pins",
    "shellcheck",
    "evals-check",
}
GATE_IF = "${{ !cancelled() && needs.changes.outputs.code != 'false' }}"
STRIP_NAME = "Strip doc-only files (enforces the doc-only skip invariant)"


def git(repo, *args):
    return subprocess.run(
        ["git", "-C", str(repo), "-c", "user.email=t@example.invalid",
         "-c", "user.name=t", *args],
        check=True, capture_output=True, text=True,
    ).stdout.strip()


def write(repo, rel, text="x\n"):
    path = repo / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def commit(repo, msg="c"):
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", msg)
    return git(repo, "rev-parse", "HEAD")


@pytest.fixture
def repo(tmp_path):
    r = tmp_path / "repo"
    r.mkdir()
    git(r, "init", "-q", "-b", "main")
    write(r, "README.md")
    write(r, "cli/src/main.rs", "fn main() {}\n")
    return r


def run_script(repo, mode, env=None, check=True):
    full = {k: v for k, v in os.environ.items()
            if k not in ("GITHUB_EVENT_NAME", "BASE_SHA", "HEAD_SHA",
                         "GITHUB_OUTPUT")}
    full.update(env or {})
    proc = subprocess.run(
        ["bash", str(SCRIPT), mode], cwd=repo, env=full,
        capture_output=True, text=True,
    )
    if check:
        assert proc.returncode == 0, proc.stderr + proc.stdout
    return proc


def classify(repo, base, head, event="pull_request", **extra):
    env = {"GITHUB_EVENT_NAME": event}
    if base is not None:
        env["BASE_SHA"] = base
    if head is not None:
        env["HEAD_SHA"] = head
    env.update(extra)
    return run_script(repo, "classify", env)


def code_of(proc):
    lines = [l for l in proc.stdout.splitlines() if l.startswith("code=")]
    assert len(lines) == 1, proc.stdout
    return lines[0]


def pr(repo, changes):
    """Commit `changes` (path -> text, or None to delete) on top of main."""
    base = git(repo, "rev-parse", "HEAD")
    for rel, text in changes.items():
        if text is None:
            (repo / rel).unlink()
        else:
            write(repo, rel, text)
    return base, commit(repo)


def test_doc_only_pr_is_not_code(repo):
    commit(repo, "base")
    base, head = pr(repo, {"README.md": "new\n", "docs/a/b.png": "bin",
                           "skills/panio/SKILL.md": "s\n", "mkdocs.yml": "a: 1\n",
                           "LICENSE": "l\n"})
    assert code_of(classify(repo, base, head)) == "code=false"


def test_mixed_pr_is_code(repo):
    commit(repo, "base")
    base, head = pr(repo, {"README.md": "new\n", "cli/src/main.rs": "// c\n"})
    proc = classify(repo, base, head)
    assert code_of(proc) == "code=true"
    assert "cli/src/main.rs" in proc.stdout


@pytest.mark.parametrize("path", [
    ".github/workflows/ci.yml",
    "skills/x/run.sh",
    "scripts/ci-doc-only.sh",
    "Cargo.lock",
    "docs.md.txt",
])
def test_non_doc_path_is_code(repo, path):
    commit(repo, "base")
    base, head = pr(repo, {path: "y\n"})
    assert code_of(classify(repo, base, head)) == "code=true"


def test_push_event_is_code(repo):
    commit(repo, "base")
    base, head = pr(repo, {"README.md": "new\n"})
    assert code_of(classify(repo, base, head, event="push")) == "code=true"


@pytest.mark.parametrize("missing", ["base", "head"])
def test_missing_sha_is_code(repo, missing):
    commit(repo, "base")
    base, head = pr(repo, {"README.md": "new\n"})
    proc = classify(repo, None if missing == "base" else base,
                    None if missing == "head" else head)
    assert code_of(proc) == "code=true"


def test_bogus_sha_fails_open_with_exit_zero(repo):
    commit(repo, "base")
    proc = classify(repo, "0" * 40, "1" * 40)
    assert proc.returncode == 0
    assert code_of(proc) == "code=true"


def test_empty_diff_is_code(repo):
    head = commit(repo, "base")
    assert code_of(classify(repo, head, head)) == "code=true"


def test_not_a_git_repo_fails_open(tmp_path):
    proc = classify(tmp_path, "a" * 40, "b" * 40)
    assert code_of(proc) == "code=true"


def test_paths_with_spaces(repo):
    commit(repo, "base")
    base, head = pr(repo, {"docs/my notes/a b.md": "n\n", "docs/x y.png": "b"})
    assert code_of(classify(repo, base, head)) == "code=false"
    base, head = pr(repo, {"cli/src/a b.rs": "n\n"})
    assert code_of(classify(repo, base, head)) == "code=true"


def test_renamed_doc_file_is_doc_only(repo):
    write(repo, "docs/old.md", "content that is long enough to detect a rename\n" * 5)
    commit(repo, "base")
    base = git(repo, "rev-parse", "HEAD")
    git(repo, "mv", "docs/old.md", "docs/new.md")
    head = commit(repo)
    assert code_of(classify(repo, base, head)) == "code=false"


def test_rename_of_code_into_docs_is_code(repo):
    write(repo, "cli/src/lib.rs", "pub fn f() {}\n" * 20)
    commit(repo, "base")
    base = git(repo, "rev-parse", "HEAD")
    (repo / "docs").mkdir()
    git(repo, "mv", "cli/src/lib.rs", "docs/lib.md")
    head = commit(repo)
    assert code_of(classify(repo, base, head)) == "code=true"


def test_merge_base_diff_ignores_new_commits_on_base(repo):
    commit(repo, "base")
    fork = git(repo, "rev-parse", "HEAD")
    git(repo, "checkout", "-q", "-b", "topic")
    write(repo, "README.md", "topic\n")
    head = commit(repo, "topic")
    git(repo, "checkout", "-q", "main")
    write(repo, "cli/src/main.rs", "// moved on\n")
    base_tip = commit(repo, "main moved on")
    assert fork != base_tip
    assert code_of(classify(repo, base_tip, head)) == "code=false"


def test_github_output_receives_the_line(repo, tmp_path):
    commit(repo, "base")
    base, head = pr(repo, {"README.md": "new\n"})
    out = tmp_path / "gh_output"
    out.write_text("other=1\n")
    classify(repo, base, head, GITHUB_OUTPUT=str(out))
    assert out.read_text() == "other=1\ncode=false\n"
    classify(repo, base, head, event="push", GITHUB_OUTPUT=str(out))
    assert out.read_text().endswith("code=false\ncode=true\n")


def test_strip_removes_exactly_the_allowlist(repo):
    docs = ["README.md", "docs/a.md", "docs/img/x.png", "docs/sp ace/n o.md",
            "skills/panio/SKILL.md", "AGENTS.md", "cli/AGENTS.md",
            "mkdocs.yml", "LICENSE"]
    keep = ["cli/src/main.rs", "skills/panio/run.sh", ".github/workflows/ci.yml",
            "scripts/ci-doc-only.sh", "mkdocs.yml.bak", "LICENSE-APACHE",
            "doc/a.txt", "cli/Cargo.toml"]
    for rel in docs + keep:
        write(repo, rel)
    commit(repo)
    write(repo, "untracked.md")
    write(repo, "docs/untracked.png")
    proc = run_script(repo, "strip")
    assert f"stripped {len(set(docs))} " in proc.stdout
    for rel in set(docs):
        assert not (repo / rel).exists(), rel
    for rel in keep:
        assert (repo / rel).exists(), rel
    assert (repo / "untracked.md").exists()
    assert (repo / "docs/untracked.png").exists()
    assert git(repo, "ls-files", "README.md") == "README.md"


def test_strip_without_git_uses_find(tmp_path):
    write(tmp_path, "README.md")
    write(tmp_path, "docs/a b.md")
    write(tmp_path, "cli/src/main.rs")
    run_script(tmp_path, "strip", {"PANIO_STRIP_NO_GIT": "1"})
    assert not (tmp_path / "README.md").exists()
    assert not (tmp_path / "docs/a b.md").exists()
    assert (tmp_path / "cli/src/main.rs").exists()


def test_strip_fails_loudly_outside_git(tmp_path):
    assert run_script(tmp_path, "strip", check=False).returncode != 0


# --- structural guard over the real ci.yml ---------------------------------

@pytest.fixture(scope="module")
def ci():
    return yaml.safe_load(CI_YML.read_text())


def test_no_workflow_level_path_filters(ci):
    # PyYAML reads the bare key `on` as boolean True.
    triggers = ci.get("on", ci.get(True))
    for event, cfg in triggers.items():
        cfg = cfg or {}
        assert "paths" not in cfg and "paths-ignore" not in cfg, (
            f"{event}: a path-filtered workflow reports no checks and leaves "
            "required checks pending forever; gate with job-level if: instead")


def test_always_run_jobs_exist_and_are_ungated(ci):
    jobs = ci["jobs"]
    assert ALWAYS_RUN <= set(jobs), ALWAYS_RUN - set(jobs)
    for name in ALWAYS_RUN:
        job = jobs[name]
        assert "needs" not in job and "if" not in job, name
        for step in job["steps"]:
            assert step.get("name") != STRIP_NAME, f"{name} must not strip"


def test_changes_job_shape(ci):
    job = ci["jobs"]["changes"]
    assert job["outputs"]["code"] == "${{ steps.classify.outputs.code }}"
    assert job["steps"][0]["with"]["fetch-depth"] == 0


def skippable(ci):
    return [n for n in ci["jobs"] if n != "changes" and n not in ALWAYS_RUN]


def test_every_other_job_is_gated_and_strips(ci):
    names = skippable(ci)
    assert names, "no skippable jobs found"
    for name in names:
        job = ci["jobs"][name]
        assert job.get("needs") in ("changes", ["changes"]) or (
            isinstance(job.get("needs"), list) and "changes" in job["needs"]
        ), f"{name}: must need `changes`"
        assert job.get("if") == GATE_IF, f"{name}: wrong or missing fail-open if"
        steps = job["steps"]
        idx = [i for i, s in enumerate(steps)
               if str(s.get("uses", "")).startswith("actions/checkout@")]
        assert idx, f"{name}: no actions/checkout step"
        nxt = steps[idx[0] + 1]
        assert nxt.get("name") == STRIP_NAME, (
            f"{name}: the step right after checkout must be '{STRIP_NAME}'")
        assert nxt.get("run") == "bash scripts/ci-doc-only.sh strip", name
        assert nxt.get("shell") == "bash", f"{name}: needs shell: bash (Windows)"
        assert nxt.get("working-directory") == "${{ github.workspace }}", (
            f"{name}: job defaults may set a subdirectory; strip must run at the root")
