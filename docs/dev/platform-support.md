<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Platform support

Per-platform support status and behavior, moved from the root AGENTS.md (which keeps a summary and the portable-code rules).

Paniolo runs on three host platforms. (Network RFB video sources are
platform-independent Rust and build and test on all three in CI; the
JetKVM-through-RFB path was hardware-verified from a macOS control host only.)

| Platform | Status | CI | Release artifacts |
| --- | --- | --- | --- |
| macOS (Apple Silicon) | Supported | `macos` job in `ci.yml` | Homebrew tap (universal tarball, poured as an `all` bottle) |
| Linux (Debian/Ubuntu) | Supported | the `ubuntu-latest` jobs in `ci.yml` | `.deb`, tarball and Homebrew bottle from `release.yml` |
| Windows (x86_64-msvc) | Supported; power and video hardware-verified | `windows` job in `ci.yml` | portable zip + winget |

All ten crates build, lint clean under `clippy -D warnings`, and pass their
unit tests on all three. The `windows` CI job runs the same fmt/clippy/test
triple as the Linux jobs, because a Unix-only CI cannot tell you whether the
Windows `#[cfg]` arm still compiles.

**Hardware-verified on Windows** (the Windows bench host, 2026-08-28, against a
Shelly Plug and an Openterface KVM-GO):

- **power** — `shellyplug` on/off/cycle, each confirmed by read-back.
- **video** — `hdmicap` enumerates the capture device by its symbolic-link id
  and captures native 3840x2160 frames through the Media Foundation backend;
  daemon start, `shot` and `stop` all work.
- **hid** — `ch9329` over a COM port reports `target_connected=true` and its
  mouse moves wake the attached machine, which is how the first (black) capture
  turned into a real frame. Keystroke delivery is confirmed by that wake rather
  than by the CH9329's LED read-back, which stayed `false`.

**What is still NOT verified on Windows:**

- **netbootd falls back to `send_to`.** The `/dev/bpf` raw-frame sender and the
  `SCM_RIGHTS` fd handoff are Unix-only, exactly as on Linux. Whether DHCP/TFTP
  netboot works against a real target on Windows is untested, and `netif` has no
  Windows implementation to configure the interface with.
- **hidrig has no console bridge.** The PTY the `serial` channel points its
  `device =` at has no Windows analogue — ConPTY is not a substitute, since it
  exposes no filesystem node another process can open. HID and control are
  unaffected.
- **OCR is absent.** Neither Apple Vision nor Tesseract has a Windows
  counterpart wired up. `Windows.Media.Ocr` is the in-box candidate, and the
  `windows` crate is already a hdmicap dependency for capture.
- **`paniolo doctor` cannot probe a Windows control host.** Probes run natively
  when the host is local, but the SSH path still renders them as POSIX shell
  (`sh -c 'test -e …'`), which a PowerShell host cannot run. Everything else
  dispatches fine (see **Dispatching to a Windows control host** below); doctor
  is the one remaining verb that speaks shell over the wire rather than pure
  paniolo.

## Dispatching to a Windows control host

`paniolo <cmd> -t <target>` works when the target's host is Windows. The
mechanism is worth knowing, because the thing that used to break it is a trap
anywhere the remote shell is not ours to choose.

Dispatch does three things over SSH: ship a lab slice, re-exec `paniolo` against
it, and clean the slice up. Only the middle one was ever shell-safe.

- **Shipping and cleanup now go over SFTP** (`ssh::sftp_put` / `ssh::sftp_rm`).
  They used to be shell: `f=$(mktemp …) && cat > "$f" && printf %s "$f"` and
  `rm -f`. On a PowerShell host the first is not a command at all, and the
  second fails with *"parameter 'f' is ambiguous. Possible matches include:
  -Filter -Force."* SFTP is a protocol, so it behaves identically whatever the
  far side runs, and it reuses the session's ControlMaster so it costs no extra
  handshake.
- **The re-exec was already fine.** `remote_command` quotes each argument
  POSIX-style, and PowerShell reads `'C:\Users\curti\lab.toml'` as the same
  literal a POSIX shell would. Verified directly. The `VAR='v' cmd` env-prefix
  form is *not* portable — but the dispatch path passes no env, so it never
  appears there.
- **The slice path is deliberately relative** (`.paniolo-lab-<pid>-<ns>.toml`).
  SFTP reports a Windows home as `/C:/Users/name`, an SFTP-protocol path no
  native Windows program can open, so an absolute path from `pwd` is unusable as
  a `--lab` argument. Both platforms start an SSH session in the user's home,
  which is also SFTP's default directory, so a relative name resolves on both.

Verified end to end against the Windows bench host: `paniolo hid send -t <target> info` returned
real CH9329 state, `hid send … move` drove the KVM, `video show` read the
channel, and the slice was cleaned up. Unix dispatch to `waldo` is unchanged.

**What still speaks shell over the wire:** `doctor`. Its probes execute natively
against a local host but render to `sh -c` for a remote one, so probing a
Windows control host fails. Narrowing that to pure paniolo verbs — the remote
side running `paniolo doctor` rather than the near side shipping shell — is the
obvious fix and is not done.

## Writing portable code here

The POSIX primitives paniolo depends on live behind `cli/src/platform.rs`, with
one implementation per platform and a single documented contract each:
`current_uid`, `runtime_root`, `ensure_private_dir`, `make_executable`,
`pid_alive`, `signal_pid` / `try_signal_pid` / `terminate_pid`, `is_superuser`,
`detach`, `exec_replace`. **Reach for those rather than `libc` or
`std::os::unix` at a call site** — that is the rule the port established, and
the reason it is one small module instead of `#[cfg]` scattered through twenty
files.

A trimmed copy of the module is duplicated into hdmicap, serialcap, ch9329 and
hidrig. That is deliberate and matches how `daemon.rs` is already duplicated:
these are standalone cargo projects with no crate between them. **Keep the five
copies in sync.**

Two semantic differences are worth knowing before you rely on them:

- **Windows has no signals.** Both `Signal::Term` and `Signal::Kill` land on
  `TerminateProcess`, so a Windows daemon never runs its graceful-shutdown path
  (discovery-file removal, the 300 ms grace) and may leave a stale discovery
  file for the next `pid_alive` probe to reap.
- **Windows has no uid.** The runtime namespace uses an FNV hash of `%USERNAME%`
  — stable per user, meaningless on its own, and never used for an
  authorization decision. The 0700-plus-ownership check on the runtime dir
  becomes "exists and is a directory", because the path sits inside the user's
  own profile whose inherited ACL already excludes other non-admin users.

## Testing the platform split

A `#[cfg]` that compiles is not a `#[cfg]` that works, and the tests have to
know the difference. `paniolo doctor` shipped in the first cut of the Windows
port depending on `sh -c` — a binary Windows does not have — and CI was green,
because the doctor tests asserted on the **text** of the generated shell script
rather than running it. A string-shape test passes identically on every
platform; it can only tell you what a command *would* say, never whether it can
launch.

So: **anything that spawns a process, touches the filesystem, or resolves a
name gets a test that executes it.** The ones that matter live in
`cli/src/platform.rs` (`pid_alive` against a real child, `shell_command`
running a command and reading its exit code back, `ensure_private_dir`
creating then revalidating), `cli/src/doctor.rs` (every `Probe` variant run
natively via `run_local`), and `cli/src/daemons.rs` (`find_binary` resolving a
bare name to a real file through `$PATH`).

Two bugs were found by writing exactly those tests, one of them serious:

- **Non-positive pids were unguarded on Unix.** `kill()` reads 0 as "every
  process in my group" and -1 as "every process I may signal", so a zero or
  corrupt pid in a discovery file made `pid_alive` answer *running* and would
  have made `signal_pid(.., Kill)` take down paniolo and the shell that
  launched it. `is_real_pid` now guards every entry point, in all five copies
  of the module, with a test in each so the guard cannot quietly go missing.
- **Opaque lab-file commands ran through `sh -c`.** Power hooks and the hid
  `cmd` are user-written strings that need a shell; `platform::shell_command`
  now supplies `sh` on Unix and `cmd.exe` on Windows.

`doctor` no longer needs a shell locally for its own probes. Probes are a
`Probe` enum with two views — `run_local()` executes natively, `to_posix()`
renders the POSIX script still used for the SSH hop to a Unix control host —
so the local path and the remote path cannot drift apart silently. The one
exception is `Probe::HelperDoctor` (a channel's opt-in helper doctor check):
it runs a lab-file command, so locally it goes through
`platform::shell_command` exactly as the hook itself would.

## Windows packaging

The Windows artifact is a **portable zip**, not an MSI — paniolo is a CLI plus
nine helpers, so the Windows analogue of the Homebrew tap and the `.deb` is a
tarball of binaries, not an installer. (Contrast `~/src/oh-brother`, which needs
WiX + a Burn bundle because it is a GUI app bootstrapping a WebView2 runtime.)

The layout is flat, because Windows has no bin/libexec split to hang helpers
off — the exe's own directory *is* the install prefix:

```
paniolo\
  paniolo.exe          <- the only thing that goes on PATH
  libexec\
    hdmicap.exe  serialcap.exe  netbootd.exe  hidrig.exe  …  winocr.exe
  share\paniolo\skills\
    paniolo\SKILL.md  kvm-puppeting\SKILL.md  control-host\SKILL.md
```

`daemons::exe_relative_dirs` adds `<exe dir>\libexec` on Windows, and
`daemons::find_binary` tries `<name>.exe` before the bare name.
`skills::skills_dirs_for_exe` likewise adds `<exe dir>\share\paniolo\skills`
there — the same `share/paniolo/skills` every other install keeps under its
prefix, with the exe's directory as the prefix. Through v0.3.0 the zip staged
only exes and the CLI only looked one level up, so `paniolo skill` on a
Windows install listed nothing; the first `/release-train` dry run
(2026-09-10) caught it because that was the first time anything ran the zip.
`package-windows` now stages the skills and smoke-tests the expanded zip
(`--help`, `paniolo skill` from outside the tree, every helper's `--help`,
winocr decoding a tiny PNG) the way the Linux and macOS jobs already did
theirs. `paniolo setup` on Windows does nothing but verify the helper layout:
there is no setuid bit, no `dialout` group, and no OCR helper to build.

Signing reuses oh-brother's Azure Artifact Signing setup — same account, same
OIDC identity, same action. Three differences from that pipeline: the **exes are
signed before zipping** (a zip carries no Authenticode signature, and SmartScreen
judges the exe), it is **ten files rather than three** (one glob, no Burn engine
detach/reattach — that dance is MSI-bundle-specific), and the RFC3161 timestamp
is asserted in a verify step for the same reason as there: Artifact Signing
certificates live 72 hours, so an untimestamped signature passes on release day
and dies in the field three days later.

`release.yml` gains `package-windows` (build → sign → verify → zip) and a
`winget` job. Both no-op until their credentials exist: signing skips while
`vars.AZURE_SIGNING_ACCOUNT` is unset, winget while `secrets.WINGET_TOKEN` is.
**The first winget version must be submitted by hand** (`wingetcreate` or
`komac`) — the action only updates a package that already exists.

## Developing against Windows

The Windows bench host (`lab-host-1` in examples; its real name, address and
checkout path are private infrastructure and live outside this repo) is reached over SSH, which lands
in PowerShell 7. Note `$HOME` there is the Windows account's `C:\Users\…`,
a different account name than on the Mac, and the repo is cloned under it.
`BRIK_HOST=<ssh host> BRIK_DEST=<checkout, forward slashes>
scripts/sync-brik.sh <crate>…` pushes sources there; both variables are
required, with no defaults, and the script excludes `target/`, which is
gigabytes and stalls the transfer. The MSVC linker resolves without a
Developer Prompt.

A local `cargo check --target x86_64-pc-windows-msvc` catches most `#[cfg]`
mistakes without leaving macOS, but **only for crates with no C dependency** —
anything pulling `ring` (i.e. anything with TLS) fails at its build script for
want of Windows headers. Those need the real host.

Per-subsystem behavior:

- **OCR backend.**
  - *macOS:* Apple Vision (`visionocr.swift`, compiled by `paniolo setup`).
  - *Linux:* Tesseract (`ocr/linuxocr`, copied by `paniolo setup`; requires the
    `tesseract-ocr` system package).
  - *Windows:* none. A port would supply a third binary behind the same
    stdin-PNG → stdout-text interface that both current backends expose via
    `PANIOLO_VISIONOCR`.
- **Netboot privileges.**
  - *macOS:* 14+ allows DHCP (port 67) and TFTP (port 69) rootless; `sudo` is
    used only for interface config (`ifconfig`).
  - *Linux:* both ports require root, so `paniolo netboot start` auto-prepends
    `sudo` when spawning `netbootd`, and `ip addr add` uses sudo too. netbootd
    drops back to the invoking user (`SUDO_UID`/`SUDO_GID`) as soon as its
    listeners are bound, so its own later `sudo ip neigh` / `sudo ip addr`
    calls run unprivileged too. With passwordless sudoers this is transparent;
    otherwise sudo prompts (and, inside the daemon, fails).
  - *Windows:* neither port is privileged — Windows has no low-port
    restriction — so `netbootd` is spawned directly with no `sudo` prefix.
- **Interface management** (`cli/src/netif.rs`, `netif::configure_interface()` /
  `restore_interface()`).
  - *macOS:* `networksetup -setmanual <service> <ip> <mask> <router>` (router
    = the host IP; the four-argument form is required) + `ifconfig`, which is
    what makes the address live — a `networksetup` failure is a warning.
  - *Linux:* iproute2 — `ip addr replace` first, then `ip addr del` of any
    other IPv4 address (parsed from `ip -4 -o addr show dev <iface>`), then
    `ip link set up`; a failed assignment never leaves the link addressless.
    `restore_interface` still flushes with `ip addr flush dev <iface>`.
  - *Windows:* not implemented.
  - Every mode change (`mode_*`, `down_hard`) and `netboot::start` refuse the
    interface carrying the system default route (`is_primary_interface`; the
    pure decision is `is_default_route_interface`, unit-tested).
- **ARP pinning** (netbootd).
  - *macOS:* `arp -s`.
  - *Linux:* `ip neigh replace ... nud permanent`.
  - *Windows:* not implemented.
- **Raw-frame sender** (netbootd).
  - *macOS:* BPF sender over `/dev/bpf*` ioctls; netbootd receives the `/dev/bpf`
    descriptor from the setuid `netbootd-bpf-helper` and stays unprivileged (see
    the **netbootd** section).
  - *Linux:* no BPF path — the ioctls don't exist; the server falls back to a
    normal `sendto()` with retry.
  - *Windows:* not implemented.
- **Interface listing** (`list_usb_ethernet_interfaces()`).
  - *macOS:* `networksetup`.
  - *Linux:* sysfs — `/sys/class/net/` (type, carrier).
  - *Windows:* not implemented.
- **Serial device paths** (`serial::list_devices()`, `cli/src/serial.rs`).
  - *macOS:* scans `/dev` for `tty.usbserial-*` and `tty.usbmodem*` nodes.
  - *Linux:* one entry per physical port, named by its stable `/dev/serial`
    symlink — `by-id` preferred (names the adapter; what lab files typically
    use), `by-path` as the fallback (port-derived; the only stable name for
    adapters without a serial number), picked by `preferred_alias()`; raw
    `/dev/ttyUSB*`/`ttyACM*` only if no symlinks exist. Store a stable symlink
    path in target configs so serial interfaces survive USB adapter
    re-enumeration. The serialcap `--interface` parser accepts these paths
    (colons in a by-path name are not confused with the optional `:SENSE`
    suffix because only the known signal names `cts`, `dsr`, `dcd`, `ri` are
    treated as the sense suffix).
  - *Windows:* `serialport::available_ports()` returns the OS's `COM<n>`
    names. There is no by-path analogue, so a lab file pinned to `COM7` is
    less stable than a Linux by-id path — the number can move when an adapter
    is re-enumerated.
- **hdmicap capture + build deps.**
  - *macOS:* our own AVFoundation layer (`hdmicap/src/capture_avf.m`); no extra
    system packages beyond the Xcode toolchain.
  - *Linux:* V4L2 with a raw-MJPEG tee. Building requires `build-essential
    pkg-config libclang-dev clang` (V4L2 bindgen via `v4l2-sys-mit`) plus `cmake
    nasm` (the `turbojpeg` crate builds a vendored libjpeg-turbo — Debian's
    system libturbojpeg is too old for its pkg-config path, and the crate's
    `require-simd` default makes nasm mandatory on x86-64). `make install` fails
    early with a hint if any are missing (`check-deps` in the Makefile);
    `paniolo setup` prints a reminder. `CaptureStream::next` hands back the
    whole mmap'd arena buffer, sized once at the format's worst-case
    `sizeimage` — for MJPG that is the *uncompressed* frame, 4,147,200 bytes at
    1080p — so `frame()` must trim it to the `Metadata.bytesused` the driver
    actually filled (the V4L2 twin of the Media Foundation stride lesson
    below). Skipping the trim is invisible to every consumer, because JPEG
    decoders stop at EOI: the preview rendered correctly while shipping a
    216 KB frame inside a 4.1 MB part, 94.8% zeros.
  - *Windows:* our own Media Foundation layer (`hdmicap/src/capture_mf.rs`),
    delivering NV12 so it reuses the macOS pixel path. It enumerates the
    device's *native* media types and selects one explicitly — the Windows form
    of the AVFoundation lesson below, and the reason an Openterface captures at
    its real 3840x2160 instead of a rescaled 1080p. MJPEG-only devices are not
    yet supported. Media Foundation samples can pad each row to a stride wider
    than the frame's visible width, so `frame()` gets the real per-sample pitch
    from `IMF2DBuffer::Lock2D` when the buffer supports it, falling back to the
    selected media type's `MF_MT_DEFAULT_STRIDE` (read once in `open()`) and
    finally to `width`; `pixel::compact_nv12` then strips the padding into
    tight Y/CbCr planes before anything downstream indexes by `width`. A
    negative pitch (bottom-up) is treated as an explicit unsupported case.

