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

"""zigpy-znp coordinator session helpers for one-shot CLI commands.

Each CLI invocation opens the coordinator, performs one operation, and shuts
down. The Zigbee network itself lives in the CC2652's NVRAM and the joined
devices (smart plugs are Zigbee routers) stay on the network between
invocations; the zigpy sqlite database persists the device interview data.
"""

# Single quotes nested in double-quoted f-strings are required on Python 3.11.
# pylint: disable=inconsistent-quotes

from __future__ import annotations

import asyncio
import contextlib
import json
import os
from pathlib import Path
from typing import AsyncIterator

import zigpy.backups
import zigpy.device
import zigpy.exceptions
import zigpy.types
from zigpy.zcl import Cluster
from zigpy.zcl.clusters.general import OnOff
from zigpy_znp.zigbee.application import ControllerApplication

# Pre-state-dir-API location (zigbee.db beside the lab file — the collision
# the helpers/ namespace exists to prevent); migrated lazily by
# default_db_path().
LEGACY_DB = Path.home() / ".config" / "panio" / "zigbee.db"


def default_db_path() -> Path:
    """The zigpy device DB path, honoring the panio helper state-dir API.

    `$PANIO_STATE_DIR/zigbee.db` when panio set the env var; the same
    canonical location (`~/.config/panio/helpers/zigplug/zigbee.db`) when
    run standalone. A DB at the legacy top-level location is atomically
    renamed into place (with its sqlite -wal/-shm sidecars) on first touch.
    """
    state = os.environ.get("PANIO_STATE_DIR")
    base = (
        Path(state)
        if state
        else Path.home() / ".config" / "panio" / "helpers" / "zigplug"
    )
    db = base / "zigbee.db"
    if not db.exists() and LEGACY_DB.exists():
        base.mkdir(parents=True, exist_ok=True)
        for suffix in ("", "-wal", "-shm"):
            src = LEGACY_DB.parent / (LEGACY_DB.name + suffix)
            if src.exists():
                os.rename(src, base / (LEGACY_DB.name + suffix))
    return db


class ZigplugError(Exception):
    """A user-actionable error (bad argument, unformed network, ...)."""


def parse_ieee(text: str) -> zigpy.types.EUI64:
    """Parse an IEEE (EUI64) address, with or without colon/dash separators."""
    cleaned = text.lower().replace(":", "").replace("-", "")
    if len(cleaned) != 16 or any(c not in "0123456789abcdef" for c in cleaned):
        raise ZigplugError(
            f"invalid IEEE address {text!r} (expected 16 hex digits, "
            "e.g. 00:12:4b:00:12:34:56:78)"
        )
    pairs = [cleaned[i : i + 2] for i in range(0, 16, 2)]
    return zigpy.types.EUI64.convert(":".join(pairs))


def build_config(device: str, db_path: Path, channel: int | None = None) -> dict:
    """Build the zigpy config dict for the ZNP radio."""
    db_path.parent.mkdir(parents=True, exist_ok=True)
    config: dict = {
        "device": {"path": device},
        "database_path": str(db_path),
    }
    if channel is not None:
        config["network"] = {"channel": channel}
    return config


@contextlib.asynccontextmanager
async def open_coordinator(
    device: str,
    db_path: Path,
    *,
    auto_form: bool = False,
    channel: int | None = None,
) -> AsyncIterator[ControllerApplication]:
    """Connect to the coordinator and start the network; shut down on exit.

    Raises ZigplugError with a config hint when the coordinator has no
    formed network and auto_form is False.
    """
    config = build_config(device, db_path, channel)
    try:
        app = await ControllerApplication.new(config, auto_form=auto_form)
    except zigpy.exceptions.NetworkNotFormed as exc:
        raise ZigplugError(
            "coordinator has no Zigbee network — run `zigplug form` first"
        ) from exc
    except zigpy.exceptions.FormationFailure as exc:
        raise ZigplugError(
            f"network formation failed: {exc} "
            "(a USB 2.0 extension cable away from USB 3.0 ports/hubs usually "
            "fixes this; retrying sometimes works too)"
        ) from exc
    try:
        yield app
        # Let in-flight replies (e.g. responses to the plug's unsolicited
        # attribute reports after a switch) drain before tearing down, so
        # shutdown doesn't cancel them and spew tracebacks.
        await asyncio.sleep(0.5)
    finally:
        await app.shutdown()


def network_summary(app: ControllerApplication) -> str:
    """One-line summary of the running network (channel, PAN ids)."""
    info = app.state.network_info
    return (
        f"channel {info.channel}, PAN 0x{info.pan_id:04x}, "
        f"extended PAN {info.extended_pan_id}"
    )


def plug_devices(app: ControllerApplication) -> list[zigpy.device.Device]:
    """All joined devices except the coordinator itself."""
    return [dev for dev in app.devices.values() if dev.nwk != 0x0000]


def find_device(
    app: ControllerApplication, ieee: zigpy.types.EUI64
) -> zigpy.device.Device:
    """Look up a joined device by IEEE address."""
    try:
        return app.get_device(ieee=ieee)
    except KeyError:
        raise ZigplugError(
            f"no joined device with IEEE {ieee} — run `zigplug list` to see "
            "joined plugs, or `zigplug permit` to pair one"
        ) from None


def on_off_cluster(device: zigpy.device.Device) -> Cluster:
    """Find the device's OnOff server cluster (first endpoint that has one)."""
    for ep_id, endpoint in device.endpoints.items():
        if ep_id == 0:  # ZDO
            continue
        cluster = endpoint.in_clusters.get(OnOff.cluster_id)
        if cluster is not None:
            return cluster
    raise ZigplugError(
        f"device {device.ieee} has no OnOff cluster — not a switchable plug?"
    )


async def read_on_off(cluster: Cluster) -> bool:
    """Read the on_off attribute from the device; True means on."""
    success, failure = await cluster.read_attributes(["on_off"])
    if "on_off" not in success:
        raise ZigplugError(
            f"reading on_off attribute failed: {failure or 'no response'}"
        )
    return bool(success["on_off"])


async def set_on_off(cluster: Cluster, state: bool) -> None:
    """Switch the plug and confirm by reading the attribute back."""
    if state:
        await cluster.on()
    else:
        await cluster.off()
    actual = await read_on_off(cluster)
    if actual != state:
        want = "on" if state else "off"
        got = "on" if actual else "off"
        raise ZigplugError(f"commanded {want} but plug reports {got}")


# ── network backup / restore ────────────────────────────────────────────────


def latest_db_backup(db_path: Path) -> dict:
    """The newest network backup zigpy auto-saved into the device DB.

    zigpy snapshots the full network state (PAN, channel, network key, frame
    counters) into a `network_backups*` table on every startup; this is what
    makes coordinator NVRAM loss recoverable without re-pairing.
    """
    import sqlite3  # pylint: disable=import-outside-toplevel

    if not db_path.is_file():
        raise ZigplugError(f"no device database at {db_path}")
    con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        tables = [
            row[0]
            for row in con.execute(
                "SELECT name FROM sqlite_master "
                "WHERE type='table' AND name LIKE 'network_backups%'"
            )
        ]
        if not tables:
            raise ZigplugError(f"{db_path} has no network_backups table")

        def version(name: str) -> int:
            suffix = name.rsplit("_v", 1)
            return int(suffix[1]) if len(suffix) == 2 and suffix[1].isdigit() else 0

        table = max(tables, key=version)
        rows = [
            json.loads(row[0])
            for row in con.execute(f"SELECT backup_json FROM {table}")  # nosec B608
        ]
    finally:
        con.close()
    if not rows:
        raise ZigplugError(f"{db_path} contains no network backups")
    return max(rows, key=lambda b: b.get("backup_time", ""))


def backup_summary(backup: dict) -> str:
    """One-line summary of a backup dict (channel, PAN, time)."""
    info = backup.get("network_info", {})
    return (
        f"channel {info.get('channel')}, PAN 0x{info.get('pan_id')}, "
        f"taken {backup.get('backup_time')}"
    )


async def restore_network(
    device: str, db_path: Path, backup: dict, *, counter_increment: int = 10000
) -> None:
    """Write a network backup into the coordinator's NVRAM.

    The frame counter is bumped past anything the old network could have
    used, so joined devices accept the restored coordinator. Must own the
    port exclusively — refuse to run while the daemon does.
    """
    network_backup = zigpy.backups.NetworkBackup.from_dict(backup)
    config = build_config(device, db_path)
    # The constructor applies SCHEMA itself — don't validate twice.
    app = ControllerApplication(config)
    await app.connect()
    try:
        await app.backups.restore_backup(
            network_backup, counter_increment=counter_increment
        )
    finally:
        await app.disconnect()
