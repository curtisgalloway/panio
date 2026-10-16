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
    main.rs      CLI subcommands: daemon (--interface NAME=DEV[@BAUD], repeatable),
                 log (-i NAME), evidence (-i NAME, --epoch, --since, --max-bytes), devices, stop
    evidence.rs  Immutable exact-byte RX journal, distinct from line/pending.
                 Records keep epoch/RX sequence, generation and original RX time;
                 capture and observation times are separate. Five owner-only
                 1 MiB segments append across restarts, with progress checkpoint.
                 Offline reader caps raw bytes without skipping oversized chunks,
                 reports continuation, retention/epoch gaps and raw fault diagnostics.
                 Successful journal progress is health.evidence_through_seq;
                 evidence_error is sticky. Process writes, not fsync durability.
    serial_io.rs Health tracks per-supervisor epoch, connection generation,
                 opened identity, RX/TX counters and capture processing watermark;
                 GET /status and `serial status` expose them without acting.
                 Synthetic markers never advance RX counters.
                 One supervisor per interface: tokio-serial port owner; reconnect
                 loop; broadcast fan-out to WS clients; mpsc client->port; 64KB
                 scrollback ring; tees every chunk to that interface's capture
                 thread (off the live fan-out path). `Serials` holds the named set.
                 Outbound writes queue in an `Outbox` and drain at most 256 B per
                 select! pass, so a large /input paste can't starve the read arm
                 (was write_all-to-completion, ~6s of dead reads at 115200 baud
                 for 64 KiB — bytes lost to a full kernel tty buffer). A DTR press
                 (`dtr_press`) pulses the already-open port and stays on it — it
                 does not close/reopen, since the OS raises/drops DTR on
                 open/close and a reopen would add a second, driver-timed press;
                 opens with `.dtr_on_open(false)` for the same reason. `attach()`
                 (scrollback snapshot + broadcast subscribe under one lock)
                 replaces separate scrollback()+subscribe() calls, which could
                 double-deliver or drop a chunk at the attach boundary.
                 `OpenFailures` backs off a missing device's log line from every
                 750ms retry to once a minute. `is_pty` resolves symlinks
                 (`resolve_links`, read_link-based — canonicalize needs the
                 target to exist) before the path-prefix test, so a stable
                 symlink to a pty (e.g. hidrig's console bridge) is recognized
    wait.rs      bounded fresh acquisition after epoch/RX seq; exact immutable
                 chunks, literal patterns, explicit incomplete outcomes, NDJSON
                 completed rows and optional final pending. Output budget includes
                 every follow event and final JSON; no input/reset/restart.
    transact.rs  POST /transact (#301): take the interface's transaction lock
                 (SerialHandle::transaction, a fair tokio RwLock held for write),
                 read the receive boundary (epoch + last_rx_seq) BEFORE writing,
                 write, then run wait::run in-process (journal reader + in-process
                 health) with the remaining deadline; result = the wait JSON +
                 sent_bytes. Runs as a spawned task so a client hang-up cannot
                 release the lock before the reply window closes. timeout_ms
                 bounds queue + write + wait; queued past it = 409, nothing sent.
                 Plain input takes the lock shared with try_input(): /input
                 answers 409 and a /stream keystroke is dropped with an in-band
                 "input refused" marker to that client, whenever a transaction
                 holds or is queued for the interface (refused, never queued —
                 the maintainer's call, so input can't land in the next reply)
    capture.rs   line assembler: splits bytes into timestamped, sequence-numbered
                 lines; appends them to a rotating on-disk JSONL log under
                 capture/<name>/ (survives restarts; resumes the seq counter);
                 mirrors the current unterminated line to a pending sidecar,
                 debounced to ~10 writes/s (SIDECAR_DEBOUNCE) rather than a
                 write+rename per ingested chunk — spawn_capture in serial_io.rs
                 waits on `sidecar_due()`'s deadline so an idle port still gets
                 the last partial line mirrored. A MAX_PENDING force-flush splits
                 at a UTF-8 char boundary (`utf8_floor_boundary`), not a raw byte
                 offset, so a chunk boundary mid-multibyte-character doesn't turn
                 into a U+FFFD. `strip_ansi` also consumes DCS/APC/PM/SOS
                 (ESC P/_/^/X) payloads to their own terminator (previously only
                 the introducer was eaten, leaking the payload) and drops Unicode
                 format (Cf) characters — bidi overrides like U+202E, zero-width
                 marks — that `char::is_control` doesn't cover. Also the `log`
                 reader (interface select; tail / range / since, ANSI-stripped by
                 default) + UTC formatting
    server.rs    axum: GET /stream (bidirectional WebSocket, 64 KiB messages),
                 /status, /interfaces, /devices; POST /button (DTR pulse),
                 /input (write bytes to port, 64 KiB body cap; ?pace_ms=N drips
                 one byte per N ms for a slow polled console, capped at 10 000;
                 409 during a transaction), /transact (transact.rs).
                 Per-interface endpoints take ?interface=NAME, defaulting to the
                 first configured interface. A `/stream` client that falls behind
                 the broadcast channel (RecvError::Lagged) gets an in-band marker
                 in its own stream, not just a debug log; client->port keystrokes
                 use write_tx.send().await (backpressure), not try_send (which
                 silently dropped input once the 256-message queue filled)
    auth.rs      token + loopback Host/Origin layer over the whole router
                 (byte-identical in hdmicap/hidrig/ch9329)
    daemon.rs    advisory lock, discovery file (pid, port, token; owner-only),
                 tokio runtime, graceful shutdown; spawns one supervisor per
                 interface. Shutdown removes daemon.json only — NOT the lock
                 file, which stays open (flock'd) until this process exits; see
                 the identical note on hdmicap's daemon.rs above
```

## Gotchas

- **Serial ports are exclusive.** Only one of `tio`/`screen`/serialcap can hold
  a port at a time. `paniolo serial watch` and `paniolo serial connect` conflict
  on the same device — use one or the other.
- **A pty is opened without a line rate.** The `serialport` crate applies baud on
  macOS with the `IOSSIOSPEED` ioctl, which returns ENOTTY ("Not a typewriter") on
  a pseudo-terminal, so serialcap detects a pty by path and opens it at baud 0 —
  the crate's documented pty path, and a pty has no line rate to set anyway. See
  `open_baud()`/`is_pty()` in `serialcap/src/serial_io.rs`. **Do not extend this to
  Linux**: there a rate of 0 is written to `c_ospeed` as `B0`, meaning *hang up*.
  This is what lets serialcap attach to a VM console (`utmctl attach`, qemu
  `-serial pty`) and what lets `evals/serial_loopback.py` run on macOS.
