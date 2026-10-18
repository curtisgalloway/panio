<!--
Copyright 2026 Curtis Galloway

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# jetkvm — JetKVM network KVM helper for panio

`jetkvm` is a panio `hid` helper for the [JetKVM](https://jetkvm.com/), a
network KVM that plugs into the target over USB and HDMI and is reached over
the network. It has the **same CLI surface** as [`ch9329`](../ch9329/README.md)
and [`hidrig`](../hidrig/README.md) (`type`, `key`, `combo`, `down`, `up`,
`releaseall`, `move`, `moveabs`, `click`, `mdown`, `mup`, `scroll`, `ping`,
`version`, `run`, `serve`, `stop`), so it drops into a `hid` channel:

```
panio hid set -t target-machine --cmd "jetkvm -d 192.0.2.10"
```

It is written in Go because the device's firmware (0.5.9) exposes keyboard and
mouse **only over WebRTC**, and the Go WebRTC stack
([pion](https://github.com/pion/webrtc)) is the practical way to speak it.
Milestone 1 covers keyboard and mouse injection; milestone 2 adds video (see
[Video](#video-get-rfb)). Virtual media and power are not part of this helper.

## How it talks to the device

1. `POST http://<host>/auth/login-local` with the password; the reply sets an
   `authToken` cookie.
2. A WebSocket to `/webrtc/signaling/client` (with that cookie) carries the
   WebRTC offer and answer.
3. One data channel labeled `rpc` carries JSON-RPC 2.0 text messages:
   `keyboardReport`, `absMouseReport`, `relMouseReport`, `wheelReport`,
   `ping`, `getUSBState`, `getVideoState`, `getKeyboardLedState`. One-shot
   commands negotiate no media. The `serve` daemon also offers a receive-only
   H.264 video track (see below).

## The one-session rule

JetKVM allows **one session at a time**. A new offer closes the current
session, which is told with an `otherSessionConnected` event. Consequences:

- Opening the JetKVM web UI while `jetkvm serve` is running kicks the daemon,
  and a `jetkvm` command kicks your browser tab.
- `jetkvm serve` holds one session and every one-shot routes through it, so
  many commands cost one login, not one each.
- When the daemon is kicked it logs that clearly and does **not** reconnect on
  its own (that would fight the person using the browser). The next command
  you send reconnects. A plain network drop is retried in the background with
  backoff (1 s up to 30 s).

## Password

Never a flag value. In order, the first source that is set wins:

1. `JETKVM_PASSWORD` in the environment
2. `--password-file <path>` (one trailing newline dropped; a warning if the
   file is readable by group or others)
3. `--password-command '<cmd>'` (run with `sh -c`, 30 s limit; stdout is the
   password), e.g. `--password-command 'op read op://vault/jetkvm/password'`

A missing password or a rejected one exits 3.

## Video (`GET /rfb`)

`jetkvm serve` can relay the device's HDMI capture. The offer carries a
receive-only **H.264-only** video m-line (H.265 is never offered), so the video
rides the *same* session the daemon already holds; the device allows only one.
The track's RTP is depacketized to Annex-B and piped to an **`ffmpeg`
subprocess** (`ffmpeg -flags low_delay -f h264 -i pipe:0 -f rawvideo -pix_fmt
bgr0 pipe:1`), which must be installed. The path is `--ffmpeg PATH` on
`serve`, else `ffmpeg` on `PATH`, else `/opt/homebrew/bin`, `/usr/local/bin`,
`/usr/bin`. Without ffmpeg the daemon starts, HID works, no video track is
offered, and `/rfb` answers 503 with the reason.

Decoding is lazy. The first `/rfb` client connects the session if it is down
and starts ffmpeg; about 10 s after the last client leaves, ffmpeg is killed.
A keyframe is requested (RTCP PLI) when decoding starts, after a reconnect,
and every 3 s until the first frame decodes. Only the newest decoded frame is
kept. A resolution change (from the stream's SPS) restarts ffmpeg.

`GET /rfb` is a WebSocket upgrade on the daemon's HTTP port, behind the same
auth as every route (token in `Authorization: Bearer` or `?token=`, loopback
Host/Origin). The subprotocol `binary` is accepted when offered (noVNC), else
none. Each binary message carries any chunk of the RFB byte stream, in either
direction; message boundaries mean nothing. The server speaks RFB 3.8
(3.7 and 3.3 clients also work):

- security type None only; framebuffer 32 bpp, depth 24, true-color,
  little-endian, shifts R16 G8 B0, name `jetkvm`
- `SetPixelFormat`: any 32 bpp true-color format is honored by converting;
  anything else is logged and the connection closed
- `SetEncodings`: only Raw is sent, plus DesktopSize (-223) when advertised
  and the size changed (sent in the same update, ahead of the Raw rectangle);
  a client without DesktopSize is disconnected on a resize
- `FramebufferUpdateRequest`: incremental requests block until a frame newer
  than the last one sent exists; non-incremental ones get the current frame.
  Each reply is the whole frame as one Raw rectangle. Until the first frame
  decodes, a request waits up to 10 s, then gets a black frame at the last
  known size (default 1920x1080)
- `KeyEvent`, `PointerEvent`, `ClientCutText`: read and ignored. Input goes
  through the `hid` path for now.

`GET /status` includes `video`: `available`, `decoding`, `width`, `height`,
`frames_decoded`, `last_frame_age_ms` (-1 before the first frame) and `error`.
`jetkvm info` appends the same object as `stream=<json>` when a daemon is
running.

Caveats: the one-session rule applies, so a browser tab kicks the daemon and
its video stops until the next `/rfb` attach or command. TODO: send only dirty
rectangles (every update is a full Raw frame, about 8 MB at 1080p), and
request a keyframe after RTP loss.

## `ok` is not "delivered"

The firmware answers every HID call with success even when the target's USB
host has not enumerated the device (target off, in firmware, cable out). Use
`jetkvm info` to see the USB state, video state and keyboard LEDs.

## Commands

See [docs/hid.md](../docs/hid.md) and the
[HID serial protocol](../docs/dev/hid-serial-protocol.md). JetKVM-specific:

| Command | What it does |
|---|---|
| `info` | Prints `usb=… video=… leds=…` from the device |
| `version` | `1 jetkvm/<ver> moveabs` (needs no device or password) |
| `serve [--port N] [--ffmpeg PATH]` | Runs the daemon; serves video on `/rfb` when ffmpeg is found |

`click`, `mdown` and `mup` use the relative-mouse report with zero motion, so
they act wherever the pointer already is; a preceding `moveabs` picks the
spot.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | the command failed (device error, unreachable, timeout) |
| 2 | usage error |
| 3 | no password, or the device rejected it |

## Build and test

```
cd jetkvm
go build .
gofmt -l . ; go vet ./... ; go test -race ./...
```

The tests include a fake JetKVM (login, signaling and a pion answerer, which
can also play an H.264 clip) driven end to end. The ffmpeg tests skip when
ffmpeg is not installed. `panio setup` builds this helper into the private libexec dir
when `go` is on PATH. Release packaging (the `.deb`, tarball, zip and
Homebrew keg) is not wired up yet.
