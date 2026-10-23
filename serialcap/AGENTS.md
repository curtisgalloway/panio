<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# serialcap/ — agent notes

This file holds the internals of this component, moved out of the root AGENTS.md to keep it short. The rules in the root AGENTS.md still apply; per-OS behavior lives in `docs/dev/platform-support.md`.

## Module layout

```
serialcap/       Rust crate: serial console daemon (parallels hdmicap)
  src/
    main.rs      CLI: daemon (--interface NAME=DEV[@BAUD], repeatable),
                 log (-i NAME), evidence (-i NAME, --epoch, --since, --max-bytes),
                 devices, stop
    evidence.rs  Immutable exact-byte RX journal, separate from line/pending.
                 Records keep epoch/RX sequence, generation and original RX time
                 (capture and observation times are separate). Five owner-only
                 1 MiB segments, appended across restarts, with a progress
                 checkpoint. The offline reader caps raw bytes without skipping
                 oversized chunks and reports continuation, retention/epoch
                 gaps and raw fault diagnostics. Progress is
                 health.evidence_through_seq; evidence_error is sticky. Process
                 writes only, no fsync durability
    serial_io.rs One supervisor per interface: tokio-serial port owner,
                 reconnect loop, broadcast fan-out to WS clients, mpsc
                 client->port, 64KB scrollback ring, tee of every chunk to the
                 interface's capture thread (off the live path). `Serials`
                 holds the named set. Health tracks epoch, connection
                 generation, opened identity, RX/TX counters and capture
                 watermark (GET /status, `serial status`, read-only); synthetic
                 markers never advance RX counters.
                 Writes queue in an `Outbox` and drain at most 256 B per
                 select! pass, so a large /input paste cannot starve the read
                 arm (a blocking write_all stalled reads ~6s per 64 KiB at
                 115200 baud and lost bytes to a full kernel tty buffer).
                 `dtr_press` pulses the already-open port and stays on it: the
                 OS raises/drops DTR on open/close, so a reopen adds a second,
                 driver-timed press. Ports open with `.dtr_on_open(false)` for
                 the same reason.
                 `attach()` takes the scrollback snapshot and broadcast
                 subscription under one lock; separate calls can double-deliver
                 or drop a chunk at the boundary.
                 `OpenFailures` backs a missing device's log line off from every
                 750ms retry to once a minute. `is_pty` resolves symlinks
                 (`resolve_links`, read_link-based; canonicalize needs the
                 target to exist) before the prefix test, so a stable symlink to
                 a pty (e.g. hidrig's console bridge) is recognized
    wait.rs      bounded fresh acquisition after epoch/RX seq: exact immutable
                 chunks, literal patterns, repeatable `--fault NAME=PATTERN` rules (earliest
                 byte offset wins; tie: `--pattern`, then rules in the order
                 given; result carries `fault` and `fault_rules`; transact
                 passes none), explicit incomplete outcomes, NDJSON
                 completed rows, optional final pending. Output budget covers
                 every follow event and the final JSON. Never sends input,
                 resets or restarts. Before its first journal read, health gates
                 a live boundary ahead of the persisted watermark;
                 catch-up timeout is timeout, and real journal faults remain
                 explicit (#317)
    transact.rs  POST /transact (#301): take the interface's transaction lock
                 (SerialHandle::transaction, fair tokio RwLock held for write),
                 read the receive boundary (epoch + last_rx_seq) BEFORE writing,
                 write, then run wait::run in-process with the remaining
                 deadline; result = wait JSON + sent_bytes. Runs as a spawned
                 task so a client hang-up cannot release the lock before the
                 reply window closes. timeout_ms bounds queue + write + wait;
                 queued past it = 409, nothing sent. Plain input takes the lock
                 shared via try_input(): while a transaction holds or is queued,
                 /input answers 409 and a /stream keystroke is dropped with an
                 in-band "input refused" marker. Refused, never queued, so input
                 cannot land in the next reply
    capture.rs   line assembler: timestamped, sequence-numbered lines appended to
                 a rotating JSONL log under capture/<name>/ (survives restarts,
                 resumes the seq counter); the unterminated line is mirrored to a
                 pending sidecar, debounced to ~10 writes/s (SIDECAR_DEBOUNCE;
                 spawn_capture waits on `sidecar_due()` so an idle port still
                 mirrors the last partial line). A MAX_PENDING force-flush splits
                 at a UTF-8 boundary (`utf8_floor_boundary`) to avoid U+FFFD.
                 `strip_ansi` consumes DCS/APC/PM/SOS (ESC P/_/^/X) payloads to
                 their terminator and drops Unicode Cf characters (bidi
                 overrides, zero-width marks) that `char::is_control` misses.
                 Also the `log` reader (tail / range / since, ANSI-stripped by
                 default) + UTC formatting
    server.rs    axum: GET /stream (bidirectional WebSocket, 64 KiB messages),
                 /status, /interfaces, /devices; POST /button (DTR pulse), /input
                 (64 KiB body cap; ?pace_ms=N drips one byte per N ms, capped at
                 10 000; 409 during a transaction), /transact. Per-interface
                 endpoints take ?interface=NAME, default the first. A /stream
                 client that lags the broadcast channel (RecvError::Lagged) gets
                 an in-band marker. Client->port keystrokes use
                 write_tx.send().await (backpressure); try_send silently drops
                 input once the 256-message queue fills
    auth.rs      token + loopback Host/Origin layer (byte-identical in
                 hdmicap/hidrig/ch9329)
    daemon.rs    advisory lock, discovery file (pid, port, token; owner-only),
                 tokio runtime, graceful shutdown, one supervisor per interface.
                 Shutdown keeps the lock file (see hdmicap's daemon.rs entry)
```

## Gotchas

- **Serial ports are exclusive.** Only one of `tio`/`screen`/serialcap can hold a port, so `panio serial watch` and `panio serial connect` conflict on the same device.
- **A pty is opened without a line rate.** The `serialport` crate applies baud on macOS with `IOSSIOSPEED`, which returns ENOTTY on a pty, so serialcap detects a pty by path and opens it at baud 0 (`open_baud()`/`is_pty()` in `serialcap/src/serial_io.rs`). **Do not extend this to Linux**: there a rate of 0 is written to `c_ospeed` as `B0`, meaning hang up. This lets serialcap attach to a VM console (`utmctl attach`, qemu `-serial pty`) and `evals/serial_loopback.py` run on macOS.
