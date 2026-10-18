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

"""Tests for the zigplug daemon's discovery, client, and request gate.

No coordinator is involved: discovery tests use a private runtime dir, the
client tests talk to a stub HTTP server that answers like the daemon, and the
auth-middleware tests run the real middleware in front of a stub handler on
an aiohttp test server.
"""

from __future__ import annotations

import asyncio
import dataclasses
import http.server
import json
import os
import secrets
import socket
import subprocess
import sys
import threading
import unittest
from pathlib import Path

import pytest
import typer
from aiohttp import test_utils, web

from zigplug import _app, _cli, _daemon

TOKEN = "test-token-not-a-secret"


@pytest.fixture
def runtime_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Point discovery at a private directory (the PANIO_RUNTIME_DIR hook)."""
    monkeypatch.setenv("PANIO_RUNTIME_DIR", str(tmp_path))
    return tmp_path


def _write_raw(runtime_dir: Path, text: str) -> None:
    (runtime_dir / "daemon.json").write_text(text)


def _record(**overrides) -> dict:
    info = {"pid": os.getpid(), "port": 4242, "device": "/dev/ttyZ", "token": TOKEN}
    info.update(overrides)
    return info


def _closed_port() -> int:
    """A loopback port nothing listens on (bound, then released)."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


# ── discovery ────────────────────────────────────────────────────────────────


def test_discovery_reads_a_live_record(runtime_dir):
    _write_raw(runtime_dir, json.dumps(_record()))
    daemon = _daemon.read_discovery()
    assert daemon == _daemon.Daemon(os.getpid(), 4242, "/dev/ttyZ", TOKEN)
    assert daemon.url == "http://127.0.0.1:4242"


@pytest.mark.parametrize("pid", [-1, 0])
def test_discovery_treats_non_positive_pid_as_not_running(runtime_dir, pid):
    # os.kill(-1, 0) and os.kill(0, 0) succeed (they address process groups),
    # which used to make a record with no real pid look alive forever.
    _write_raw(runtime_dir, json.dumps(_record(pid=pid)))
    assert _daemon.read_discovery() is None


def test_discovery_treats_dead_pid_as_not_running(runtime_dir):
    proc = subprocess.Popen([sys.executable, "-c", "pass"])
    proc.wait()  # reaped: the pid no longer names a process
    _write_raw(runtime_dir, json.dumps(_record(pid=proc.pid)))
    assert _daemon.read_discovery() is None


@pytest.mark.parametrize(
    "text",
    [
        "",
        '{"pid": 1234, "port": 42',  # truncated mid-write
        "[]",
        json.dumps({"pid": os.getpid(), "port": 4242, "device": "/dev/ttyZ"}),
        json.dumps(_record(pid="1234")),
        json.dumps(_record(port=0)),
        json.dumps(_record(token="")),
    ],
)
def test_discovery_rejects_partial_or_malformed_records(runtime_dir, text):
    _write_raw(runtime_dir, text)
    assert _daemon.read_discovery() is None


def test_discovery_without_a_file_is_not_running(runtime_dir):
    assert _daemon.read_discovery() is None


def test_write_discovery_is_private_and_leaves_no_temp_file(runtime_dir):
    daemon = _daemon.Daemon(os.getpid(), 4242, "/dev/ttyZ", TOKEN)
    path = _daemon.discovery_path()
    _daemon.write_discovery(path, daemon)
    assert oct(path.stat().st_mode & 0o777) == "0o600"
    assert [p.name for p in runtime_dir.iterdir()] == ["daemon.json"]
    assert _daemon.read_discovery() == daemon


def test_write_discovery_tightens_a_leftover_temp_files_mode(runtime_dir):
    path = _daemon.discovery_path()
    leftover = path.with_name("daemon.json.tmp")
    leftover.write_text("{}")
    leftover.chmod(0o644)  # O_CREAT's mode would not apply to an existing file
    _daemon.write_discovery(path, _daemon.Daemon(os.getpid(), 1, "/dev/ttyZ", TOKEN))
    assert oct(path.stat().st_mode & 0o777) == "0o600"


def test_forget_only_drops_the_matching_record(runtime_dir):
    stale = _daemon.Daemon(os.getpid(), 4242, "/dev/ttyZ", TOKEN)
    fresh = _daemon.Daemon(os.getpid(), 4343, "/dev/ttyZ", "another-token")
    _daemon.write_discovery(_daemon.discovery_path(), fresh)
    _daemon.forget(stale)  # a one-shot that lost the respawn race
    assert _daemon.read_discovery() == fresh
    _daemon.forget(fresh)
    assert _daemon.read_discovery() is None


# ── client: call / request against a stub daemon ─────────────────────────────


class _StubDaemon(http.server.BaseHTTPRequestHandler):
    """Answers like the daemon: JSON bodies, 401 without the bearer token."""

    seen: list[dict] = []

    def _reply(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _handle(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        self.seen.append(
            {
                "path": self.path,
                "authorization": self.headers.get("Authorization"),
                "content_type": self.headers.get("Content-Type"),
                "body": self.rfile.read(length) if length else b"",
            }
        )
        if self.headers.get("Authorization") != f"Bearer {TOKEN}":
            self._reply(401, {"error": "missing or invalid bearer token"})
            return
        self._reply(200, {"ok": True, "path": self.path})

    do_GET = _handle
    do_POST = _handle

    def log_message(self, *args) -> None:  # keep pytest output clean
        pass


@pytest.fixture
def stub_daemon():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _StubDaemon)
    _StubDaemon.seen = []
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield _daemon.Daemon(os.getpid(), server.server_address[1], "/dev/ttyZ", TOKEN)
    finally:
        server.shutdown()
        server.server_close()


def test_call_presents_the_bearer_token(stub_daemon):
    reply = _daemon.call(stub_daemon, "POST", "/on", {"ieee": "x"}, timeout=5.0)
    assert reply == {"ok": True, "path": "/on"}
    [seen] = _StubDaemon.seen
    assert seen["authorization"] == f"Bearer {TOKEN}"
    assert seen["content_type"] == "application/json"
    assert json.loads(seen["body"]) == {"ieee": "x"}


def test_call_surfaces_a_rejected_token_as_an_error(stub_daemon):
    daemon = dataclasses.replace(stub_daemon, token="wrong")
    with pytest.raises(_app.ZigplugError, match="bearer token"):
        _daemon.call(daemon, "GET", "/healthz", None, timeout=5.0)


def test_call_ignores_the_environments_http_proxy(stub_daemon, monkeypatch):
    # Routing through this (dead) proxy would refuse the connection; urllib's
    # default opener would do exactly that for 127.0.0.1 absent a no_proxy.
    proxy = f"http://127.0.0.1:{_closed_port()}"
    for name in ("http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"):
        monkeypatch.setenv(name, proxy)
    for name in ("no_proxy", "NO_PROXY"):
        monkeypatch.delenv(name, raising=False)
    reply = _daemon.call(stub_daemon, "GET", "/healthz", None, timeout=5.0)
    assert reply["ok"] is True


def test_call_drops_a_stale_record_when_nothing_listens(runtime_dir):
    stale = _daemon.Daemon(os.getpid(), _closed_port(), "/dev/ttyZ", TOKEN)
    _daemon.write_discovery(_daemon.discovery_path(), stale)
    with pytest.raises(_daemon.DaemonGone):
        _daemon.call(stale, "GET", "/healthz", None, timeout=5.0)
    assert not _daemon.discovery_path().exists()


def test_request_respawns_once_after_a_stale_record(
    runtime_dir, stub_daemon, monkeypatch
):
    # A crashed daemon whose pid a live process (this one) has since reused:
    # read_discovery cannot tell, and used to report "unreachable" forever.
    stale = _daemon.Daemon(os.getpid(), _closed_port(), "/dev/ttyZ", TOKEN)
    _daemon.write_discovery(_daemon.discovery_path(), stale)
    spawns: list[str] = []

    def fake_spawn(device: str, db_path: Path) -> _daemon.Daemon:
        spawns.append(device)
        _daemon.write_discovery(_daemon.discovery_path(), stub_daemon)
        return stub_daemon

    monkeypatch.setattr(_daemon, "spawn", fake_spawn)
    reply = _daemon.request(
        "/dev/ttyZ", Path("unused.db"), "GET", "/healthz", None, timeout=5.0
    )
    assert reply["ok"] is True
    assert spawns == ["/dev/ttyZ"]
    assert _daemon.read_discovery() == stub_daemon


def test_request_does_not_loop_when_the_respawn_is_dead_too(runtime_dir, monkeypatch):
    stale = _daemon.Daemon(os.getpid(), _closed_port(), "/dev/ttyZ", TOKEN)
    _daemon.write_discovery(_daemon.discovery_path(), stale)
    spawns: list[str] = []

    def fake_spawn(device: str, db_path: Path) -> _daemon.Daemon:
        spawns.append(device)
        return dataclasses.replace(stale, port=_closed_port())

    monkeypatch.setattr(_daemon, "spawn", fake_spawn)
    with pytest.raises(_daemon.DaemonGone):
        _daemon.request(
            "/dev/ttyZ", Path("unused.db"), "GET", "/healthz", None, timeout=5.0
        )
    assert spawns == ["/dev/ttyZ"]


# ── CLI: `zigplug stop` never falls back to signaling the recorded pid ─────


def test_stop_never_signals_a_stale_pid_when_the_daemon_wont_die(
    runtime_dir, stub_daemon, monkeypatch
):
    """Something is listening on the recorded port and answers `/stop`, but
    it rejects the token (a stand-in for the real bug: the kernel reused the
    pid/port and whatever is there now is not our daemon) and never removes
    the discovery file. Before the fix, `stop` waited 5s and then fell back
    to `os.kill(daemon.pid, SIGTERM)`. The recorded pid here is this test
    process's own pid, so the old behavior would kill the test runner. The
    fix must report failure and exit non-zero, and must never call
    `os.kill` at all.
    """
    mismatched = dataclasses.replace(stub_daemon, token="not-the-real-token")
    _daemon.write_discovery(_daemon.discovery_path(), mismatched)

    # Speed the 5s poll loop up to nothing: fake time.monotonic()/time.sleep()
    # under the same names `_cli.py` calls (`import time`), so the test does
    # not need to wait out the real deadline.
    clock = {"t": 0.0}
    monkeypatch.setattr(_cli.time, "monotonic", lambda: clock["t"])
    monkeypatch.setattr(
        _cli.time, "sleep", lambda s: clock.__setitem__("t", clock["t"] + s)
    )

    # `_daemon.read_discovery()` legitimately probes liveness with
    # `os.kill(pid, 0)` (signal 0 raises nothing on a live pid and is not a
    # termination); only a real signal (SIGTERM below) is what must never
    # happen.
    killed: list[tuple] = []

    def fake_kill(pid, sig=0):
        if sig != 0:
            killed.append((pid, sig))

    monkeypatch.setattr(os, "kill", fake_kill)

    with pytest.raises(typer.Exit) as excinfo:
        _cli.stop()
    assert excinfo.value.exit_code != 0
    assert not killed, f"stop() must never signal the recorded pid, got {killed}"
    # The daemon never actually exited (its discovery file never changed),
    # and `stop` must not have deleted it out from under a real daemon.
    assert _daemon.read_discovery() == mismatched


# ── server: the auth middleware on a real aiohttp test server ────────────────


class AuthMiddlewareTest(test_utils.AioHTTPTestCase):
    """The request gate in front of a stub handler that echoes what it got."""

    async def get_application(self) -> web.Application:
        # The daemon binds before building its app, so the middleware knows
        # the port up front; the test server is pinned to the same one.
        self.port = test_utils.unused_port()
        app = web.Application(middlewares=[_daemon.auth_middleware(TOKEN, self.port)])

        async def stub(request: web.Request) -> web.Response:
            body = await request.json() if request.can_read_body else None
            return web.json_response({"handled": request.path, "body": body})

        app.router.add_get("/healthz", stub)
        app.router.add_post("/on", stub)
        return app

    async def get_server(self, app: web.Application) -> test_utils.TestServer:
        return test_utils.TestServer(app, port=self.port, loop=self.loop)

    @staticmethod
    def _auth(**extra: str) -> dict[str, str]:
        return {"Authorization": f"Bearer {TOKEN}", **extra}

    async def test_missing_token_is_401(self):
        resp = await self.client.get("/healthz")
        self.assertEqual(resp.status, 401)
        self.assertEqual(
            await resp.json(), {"error": "missing or invalid bearer token"}
        )

    async def test_wrong_token_is_401(self):
        resp = await self.client.get(
            "/healthz", headers={"Authorization": "Bearer not-the-token"}
        )
        self.assertEqual(resp.status, 401)

    async def test_wrong_scheme_is_401(self):
        resp = await self.client.get(
            "/healthz", headers={"Authorization": f"Basic {TOKEN}"}
        )
        self.assertEqual(resp.status, 401)

    async def test_foreign_host_is_403_even_with_the_token(self):
        resp = await self.client.get(
            "/healthz", headers=self._auth(Host=f"attacker.example:{self.port}")
        )
        self.assertEqual(resp.status, 403)

    async def test_localhost_host_is_accepted(self):
        resp = await self.client.get(
            "/healthz", headers=self._auth(Host=f"localhost:{self.port}")
        )
        self.assertEqual(resp.status, 200)

    async def test_origin_header_is_403_even_with_the_token(self):
        resp = await self.client.get(
            "/healthz", headers=self._auth(Origin=f"http://127.0.0.1:{self.port}")
        )
        self.assertEqual(resp.status, 403)

    async def test_post_without_json_content_type_is_415(self):
        # bytes payloads go out as application/octet-stream
        resp = await self.client.post(
            "/on", headers=self._auth(), data=b'{"ieee": "x"}'
        )
        self.assertEqual(resp.status, 415)

    async def test_valid_get_reaches_the_handler(self):
        resp = await self.client.get("/healthz", headers=self._auth())
        self.assertEqual(resp.status, 200)
        self.assertEqual(await resp.json(), {"handled": "/healthz", "body": None})

    async def test_valid_post_reaches_the_handler(self):
        resp = await self.client.post("/on", headers=self._auth(), json={"ieee": "x"})
        self.assertEqual(resp.status, 200)
        self.assertEqual(await resp.json(), {"handled": "/on", "body": {"ieee": "x"}})


class ClientThroughGateTest(unittest.IsolatedAsyncioTestCase):
    """The one-shot client gets through the gate, wired the way serve() wires it."""

    async def test_call_passes_the_gate_on_a_prebound_socket(self):
        token = secrets.token_urlsafe(32)
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
        app = web.Application(middlewares=[_daemon.auth_middleware(token, port)])

        async def stub(request: web.Request) -> web.Response:
            return web.json_response({"ok": True, "body": await request.json()})

        app.router.add_post("/on", stub)
        runner = web.AppRunner(app)
        await runner.setup()
        await web.SockSite(runner, sock).start()
        try:
            daemon = _daemon.Daemon(os.getpid(), port, "/dev/ttyZ", token)
            reply = await asyncio.to_thread(
                _daemon.call, daemon, "POST", "/on", {"ieee": "x"}, 5.0
            )
            self.assertEqual(reply, {"ok": True, "body": {"ieee": "x"}})
            wrong = dataclasses.replace(daemon, token="not-the-token")
            with self.assertRaisesRegex(_app.ZigplugError, "bearer token"):
                await asyncio.to_thread(_daemon.call, wrong, "POST", "/on", {}, 5.0)
        finally:
            await runner.cleanup()
