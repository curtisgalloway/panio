<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# jetkvm/ — agent notes

This file holds the internals of jetkvm/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
jetkvm/          Go module (the one non-Rust helper besides zigplug): hid
                 helper for the JetKVM network KVM (firmware 0.5.9), whose
                 keyboard and mouse exist only over WebRTC, hence Go and
                 pion/webrtc. Same CLI/daemon surface as ch9329 (plus `info`),
                 so a drop-in `hid` channel (`--cmd "jetkvm -d 192.0.2.10"`),
                 started by panio as `<cmd> serve --port 0` like the other hid helpers, with no jetkvm code in cli/ (cli/ knows only that a hid channel can host a feed). One flat `package main`:
  rpc.go           login -> signaling WebSocket -> "rpc" data channel,
                   JSON-RPC, `Link`
  hid.go           `Session`: keyboardReport/absMouseReport/relMouseReport/
                   wheelReport, `executeLine`, `parseSequence`; vocabulary and
                   bounds ported from ch9329
  keys.go          US layout
  owner.go         `Owner`: the one session, lazy connect, one serializing
                   queue, transcript broadcast, background reconnect
  server.go        /status /version /send /hid /stop
  auth.go          token + loopback Host/Origin, the Go twin of auth.rs
  daemon.go        discovery file 0600 via temp+rename, daemon.lock never
                   unlinked, runtime dir rules of platform::ensure_private_dir
  secret.go        JETKVM_PASSWORD -> --password-file -> --password-command (the
                   `secret` crate's order); exit 3 when unconfigured
  video.go, rfb.go see Video below

  Session rules:
  - The firmware allows one session: a new offer kicks the old one with an
    `otherSessionConnected` event. The daemon treats that as a person's browser
    taking it and does NOT redial in the background; the next command
    reconnects. Only a plain drop is retried, with 1-30 s backoff.
  - HID RPCs succeed even when the target's USB is down, so `ok` is not
    delivery; check `info`.

  Video (milestone 2):
  - A receive-only H.264 track on the same session (video.go: RTP -> Annex-B
    -> `ffmpeg` subprocess; `serve --ffmpeg PATH`, else PATH, else
    Homebrew/usr paths). ffmpeg is needed for video only. Decoding runs only
    while an /rfb client is attached.
  - rfb.go serves RFB 3.8 over WebSocket at token-authenticated `GET /rfb`:
    subprotocol `binary` accepted (so noVNC works), security None, Raw full
    frames, DesktopSize. Input messages are ignored; input goes through the hid
    path. `/status` and `info` gain a `video`/`stream` object.
  - A lab video channel reaches it with `device = "rfb+hid:"` (the RFB feed of
    this target's own hid daemon). It needs a hid channel on the same host
    (validated); `hid rm` is refused while it is used. `video watch`,
    `console` and `daemons restart` start the hid daemon first, then hdmicap
    with `rfb+discovery:`.
  - H.264 decode noise (measured up to 8 luma levels on an unchanged screen) is
    absorbed by hdmicap's change threshold (frame.rs `Settler`, default 16), so
    the frame hash and `changed_since` stay put.

  Tests use a fake JetKVM (httptest + a pion answerer). CI: own `jetkvm` job
  (gofmt, vet, `go test -race`), `go_job` in scripts/ci-local.sh, and
  `ci-coverage-check.sh` covers Go modules. `panio setup` runs `go build`
  into libexec when `go` is present (`SourceStep::JetKvm`, `GO_HELPERS` in
  setup.rs). Release packaging is not done (milestone 1b; exemption is in the
  coverage script).
```
