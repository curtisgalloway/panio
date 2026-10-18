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

"""zigplug daemon: a persistent owner for the ZNP coordinator session.

One-shot invocations are unreliable by construction on CC2652 sticks: every
serial-port open toggles DTR/RTS through the auto-bootloader circuit and
resets the chip (sometimes *into* the bootloader, which hangs the client),
and two concurrent invocations collide on the stateful ZNP session.

The daemon opens the coordinator once and serves operations over localhost
HTTP, serialized on a single lock with hard per-operation timeouts. It
follows panio's daemon contract (see cli/src/daemons.rs): it binds an
OS-assigned port on 127.0.0.1 and publishes
`/tmp/panio-<uid>/zigplug/daemon.json` containing `{pid, port, device,
token}`. The token is a per-run secret that every request must present as a
bearer token; the file is written mode 0600 inside the 0700 runtime dir, so
only the daemon's owner can read it. The one-shot CLI auto-spawns the daemon
and proxies through it transparently, so panio power hooks
(`zigplug -d <dev> on <ieee>`) don't change.
"""

# Single quotes nested in double-quoted f-strings are required on Python 3.11;
# the aiohttp handlers share a (request, body) signature whether they use both
# or not.
# pylint: disable=inconsistent-quotes,unused-argument

from __future__ import annotations

import asyncio
import contextlib
import dataclasses
import fcntl
import hmac
import json
import logging
import os
import secrets
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from . import _app

LOGGER = logging.getLogger(__name__)

# Hard ceiling for one radio operation (a switch + read-back takes ~1 s on a
# healthy network); cycle/permit add their own wait on top. A wedged session
# returns an error instead of hanging the power hook forever.
OP_TIMEOUT_S = 30.0

# How long a client waits for a freshly spawned daemon to come up (ZNP
# connect + network start is a few seconds; bootloader-entry wedges never
# come up — better to fail and let the next invocation respawn).
SPAWN_TIMEOUT_S = 20.0

DISCOVERY_NAME = "zigplug"


# ── runtime dir + discovery (must mirror cli/src/daemons.rs) ────────────────


def runtime_dir() -> Path:
    """The daemon's runtime dir (discovery, locks, log).

    Panio passes the canonical location as `PANIO_RUNTIME_DIR` (the
    helper state/runtime-dir API in the CLI's daemons.rs); the literal
    fallback below matches it byte-for-byte for standalone invocations:
    `/tmp/panio-<uid>/zigplug`, created 0700 with an ownership check.
    """
    env = os.environ.get("PANIO_RUNTIME_DIR")
    if env:
        d = Path(env)
        d.mkdir(parents=True, exist_ok=True)
        return d
    base = Path(f"/tmp/panio-{os.getuid()}")
    try:
        base.mkdir(mode=0o700)
    except FileExistsError:
        st = base.lstat()
        if not base.is_dir() or st.st_uid != os.getuid():
            raise _app.ZigplugError(
                f"{base} exists but is not a directory owned by uid {os.getuid()}"
            ) from None
    d = base / DISCOVERY_NAME
    d.mkdir(exist_ok=True)
    return d


def discovery_path() -> Path:
    return runtime_dir() / "daemon.json"


def log_path() -> Path:
    return runtime_dir() / "daemon.log"


def _pid_alive(pid: int) -> bool:
    # `os.kill(0, 0)` addresses the caller's own process group and a negative
    # pid a group (or, for -1, every process the caller may signal); both
    # succeed without any daemon existing, so only a real pid counts.
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError):
        return False
    return True


@dataclasses.dataclass(frozen=True)
class Daemon:
    """A daemon as published in its discovery file."""

    pid: int
    port: int
    device: str
    token: str

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"


def _load_discovery() -> Daemon | None:
    """Parse the discovery file without checking liveness; None if unusable.

    A missing, truncated, or field-incomplete file reads as "no daemon", so
    the next invocation simply overwrites it.
    """
    try:
        info = json.loads(discovery_path().read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(info, dict):
        return None
    pid, port, device, token = (
        info.get(key) for key in ("pid", "port", "device", "token")
    )
    if not (
        isinstance(pid, int)
        and isinstance(port, int)
        and 0 < port < 65536
        and isinstance(device, str)
        and isinstance(token, str)
        and token
    ):
        return None
    return Daemon(pid=pid, port=port, device=device, token=token)


def read_discovery() -> Daemon | None:
    """The running daemon's discovery record, or None."""
    daemon = _load_discovery()
    if daemon is None or not _pid_alive(daemon.pid):
        return None
    return daemon


def write_discovery(path: Path, daemon: Daemon) -> None:
    """Publish `daemon` at `path`: mode 0600, and atomically.

    The record carries the bearer token, so it must never be readable by
    another user (the runtime dir is already 0700; the file mode is belt and
    braces), and a one-shot must never see a half-written file — write a
    sibling and rename it over the destination.
    """
    tmp = path.with_name(path.name + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        with os.fdopen(fd, "w") as fh:
            os.fchmod(fh.fileno(), 0o600)  # O_CREAT's mode only applies when new
            json.dump(dataclasses.asdict(daemon), fh)
            fh.flush()
            os.fsync(fh.fileno())
    except BaseException:
        with contextlib.suppress(OSError):
            tmp.unlink()
        raise
    os.replace(tmp, path)


def forget(daemon: Daemon) -> None:
    """Drop the discovery file if it still describes `daemon`.

    Guarded so a one-shot that lost a race — another one already respawned
    and republished — does not delete the fresh daemon's record.
    """
    if _load_discovery() == daemon:
        with contextlib.suppress(OSError):
            discovery_path().unlink()


def find_daemon(device: str) -> Daemon | None:
    """The running daemon serving `device`, or None.

    A daemon serving a *different* device is an error, not a miss — one
    coordinator per daemon, and silently bypassing it would reintroduce
    the port collision this daemon exists to prevent.
    """
    daemon = read_discovery()
    if daemon is None:
        return None
    if daemon.device != device:
        raise _app.ZigplugError(
            f"zigplug daemon (pid {daemon.pid}) is serving {daemon.device!r}, "
            f"not {device!r} — stop it first (`zigplug stop`)"
        )
    return daemon


# ── client side: proxy calls + auto-spawn ────────────────────────────────────


class DaemonGone(_app.ZigplugError):
    """The discovery file named a daemon that is no longer listening.

    Raised by `call` after it has dropped the stale file; `request` recovers
    by spawning afresh, once.
    """


# urllib honours $http_proxy (and, on macOS, the system proxy settings) even
# for 127.0.0.1 unless $no_proxy says otherwise, which would route every power
# hook through the proxy — or fail it while the proxy is down. An empty
# ProxyHandler turns the lookup off.
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _connection_refused(exc: BaseException) -> bool:
    reason = getattr(exc, "reason", exc)  # URLError wraps the socket error
    return isinstance(reason, ConnectionRefusedError)


def call(daemon: Daemon, method: str, path: str, body: dict | None, timeout: float):
    """One JSON request to `daemon`; raises ZigplugError on error replies.

    Nothing listening on the daemon's port means it crashed without cleaning
    up (and a reused pid keeps `read_discovery` fooled indefinitely): the
    stale discovery file is dropped and DaemonGone raised so the caller can
    respawn.
    """
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        f"{daemon.url}{path}",
        data=data,
        method=method,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {daemon.token}",
        },
    )
    try:
        with _OPENER.open(req, timeout=timeout) as resp:
            return json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as exc:
        try:
            message = json.loads(exc.read()).get("error", str(exc))
        except ValueError:
            message = str(exc)
        raise _app.ZigplugError(message) from exc
    except (urllib.error.URLError, TimeoutError, ConnectionError) as exc:
        if _connection_refused(exc):
            forget(daemon)
            raise DaemonGone(
                f"zigplug daemon (pid {daemon.pid}) is not listening on port "
                f"{daemon.port} — it died without cleaning up; dropped its stale "
                "discovery file"
            ) from exc
        raise _app.ZigplugError(
            f"zigplug daemon unreachable ({exc}) — it may have died; "
            "the next invocation will restart it"
        ) from exc


def spawn(device: str, db_path: Path) -> Daemon:
    """Start a detached daemon for `device` and wait for it; returns its record.

    Serialized on a lock file so two concurrent one-shots can't both spawn
    (the loser of the race finds the winner's daemon via discovery).
    """
    lock_file = (runtime_dir() / "spawn.lock").open("w")
    try:
        fcntl.flock(lock_file, fcntl.LOCK_EX)
        daemon = find_daemon(device)  # someone else may have won the race
        if daemon is not None:
            return daemon
        log = log_path().open("w")
        with contextlib.redirect_stdout(sys.stderr):
            print(f"starting zigplug daemon for {device}…")
        subprocess.Popen(  # noqa: consider-using-with — outlives us by design
            [
                sys.executable,
                "-m",
                "zigplug",
                "--device",
                device,
                "--db",
                str(db_path),
                "serve",
                "--foreground",
            ],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        deadline = time.monotonic() + SPAWN_TIMEOUT_S
        while time.monotonic() < deadline:
            daemon = find_daemon(device)
            if daemon is not None:
                try:
                    call(daemon, "GET", "/healthz", None, timeout=2.0)
                    return daemon
                except _app.ZigplugError:
                    pass
            time.sleep(0.25)
        tail = ""
        with contextlib.suppress(OSError):
            tail = "\n  ".join(log_path().read_text().splitlines()[-5:])
        raise _app.ZigplugError(
            f"zigplug daemon did not start within {SPAWN_TIMEOUT_S:.0f}s"
            + (f"; last log lines:\n  {tail}" if tail else "")
        )
    finally:
        fcntl.flock(lock_file, fcntl.LOCK_UN)
        lock_file.close()


def ensure(device: str, db_path: Path) -> Daemon:
    """The daemon serving `device`, spawning one if needed."""
    return find_daemon(device) or spawn(device, db_path)


def request(
    device: str,
    db_path: Path,
    method: str,
    path: str,
    body: dict | None,
    timeout: float,
) -> dict:
    """Route one operation through the daemon serving `device`.

    Spawns the daemon if needed, and recovers once from a stale discovery
    file: `call` drops the file when nothing answers on the recorded port,
    so the second `ensure` spawns afresh. Once only — a daemon that dies on
    every start must surface as an error, not loop.
    """
    try:
        return call(ensure(device, db_path), method, path, body, timeout)
    except DaemonGone:
        return call(ensure(device, db_path), method, path, body, timeout)


# ── server side ──────────────────────────────────────────────────────────────


def auth_middleware(token: str, port: int):
    """aiohttp middleware admitting only a local client that holds `token`.

    Listening on loopback keeps other hosts out, but not other users of this
    host or a web page its owner happens to have open. So every request must
    present `Authorization: Bearer <token>` (401 otherwise), name this
    daemon's own loopback socket in `Host` (403 — a DNS-rebound page does
    not), carry no `Origin` at all (403 — no browser ever talks to this
    daemon, so a cross-origin request is an attack), and a POST must declare
    `Content-Type: application/json` (415) before any handler parses it.
    """
    # Local import: the server is the only consumer (see serve()).
    from aiohttp import web  # pylint: disable=import-outside-toplevel

    allowed_hosts = {f"127.0.0.1:{port}", f"localhost:{port}"}
    expected = token.encode()

    def reject(status: int, message: str) -> web.Response:
        return web.json_response({"error": message}, status=status)

    @web.middleware
    async def middleware(request: web.Request, handler):
        if "Origin" in request.headers:
            return reject(403, "cross-origin requests are refused")
        if request.host not in allowed_hosts:
            return reject(403, f"unexpected Host {request.host!r}")
        scheme, _, presented = request.headers.get("Authorization", "").partition(" ")
        if scheme != "Bearer" or not hmac.compare_digest(
            presented.strip().encode(), expected
        ):
            return reject(401, "missing or invalid bearer token")
        if request.method == "POST" and request.content_type != "application/json":
            return reject(415, "POST bodies must be Content-Type: application/json")
        return await handler(request)

    return middleware


async def serve(device: str, db_path: Path) -> int:
    """Run the daemon in the foreground until stopped; returns an exit code.

    Exit code 1 (e.g. on radio connection loss) tells the wrapper layers the
    session died abnormally; the discovery file is removed either way so the
    next one-shot respawns cleanly.
    """
    # Local import: the server is the only consumer, and one-shot fallback
    # paths must not require it at import time.
    from aiohttp import web  # pylint: disable=import-outside-toplevel

    config = _app.build_config(device, db_path)
    try:
        app = await _app.ControllerApplication.new(config)
    except Exception as exc:  # surface a one-line reason in the daemon log
        LOGGER.error("coordinator startup failed: %s", exc)
        raise
    started = time.monotonic()
    lock = asyncio.Lock()
    stop_event = asyncio.Event()
    exit_code = 0

    class RadioListener:
        """Exit (code 1) when the ZNP session dies so a fresh daemon respawns."""

        def connection_lost(self, exc: Exception | None = None) -> None:
            nonlocal exit_code
            LOGGER.error("radio connection lost: %s", exc)
            exit_code = 1
            stop_event.set()

    app.add_listener(RadioListener())

    def fail(status: int, message: str) -> web.Response:
        return web.json_response({"error": message}, status=status)

    def handler(fn, *, extra_timeout: float = 0.0):
        async def wrapped(request: web.Request) -> web.Response:
            body = {}
            if request.can_read_body:
                try:
                    body = await request.json()
                except ValueError:
                    return fail(400, "invalid JSON body")
            timeout = OP_TIMEOUT_S + extra_timeout_from(body) + extra_timeout
            try:
                async with lock:
                    result = await asyncio.wait_for(fn(request, body), timeout)
            except _app.ZigplugError as exc:
                return fail(400, str(exc))
            except asyncio.TimeoutError:
                return fail(504, f"operation timed out after {timeout:.0f}s")
            except Exception as exc:  # pylint: disable=broad-exception-caught
                LOGGER.exception("operation failed")
                return fail(500, f"{type(exc).__name__}: {exc}")
            return web.json_response(result)

        return wrapped

    def extra_timeout_from(body: dict) -> float:
        # cycle holds the lock for its off-delay; permit for its window.
        return float(body.get("delay_ms", 0)) / 1000.0 + float(body.get("time_s", 0))

    def cluster_for(ieee_text: str):
        ieee = _app.parse_ieee(ieee_text)
        device_obj = _app.find_device(app, ieee)
        return _app.on_off_cluster(device_obj)

    async def h_healthz(request, body):
        info = app.state.network_info
        return {
            "device": device,
            "channel": info.channel,
            "pan_id": f"0x{info.pan_id:04x}",
            "uptime_s": round(time.monotonic() - started, 1),
        }

    async def h_state(request, body):
        cluster = cluster_for(request.query["ieee"])
        return {"state": "on" if await _app.read_on_off(cluster) else "off"}

    async def h_on(request, body):
        await _app.set_on_off(cluster_for(body["ieee"]), True)
        return {"state": "on"}

    async def h_off(request, body):
        await _app.set_on_off(cluster_for(body["ieee"]), False)
        return {"state": "off"}

    async def h_cycle(request, body):
        cluster = cluster_for(body["ieee"])
        delay_ms = int(body.get("delay_ms", 3000))
        await _app.set_on_off(cluster, False)
        await asyncio.sleep(delay_ms / 1000.0)
        await _app.set_on_off(cluster, True)
        return {"state": "on", "held_off_ms": delay_ms}

    async def h_list(request, body):
        plugs = []
        for dev in _app.plug_devices(app):
            try:
                cluster = _app.on_off_cluster(dev)
                state = "on" if await _app.read_on_off(cluster) else "off"
            except (_app.ZigplugError, asyncio.TimeoutError, OSError):
                state = "?"
            plugs.append(
                {
                    "ieee": str(dev.ieee),
                    "nwk": f"0x{dev.nwk:04x}",
                    "manufacturer": dev.manufacturer,
                    "model": dev.model,
                    "state": state,
                }
            )
        return {"plugs": plugs}

    async def h_permit(request, body):
        time_s = int(body.get("time_s", 60))
        joined: list[dict] = []

        class Listener:

            def device_initialized(self, dev) -> None:
                joined.append(
                    {
                        "ieee": str(dev.ieee),
                        "manufacturer": dev.manufacturer,
                        "model": dev.model,
                    }
                )

        app.add_listener(Listener())
        await app.permit(time_s=time_s)
        await asyncio.sleep(time_s)
        return {"joined": joined}

    async def h_remove(request, body):
        ieee = _app.parse_ieee(body["ieee"])
        _app.find_device(app, ieee)
        await app.remove(ieee)
        await asyncio.sleep(2)  # let the background leave request go out
        return {"removed": str(ieee)}

    async def h_backup(request, body):
        backup = app.backups.create_backup(load_devices=True)
        return backup.as_dict()

    async def h_stop(request, body):
        stop_event.set()
        return {"stopping": True}

    # Bind first so the request gate knows this daemon's own port before the
    # application exists; SockSite then serves on the bound socket.
    token = secrets.token_urlsafe(32)
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))  # OS-assigned port
    port = sock.getsockname()[1]

    web_app = web.Application(middlewares=[auth_middleware(token, port)])
    web_app.router.add_get("/healthz", handler(h_healthz))
    web_app.router.add_get("/state", handler(h_state))
    web_app.router.add_post("/on", handler(h_on))
    web_app.router.add_post("/off", handler(h_off))
    web_app.router.add_post("/cycle", handler(h_cycle))
    web_app.router.add_get("/list", handler(h_list))
    web_app.router.add_post("/permit", handler(h_permit))
    web_app.router.add_post("/remove", handler(h_remove))
    web_app.router.add_get("/backup", handler(h_backup))
    web_app.router.add_post("/stop", handler(h_stop))

    runner = web.AppRunner(web_app)
    await runner.setup()
    site = web.SockSite(runner, sock)
    await site.start()

    discovery = discovery_path()
    write_discovery(
        discovery, Daemon(pid=os.getpid(), port=port, device=device, token=token)
    )
    LOGGER.info("zigplug daemon up: %s port %d", device, port)

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop_event.set)

    try:
        await stop_event.wait()
    finally:
        with contextlib.suppress(OSError):
            discovery.unlink()
        await runner.cleanup()
        await app.shutdown()
    return exit_code
