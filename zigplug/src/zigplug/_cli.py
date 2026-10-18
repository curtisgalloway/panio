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

"""zigplug — Zigbee smart plug control for panio power hooks.

Operations route through a persistent daemon that owns the coordinator
session (auto-spawned on first use; see `_daemon.py` for why one-shots are
unreliable on CC2652 sticks). The lab-file hook strings are unchanged —
`zigplug -d <dev> on <ieee>` transparently proxies.

Subcommands:
  form                 One-time: form the Zigbee network on the coordinator.
  permit [--time 60]   Open a pairing window and report plugs that join.
  list                 Table of joined plugs (IEEE, model, on/off).
  state <ieee>         Print exactly `on` or `off` (state_cmd contract).
  on <ieee>            Switch a plug on (confirms by reading back).
  off <ieee>           Switch a plug off (confirms by reading back).
  cycle <ieee>         Power-cycle: off → delay → on → confirm.
  remove <ieee>        Unpair a plug from the network.
  serve                Run the coordinator-owning daemon (--foreground).
  stop                 Stop the running daemon (no -d needed).
  status               Show daemon + network status (no -d needed).
  backup [-o FILE]     Save a network backup (key, counters) as JSON.
  restore [-i FILE]    Write a backup into the coordinator NVRAM.
"""

# Single quotes nested in double-quoted f-strings are required on Python 3.11.
# pylint: disable=inconsistent-quotes

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Annotated, Optional

import typer
import zigpy.device

from . import _app, _daemon

app = typer.Typer(
    help="Zigbee smart plug control via a CC2652 (ZNP) coordinator dongle.",
    no_args_is_help=True,
)


@dataclass
class _Options:
    device: str = ""
    # Resolved in main() — default_db_path() has a migration side effect that
    # must not run when --db points elsewhere.
    db_path: Optional[Path] = None
    no_daemon: bool = False


_options = _Options()

# Subcommands that only talk to the already-running daemon. It is found
# through its host-singleton discovery file, so they need no --device.
_DAEMON_ONLY = frozenset({"stop", "status"})


@app.callback()
def main(
    ctx: typer.Context,
    device: Annotated[
        Optional[str],
        typer.Option(
            "--device",
            "-d",
            help="Coordinator UART device path (e.g. /dev/cu.usbserial-8310). "
            "Not needed by `stop`/`status`.",
        ),
    ] = None,
    db: Annotated[
        Optional[Path],
        typer.Option("--db", help="zigpy device database path."),
    ] = None,
    no_daemon: Annotated[
        bool,
        typer.Option(
            "--no-daemon",
            help="Bypass the daemon and open the coordinator directly "
            "(debugging only — collides with a running daemon).",
        ),
    ] = False,
    verbose: Annotated[
        int,
        typer.Option("--verbose", "-v", count=True, help="-v info, -vv debug."),
    ] = 0,
) -> None:
    """Store global options and configure logging."""
    if device is None and ctx.invoked_subcommand not in _DAEMON_ONLY:
        raise typer.BadParameter("required option '--device' (-d) was not provided")
    _options.device = device or ""
    _options.db_path = db if db is not None else _app.default_db_path()
    _options.no_daemon = no_daemon
    level = {0: logging.WARNING, 1: logging.INFO}.get(verbose, logging.DEBUG)
    logging.basicConfig(level=level, format="%(asctime)s %(name)s %(message)s")


def _fail(exc: Exception) -> typer.Exit:
    typer.echo(f"error: {exc}", err=True)
    return typer.Exit(code=1)


def _run(coro):
    """Run an async command, mapping ZigplugError to a clean CLI failure."""
    try:
        return asyncio.run(coro)
    except _app.ZigplugError as exc:
        raise _fail(exc) from exc


def _proxy(
    method: str, path: str, body: dict | None = None, *, extra_wait: float = 0.0
):
    """Route an operation through the daemon, spawning it if needed."""
    try:
        timeout = _daemon.OP_TIMEOUT_S + extra_wait + 10.0
        return _daemon.request(
            _options.device, _options.db_path, method, path, body, timeout=timeout
        )
    except _app.ZigplugError as exc:
        raise _fail(exc) from exc


def _require_no_daemon(what: str) -> None:
    """Exclusive-port commands refuse to fight a running daemon."""
    if _daemon.read_discovery() is not None:
        raise _fail(
            _app.ZigplugError(
                f"{what} needs exclusive access to the coordinator — "
                "stop the daemon first (`zigplug stop`)"
            )
        )


def _ieee_argument(value: str) -> str:
    """Eagerly validate the IEEE argument so typer reports bad input."""
    try:
        _app.parse_ieee(value)
    except _app.ZigplugError as exc:
        raise typer.BadParameter(str(exc)) from exc
    return value


_IeeeArg = Annotated[
    str,
    typer.Argument(
        callback=_ieee_argument,
        help="Plug IEEE address (from `zigplug list`).",
        metavar="IEEE",
    ),
]


# ── network lifecycle (direct: needs exclusive port access) ──────────────────


@app.command()
def form(
    channel: Annotated[
        Optional[int],
        typer.Option(
            "--channel",
            min=11,
            max=26,
            help="Zigbee channel (11-26); default: pick by energy scan. "
            "Channels 25-26 avoid 2.4 GHz Wi-Fi.",
        ),
    ] = None,
) -> None:
    """Form the Zigbee network on the coordinator (one-time setup)."""
    _require_no_daemon("`form`")

    async def go() -> None:
        try:
            async with _app.open_coordinator(_options.device, _options.db_path) as zapp:
                typer.echo(f"network already formed: {_app.network_summary(zapp)}")
                return
        except _app.ZigplugError as exc:
            if "no Zigbee network" not in str(exc):
                raise  # a real failure, not just "unformed"
        async with _app.open_coordinator(
            _options.device, _options.db_path, auto_form=True, channel=channel
        ) as zapp:
            typer.echo(f"network formed: {_app.network_summary(zapp)}")

    _run(go())


@app.command()
def backup(
    out: Annotated[
        Optional[Path],
        typer.Option(
            "--out", "-o", help="Write the backup JSON here (default: stdout)."
        ),
    ] = None,
) -> None:
    """Save a network backup (PAN, channel, key, frame counters) as JSON."""
    if _daemon.read_discovery() is not None and not _options.no_daemon:
        data = _proxy("GET", "/backup")
    else:

        async def go() -> dict:
            async with _app.open_coordinator(_options.device, _options.db_path) as zapp:
                return zapp.backups.create_backup(load_devices=True).as_dict()

        data = _run(go())
    text = json.dumps(data, indent=2)
    if out is None:
        typer.echo(text)
    else:
        out.write_text(text + "\n")
        typer.echo(f"backup written to {out} ({_app.backup_summary(data)})")


@app.command()
def restore(
    infile: Annotated[
        Optional[Path],
        typer.Option(
            "--in",
            "-i",
            help="Backup JSON to restore (default: the newest backup zigpy "
            "auto-saved in the device database).",
        ),
    ] = None,
    counter_increment: Annotated[
        int,
        typer.Option(
            "--counter-increment",
            help="Bump the network key frame counter past anything the old "
            "network used, so devices accept the restored coordinator.",
        ),
    ] = 10000,
) -> None:
    """Write a network backup into the coordinator's NVRAM.

    Recovers from coordinator NVRAM loss/corruption without re-pairing:
    joined plugs keep their network key and simply see the coordinator
    return.
    """
    _require_no_daemon("`restore`")
    try:
        if infile is not None:
            data = json.loads(infile.read_text())
        else:
            data = _app.latest_db_backup(_options.db_path)
    except (OSError, ValueError) as exc:
        raise _fail(exc) from exc
    typer.echo(f"restoring: {_app.backup_summary(data)}")
    _run(
        _app.restore_network(
            _options.device,
            _options.db_path,
            data,
            counter_increment=counter_increment,
        )
    )
    typer.echo("restore complete — verify with `zigplug list`")


# ── daemon lifecycle ─────────────────────────────────────────────────────────


@app.command()
def serve(
    foreground: Annotated[
        bool,
        typer.Option("--foreground", help="Run in this process (don't detach)."),
    ] = False,
) -> None:
    """Run the daemon that owns the coordinator session (detached by default)."""
    existing = _daemon.read_discovery()
    if existing is not None:
        typer.echo(
            f"daemon already running (pid {existing.pid}, device {existing.device})"
        )
        return
    if foreground:
        code = _run(_daemon.serve(_options.device, _options.db_path))
        raise typer.Exit(code=code or 0)
    try:
        daemon = _daemon.spawn(_options.device, _options.db_path)
    except _app.ZigplugError as exc:
        raise _fail(exc) from exc
    typer.echo(f"daemon running at {daemon.url}")


@app.command()
def stop() -> None:
    """Stop the running daemon.

    Shuts it down through its token-protected `POST /stop`. Never falls back
    to signaling the recorded pid: a discovery file outlives the daemon
    whenever it did not exit cleanly, and once the kernel reuses that pid the
    record names an unrelated process. If the authenticated request does not
    make the daemon exit, use `panio daemons stop zigplug`, which checks
    the process identity before it signals anything.
    """
    daemon = _daemon.read_discovery()
    if daemon is None:
        typer.echo("no daemon running")
        return
    try:
        _daemon.call(daemon, "POST", "/stop", None, timeout=5.0)
    except _daemon.DaemonGone as exc:
        # Nothing to stop — and the recorded pid may belong to some other
        # process by now, so it must not be signalled below.
        typer.echo(str(exc), err=True)
        return
    except _app.ZigplugError:
        pass
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if _daemon.read_discovery() is None:
            typer.echo(f"daemon (pid {daemon.pid}) stopped")
            return
        time.sleep(0.2)
    raise _fail(
        _app.ZigplugError(
            f"daemon (pid {daemon.pid}) did not stop within 5s of an "
            "authenticated /stop request; use `panio daemons stop "
            "zigplug`, which checks process identity before signaling"
        )
    )


@app.command()
def status() -> None:
    """Show daemon and network status."""
    daemon = _daemon.read_discovery()
    if daemon is None:
        typer.echo("daemon\tnot running")
        return
    try:
        health = _daemon.call(daemon, "GET", "/healthz", None, timeout=5.0)
    except _app.ZigplugError as exc:
        raise _fail(exc) from exc
    typer.echo(f"daemon\trunning (pid {daemon.pid}, port {daemon.port})")
    typer.echo(f"device\t{health['device']}")
    typer.echo(f"network\tchannel {health['channel']}, PAN {health['pan_id']}")
    typer.echo(f"uptime\t{health['uptime_s']}s")


# ── plug operations (proxied through the daemon) ─────────────────────────────


@app.command()
def permit(
    time_s: Annotated[
        int,
        typer.Option(
            "--time", min=10, max=254, help="Seconds to keep the join window open."
        ),
    ] = 60,
) -> None:
    """Open a pairing window; put plugs in pairing mode while it runs."""
    if _options.no_daemon:
        _run(_permit_direct(time_s))
        return
    typer.echo(f"pairing window open for {time_s}s — put the plug in pairing mode now")
    result = _proxy("POST", "/permit", {"time_s": time_s}, extra_wait=float(time_s))
    joined = result["joined"]
    for dev in joined:
        typer.echo(
            f"paired: {dev['ieee']}  {dev['manufacturer'] or '?'} {dev['model'] or '?'}"
        )
    if not joined:
        typer.echo("no plugs paired", err=True)
        raise typer.Exit(code=1)
    typer.echo(f"{len(joined)} plug(s) paired")


async def _permit_direct(time_s: int) -> None:
    async with _app.open_coordinator(_options.device, _options.db_path) as zapp:
        joined: list[zigpy.device.Device] = []

        class Listener:
            """Print join progress as devices arrive and interview."""

            def device_joined(self, device: zigpy.device.Device) -> None:
                typer.echo(f"joined: {device.ieee} (interviewing...)")

            def device_initialized(self, device: zigpy.device.Device) -> None:
                joined.append(device)
                typer.echo(
                    f"paired: {device.ieee}  "
                    f"{device.manufacturer or '?'} {device.model or '?'}"
                )

        zapp.add_listener(Listener())
        await zapp.permit(time_s=time_s)
        typer.echo(
            f"pairing window open for {time_s}s — "
            "put the plug in pairing mode now (Ctrl-C to stop early)"
        )
        try:
            await asyncio.sleep(time_s)
        except asyncio.CancelledError:
            pass
        if not joined:
            typer.echo("no plugs paired", err=True)
            raise typer.Exit(code=1)
        typer.echo(f"{len(joined)} plug(s) paired")


@app.command(name="list")
def list_cmd() -> None:
    """List joined plugs: IEEE, NWK, manufacturer, model, state."""
    if _options.no_daemon:
        _run(_list_direct())
        return
    plugs = _proxy("GET", "/list")["plugs"]
    _print_plug_table(plugs)


def _print_plug_table(plugs: list[dict]) -> None:
    if not plugs:
        typer.echo("no plugs joined — run `zigplug permit` to pair one")
        return
    typer.echo(f"{'IEEE':<25} {'NWK':<8} {'Manufacturer':<16} {'Model':<16} State")
    typer.echo("-" * 75)
    for dev in plugs:
        typer.echo(
            f"{dev['ieee']:<25} {dev['nwk']:<8} "
            f"{(dev['manufacturer'] or '?'):<16} {(dev['model'] or '?'):<16} "
            f"{dev['state']}"
        )


async def _list_direct() -> None:
    async with _app.open_coordinator(_options.device, _options.db_path) as zapp:
        plugs = []
        for dev in _app.plug_devices(zapp):
            try:
                cluster = _app.on_off_cluster(dev)
                plug_state = "on" if await _app.read_on_off(cluster) else "off"
            except (_app.ZigplugError, asyncio.TimeoutError, OSError):
                plug_state = "?"
            plugs.append(
                {
                    "ieee": str(dev.ieee),
                    "nwk": f"0x{dev.nwk:04x}",
                    "manufacturer": dev.manufacturer,
                    "model": dev.model,
                    "state": plug_state,
                }
            )
        _print_plug_table(plugs)


async def _with_cluster(ieee_text: str, fn) -> None:
    """Direct path: open the coordinator, resolve the OnOff cluster, run fn."""
    ieee = _app.parse_ieee(ieee_text)
    async with _app.open_coordinator(_options.device, _options.db_path) as zapp:
        device = _app.find_device(zapp, ieee)
        cluster = _app.on_off_cluster(device)
        await fn(cluster)


@app.command()
def state(ieee: _IeeeArg) -> None:
    """Print exactly `on` or `off` (panio state_cmd contract)."""
    if _options.no_daemon:

        async def go(cluster) -> None:
            typer.echo("on" if await _app.read_on_off(cluster) else "off")

        _run(_with_cluster(ieee, go))
        return
    encoded = ieee.replace(":", "%3A")
    typer.echo(_proxy("GET", f"/state?ieee={encoded}")["state"])


@app.command()
def on(ieee: _IeeeArg) -> None:
    """Switch a plug on and confirm."""
    if _options.no_daemon:

        async def go(cluster) -> None:
            await _app.set_on_off(cluster, True)

        _run(_with_cluster(ieee, go))
    else:
        _proxy("POST", "/on", {"ieee": ieee})
    typer.echo(f"plug {ieee}: on")


@app.command()
def off(ieee: _IeeeArg) -> None:
    """Switch a plug off and confirm."""
    if _options.no_daemon:

        async def go(cluster) -> None:
            await _app.set_on_off(cluster, False)

        _run(_with_cluster(ieee, go))
    else:
        _proxy("POST", "/off", {"ieee": ieee})
    typer.echo(f"plug {ieee}: off")


@app.command()
def cycle(
    ieee: _IeeeArg,
    delay_ms: Annotated[
        int,
        typer.Option("--delay-ms", help="Milliseconds to hold the plug off."),
    ] = 3000,
) -> None:
    """Power-cycle a plug: off → delay → on → confirm."""
    if _options.no_daemon:

        async def go(cluster) -> None:
            await _app.set_on_off(cluster, False)
            await asyncio.sleep(delay_ms / 1000.0)
            await _app.set_on_off(cluster, True)

        _run(_with_cluster(ieee, go))
    else:
        _proxy(
            "POST",
            "/cycle",
            {"ieee": ieee, "delay_ms": delay_ms},
            extra_wait=delay_ms / 1000.0,
        )
    typer.echo(f"plug {ieee}: cycled (held off {delay_ms} ms, now on)")


@app.command()
def remove(ieee: _IeeeArg) -> None:
    """Unpair a plug (send a ZDO leave and forget it)."""
    if _options.no_daemon:

        async def go() -> None:
            parsed = _app.parse_ieee(ieee)
            async with _app.open_coordinator(_options.device, _options.db_path) as zapp:
                _app.find_device(zapp, parsed)  # error early if unknown
                await zapp.remove(parsed)
                # The leave request is sent from a background task; give it a
                # moment to go out before the one-shot process exits.
                await asyncio.sleep(2)

        _run(go())
    else:
        _proxy("POST", "/remove", {"ieee": ieee})
    typer.echo(f"plug {ieee}: removed")


if __name__ == "__main__":
    app()
