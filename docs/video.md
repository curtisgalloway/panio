# Video capture

panio captures the target's screen through `hdmicap`, a daemon that keeps a
USB HDMI capture device open and serves the current frame over HTTP, so there
is no per-capture reopen delay.

On Linux an unwatched daemon captures at 5 fps instead of 30, to save CPU on
small control hosts. `shot`, `read` and `/snapshot` still get a current frame:
a request for an old frame waits a moment for a fresh one.

---

## Hardware

Any USB HDMI capture card that presents as a UVC device (standard webcam
protocol). Tested with MS2109-based cards (e.g. generic "USB3.0 HDMI Capture"
dongles). Connect the target's HDMI output to the card, and the card to the
control host.

---

## Setup

```bash
# Detect available capture devices (each line ends with its stable id)
panio video devices

# Configure the target's video channel — prefer the stable id
panio video set -t target-machine --device "0x8300000534d2109"
```

The `--device` value may be:

| Form | Notes |
|---|---|
| **Stable id** (preferred) | The AVFoundation `uniqueID` on macOS, the `/dev/v4l/by-path/...` symlink on Linux. Survives reboots, tells identical dongles apart, and changes only if the dongle moves to another USB port. |
| **Name substring** (e.g. `"USB Video"`) | A substring matching more than one device is an error listing the candidates' ids. |
| **`/dev/video*` path** (Linux) | Accepted, but not stable across reboots. |

On Linux, `video devices` hides SoC-internal video nodes (e.g. a Raspberry
Pi's `pispbe-*` stages). `panio helper hdmicap devices --all` lists
everything, and an internal device you configure explicitly still resolves.

The device is stored on the target's `video` channel in the lab file (see
[config-redesign.md](https://github.com/curtisgalloway/panio/blob/main/notes/config-redesign.md)).
`panio configure` proposes the stable id when it finds one capture device,
and lists the ids when it finds several.

---

## Network sources (RFB)

A video channel can also read a screen over the network instead of from a
capture device. The protocol is **RFB** (the one VNC uses; see the
[glossary](glossary.md)), and `hdmicap` contains the client, so the same
daemon, `shot`, `read` and dashboard work unchanged.

| `--device` form | Meaning |
|---|---|
| `rfb+hid:` | The RFB feed of **this target's own `hid` daemon**. Needs a `hid` channel on the same host (set it first; `hid rm` is refused while the video channel uses it). `video watch`, `console` and `daemons restart` start the hid daemon, then `hdmicap`. |
| `rfb://HOST:PORT` | A plain-TCP RFB server (a VM, an AMT machine), security type None only. |

(`hdmicap` itself also accepts `rfb+ws://127.0.0.1:PORT/rfb` and
`rfb+discovery:`, which is what `rfb+hid:` expands to. The token reaches it in
the `HDMICAP_RFB_TOKEN` environment variable, or through the discovery file
named by `HDMICAP_RFB_DISCOVERY`, never on a command line.)

`hdmicap` speaks RFB 3.3, 3.7 and 3.8 with Raw, CopyRect and DesktopSize
encodings. It sends a keepalive request every 5 s; after 15 s of silence it
reports `no_device` and reconnects.

### Example: a JetKVM

```bash
panio hid set   -t target-machine --cmd "jetkvm -d 192.0.2.10 --password-command 'op read op://vault/jetkvm/password'"
panio video set -t target-machine --device rfb+hid:
panio video watch target-machine     # starts the jetkvm daemon, then hdmicap
panio video shot target-machine -o screen.png
```

The `jetkvm` daemon decodes the device's H.264 video with an **`ffmpeg`
subprocess**, so `ffmpeg` must be installed on the control host (on `PATH`,
in a common Homebrew or `/usr` location, or named with `serve --ffmpeg PATH`
in the hid `--cmd`). Without it HID still works and video reports the reason.
Decoding runs only while `hdmicap` is attached. Details:
[jetkvm/README.md](https://github.com/curtisgalloway/panio/blob/main/jetkvm/README.md).

### Caveats

- **One session.** A JetKVM allows one session at a time, and the daemon holds
  it for HID and video together. Opening the JetKVM's own web UI in a browser
  evicts the daemon (video and HID stop until the next command), and a command
  that opens its own session evicts your browser tab. Leave the web UI closed
  while automating.
- **H.264 noise is absorbed by the change threshold.** A JetKVM's decoded
  frames differ slightly even when the screen does not: measured, a few hundred
  pixels per frame off by at most 8 brightness levels. The frame hash ignores
  that because of the [change threshold](#change-threshold), which defaults to
  16. With it on, the hash of an unchanged screen stays put; what still moves
  it is real change, such as a blinking text cursor (up to 250 levels) and the
  H.264 artifacts immediately around it (up to 45 levels, measured). A
  blinking cursor counts as a change on any source, USB capture included.
- **Not supported yet:** VNC password authentication (so an AMT machine or VM
  that requires a password does not work), dirty-rectangle updates (every
  update is a whole frame), and a keyframe request after packet loss.
- **Verified** on one JetKVM (firmware 0.5.9) in front of an x86 board, from a
  macOS control host. Not verified: Windows, a Linux control host, `rfb://`
  against AMT or a VM.

Design record: [network-video-rfb.md](https://github.com/curtisgalloway/panio/blob/main/notes/network-video-rfb.md).

---

## Starting and stopping the daemon

```bash
panio video watch [target-machine]   # start hdmicap daemon for a target
panio video watch --restart          # force-restart a running (stalled) daemon
panio video stop  [target-machine]   # stop it (on the target's host)
panio video show  [target-machine]   # show daemon address and status
```

`watch` starts `hdmicap daemon` detached and prints its address. To open the
dashboard, run `panio video preview`.

The port is OS-assigned unless the lab pins one (`video set --port N`;
`video watch --port N` overrides it for one start). See
[Fixed ports](dashboard.md#fixed-ports).

### The token and the dashboard URL

**Every request to the daemon needs its token**, from `token` in the
discovery file ([Runtime paths](#runtime-paths)), new on each start.
panio's commands (`shot`, `read`, `console`, …) send it automatically. By
hand, send `Authorization: Bearer <token>` or `?token=<token>`. The daemon
also requires a loopback `Host` and `Origin`. A daemon from a panio older
than the token has none; `panio daemons restart hdmicap` replaces it.

**`video preview` is the only command that prints the URL with the token.**
`show`, `console` and `watch` print `http://127.0.0.1:<port>` only, to keep
the credential out of logs and transcripts.

**If `console` cannot launch a browser** (headless host, no `xdg-open`), it
writes the full URL to a `0600` `dashboard-url.txt` in the target's runtime
dir and prints the **path**:

```bash
xdg-open "$(cat /tmp/panio-1000/hdmicap/target-machine/dashboard-url.txt)"
```

Inside an SSH session `console` opens no browser and prints an `ssh -N -L …`
forwarding command instead; `--print-url` prints the URL, `--open` forces a
launch. See [dashboard.md](dashboard.md#viewing-from-another-machine-over-ssh).

### Stopping safely

`video stop` (and `hdmicap stop`) uses the authenticated `POST /stop` and
never signals the discovery-file PID, which a crash could leave pointing at an
unrelated process. Stop a daemon too old for the endpoint with
`panio daemons stop hdmicap`, which checks process identity first.

### Stale and untracked daemons

**Stale.** A daemon whose running executable content differs from the installed
helper shows as **stale** in `panio video show` and `panio daemons`.
`watch` restarts it automatically (no `--restart` needed), or run
`panio daemons restart --stale`. Reinstalling identical bytes, changing a
modification time, or moving the same binary does not make it stale.

A successful content comparison reports **binary content matches installed**
when the bytes are equal, including after an identical reinstall.

If content cannot be compared, the commands report **binary identity unknown**
with a reason (missing recorded identity, inaccessible executable, or metadata
changes without readable content). Unknown identity does not recommend or trigger
a restart and is excluded from `--stale`; an intentional
`panio daemons restart hdmicap` or `video watch --restart` still works.
See [architecture](dev/architecture.md) for content identity details.

**Untracked.** A daemon whose discovery file was deleted keeps running and
holding the device. On Linux, systemd's `/tmp` cleanup
(`q /tmp 1777 root root 10d` on Debian) removes the file after ten idle days.
A new `video watch` then dies on the advisory lock
(`another hdmicap daemon is already running`).

- `video show` reports `running, untracked (pid N)` instead of `stopped`.
- `panio daemons` lists it under **Untracked daemons**.
- `video watch` and `video stop` reap it (`SIGTERM`, then `SIGKILL`).

The serial channel works the same way (`panio serial show` / `watch` /
`stop`); see [serial.md](serial.md).

**Prevention.** The `.deb` ships `/usr/lib/tmpfiles.d/panio.conf`
(`x /tmp/panio-*`). **A control host installed with `make install` must add
it itself**, or its daemons go untracked every ten days:

```bash
echo 'x /tmp/panio-*' | sudo tee /usr/lib/tmpfiles.d/panio.conf
```

### Stall recovery and format choice

**A stalled capture recovers on its own.** A watchdog reopens the device when
no frame arrives (12s after opening, or 4s without progress after that),
and `/snapshot`/`/status` show `no_device` meanwhile. The daemon exits only after 8 consecutive
stalls with no healthy frame between; `panio video watch` (or
`daemons restart --stale`) brings it back.

**The capture format is chosen by what streams.** On Linux the daemon tries
formats highest resolution first and accepts one only if it delivers a frame
within 2 s (uncompressed 1080p over USB 2.0 typically fails). A rejected
format is logged as `allocated buffers but produced no frame`. If nothing
delivers (e.g. the target is off), the best format that allocated is used.

---

## Capturing frames

```bash
panio video shot [target-machine] -o out.png   # save a screenshot (PNG)
panio video shot [target-machine]              # PNG to stdout (default -o -)
panio video shot --stable -o out.png           # wait for a steady frame first
panio video shot --changed-since <hex-hash> --timeout 10000 -o out.png
                                                 # block until the frame differs
panio video preview [target-machine]           # print the live-dashboard URL (optional target, like `show`)
panio video preview --open                     # open it in a browser instead of printing it
```

### `shot`

`shot` prints `signal=… hash=…` to stderr. Pass the hash to a later
`--changed-since` to wait for the screen to change.

- `--stable` waits for any steady frame.
- `--changed-since` waits for any frame that differs from the hash. The hash
  covers every pixel, so one new line of text counts, and so does a blinking
  cursor or a clock ticking over. Differences too faint to matter are ignored;
  see [Change threshold](#change-threshold).
- **Both together** (`GET /snapshot?wait=stable&changed_since=<hash>`) wait
  for the next steady screen that differs.

### Change threshold

The hash changes only when some pixel's brightness moves by **more than the
threshold** (default 16 levels out of 255) from the last frame that changed
it. Below that it is treated as noise: snapshots still show the newest
pixels, but the hash, `--changed-since` and `--stable` do not move. Lossy
sources need this; a JetKVM's H.264 video wobbles by up to 8 levels on a
screen nothing is touching.

```bash
panio video set -t target-machine --change-threshold 16   # the default
panio video set -t target-machine --change-threshold 0    # exact: any 1-level change counts
```

- The comparison is against the last *accepted* frame, not the previous one,
  so a slow drift is still caught once it adds up past the threshold.
- One pixel over the threshold is enough; there is no minimum area. A typed
  character or a blinking cursor still registers.
- It costs nothing on a screen that is bit-for-bit still, which is what a USB
  capture of a static screen delivers. The brightness comparison runs only
  when the exact digest changes.
- On Linux MJPEG capture the comparison runs on the half-scale grayscale
  image hdmicap already makes for each frame, so a lone single-pixel change
  is averaged with its neighbors and can fall under the threshold.
- A new value takes effect when the daemon next starts
  (`panio video watch --restart`). `video show` prints it, and the daemon's
  `/status` reports it as `change_threshold`.

`-o <path>` always writes on the **invoking machine**, even when the video
channel is on a remote control host. A failed capture removes the stub file.

`GET /snapshot` returns **503** in three cases:

| Response | Meaning |
|---|---|
| `x-signal: stale` | The last frame is too old to describe the screen now (capture stopped delivering, though the daemon process is up). |
| `x-signal: no_device` | No capture device is open. |
| **No `x-signal` header, body `capture thread gone`** | The daemon's capture thread has exited and is not coming back. This daemon needs `panio video watch --restart`, not another `shot`. |

### `preview`

`preview` prints the URL **with** the token. Treat it as a credential.
`--open` opens the default browser and prints only the token-free address. If
no browser launches, it writes the URL to the same `0600` file `console` uses.

`--open` is refused when the video channel is on another host. Use
`panio console <target>`, which forwards ports and opens a local browser.

`GET /rfb` serves the warm frame as RFB over a WebSocket (token required, as
`Authorization: Bearer` or `?token=`; subprotocol `binary`; security None; up to
8 clients) for **every** video source. It is what the [dashboard](dashboard.md)'s
noVNC view connects to. Only changed 64x64 tiles are sent, as ZRLE (lossless)
with a Raw fallback, plus DesktopSize. There is no plain TCP RFB listener, so
only noVNC-style clients connect. Key and pointer events are forwarded to the
target's hid daemon when `video watch`/`console` found a local `hid` channel
(the `HDMICAP_HID_DISCOVERY` environment variable names its discovery file);
otherwise the view is watch-only. `GET /status` shows `rfb_clients` and
`rfb_input`.

`GET /preview` is the MJPEG stream behind `panio video preview` and the
[dashboard](dashboard.md):

- Open preview connections share one encode per frame.
- Each part carries an `X-Signal` header. When the signal is not
  `stable`/`mode_switching` (e.g. `stale`, `no_signal`, `no_device`), the
  stream shows a placeholder (dark gray with a red X) instead of a frozen
  frame.
- A frame whose JPEG encode fails logs one `warn!` and is skipped until a new
  frame arrives.

---

## OCR

```bash
panio video read [target-machine]            # OCR the current frame, text to stdout
panio video read --stable [--timeout <ms>]   # wait for a steady frame first
```

OCR runs locally: Apple Vision's `VNRecognizeTextRequest` on macOS, Tesseract
on Linux. With **no video signal**, `read` fails with `no video signal`
instead of returning empty text.

`read` wraps the daemon's `GET /ocr`, which the [dashboard](dashboard.md)'s
OCR button also uses:

```bash
d=/tmp/panio-$(id -u)/hdmicap/target-machine/daemon.json
curl -s -H "Authorization: Bearer $(jq -r .token "$d")" \
    "http://127.0.0.1:$(jq -r .port "$d")/ocr"
```

### GUI screens on Linux

GUI screens on Linux read more accurately with a different engine (see
[dev/ocr.md](dev/ocr.md#linux-needs-two-engines-the-other-platforms-need-one)).
Set it per target:

```bash
panio video set -t target-machine --device "0x8300000534d2109" --ocr-mode gui
panio video set -t target-machine --ocr-mode text   # back to the platform default
```

- `--ocr-mode` is `text` (default) or `gui`. It matters only on Linux.
- It works when the video channel is on a remote control host.
- `panio setup` builds the ~317 MB `rapidocr` venv only when a target in the
  active lab has `--ocr-mode gui`.

### Limits

`GET /ocr` gives the `visionocr`/`linuxocr`/`winocr` subprocess 30 seconds,
then kills it and answers **504**. `/snapshot`, `/ocr` and `/preview`'s encode
fallback share a concurrency limit (2 at a time), so bursts of clicks queue.

### Treat `signal` as a hint

When collecting frames, **save every frame and de-duplicate by hash
afterwards** instead of filtering on `signal`. A frame older than
`STALE_AFTER` reports `stale`, and `/snapshot`, `/ocr`, `--stable`, and
`/preview` refuse to treat it as live. Captures from older versions may show
mostly-black firmware screens as `no_signal` and stalled captures as
`stable`.

### First OCR call on macOS is slow

The first `video read` after Apple Vision has not run for a while can take
about 15 s while the framework loads its model; later calls on a warm frame
took about 0.3 s. This is a one-time cold start, not specific to any video
source.

### Helper installation

`panio setup` installs the platform's helper into the private libexec dir:

- **macOS:** compiles `ocr/visionocr.swift` with `swiftc`
  (`~/.local/libexec/panio/bin/visionocr`).
- **Linux:** installs `linuxocr`, which shells out to Tesseract
  (`apt-get install tesseract-ocr`; Pillow is optional, for preprocessing).

The daemon finds the helper there, or via `PANIO_VISIONOCR`.

**OCR tuning:** `.fast` recognition (not `.accurate`, which misses small
console text), 2× upscale with black padding, and a lowered
`minimumTextHeight`.

---

## Runtime paths

| Purpose | Path |
|---|---|
| Video config | the target's `video` channel in the lab file (`~/.config/panio/lab.toml`) |
| hdmicap discovery | `/tmp/panio-<uid>/hdmicap/<target>/daemon.json` (`{pid, port, token}`; owner-only, it holds the token) |
| hdmicap advisory lock | `/tmp/panio-<uid>/hdmicap/<target>/daemon.lock` |
| hdmicap stderr log | `/tmp/panio-<uid>/hdmicap/<target>/daemon.log` (truncated on each start; shown on start timeout) |

The daemon is **per target**, so several targets can capture at once. The
runtime base honors `$PANIO_RUNTIME_BASE` (default `/tmp`). A `/tmp` sweep
can remove these files (see
[Stale and untracked daemons](#stale-and-untracked-daemons)).
