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

"""Latency/throughput bench for the HID injector's UART control link.

Sends `moveabs` commands with wall-clock send timestamps so arrivals logged by
`hid_seize_reports` (which prints CLOCK_REALTIME per report) can be correlated
offline. The sequence number is encoded in the X coordinate, so each seized
absolute-mouse report (id=2, payload [buttons, xlo, xhi, ylo, yhi, wheel])
self-identifies the command that produced it.

Run (pyserial via uv):
  uv run --with pyserial hid_bench.py --device /dev/cu.usbserial-XXXX \
      --baud 460800 --latency-us 1000 --mode latency --count 100

Modes:
  latency   one command at a time with a gap between samples; reports the
            cmd->OK round trip per sample plus summary percentiles.
  rr        request/reply back-to-back (the daemon's behavior today);
            reports commands/sec.
  pipe      windowed pipelining (--window commands in flight); reports
            commands/sec — the protocol ceiling without RTT serialization.
"""

import argparse
import array
import fcntl
import statistics
import sys
import time

import serial

# IOKit <IOKit/serial/ioss.h>: #define IOSSDATALAT _IOW('T', 0, unsigned long)
# Sets the BSD serial layer's read data-latency in MICROSECONDS for this fd.
# The default for FTDI adapters is ~16ms, which dominates request/reply RTT.
IOSSDATALAT = 0x80085400

BOOT_BAUD = 115200


def log(line):
    print(line, flush=True)


def wall_ts():
    """Wall-clock seconds.microseconds, same base as the seize tool's ts=."""
    ns = time.time_ns()
    return "%d.%06d" % (ns // 1_000_000_000, (ns % 1_000_000_000) // 1000)


def set_data_latency(port, usec):
    buf = array.array("L", [usec])
    fcntl.ioctl(port.fd, IOSSDATALAT, buf, True)


def read_reply(port):
    """Read one reply line; returns the stripped text or None on timeout."""
    line = port.readline()
    if not line:
        return None
    return line.decode("utf-8", "replace").strip()


def command(port, cmd):
    port.write(cmd.encode("utf-8") + b"\n")
    reply = read_reply(port)
    if reply is None:
        raise TimeoutError("no reply to %r" % cmd)
    if not reply.startswith("OK"):
        raise RuntimeError("board rejected %r: %s" % (cmd, reply))
    return reply


def open_synced(device, target_baud):
    """Open the UART, find the rate the board is at, then move to target_baud."""
    port = serial.Serial(device, BOOT_BAUD, timeout=0.5)
    port.reset_input_buffer()
    actual = None
    for probe in (BOOT_BAUD, target_baud):
        port.baudrate = probe
        try:
            command(port, "ping")
            actual = probe
            break
        except (TimeoutError, RuntimeError):
            port.reset_input_buffer()
    if actual is None:
        port.close()
        raise SystemExit(
            "board not answering at %d or %d baud — power-cycle it (unplug USB)"
            % (BOOT_BAUD, target_baud)
        )
    if actual != target_baud:
        command(port, "baud %d" % target_baud)
        time.sleep(0.12)
        port.baudrate = target_baud
        time.sleep(0.04)
        command(port, "ping")
    port.timeout = 2.0
    return port


def seq_cmd(seq):
    """moveabs with the sequence number as X; Y alternates so every report
    differs from its predecessor even if X repeats across runs."""
    return "moveabs %d %d" % (seq % 32768, 1000 + (seq % 2) * 1000)


def run_latency(port, count, gap_ms):
    rtts = []
    for seq in range(count):
        ts = wall_ts()
        t0 = time.monotonic_ns()
        command(port, seq_cmd(seq))
        rtt_us = (time.monotonic_ns() - t0) // 1000
        log("cmd seq=%d ts=%s rtt_us=%d" % (seq, ts, rtt_us))
        rtts.append(rtt_us)
        time.sleep(gap_ms / 1000.0)
    q = statistics.quantiles(rtts, n=100)
    log(
        "summary n=%d min=%d p50=%d p90=%d p99=%d max=%d (us)"
        % (len(rtts), min(rtts), q[49], q[89], q[98], max(rtts))
    )


def run_rr(port, count):
    t0 = time.monotonic_ns()
    log("start ts=%s" % wall_ts())
    for seq in range(count):
        command(port, seq_cmd(seq))
    dt_s = (time.monotonic_ns() - t0) / 1e9
    log("end ts=%s" % wall_ts())
    log("rr n=%d dt=%.3fs rate=%.1f cmd/s" % (count, dt_s, count / dt_s))


def run_pipe(port, count, window):
    inflight = 0
    sent = 0
    acked = 0
    errs = 0
    t0 = time.monotonic_ns()
    log("start ts=%s" % wall_ts())
    while acked + errs < count:
        while sent < count and inflight < window:
            port.write(seq_cmd(sent).encode("utf-8") + b"\n")
            sent += 1
            inflight += 1
        reply = read_reply(port)
        if reply is None:
            log("timeout: sent=%d acked=%d errs=%d" % (sent, acked, errs))
            break
        inflight -= 1
        if reply.startswith("OK"):
            acked += 1
        else:
            errs += 1
            log("err reply: %s" % reply)
    dt_s = (time.monotonic_ns() - t0) / 1e9
    log("end ts=%s" % wall_ts())
    log(
        "pipe n=%d window=%d acked=%d errs=%d dt=%.3fs rate=%.1f cmd/s"
        % (count, window, acked, errs, dt_s, acked / dt_s if dt_s else 0)
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--device", required=True)
    ap.add_argument("--baud", type=int, default=BOOT_BAUD)
    ap.add_argument(
        "--latency-us",
        type=int,
        default=0,
        help="set the serial read data-latency (IOSSDATALAT) in us; 0 = leave default",
    )
    ap.add_argument("--mode", choices=("latency", "rr", "pipe"), default="latency")
    ap.add_argument("--count", type=int, default=100)
    ap.add_argument("--gap-ms", type=float, default=20.0)
    ap.add_argument("--window", type=int, default=8)
    args = ap.parse_args()

    port = open_synced(args.device, args.baud)
    if args.latency_us:
        set_data_latency(port, args.latency_us)
    ver = command(port, "version")
    log(
        "bench device=%s baud=%d latency_us=%d mode=%s count=%d (%s)"
        % (args.device, args.baud, args.latency_us, args.mode, args.count, ver)
    )

    if args.mode == "latency":
        run_latency(port, args.count, args.gap_ms)
    elif args.mode == "rr":
        run_rr(port, args.count)
    else:
        run_pipe(port, args.count, args.window)
    port.close()


if __name__ == "__main__":
    sys.exit(main())
