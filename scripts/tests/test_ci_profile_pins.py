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

"""Tests for issue #219: scripts/ci-profile-pins.sh fails the build when a
path pinned in RELEASE-TRAIN.md's `## Sources` table no longer hashes to the
blob id recorded beside it.

The script is what stops the profile stating facts derived from a file that has
since moved on -- it had already drifted twice through a full green CI matrix
before it existed. Nothing exercised the script itself, so a revert of any of
its three verdicts (current, stale, unusable table) left the suite green.

Every case runs the real script as a subprocess against a synthetic profile,
because the exit status is the whole contract: CI reads nothing else. Exit 0 is
"every pin current", 1 is "a pin is stale or its file is gone", and 2 is "the
table could not be read", which must never be reported as a pass -- a checker
that silently passes is worse than none.

The script always hashes relative to the repository root, whatever profile it
is handed, so these profiles pin real tracked files (`README.md`, `LICENSE`)
and vary only the blob ids.
"""

import pathlib
import subprocess

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci-profile-pins.sh"

# Long enough to look like a real short blob id, and hex, so a row carrying it
# reaches the comparison rather than being rejected as malformed.
WRONG_BLOB = "deadbeefdead"


def run(profile: pathlib.Path) -> subprocess.CompletedProcess:
    """Run the checker against `profile` and return the finished process."""
    return subprocess.run(
        ["bash", str(SCRIPT), str(profile)],
        capture_output=True,
        text=True,
        check=False,
        cwd=str(ROOT),
    )


def blob_of(path: str) -> str:
    """The short blob id the checker expects to find pinned for `path`."""
    out = subprocess.run(
        ["git", "-C", str(ROOT), "hash-object", "--no-filters", path],
        capture_output=True,
        text=True,
        check=True,
    )
    return out.stdout.strip()[:12]


def write_profile(tmp_path: pathlib.Path, rows: str) -> pathlib.Path:
    """A minimal profile whose `## Sources` table holds `rows`."""
    profile = tmp_path / "RELEASE-TRAIN.md"
    profile.write_text(
        "# Profile\n\n"
        "## Channels\n\n"
        "| path | blob | feeds |\n"
        "|---|---|---|\n"
        f"| `LICENSE` | {WRONG_BLOB} | not a pinned row: wrong section |\n\n"
        "## Sources\n\n"
        "| path | blob | feeds |\n"
        "|---|---|---|\n" + rows,
        encoding="utf-8",
    )
    return profile


def test_a_current_pin_passes(tmp_path):
    """A row whose file still hashes to its pin is the passing case."""
    profile = write_profile(
        tmp_path, f"| `README.md` | {blob_of('README.md')} | Channels: source |\n"
    )
    r = run(profile)
    assert r.returncode == 0, r.stdout + r.stderr
    assert "All 1 pinned sources match" in r.stdout


def test_a_stale_pin_fails_and_names_the_path(tmp_path):
    """The bug itself: a file that moved on since the profile was written.

    Exit 1, and the path is named, because the operator's next move is to
    re-read the sections that row feeds.
    """
    profile = write_profile(
        tmp_path, f"| `README.md` | {WRONG_BLOB} | Channels: source |\n"
    )
    r = run(profile)
    assert r.returncode == 1, r.stdout + r.stderr
    assert "FAIL  README.md" in r.stdout
    assert WRONG_BLOB in r.stdout


def test_one_stale_row_fails_a_table_of_otherwise_current_pins(tmp_path):
    """A single drifted row is enough; the current ones do not outvote it."""
    profile = write_profile(
        tmp_path,
        f"| `README.md` | {blob_of('README.md')} | Channels: source |\n"
        f"| `LICENSE` | {WRONG_BLOB} | Project |\n",
    )
    r = run(profile)
    assert r.returncode == 1, r.stdout + r.stderr
    assert "ok    README.md" in r.stdout
    assert "FAIL  LICENSE" in r.stdout
    assert "1 of 2 pinned sources have changed" in r.stderr


def test_a_pinned_path_that_no_longer_exists_fails(tmp_path):
    """A pinned file that is gone took the fact it fed with it."""
    profile = write_profile(
        tmp_path, f"| `no/such/file.md` | {WRONG_BLOB} | Channels: source |\n"
    )
    r = run(profile)
    assert r.returncode == 1, r.stdout + r.stderr
    assert "pinned but missing" in r.stdout


def test_a_non_hex_blob_id_fails(tmp_path):
    """A malformed id is a broken row, not a pass by prefix comparison."""
    profile = write_profile(
        tmp_path, "| `README.md` | not-a-blob | Channels: source |\n"
    )
    r = run(profile)
    assert r.returncode == 1, r.stdout + r.stderr
    assert "is not hex" in r.stdout


def test_an_empty_sources_table_is_a_failure_not_a_pass(tmp_path):
    """The guard that matters most: no rows must not read as "nothing wrong"."""
    profile = write_profile(tmp_path, "")
    r = run(profile)
    assert r.returncode == 2, r.stdout + r.stderr
    assert "no pinned rows found" in r.stderr


def test_a_missing_profile_is_a_failure_not_a_pass(tmp_path):
    """Same reasoning one step earlier: no profile at all is exit 2."""
    r = run(tmp_path / "does-not-exist.md")
    assert r.returncode == 2, r.stdout + r.stderr
    assert "no profile at" in r.stderr


def test_a_backticked_path_outside_the_sources_section_is_not_a_pinned_row(tmp_path):
    """Only the `## Sources` table is pinned.

    Every synthetic profile here carries a deliberately wrong pin for `LICENSE`
    in a `## Channels` table above it. If the checker matched backticked rows
    anywhere in the file, every one of these cases would fail for the wrong
    reason -- and the real profile, whose other tables quote paths freely,
    would be permanently red.
    """
    profile = write_profile(
        tmp_path, f"| `README.md` | {blob_of('README.md')} | Channels: source |\n"
    )
    r = run(profile)
    assert r.returncode == 0, r.stdout + r.stderr
    assert "All 1 pinned sources match" in r.stdout
    assert "LICENSE" not in r.stdout


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__]))
