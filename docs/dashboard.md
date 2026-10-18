# Combined dashboard

One web page to watch and drive a target: live video on top, a serial
terminal below. hdmicap (video daemon) serves the page; the terminal connects
to serialcap (serial daemon) over WebSocket.

---

## Starting the dashboard

```bash
panio console                  # open in the default browser
```

`panio console` starts any daemon not already running (hdmicap, serialcap,
and the hid daemon if the target has a `hid` channel), then opens the page.
`panio video watch` / `panio serial watch` start them individually.

**Use the URL the browser was handed**, not a bare `http://127.0.0.1:<port>/`:
it carries each daemon's token (hdmicap's as `?token=`, the others inside
`?serialws=`/`?hidws=`). `console` prints only the bare address, to keep tokens
out of pasted output. If no browser launches, it writes the full URL to a
`0600` `dashboard-url.txt` in the target's runtime dir and prints that path.

### Viewing from another machine over SSH

When `console` runs inside an SSH session (`SSH_CONNECTION`, `SSH_CLIENT` or
`SSH_TTY` is set), it does not launch a browser on the control host. It prints
the command that forwards every dashboard port to your own machine, each to the
same port number so the URL works unchanged:

```text
Running over SSH, so no browser was opened here. To view the dashboard from your own machine, run this there and keep it running:
  ssh -N -L 41001:127.0.0.1:41001 -L 41002:127.0.0.1:41002 bench1
```

The daemons accept only a loopback `Host` and `Origin`, which a tunnel to
`127.0.0.1` satisfies. The URL itself goes to the `0600` file, with an
`ssh bench1 cat …` line to fetch it. `--print-url` prints it instead; the
output is then a credential in your scrollback. `--open` launches a browser
on the control host anyway.

To get the URL again after the dashboard is open, rerun
`panio console <target> --print-url`: the daemons are already running, so it
only rebuilds the URL from their discovery files.

The page shows one terminal pane per serialcap interface. To pin one
interface (single-pane mode):

```bash
panio console -i bmc
```

### Fixed ports

The daemons take a port from the OS on every start, so the dashboard's ports
change. To reach it through a standing SSH forward instead, pin each daemon's
port in the lab file:

```bash
panio video set -t target-machine --device <dev> --port 8723
panio serial set console -t target-machine --port 8724
panio hid set -t target-machine --cmd "<helper>" --port 8725
```

Then, on the machine you browse from, forward them permanently in
`~/.ssh/config`:

```text
Host bench1
  LocalForward 8723 127.0.0.1:8723
  LocalForward 8724 127.0.0.1:8724
  LocalForward 8725 127.0.0.1:8725
```

serialcap is one daemon for all of a target's interfaces, so set its port on
any one interface; interfaces naming different ports are refused. Two daemons
on one host cannot share a port, so validation refuses that too. `--port 0`
clears a pin. A new port takes effect when the daemon next starts (`panio
daemons restart <name>`, or `video watch --restart`).

---

## Features

**Live video (noVNC):** the video pane is a vendored
[noVNC](https://github.com/novnc/noVNC) viewer on hdmicap's `GET /rfb`, so it
works for every video source, a USB capture device or a network one. Only the
screen tiles that changed are sent (lossless ZRLE; a mostly-text 1080p frame
measured 81,417 bytes against 8,294,400 raw), and the view scales to the pane.
A **video: VNC | MJPEG** toggle switches to the older MJPEG stream (the choice
is remembered in `localStorage` under `panio-video-mode`). If `/rfb` cannot
connect, for instance to a daemon from an older panio, the page falls back to
MJPEG on its own.

**Typing and pointing in the VNC view:** click into the video and type. When
the target has a `hid` channel on the control host, key and pointer events go to
the target's hid daemon, intermixed with CLI `panio hid send` input. Details:

- Keys: printable ASCII, editing and navigation keys, F1 to F12, modifiers
  (Meta maps to Alt, AltGr to Right Alt), and the keypad (mapped to the
  main-block keys). F13 to F24 and keysyms outside that set are dropped, since
  the hid vocabulary has no F13 and up.
- The pointer is absolute (scaled to 0..32767); the wheel scrolls. Held keys
  and buttons are released when the page disconnects.
- With no `hid` channel the view is watch-only.
- `video watch`, `console` and `daemons restart` start the target's local hid
  daemon for every video device, not just `rfb+hid:`. On a USB-capture target
  with a `hid` channel, starting video therefore also starts the hid daemon,
  which then owns the injector. If the hid daemon fails to start, the view
  stays watch-only and the command prints a note; only `rfb+hid:` treats that
  as an error.
- `GET /status` reports `rfb_clients` and `rfb_input` (up to 8 viewers at once).

**`/rfb` is not a general VNC server.** It is a WebSocket endpoint (RFB 3.8,
3.7 and 3.3, security None, subprotocol `binary`) that needs hdmicap's token, as
`Authorization: Bearer` or `?token=`. There is no plain TCP RFB listener, so a
desktop VNC client cannot connect; use noVNC-style clients.

**Serial terminal:** xterm.js, vendored, so it works on an isolated lab
network. The `?interface=<name>` URL parameter (or `console -i <name>`) pins
one interface.

**Layout toggle:** switches the terminal between bottom (default, 40 vh) and
right-panel (380 px) layouts; saved in `localStorage`.

**OCR button:** calls `GET /ocr` on hdmicap, which OCRs the current frame
(`visionocr` on macOS, `linuxocr` on Linux). Needs the OCR helper
(`panio setup`).

**Capture input (KVM, MJPEG mode only):** in the MJPEG view, with a `hid`
channel, **⌨ Capture input** sends your
keyboard and absolute mouse to the target; it reads **⌨ Capturing** while
active. Click again or leave the window to release. Your cursor stays visible
(no pointer lock), and CLI `panio hid send` input intermixes. See
[HID injection › KVM mode](hid.md#kvm-mode-type-and-click-from-the-web-console).

**Power:** with a `power` channel, a live **toggle switch**
(`Power [switch] ON/OFF`) and a **⟳ Cycle** button, each confirming before it
acts. Loading the page never powers the target (`GET /power` only reads).

---

## URL parameters

| Parameter | Effect |
|---|---|
| `?token=<token>` | hdmicap's own token; the page puts it on every request it makes back to hdmicap |
| `?serialws=<url>` | Connect the terminal to this serialcap WebSocket URL, serialcap's `?token=` inside (percent-encoded; what `panio console` passes) |
| `?serial=<port>` | Connect to serialcap on this local port; no token, so only a daemon started without one accepts it |
| `?serial=none` | No terminal pane (a target with no serial channel) |
| `?interface=<name>` | Preselect a named serial interface |
| `?hidws=<url>` | Enable KVM input via this hid WebSocket URL, hid's `?token=` inside (what `panio console` passes) |
| `?hid=<port>` | Enable KVM input via the hid daemon on this local port (no token, as for `?serial=`) |

`serialws`/`hidws` (and URLs built from `serial`/`hid`) must be loopback
(`127.0.0.1`, `localhost` or `[::1]`); the page refuses anything else. It also
refuses to render in a frame (`Content-Security-Policy:
frame-ancestors 'none'`).

---

## Third-party code

The page vendors xterm.js and noVNC v1.7.0 so it works on an isolated network.
noVNC is one esbuild bundle (`hdmicap/assets/novnc.js`) under MPL-2.0, with its
pako dependency under MIT; both notices are in
`hdmicap/assets/novnc-LICENSE.txt`. `scripts/vendor-novnc.sh` rebuilds it
reproducibly from a pinned, checksummed release.

---

## Connecting the daemons

`panio console` reads each daemon's discovery file (port and token) and
passes `?serialws=ws://127.0.0.1:<port>/stream?token=…` and
`?hidws=…/hid?token=…`; over a remote tunnel, `<port>` is the tunnel's local
end.

The `?serial=` / `?hid=` port forms carry no token, so they work only against a
daemon started by an older panio. With no parameters, the page falls back to
`ws://<host>:8724/stream` (the standalone `serialcap --port` default).
