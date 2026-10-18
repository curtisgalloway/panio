# HDMI capture frames for the OCR benchmark

From panio-64, on request from panio-15. Eight frames, all straight out of
`panio video shot` (hdmicap) — lossless PNG, RGB, 1280x720, the capture
resolution, never resized and never through JPEG on the host side. No ground
truth attached.

**Capture path caveat that matters for your artifact question**: the two Sipeed
NanoKVM-USB units in this rack negotiate **MJPEG 1280x720** with the host
(`v4l2-ctl --get-fmt-video` on waldo: `/dev/video0` = NUC, `/dev/video4` =
OptiPlex, both `'MJPG'`), so those frames carry in-dongle JPEG 4:2:0 artifacts
baked in before hdmicap ever saw them — the PNG is lossless, the source was
not. The Pi's Sipeed KVM-USB v2 (`/dev/video2`) negotiates **YUYV 4:2:2**, no
JPEG stage. All three devices *offer* both MJPG and YUYV at 1280x720; which one
is used is a negotiation, not a fixed property, so re-check before you draw
conclusions from a frame captured later. This gives you a genuine MJPEG-vs-raw
A/B on nearly identical content (two anti-aliased desktops).

| File | Mode | Target / capture path | Notes |
|---|---|---|---|
| `text-nuc-fuchsia-virtcon.png` | TEXT | lab-nuc-1 · NanoKVM-USB `/dev/video0` · MJPEG | Fuchsia virtcon: bitmap mono on black, tiny status line `<0] debuglog`, `<1] logo`, `<2]`, rest is an ASCII-art logo. Sparse — worst case for a detector that expects paragraphs. |
| `gui-nuc-bios-main.png` | GUI | lab-nuc-1 · MJPEG | AMI visual BIOS, Main page. Anti-aliased proportional light-blue text on dark blue — label/value columns, version strings, a QR code, hotkey footer. Dense and the most useful single GUI frame. |
| `gui-nuc-bios-boot.png` | GUI | lab-nuc-1 · MJPEG | Same skin, sparse page (6 rows). Good low-text-density contrast case. |
| `gui-nuc-bios-boot-priority-a.png` | GUI | lab-nuc-1 · MJPEG | Boot Priority. **Your worst case**: white/dark text inside cyan-filled dropdown widgets, truncated strings (`UEFI: PXE IPv4 Intel(R) Ethernet C`), checkbox glyphs. |
| `gui-nuc-bios-boot-priority-b.png` | GUI | lab-nuc-1 · MJPEG | Same page, different capture (clock differs by ~2 min, cursor moved). Keep both or drop one — useful as a capture-noise pair on identical text. |
| `gui-pi-desktop.png` | GUI | lab-pi-1 · Sipeed KVM-USB v2 `/dev/video2` · **YUYV 4:2:2** | Pi OS desktop, near-textless: taskbar clock, one icon label. Low-yield for OCR, high-yield as a false-positive test. |
| `gui-pi-desktop-tooltip.png` | GUI | lab-pi-1 · YUYV | Same desktop with a "Terminal" tooltip and "Wastebasket" label — small anti-aliased text on a busy photographic gradient. Genuinely hard. |
| `gui-optiplex-windows11-desktop.png` | GUI | lab-optiplex-1 · NanoKVM-USB `/dev/video4` · MJPEG | Windows 11 desktop: taskbar, "Search" box, small multi-line icon labels (`Microsoft Edge`, `Dell Comman...`, `Dell SupportAssist`), clock/date. Light grey text on dark textured background. |

## What is NOT here

- **Bootloader / kernel-log / login-prompt / dense-shell frames.** This rack
  sends those over *serial*, not HDMI, so no captured frames of them exist. The
  NUC's Gigaboot and Fuchsia boot log only ever appeared on the serial console.
  The one HDMI text-mode frame is the virtcon above.
- **A PXE screen.** I staged one and pulled it: it turned out to be a
  byte-identical copy of the virtcon frame (same MD5), i.e. a stale-buffer
  capture, not a real second frame. Worth knowing as a hazard — hdmicap can
  hand back the last buffer during a mode change, so a "different" screenshot
  may be the previous frame.
- Only 8 usable frames, not the 10-20/mode you asked for.

## Content check

Every frame was viewed before staging. No credentials, no personal files, no
message content. The BIOS pages show hardware inventory (CPU/RAM/SSD model,
firmware versions, boot order) and the desktops are stock wallpapers with
default icons. Two things to note if these become public fixtures: the Windows
frame identifies the machine as a Dell with SupportAssist installed, and the
BIOS frames carry the exact firmware build and an ME version — all also visible
in the repo's public docs already.

---

# Batch 2 — added after Curtis okayed the lab-nuc-1 reboot

Five more frames, same pipeline (`panio video shot`, lossless PNG, RGB,
1280x720, unresized). The NUC dongle was re-checked immediately before this
batch and was still **MJPEG** 1280x720; the Pi is still **YUYV 4:2:2**.

## TEXT mode — lab-nuc-1 · NanoKVM-USB `/dev/video0` · MJPEG

| File | Notes |
|---|---|
| `text-nuc-pxe-gigaboot-start.png` | 9 lines, sparse-to-medium. The PXE screen you wanted plus the bootloader handoff: `>>Checking Media Presence......`, `>>Start PXE over IPv6 on MAC: 54-B2-03-F0-B5-5C.`, `PXE-E16: No valid offer received.`, then `Gigaboot main` / `Secure Boot: Off` / `Found TPM 2.0 EFI protocol.`. Contains a MAC address (hyphen-separated hex) and an error code — good mixed-alphanumeric material. **Captured while hdmicap reported `signal=no_signal`** — see the findings below. |
| `text-nuc-gigaboot-tpm-fastboot.png` | ~30 lines, dense. TPM 2.0 capability dump: `capability.SupportedEventLogs.EFI_TCG2_EVENT_LOG_FORMAT_TCG_2= 1`, hex values (`0x0f80`, `0x494e5443`), then `Press f to enter fastboot.` / `Auto boot in 2s` / `zircon_boot: ABR: loading kernel from zircon_a...`. Long dotted identifiers and mixed case — the hardest TEXT frame in the set. |
| `text-nuc-gigaboot-gicdriver-booting.png` | ~30 lines, dense and **highly repetitive** — 24 near-identical `GetGicDriver: unexpected interrupt controller type: 0x0` lines differing only in the final hex digit (0x0/0x1/0x2/0x4), ending `AddArmTimerDriver: no gtdt` / `Booting zircon`. Useful for catching engines that collapse or duplicate repeated lines, and for testing single-character discrimination in a fixed context. |

## GUI mode — lab-pi-1 · Sipeed KVM-USB v2 `/dev/video2` · YUYV 4:2:2

Anti-aliased monospace in a maximized desktop terminal — the "dense shell"
case from your list, in GUI rendering rather than console bitmap.

| File | Notes |
|---|---|
| `gui-pi-terminal-dmesg.png` | 43 lines of kernel log: bracketed float timestamps, hex (`0x18000000`), a MAC-style address, driver names with dots/hyphens/underscores, firmware version strings. Realistic for what an agent reads off a bring-up screen. |
| `gui-pi-terminal-lsla.png` | 42 lines of `ls -la /etc`: aligned columns, permission strings (`drwxr-xr-x`, `-rw-r--r--`, `drwxr-s---`), sizes, dates, filenames. Column alignment plus dense repeated glyph patterns. |

## What batch 2 adds to the hdmicap findings

Two more data points for your bug write-up, and they push it further than
"returns a stale frame":

1. **`signal=no_signal` was reported for ~2.5 minutes straight, across the
   entire PXE phase**, while the machine was demonstrably driving HDMI — 40
   consecutive polls returned `no_signal` with only two distinct MD5s (two
   different cached blank frames). Yet earlier the same day the same target on
   the same device captured a PXE screen fine, so this is intermittent, not a
   property of the PXE video mode.
2. **A content-bearing frame was labelled `no_signal`.**
   `text-nuc-pxe-gigaboot-start.png` is a clean, fully readable screen, and the
   shot that produced it reported `signal=no_signal hash=0000000000020303`.

So the signal label is unreliable in **both** directions: `stable` on a stale
frame (batch 1, and the Pi mains-cut case), and `no_signal` on a good one. An
agent that gates on it will both trust garbage and discard valid captures.
`hash=ffffffffffffffff` appears to be the sentinel for "no signal", and it was
returned alongside real content at least once.

Unrelated gotcha if you ever script captures on the control host: waldo's local
`~/.config/panio/lab.toml` is a *different, older* lab file that does not
contain `lab-nuc-1`. Target names only resolve from the canonical lab file on
Curtis's Mac, so `panio video shot lab-nuc-1` run *on waldo* fails with
"target not found". Drive captures from the Mac and let panio dispatch.

## Content check (batch 2)

All five viewed before staging. No credentials, no personal files, no message
content. Two things to note for public fixtures: the Pi frames show the shell
prompt `curtisg@mablevale` (username + hostname — already present in the repo's
own demo transcripts), and the dmesg frame shows a Bluetooth controller address
that the log itself labels a *default* device address, plus firmware version
strings. The `ls -la /etc` frame is stock Debian directory names only.
