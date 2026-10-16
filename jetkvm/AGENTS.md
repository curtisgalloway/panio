<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# jetkvm/ — agent notes

This file holds the internals of jetkvm/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
jetkvm/          Go module (the one non-Rust helper besides zigplug): hid helper
                 for the JetKVM network KVM (firmware 0.5.9), whose keyboard and
                 mouse exist ONLY over WebRTC -- hence Go and pion/webrtc. Same
                 CLI/daemon surface as ch9329 (`-d <host[:port]>`, `serve`,
                 `stop`, `run`, plus `info`), so it is a drop-in `hid` channel
                 (`--cmd "jetkvm -d 192.0.2.10"`); paniolo starts it as
                 `<cmd> serve --port 0` like the others, no jetkvm code in cli/.
                 One flat `package main`: rpc.go (login -> signaling WebSocket ->
                 "rpc" data channel, JSON-RPC, `Link`), hid.go (`Session`
                 composing keyboardReport/absMouseReport/relMouseReport/
                 wheelReport, `executeLine`, `parseSequence`; the vocabulary and
                 bounds are ported from ch9329), keys.go (US layout), owner.go
                 (`Owner`: the ONE session, lazy connect, a single serializing
                 queue, transcript broadcast, background reconnect), server.go
                 (/status /version /send /hid /stop), auth.go (token +
                 loopback Host/Origin, the Go twin of auth.rs), daemon.go
                 (discovery file 0600 temp+rename, daemon.lock never unlinked,
                 runtime dir rules of platform::ensure_private_dir),
                 secret.go (JETKVM_PASSWORD -> --password-file ->
                 --password-command, the `secret` crate's order; exit 3).
                 **The firmware allows one session**: a new offer kicks the old
                 one with an `otherSessionConnected` event. The daemon treats
                 that as "a person's browser took it" and does NOT redial in
                 the background (the next command reconnects); only a plain
                 drop is retried, with 1-30 s backoff. HID RPCs succeed even
                 when the target's USB is down, so `ok` is not delivery (`info`).
                 **Video (milestone 2)**: the daemon adds a receive-only H.264
                 track to its ONE session (video.go: RTP -> Annex-B -> an
                 `ffmpeg` subprocess, `serve --ffmpeg PATH`, else PATH, else
                 Homebrew/usr paths; ffmpeg is a runtime requirement for
                 JetKVM video only, HID works without it) and serves RFB 3.8
                 over WebSocket at the token-authenticated `GET /rfb`
                 (rfb.go; subprotocol `binary` accepted, so noVNC works;
                 security None; Raw full frames; DesktopSize; input messages
                 ignored, input goes through the hid path). Decoding runs only
                 while an /rfb client is attached. `/status` and `info` gain a
                 `video`/`stream` object. A lab video channel reaches it with
                 `device = "rfb+hid:"` (the RFB feed of this target's own hid
                 daemon; needs a hid channel on the same host, validated, and
                 `hid rm` is refused while used; `video watch`/`console`/
                 `daemons restart` start the hid daemon first, then hdmicap
                 with `rfb+discovery:`). cli/ knows only that a hid channel
                 can host a feed. H.264 decode noise (measured: up to 8 luma
                 levels on an unchanged screen) is absorbed by hdmicap's
                 change threshold (frame.rs `Settler`, default 16), so the
                 frame hash and `changed_since` stay put on it.
                 Tests run a fake JetKVM (httptest + a pion answerer) end to end.
                 CI: its own `jetkvm` job (gofmt, vet, `go test -race`) and
                 `go_job` in scripts/ci-local.sh; `ci-coverage-check.sh` checks
                 Go modules too. `paniolo setup` builds it with `go build` into
                 libexec when `go` is present (`SourceStep::JetKvm`,
                 `GO_HELPERS` in setup.rs). Release packaging is NOT done yet
                 (milestone 1b; the exemption is written in the coverage script).

```
