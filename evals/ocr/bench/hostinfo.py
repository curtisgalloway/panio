# SPDX-FileCopyrightText: 2026 The Panio Authors
# SPDX-License-Identifier: Apache-2.0

"""What machine produced these numbers, and was it throttling?

A Pi 4B under sustained neural OCR will thermally throttle, and a throttled run
produces latency figures that look like a slow engine rather than a hot board.
The throttle state is therefore recorded before and after each engine's block
and any change is flagged — a latency comparison across a throttle event is not
a comparison at all.
"""

from __future__ import annotations

import platform
import shutil
import subprocess
from pathlib import Path


def _read(path: str) -> str | None:
    try:
        return Path(path).read_text(errors="replace").strip("\x00\n ")
    except OSError:
        return None


def _run(cmd: list[str]) -> str | None:
    if not shutil.which(cmd[0]):
        return None
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=5)
        return out.stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def throttled() -> str | None:
    """Raspberry Pi throttle bits, or None off a Pi."""
    return _run(["vcgencmd", "get_throttled"])


def cpu_temp_c() -> float | None:
    raw = _read("/sys/class/thermal/thermal_zone0/temp")
    if raw and raw.isdigit():
        return int(raw) / 1000.0
    out = _run(["vcgencmd", "measure_temp"])
    if out and "=" in out:
        try:
            return float(out.split("=")[1].rstrip("'C"))
        except ValueError:
            return None
    return None


def describe() -> dict:
    return {
        "model": _read("/proc/device-tree/model") or platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "governor": _read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "tesseract": (_run(["tesseract", "--version"]) or "").splitlines()[:1],
        "throttled_before": throttled(),
        "temp_before_c": cpu_temp_c(),
    }
