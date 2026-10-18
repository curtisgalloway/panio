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
"""Tests for the eval runner's sandboxing (stdlib + pytest only).

Run from the repo root: `python3.12 -m pytest evals/tests -q`.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

EVALS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(EVALS))
import run as run_mod  # noqa: E402


def _sandbox(sb_dir: Path) -> dict:
    return {"dir": sb_dir, "lab": sb_dir / "lab.toml",
            "argv_log": sb_dir / "argv.log"}


@pytest.mark.parametrize("isolation", ["none", "light", "home"])
def test_base_env_scopes_the_runtime_base_to_the_sandbox(
        tmp_path, monkeypatch, isolation):
    # Neither the default /tmp nor an operator override may leak through: the
    # post-run `daemons stop --all` sweep runs with this env.
    monkeypatch.setenv("PANIO_RUNTIME_BASE", "/tmp")
    monkeypatch.setenv("HOME", str(tmp_path / "real-home"))
    sb_dir = tmp_path / "sandbox"
    sb_dir.mkdir()

    env = run_mod.base_env(_sandbox(sb_dir), isolation)

    runtime = Path(env["PANIO_RUNTIME_BASE"])
    assert sb_dir in runtime.parents
    assert runtime.is_dir()


def test_home_isolation_links_credentials_and_cleanup_removes_the_link(
        tmp_path, monkeypatch):
    real_home = tmp_path / "real-home"
    cred = real_home / run_mod.CREDENTIALS_REL
    cred.parent.mkdir(parents=True)
    cred.write_text('{"token": "not-a-real-token"}')
    monkeypatch.setenv("HOME", str(real_home))
    sb_dir = tmp_path / "sandbox"
    sb_dir.mkdir()
    sb = _sandbox(sb_dir)

    env = run_mod.base_env(sb, "home")

    assert env["HOME"] == str(sb_dir / "home")
    link = Path(env["HOME"]) / run_mod.CREDENTIALS_REL
    assert link.is_symlink()
    assert link.resolve() == cred.resolve()
    # A token refresh written through the sandbox path lands in the real file.
    link.write_text('{"token": "refreshed"}')
    assert cred.read_text() == '{"token": "refreshed"}'

    run_mod.remove_credential_link(sb)

    assert not link.is_symlink() and not link.exists()
    assert cred.read_text() == '{"token": "refreshed"}'
    # A kept sandbox holds no credential file of any kind.
    assert not [p for p in sb_dir.rglob("*") if p.name == ".credentials.json"]


def test_cleanup_removes_a_credential_copy_the_agent_left_behind(
        tmp_path, capsys):
    sb_dir = tmp_path / "sandbox"
    copy = sb_dir / "home" / run_mod.CREDENTIALS_REL
    copy.parent.mkdir(parents=True)
    copy.write_text("{}")

    run_mod.remove_credential_link(_sandbox(sb_dir))

    assert not copy.exists()
    assert "credential" in capsys.readouterr().err


def test_cleanup_is_a_no_op_for_a_removed_sandbox(tmp_path):
    run_mod.remove_credential_link(_sandbox(tmp_path / "gone"))


def test_unknown_scenario_ids_are_an_error(monkeypatch, capsys):
    # A judge-graded scenario, so that a runner which silently dropped the
    # unknown ids (the old behaviour) would still not start an agent here:
    # `--reference` filters it out and returns 0 instead of raising.
    scenarios = run_mod.load_scenarios()
    known = next(sid for sid, sc in scenarios.items()
                 if sc.get("grader", {}).get("type") != "t1_config")
    monkeypatch.setattr(sys, "argv", [
        "run.py", "--reference", "--scenario", known,
        "--scenario", "zz9", "--scenario", "zz8",
    ])

    with pytest.raises(SystemExit) as exc:
        run_mod.main()

    assert exc.value.code == 2
    assert "unknown scenario id(s): zz9, zz8" in capsys.readouterr().err


def test_surface_cache_is_rebuilt_when_the_binary_is_newer(tmp_path, monkeypatch):
    fake = tmp_path / "panio"
    fake.write_text("#!/bin/sh\necho 'Usage: panio [OPTIONS]'\n")
    fake.chmod(0o755)
    monkeypatch.setattr(run_mod, "HERE", tmp_path)  # the cache lives under HERE
    cache = tmp_path / ".panio_surface.txt"
    cache.write_text("STALE")
    old = 1_000_000_000
    os.utime(cache, (old, old))
    os.utime(fake, (old + 60, old + 60))  # rebuilt after the last walk

    monkeypatch.setattr(run_mod, "_SURFACE_CACHE", None)
    surface = run_mod.command_surface(str(fake))

    assert "Usage: panio [OPTIONS]" in surface
    assert cache.read_text() == surface

    # A cache newer than the binary is served as-is.
    cache.write_text("CACHED")
    os.utime(cache, (old + 120, old + 120))
    monkeypatch.setattr(run_mod, "_SURFACE_CACHE", None)
    assert run_mod.command_surface(str(fake)) == "CACHED"
