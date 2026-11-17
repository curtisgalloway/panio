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

"""Tests for issue #407: the apt pool could be published without a release.

docs.yml rebuilds the apt pool from the newest Releases on every deploy, and a
Pages deploy replaces the whole site. The old download loop turned any
`gh release download` failure into "no .deb assets on <tag> -- skipped", so an
HTTP 503 on the newest release deployed a pool of only the older ones.

packaging/scripts/fetch-apt-debs.sh replaces that loop. Each case runs the real
script against a mock `gh` on PATH, driven by a JSON spec, with no network.
"""

import json
import os
import pathlib
import stat
import subprocess
import textwrap

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "packaging" / "scripts" / "fetch-apt-debs.sh"

# A stand-in for the three gh calls the script makes. The spec maps each tag to
# its asset names and to how many times each call fails before it succeeds
# (-1 = always fails). `download_only` limits which .debs a "successful"
# download writes, to model a partial download.
MOCK_GH = textwrap.dedent(
    r'''
    #!/usr/bin/env python3
    import json, os, pathlib, sys

    spec = json.loads(pathlib.Path(os.environ["MOCK_GH_SPEC"]).read_text())
    state_path = pathlib.Path(os.environ["MOCK_GH_STATE"])
    state = json.loads(state_path.read_text()) if state_path.exists() else {}
    args = sys.argv[1:]

    def flag(name):
        return args[args.index(name) + 1]

    def failing(key, budget):
        """True while `key` still has failures left in its budget."""
        n = state.get(key, 0)
        state[key] = n + 1
        state_path.write_text(json.dumps(state))
        return budget == -1 or n < budget

    if args[:2] == ["release", "list"]:
        if failing("list", spec.get("fail_list", 0)):
            sys.exit("HTTP 503: Service Unavailable")
        tags = [r["tag"] for r in spec["releases"]][: int(flag("--limit"))]
        print("\n".join(tags))
    elif args[:2] == ["release", "view"]:
        rel = next(r for r in spec["releases"] if r["tag"] == args[2])
        if failing("view " + args[2], rel.get("fail_view", 0)):
            sys.exit("HTTP 502: Bad Gateway")
        for name in rel["assets"]:
            if name.endswith(".deb"):
                print(name)
    elif args[:2] == ["release", "download"]:
        rel = next(r for r in spec["releases"] if r["tag"] == args[2])
        if failing("download " + args[2], rel.get("fail_download", 0)):
            sys.exit("HTTP 503: Service Unavailable")
        debs = [a for a in rel["assets"] if a.endswith(".deb")]
        if not debs:
            sys.exit("no assets match the file pattern")
        out = pathlib.Path(flag("--dir"))
        for name in rel.get("download_only", debs):
            (out / name).write_bytes(b"deb")
    else:
        sys.exit("mock gh: unexpected call " + " ".join(args))
    '''
).lstrip()


def debs(version):
    """The asset list release.yml attaches: both arches plus sha256 sidecars."""
    names = [f"panio_{version}_amd64.deb", f"panio_{version}_arm64.deb"]
    return names + [n + ".sha256" for n in names] + [f"panio-{version}-linux-amd64.tar.gz"]


@pytest.fixture
def run(tmp_path):
    bindir = tmp_path / "bin"
    bindir.mkdir()
    gh = bindir / "gh"
    gh.write_text(MOCK_GH)
    gh.chmod(gh.stat().st_mode | stat.S_IEXEC)
    out = tmp_path / "debs"

    def _run(releases, count=5, **top):
        spec = tmp_path / "spec.json"
        spec.write_text(json.dumps({"releases": releases, **top}))
        env = dict(
            os.environ,
            PATH=f"{bindir}{os.pathsep}{os.environ['PATH']}",
            MOCK_GH_SPEC=str(spec),
            MOCK_GH_STATE=str(tmp_path / "state.json"),
            APT_FETCH_ATTEMPTS="3",
            APT_FETCH_BACKOFF="0",
        )
        r = subprocess.run(
            ["bash", str(SCRIPT), str(out), "owner/repo", str(count)],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )
        got = sorted(p.name for p in out.glob("*")) if out.exists() else []
        return r, got

    return _run


def test_complete_download_succeeds(run):
    r, got = run([{"tag": "v2", "assets": debs("2")}, {"tag": "v1", "assets": debs("1")}])
    assert r.returncode == 0, r.stdout + r.stderr
    assert got == [
        "panio_1_amd64.deb",
        "panio_1_arm64.deb",
        "panio_2_amd64.deb",
        "panio_2_arm64.deb",
    ]
    assert "fetched 4 .deb files" in r.stdout


def test_release_without_debs_is_skipped(run):
    """The one legitimate skip: the release's metadata lists no .deb."""
    r, got = run(
        [
            {"tag": "v3", "assets": ["panio-3-linux-amd64.tar.gz"]},
            {"tag": "v2", "assets": debs("2")},
        ]
    )
    assert r.returncode == 0, r.stdout + r.stderr
    assert "v3 has no .deb assets; skipped" in r.stdout
    assert got == ["panio_2_amd64.deb", "panio_2_arm64.deb"]


def test_persistent_download_failure_fails_even_with_older_debs(run):
    """The #407 reproduction: newest release 503s, an older one downloads."""
    r, got = run(
        [
            {"tag": "v2", "assets": debs("2"), "fail_download": -1},
            {"tag": "v1", "assets": debs("1")},
        ]
    )
    assert r.returncode != 0
    assert "could not download the .deb assets of v2" in r.stderr
    assert "skipped" not in r.stdout + r.stderr


def test_transient_download_failure_is_retried(run):
    r, got = run([{"tag": "v2", "assets": debs("2"), "fail_download": 2}])
    assert r.returncode == 0, r.stdout + r.stderr
    assert r.stderr.count("retrying") == 2
    assert got == ["panio_2_amd64.deb", "panio_2_arm64.deb"]


def test_partial_architecture_download_fails(run):
    r, got = run(
        [
            {
                "tag": "v2",
                "assets": debs("2"),
                "download_only": ["panio_2_amd64.deb"],
            }
        ]
    )
    assert r.returncode != 0
    assert "panio_2_arm64.deb is listed on its release but did not download" in r.stderr


def test_metadata_failure_fails_instead_of_skipping(run):
    """Unreadable assets must not be mistaken for a release with none."""
    r, got = run(
        [
            {"tag": "v2", "assets": debs("2"), "fail_view": -1},
            {"tag": "v1", "assets": debs("1")},
        ]
    )
    assert r.returncode != 0
    assert "could not read the assets of v2" in r.stderr
    assert got == []


def test_release_list_failure_fails(run):
    r, got = run([{"tag": "v1", "assets": debs("1")}], fail_list=-1)
    assert r.returncode != 0
    assert "could not list releases" in r.stderr


def test_no_debs_in_any_release_fails(run):
    r, got = run([{"tag": "v1", "assets": ["panio-1-linux-amd64.tar.gz"]}])
    assert r.returncode != 0
    assert "has a .deb asset" in r.stderr


def test_count_limits_the_releases_fetched(run):
    r, got = run(
        [{"tag": "v3", "assets": debs("3")}, {"tag": "v2", "assets": debs("2")}],
        count=1,
    )
    assert r.returncode == 0, r.stdout + r.stderr
    assert got == ["panio_3_amd64.deb", "panio_3_arm64.deb"]
