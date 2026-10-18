#!/usr/bin/env python3
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
"""Executable harness for the 'operating serial' scenarios.

Most serial scenarios (s2–s10) are graded as stated commands by an LLM judge,
because driving the capture daemon needs a real device. This harness upgrades
the ones tagged `loopback = true` to *actually run*: it opens a PTY, plays a
fake DUT on the far end (banner + optional command responses), drives panio's
`serial watch` / `send` / `log` against the near end, and asserts on the
captured log. No hardware.

**Platform.** Runs on macOS and Linux. macOS applies a line rate with the
`IOSSIOSPEED` ioctl, which ptys reject with ENOTTY ("Not a typewriter"); until
serialcap learned to open a pty without one this harness could only run on
Linux. The ENOTTY check below is kept as a safety net — if it ever fires again
the scenario SKIPs rather than reporting a misleading FAIL.

Usage:
  python3.12 serial_loopback.py                 # all loopback scenarios
  python3.12 serial_loopback.py s4 s2           # by id
  python3.12 serial_loopback.py scenarios/s4_send_and_capture.toml
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import tty
from pathlib import Path
from select import select as _poll_fd

HERE = Path(__file__).resolve().parent
CAP_TIMEOUT = 6.0  # seconds to wait for the daemon to capture anything


def out(tag: str, sid: str, msg: str = "") -> None:
    print(f"  {tag:4} {sid:4} {msg}".rstrip())


def select(args: list[str]) -> list[dict]:
    if not args or args == ["--all"]:
        paths = sorted((HERE / "scenarios").glob("*.toml"))
    else:
        paths = [
            Path(a) if os.path.exists(a) else HERE / "scenarios" / f"{a}*.toml"
            for a in args
        ]
        paths = [p for pat in paths for p in ([pat] if pat.exists()
                 else sorted(pat.parent.glob(pat.name)))]
    scs = []
    for p in paths:
        with open(p, "rb") as fh:
            sc = tomllib.load(fh)
        if isinstance(sc.get("loopback"), dict):
            sc["_path"] = str(p)
            scs.append(sc)
    return scs


def serialcap_daemon_log() -> str:
    path = Path(f"/tmp/panio-{os.getuid()}/serialcap/daemon.log")
    try:
        return path.read_text()
    except OSError:
        return ""


def max_seq(run, iface: str) -> int:
    """Highest seq whose line is *finished*, which is what `--since` means.

    A trailing `*` marks the line still being written to (a `login:` prompt with
    no newline yet). Counting it as seen loses whatever completes it: the next
    write appends into that same line, and `--since <that seq>` then skips it.
    """
    o = run("serial", "log", "dut", "-i", iface, "--tail", "1").stdout
    m = re.findall(r"#(\d+)(\*?)", o)
    if not m:
        return 0
    seq, unterminated = m[-1]
    return int(seq) - 1 if unterminated else int(seq)


def wait_capture(run, iface: str, needles=None, timeout=CAP_TIMEOUT) -> str:
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        last = run("serial", "log", "dut", "-i", iface, "--tail", "40").stdout
        if last.strip() and (needles is None or all(n in last for n in needles)):
            return last
        time.sleep(0.25)
    return last


def cleanup(sb: Path, fds, stop_evt, run, dut=None) -> None:
    stop_evt.set()
    if dut is not None:
        dut.join(timeout=2.0)
    run("serial", "stop", "dut")
    run("daemons", "stop", "--all")
    for m, s in fds:
        # Slave first: with the reader thread already stopped, dropping the
        # slave lets any pending read on the master resolve instead of pinning
        # the close.
        for fd in (s, m):
            try:
                os.close(fd)
            except OSError:
                pass
    shutil.rmtree(sb, ignore_errors=True)


def run_scenario(sc: dict, real: str) -> str:
    sid = sc["id"]
    lb = sc["loopback"]
    iface = lb.get("interface", "console")
    sb = Path(tempfile.mkdtemp(prefix=f"ser-lb-{sid}-"))
    env = dict(os.environ, PANIO_LAB=str(sb / "lab.toml"))

    def run(*a):
        return subprocess.run([real, *a], env=env, capture_output=True, text=True)

    # PTY for the driven interface (+ a second one if the scenario needs a
    # different non-default interface, so the other configured port opens too).
    drv_m, drv_s = os.openpty()
    tty.setraw(drv_m)
    fds = [(drv_m, drv_s)]
    run("init")
    run("target", "add", "dut")
    run("serial", "add", iface, "-t", "dut", "--device", os.ttyname(drv_s))
    if iface != "console":
        m2, s2 = os.openpty()
        tty.setraw(m2)
        run("serial", "add", "console", "-t", "dut", "--device", os.ttyname(s2))
        fds.append((m2, s2))

    stop_evt = threading.Event()
    responders = lb.get("respond", [])

    def fake_dut():
        while not stop_evt.is_set():
            try:
                # Poll rather than block: `stop_evt` is only observed between
                # reads, and on macOS closing the master while a thread sits in
                # os.read() on it deadlocks close(2) — the reader waits for the
                # close and the close waits for the reader.
                if not _poll_fd([drv_m], [], [], 0.2)[0]:
                    continue
                data = os.read(drv_m, 1024)
            except OSError:
                break
            if not data:
                break
            try:
                os.write(drv_m, data)  # echo
                for r in responders:
                    if r["match"].encode() in data:
                        os.write(drv_m, r["reply"].encode())
            except OSError:
                break

    dut = threading.Thread(target=fake_dut, daemon=True)
    dut.start()

    run("serial", "watch", "dut")
    time.sleep(0.7)
    dlog = serialcap_daemon_log()
    if "Not a typewriter" in dlog:
        cleanup(sb, fds, stop_evt, run, dut)
        out("SKIP", sid, "serialcap ENOTTY on pty — see open_baud() in serialcap")
        return "SKIP"

    os.write(drv_m, lb["banner"].encode())
    mode = lb.get("mode", "default")
    expect = lb["expect"]
    ok = False
    detail = ""
    try:
        if mode == "json_range":
            wait_capture(run, iface)
            o = run("serial", "log", "dut", "-i", iface,
                    "--from", "1", "--to", "100000", "--json").stdout
            ok = all(e in o for e in expect)
            detail = o[:160]
        elif mode == "stop_then_log":
            wait_capture(run, iface, expect)
            run("serial", "stop", "dut")
            time.sleep(0.5)
            o = run("serial", "log", "dut", "-i", iface, "--tail", "50").stdout
            ok = all(e in o for e in expect)
            detail = "(read after stop) " + o[:140]
        elif "followup" in lb:
            wait_capture(run, iface)
            seq = max_seq(run, iface)
            os.write(drv_m, lb["followup"].encode())
            time.sleep(0.8)
            o = run("serial", "log", "dut", "-i", iface, "--since", str(seq)).stdout
            ok = all(e in o for e in expect)
            detail = o[:160]
        elif responders:
            wait_capture(run, iface)
            seq = max_seq(run, iface)
            run("serial", "send", "dut", responders[0]["match"])
            time.sleep(0.9)
            o = run("serial", "log", "dut", "-i", iface, "--since", str(seq)).stdout
            ok = all(e in o for e in expect)
            detail = o[:160]
        else:
            o = wait_capture(run, iface, expect)
            ok = all(e in o for e in expect)
            detail = o[:160]
    finally:
        cleanup(sb, fds, stop_evt, run, dut)

    out("PASS" if ok else "FAIL", sid,
        "" if ok else f"missing {expect}  ::  {detail!r}")
    return "PASS" if ok else "FAIL"


def main() -> int:
    scs = select(sys.argv[1:])
    if not scs:
        print("no loopback scenarios selected")
        return 0
    print(f"serial loopback: {len(scs)} scenario(s)")
    real = shutil.which("panio")
    if not real:
        print("panio not found on PATH")
        return 2
    results = [run_scenario(sc, real) for sc in scs]
    npass = results.count("PASS")
    nrun = sum(1 for r in results if r != "SKIP")
    print(f"\n{npass}/{nrun} loopback scenarios passed "
          f"({results.count('SKIP')} skipped)")
    return 0 if all(r in ("PASS", "SKIP") for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
