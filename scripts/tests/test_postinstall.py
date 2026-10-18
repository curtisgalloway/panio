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

"""Tests for issue #228: packaging/scripts/postinstall.sh told a fresh
install it was an upgrade.

dpkg runs this script as `postinst configure <previously-configured-version>`,
and that second argument is empty only on a first-time install (Debian Policy
6.5). Before the fix the script printed the "upgrade complete" wording
unconditionally, so a brand-new user was told to restart daemons that never
existed and was never told to run `panio setup`.

Each case runs the real script as a subprocess with the arguments dpkg would
pass, because the printed wording is the whole user-visible contract.
"""

import pathlib
import subprocess

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "packaging" / "scripts" / "postinstall.sh"


def run(*args: str) -> subprocess.CompletedProcess:
    """Run postinstall.sh with `args` the way dpkg invokes a maintainer script."""
    return subprocess.run(
        ["sh", str(SCRIPT), *args],
        capture_output=True,
        text=True,
        check=False,
        cwd=str(ROOT),
    )


def test_fresh_install_is_told_to_run_setup():
    """`configure` with no previous version: a first-time installer."""
    r = run("configure")
    assert r.returncode == 0, r.stdout + r.stderr
    assert "installed. Run 'panio setup'" in r.stdout
    assert "upgrade" not in r.stdout


def test_upgrade_names_the_previous_version_and_daemon_restart():
    """`configure <version>`: the bug itself. Must not claim a fresh install."""
    r = run("configure", "0.3.1")
    assert r.returncode == 0, r.stdout + r.stderr
    assert "upgrade from 0.3.1 complete" in r.stdout
    assert "restart them with: panio daemons restart --stale" in r.stdout
    assert "Run 'panio setup'" not in r.stdout


def test_other_dpkg_actions_print_nothing():
    """abort-upgrade, abort-remove, etc. are not configure and get no advice."""
    r = run("abort-upgrade", "0.3.1")
    assert r.returncode == 0, r.stdout + r.stderr
    assert r.stdout == ""


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__]))
