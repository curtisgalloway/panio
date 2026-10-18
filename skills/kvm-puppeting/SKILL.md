---
name: kvm-puppeting
description: >
  Operate a GUI application on a separate machine that you can only see over a
  video capture and only touch through an emulated USB keyboard and mouse — the
  look-act-settle-verify discipline for driving a desktop, app, installer, or
  BIOS with no DOM, no accessibility API, and no element tree, just pixels and
  HID events. Use when an agent must click, type into, navigate, automate, or
  operate a graphical OS/application on a panio target that has `video` + `hid`
  channels (an Openterface Mini-KVM, or a capture dongle plus a KB2040 injector)
  — especially a machine with no network, no installable agent, or a pre-boot
  screen. Covers the perception/action loop, keyboard-first navigation, the
  pixel→logical mouse-coordinate scaling, warming the hid daemon for reliable
  multi-step interactions, OCR-assisted reading, and the safety rules for acting
  on a real machine. Pairs with the `panio` skill (the command mechanics).
---

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

# Puppeting a GUI through a KVM-like interface

You have **eyes and hands on a separate physical computer** (the *target*)
through panio's `video` and `hid` channels. The target's screen arrives as a
captured image (`panio video`); your keystrokes and mouse events go out
through an emulated USB keyboard/mouse (`panio hid`), so the target sees
ordinary HID input. This works on any target with HDMI + USB — any OS, an
installer, the BIOS, a machine you may not install software on.

There is **no element tree, no DOM, and no accessibility API** — only pixels and
HID events. There is no undo and no dry-run: the target is a real machine and
every keystroke and click has real consequences. Work accordingly.

This skill is the *doctrine* — how to think and what loop to run. For the
command surface (channel setup, every `video`/`hid` flag, the lab file, remote
hosts) see the **`panio`** skill; this skill assumes the target already has
working `video` and `hid` channels.

## Hardware is irrelevant to the method

Four injectors are common and all speak the same `panio hid` commands:

- an **Openterface Mini-KVM** (one box: an MS2109 HDMI capture + a CH9329 HID
  bridge, driven by the `ch9329` helper),
- an **Openterface KVM-Go** (one box: an MS2130S HDMI capture + a CH32V208 MCU
  that emulates the CH9329 protocol over its own USB-CDC port — same `ch9329`
  helper, 115200; it reports `chip_version=0x01` instead of a real chip's
  `0x38`),
- a **Sipeed NanoKVM-USB** (one box: an MS2130 HDMI capture + a CH9329 behind a
  CH340 UART bridge, also driven by the `ch9329` helper — same protocol, but the
  link runs at **57600** baud, not the Openterface's 115200), or
- a **capture dongle + a KB2040** injector (the `hidrig` helper).

The doctrine below is identical for all of them. A couple of CH9329-only
conveniences are noted where relevant (`hid send info` reports target-USB
enumeration and lock-LED state; `hid send baud <rate>` **persistently**
reprograms the chip's link rate and re-enumerates it — the helper autodetects
115200/57600/9600, so any other rate needs `-b <rate>` in the channel's
`--cmd`. That is different from the protocol's optional *transient* `baud`
capability, which the current dual-board KB2040 rig does not advertise at
all).

## Orient before you touch anything (once per session)

1. Start the capture daemon and warm the hid daemon (see *Warm the daemon*):

   ```sh
   panio video watch <target>      # start the HDMI capture daemon
   panio hid serve   <target>      # warm the hid daemon (idempotent; video watch also starts it)
   panio hid send -t <target> ping # confirm the injector answers
   ```

2. **Take one screenshot and view it before doing anything else.**

   ```sh
   panio video shot <target> -o screen.png    # prints "signal=… hash=H" on stderr
   ```

   Confirm the image shows the machine you were asked to operate. **If you
   cannot identify the target from its screen, stop and ask** — typing or
   clicking into the wrong machine is the worst failure this tool has.

3. Note the screenshot's **pixel dimensions** (e.g. 1920×1080). The capture
   tracks the target's video mode, so these are the screen size you scale mouse
   coordinates against (see *Clicking accurately*). Re-check them if the target
   changes resolution (login → desktop, a mode switch, a reboot).

## The loop: look → act → settle → verify

Every interaction is the same cycle. Treat it as non-negotiable while you are
learning a UI.

```sh
panio video shot <target> -o screen.png            # 1. look — then VIEW the image
panio hid send  -t <target> combo LEFT_ALT F       # 2. act (one small action)
panio video shot <target> --changed-since H \
        --timeout 5000 -o screen.png                 # 3. settle — block until pixels change
                                                     # 4. verify — VIEW the saved frame
```

(`H` is the `hash=` value the previous `shot` printed on stderr.)

The hash ignores brightness wobble up to the video channel's change threshold
(default 16 levels), so H.264 noise on a JetKVM (`--device rfb+hid:`) does not
move it. A blinking text cursor does, on every source, so `--changed-since` can
return on a blink alone. Use `--stable`, and still view the frame to verify.

Rules that keep the loop honest:

- **Never act on a stale screenshot.** If anything has happened since your last
  capture — including your own previous action — capture again first.
- **One action per cycle** while exploring a UI. Batch actions only after the
  exact sequence has worked before (a known menu path).
- **Never assume an action landed.** Act, settle, look, and only then decide the
  next step. If the screen did not change, your action probably missed — re-read
  the screen rather than repeating harder.
- `--changed-since` exits non-zero on timeout. A timeout after an action that
  *should* have a visible effect is a signal, not an inconvenience.
- For slow targets (loading dialogs, device scans, installs) poll with repeated
  `--changed-since … --timeout 30000` rather than fixed sleeps.

## Warm the daemon for multi-step work

Each bare `panio hid send` is a fresh process. With the **hid daemon** running
(`panio hid serve <target>`, or `panio video watch`/`panio console <target>`, which
start it for any target with a `hid` channel; the browser's noVNC view types through the
same daemon), every injection flows through one long-lived connection that owns the
injector. This matters because the daemon is what makes **held state survive
across commands**:

- `down`/`up` and `mdown`/`mup` (holding a modifier or button across several
  later commands), and click-and-drag, only compose reliably *through the
  daemon* — its one session carries the in-flight HID report. Without it, a
  held key may be released when the next process starts.
- The CLI and the browser KVM (`panio console`) intermix cleanly, because the
  daemon serializes everyone's commands onto the one wire.

So: for anything beyond a single tap/click, **warm the daemon first**. A single
`combo` or a `run` sequence is self-contained and works either way.

## Keyboard first, mouse second

Vision-guided clicking is the least reliable primitive you have. Prefer, in
order:

1. **Keyboard shortcuts** — `panio hid send combo LEFT_CONTROL S`,
   `panio hid send key F5`.
2. **Menu accelerators** — on Windows, tap `LEFT_ALT` to reveal the underlined
   letters, then the letter: `hid send key LEFT_ALT` then `hid send key F` then
   `hid send key O` for File→Open.
3. **Tab/arrow navigation** — `hid send key TAB`, `hid send key ENTER` walks a
   dialog; arrows move within lists and menus.
4. **Type the path instead of browsing** — in any file dialog, focus the path
   field and type the full path rather than clicking through folders.
5. **Mouse clicks** — only when no keyboard route exists.

Key names are **`adafruit_hid` Keycode names**, not `ctrl+s`-style chords:
`A`–`Z`, `ENTER`, `TAB`, `SPACE`, `ESCAPE`, `BACKSPACE`, `DELETE`, `UP_ARROW`/
`DOWN_ARROW`/`LEFT_ARROW`/`RIGHT_ARROW`, `LEFT_CONTROL`/`LEFT_ALT`/`LEFT_SHIFT`/
`LEFT_GUI` (and `RIGHT_*`), `F1`–`F12`, `FORWARD_SLASH`, `MINUS`, etc. `combo`
presses all named keys together then releases them: `combo LEFT_CONTROL LEFT_ALT
DELETE`.

## Clicking accurately — scale pixels to the logical space

This is the one mechanic everyone gets wrong. **`moveabs` does not take
screenshot pixels.** It takes an absolute position in a `0..32767` logical space
on each axis, which the target OS maps across the full screen. You must scale
your measured pixel against the screenshot's own dimensions:

```
logical_x = round(pixel_x * 32767 / screen_width)
logical_y = round(pixel_y * 32767 / screen_height)
```

Example — click a button whose center is at pixel `(412, 309)` on a 1920×1080
capture:

```sh
# 412*32767/1920 = 7031 ; 309*32767/1080 = 9375
panio hid send -t <target> moveabs 7031 9375
panio hid send -t <target> click left
```

- Use the screenshot's **native** pixel size. If you viewed it downscaled,
  convert your measured coordinates back to native pixels first.
- Aim at the **center** of a control, not its edge or its text baseline.
- For small/dense targets (toolbar icons, tree expanders, checkboxes), **crop
  and zoom** the screenshot file locally to measure the center precisely (e.g.
  `magick screen.png -crop 400x300+800+200 zoom.png`), then translate offsets
  back to full-image coordinates.
- After a precision click, **verify the intended control responded** (a
  highlight, a dialog, a state change) — not merely that *something* changed.
- `moveabs` then a screenshot (no click) shows hover state and tooltips — useful
  to confirm you are over the right control before committing to the click.
- Right-click is `click right`; double-click is two `click left` in quick
  succession (warm the daemon so they are not split across processes).
- Scrolling is `scroll <amount>` (positive scrolls up, negative down — e.g.
  `panio hid send -t <target> scroll -3`) — prefer it over dragging a
  scrollbar or spamming arrow keys in scrollable lists and pages.

If a click consistently lands a little off, your `screen_width`/`screen_height`
is stale (the target changed resolution) — re-screenshot and re-read its size
before adjusting anything else.

## Read the screen with OCR

You do not have to eyeball every pixel. `panio video read <target>` runs OCR
on the current frame and prints the text — use it to confirm a window title, a
button label, an error message, or which screen you are on, and to locate text
you then translate to a click. It reads large UI text well; very small console
or status-bar fonts may produce a few character confusions (`1`/`l`, `O`/`0`),
so verify critical strings against a screenshot.

## Typing

- `panio hid send type "some text"` types a string. It assumes a **US
  keyboard layout** on the target; if output looks transposed, the target's
  layout differs — tell the user rather than guessing.
- **Verify focus before typing anything that matters.** Click the field, settle,
  screenshot, and confirm the caret/highlight is where you expect — text typed
  into the wrong field (or a defocused window) goes nowhere good.
- Press `ENTER` as a separate `key ENTER` only when you intend to submit; check
  the typed text in a screenshot *before* committing it.

## Safety on a real machine

- Treat destructive UI (Delete, Format, "Don't Save", firmware/boot menus,
  partition tools) with the same caution as destructive shell commands:
  screenshot first, and confirm with the user unless already authorized.
- A stray `key LEFT_GUI` or misplaced click can change target state. If you get
  lost: screenshot, `key ESCAPE`, settle, re-orient — do not click blindly to
  recover.
- If the screen shows a login prompt, lock screen, UAC/credential dialog, or
  anything you were not told to expect, **stop and ask** rather than entering
  guessed credentials or dismissing it.
- When you are done, leave the target in a sane state (close what you opened,
  release any held keys with `panio hid send releaseall` — note it releases
  **keyboard** state only; a mouse button held with `mdown` needs an explicit
  `mup`).

## Troubleshooting

| symptom | likely cause / fix |
|---|---|
| screenshot is black / "no signal" | target asleep or not driving video; wake it (`hid send key LEFT_SHIFT`, or move the mouse) and re-shoot |
| `video shot` errors / no frame | capture daemon not running — `panio video watch <target>` first (or `panio console`) |
| clicks land offset | wrong screen size in the scaling — re-screenshot, use its native pixel dimensions; the capture tracks the target's current mode |
| click seems ignored | scale was off (clicked empty space), or a multi-step interaction split across processes — warm the daemon (`panio hid serve`) and retry |
| `moveabs` didn't move the cursor where expected | you passed pixels, not logical `0..32767` — apply the scaling formula |
| held modifier/drag doesn't work | one-shot processes don't carry held state — run through the daemon |
| typed text garbled / wrong characters | non-US layout on the target, or you typed into a defocused window — click the field and confirm the caret first |
| injector doesn't respond (`ping` fails) | host-side problem: the hid channel's device/cmd is wrong, or the injector's control link is unplugged — see the `panio` skill's `hid`/`doctor`. (A passing `ping` does not prove the target sees input: on the dual-board rig the host-powered control board answers `ping` itself, and the Openterface Mini-KVM is powered from either side, so `ping` passes with the target off) |
| CH9329: target sees no input though `ping` works | the target hasn't enumerated the emulated HID — check `panio hid send info` (`target_connected`); re-seat the target USB cable |
