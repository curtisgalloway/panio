<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Platform support

Per-platform support status and behavior, moved from the root AGENTS.md (which keeps a summary and the portable-code rules).

Panio runs on three host platforms. (Network RFB video sources are
platform-independent Rust and build and test on all three in CI; the
JetKVM-through-RFB path was hardware-verified from a macOS control host only.)

| Platform | Status | CI | Release artifacts |
| --- | --- | --- | --- |
| macOS (Apple Silicon) | Supported | `macos` job in `ci.yml` | Homebrew tap (universal tarball, poured as an `all` bottle) |
| Linux (Debian/Ubuntu) | Supported | the `ubuntu-latest` jobs in `ci.yml` | `.deb`, tarball and Homebrew bottle from `release.yml` |
| Windows (x86_64-msvc) | Supported; power and video hardware-verified | `windows` job in `ci.yml` | portable zip + winget |

All ten crates build, lint clean under `clippy -D warnings`, and pass their unit
tests on all three. The `windows` CI job runs the same fmt/clippy/test triple as
the Linux jobs, since a Unix-only CI cannot tell whether the Windows `#[cfg]` arm
still compiles.

**Hardware-verified on Windows** (2026-08-28, a Shelly Plug and an Openterface
KVM-GO):

- **power** — `shellyplug` on/off/cycle, each confirmed by read-back.
- **video** — `hdmicap` enumerates the capture device by symbolic-link id and
  captures native 3840x2160 through the Media Foundation backend; daemon start,
  `shot` and `stop` work.
- **hid** — `ch9329` over a COM port reports `target_connected=true`, and its mouse
  moves wake the attached machine. Keystroke delivery is confirmed by that wake,
  not by the CH9329 LED read-back, which stayed `false`.

**Not verified on Windows:**

- **netbootd** falls back to `send_to`: the `/dev/bpf` raw-frame sender and
  `SCM_RIGHTS` fd handoff are Unix-only, as on Linux. DHCP/TFTP netboot against a
  real target is untested, and `netif` has no Windows implementation.
- **hidrig has no console bridge.** The PTY the `serial` channel's `device =` points
  at has no Windows analogue (ConPTY exposes no filesystem node another process can
  open). HID and control are unaffected.
- **OCR is absent.** `Windows.Media.Ocr` is the in-box candidate, and the `windows`
  crate is already a hdmicap dependency.
- **`panio doctor` cannot probe a Windows control host.** Probes run natively
  locally, but the SSH path renders them as POSIX shell (`sh -c 'test -e …'`), which
  PowerShell cannot run. It is the one verb that speaks shell over the wire rather
  than pure panio; the fix is to have the remote side run `panio doctor`
  (not done).

## Dispatching to a Windows control host

`panio <cmd> -t <target>` works when the target's host is Windows. Dispatch
ships a lab slice, re-execs `panio` against it, and cleans the slice up; only the
re-exec was ever shell-safe, which matters wherever the remote shell is not ours to
choose.

- **Shipping and cleanup go over SFTP** (`ssh::sftp_put` / `ssh::sftp_rm`), not
  shell. The old `mktemp … && cat > "$f"` is not a command in PowerShell, and `rm -f`
  fails there ("parameter 'f' is ambiguous"). SFTP behaves identically whatever the
  far side runs and reuses the session's ControlMaster.
- **The re-exec is fine as is.** `remote_command` quotes arguments POSIX-style, and
  PowerShell reads `'C:\Users\x\lab.toml'` as the same literal. The `VAR='v' cmd`
  env-prefix form is *not* portable, but dispatch passes no env.
- **The slice path is deliberately relative** (`.panio-lab-<pid>-<ns>.toml`).
  SFTP reports a Windows home as `/C:/Users/name`, which no native Windows program
  can open, so an absolute path from `pwd` is unusable as `--lab`. Both platforms
  start an SSH session in the home directory (SFTP's default), so a relative name
  resolves on both.

Verified end to end against the Windows bench host: `hid send … info` returned real
CH9329 state, `hid send … move` drove the KVM, `video show` read the channel, and
the slice was cleaned up. Unix dispatch is unchanged.

## Writing portable code here

The POSIX primitives panio depends on live behind `cli/src/platform.rs`, one
implementation per platform with a documented contract each: `current_uid`,
`runtime_root`, `ensure_private_dir`, `make_executable`, `pid_alive`, `signal_pid` /
`try_signal_pid` / `terminate_pid`, `is_superuser`, `detach`, `exec_replace`.
**Use those rather than `libc` or `std::os::unix` at a call site**, so `#[cfg]`
stays out of the rest of the code.

A trimmed copy of the module is duplicated into hdmicap, serialcap, ch9329 and
hidrig (standalone cargo projects with no crate between them, like `daemon.rs`).
**Keep the five copies in sync.**

- **Windows has no signals.** `Signal::Term` and `Signal::Kill` both land on
  `TerminateProcess`, so a Windows daemon never runs graceful shutdown
  (discovery-file removal, the 300 ms grace) and may leave a stale discovery file
  for the next `pid_alive` probe to reap.
- **Windows has no uid.** The runtime namespace uses an FNV hash of `%USERNAME%`:
  stable per user, meaningless on its own, never used for authorization. The
  0700-plus-ownership check on the runtime dir becomes "exists and is a directory",
  because the path is inside the user's profile, whose inherited ACL already
  excludes other non-admin users.

## Testing the platform split

A `#[cfg]` that compiles is not one that works. The first Windows `doctor` depended
on `sh -c` (absent on Windows) with CI green, because its tests asserted on the
**text** of the generated shell script; a string-shape test passes identically on
every platform and never shows whether a command can launch.

**Anything that spawns a process, touches the filesystem, or resolves a name gets a
test that executes it.** Examples: `cli/src/platform.rs` (`pid_alive` against a real
child, `shell_command` running a command and reading its exit code, 
`ensure_private_dir` creating then revalidating), `cli/src/doctor.rs` (every
`Probe` variant run natively via `run_local`), `cli/src/daemons.rs` (`find_binary`
resolving a bare name through `$PATH`).

Two bugs found that way:

- **Non-positive pids were unguarded on Unix.** `kill()` reads 0 as "every process
  in my group" and -1 as "every process I may signal", so a zero or corrupt pid in a
  discovery file made `pid_alive` answer *running* and would have made
  `signal_pid(.., Kill)` kill panio and its launching shell. `is_real_pid` now
  guards every entry point in all five copies, with a test in each.
- **Opaque lab-file commands ran through `sh -c`.** Power hooks and the hid `cmd`
  are user-written strings that need a shell; `platform::shell_command` supplies
  `sh` on Unix and `cmd.exe` on Windows.

`doctor` probes are a `Probe` enum with two views, `run_local()` (native) and
`to_posix()` (the POSIX script for the SSH hop to a Unix host), so the local and
remote paths cannot drift. The exception is `Probe::HelperDoctor` (a channel's
opt-in helper doctor check): it runs a lab-file command, so locally it goes through
`platform::shell_command` like the hook itself.

## Windows packaging

The Windows artifact is a **portable zip**, not an MSI: panio is a CLI plus nine
helpers, so the analogue of the Homebrew tap and `.deb` is a tarball of binaries.
The layout is flat, since Windows has no bin/libexec split; the exe's own directory
is the install prefix:

```
panio\
  panio.exe          <- the only thing that goes on PATH
  libexec\
    hdmicap.exe  serialcap.exe  netbootd.exe  hidrig.exe  …  winocr.exe
  share\panio\skills\
    panio\SKILL.md  kvm-puppeting\SKILL.md  control-host\SKILL.md
```

`daemons::exe_relative_dirs` adds `<exe dir>\libexec` on Windows, and
`daemons::find_binary` tries `<name>.exe` before the bare name.
`skills::skills_dirs_for_exe` adds `<exe dir>\share\panio\skills`. (Through v0.3.0
the zip staged only exes and the CLI looked one level up, so `panio skill` listed
nothing; the first `/release-train` dry run caught it.) `package-windows` stages the
skills and smoke-tests the expanded zip: `--help`, `panio skill` from outside the
tree, every helper's `--help`, winocr decoding a tiny PNG. `panio setup` on
Windows only verifies the helper layout: no setuid bit, no `dialout` group, no OCR
helper to build.

Signing reuses the Azure Artifact Signing setup (same account, OIDC identity and
action) from another project, with three differences: **exes are signed before
zipping** (a zip carries no Authenticode signature, and SmartScreen judges the exe);
it is **ten files rather than three** (one glob, no Burn engine detach/reattach);
and the RFC3161 timestamp is asserted in a verify step, because Artifact Signing
certificates live 72 hours and an untimestamped signature dies in the field three
days after release.

`release.yml` has `package-windows` (build → sign → verify → zip) and a `winget`
job. Both no-op until credentials exist: signing skips while
`vars.AZURE_SIGNING_ACCOUNT` is unset, winget while `secrets.WINGET_TOKEN` is.
**The first winget version must be submitted by hand** (`wingetcreate` or `komac`);
the action only updates an existing package.

## Developing against Windows

The Windows bench host (`lab-host-1` in examples; its real name, address and
checkout path are private and live outside this repo) is reached over SSH, which
lands in PowerShell 7. `$HOME` there is the Windows account's `C:\Users\…`, a
different account name than on the Mac, and the repo is cloned under it.
`BRIK_HOST=<ssh host> BRIK_DEST=<checkout, forward slashes>
scripts/sync-brik.sh <crate>…` pushes sources there; both variables are required,
with no defaults, and the script excludes `target/` (gigabytes; stalls the
transfer). The MSVC linker resolves without a Developer Prompt.

A local `cargo check --target x86_64-pc-windows-msvc` catches most `#[cfg]`
mistakes without leaving macOS, but **only for crates with no C dependency**:
anything pulling `ring` (anything with TLS) fails at its build script for want of
Windows headers. Those need the real host.

Per-subsystem behavior:

- **OCR backend.**
  - *macOS:* Apple Vision (`visionocr.swift`, compiled by `panio setup`).
  - *Linux:* Tesseract (`ocr/linuxocr`, copied by `panio setup`; needs the
    `tesseract-ocr` package).
  - *Windows:* none. A port would supply a third binary behind the same stdin-PNG →
    stdout-text interface, exposed via `PANIO_VISIONOCR`.
- **Netboot privileges.**
  - *macOS:* 14+ allows DHCP (67) and TFTP (69) rootless; `sudo` only for interface
    config (`ifconfig`).
  - *Linux:* both ports need root, so `panio netboot start` prepends `sudo` when
    spawning `netbootd`, and `ip addr add` uses sudo. netbootd drops to the invoking
    user (`SUDO_UID`/`SUDO_GID`) once its listeners are bound, so its later `sudo ip
    neigh` / `sudo ip addr` calls run unprivileged. Transparent with passwordless
    sudoers; otherwise sudo prompts (and fails inside the daemon).
  - *Windows:* no privileged ports, so `netbootd` is spawned directly, no `sudo`.
- **Interface management** (`cli/src/netif.rs`, `netif::configure_interface()` /
  `restore_interface()`).
  - *macOS:* `networksetup -setmanual <service> <ip> <mask> <router>` (router = the
    host IP; the four-argument form is required) + `ifconfig`, which makes the
    address live; a `networksetup` failure is a warning.
  - *Linux:* iproute2: `ip addr replace`, then `ip addr del` of any other IPv4
    address (from `ip -4 -o addr show dev <iface>`), then `ip link set up`; a failed
    assignment never leaves the link addressless. `restore_interface` flushes with
    `ip addr flush dev <iface>`.
  - *Windows:* not implemented.
  - Every mode change (`mode_*`, `down_hard`) and `netboot::start` refuse the
    interface carrying the default route (`is_primary_interface`; pure decision
    `is_default_route_interface`, unit-tested).
- **ARP pinning** (netbootd): macOS `arp -s`; Linux `ip neigh replace ... nud
  permanent`; Windows not implemented.
- **Raw-frame sender** (netbootd).
  - *macOS:* BPF over `/dev/bpf*`; netbootd gets the descriptor from the setuid
    `netbootd-bpf-helper` and stays unprivileged (see the **netbootd** section).
  - *Linux:* no BPF; falls back to `sendto()` with retry.
  - *Windows:* not implemented.
- **Interface listing** (`list_usb_ethernet_interfaces()`): macOS `networksetup`;
  Linux sysfs `/sys/class/net/` (type, carrier); Windows not implemented.
- **Serial device paths** (`serial::list_devices()`, `cli/src/serial.rs`).
  - *macOS:* scans `/dev` for `tty.usbserial-*` and `tty.usbmodem*`.
  - *Linux:* one entry per physical port, named by its stable `/dev/serial`
    symlink: `by-id` preferred (names the adapter), `by-path` as fallback (the only
    stable name for adapters without a serial number), chosen by
    `preferred_alias()`; raw `/dev/ttyUSB*`/`ttyACM*` only if no symlinks exist.
    Store a stable symlink path in target configs to survive re-enumeration. The
    serialcap `--interface` parser accepts these paths; colons in a by-path name
    are not mistaken for the `:SENSE` suffix because only `cts`, `dsr`, `dcd`, `ri`
    count as sense names.
  - *Windows:* `serialport::available_ports()` returns `COM<n>`. No by-path
    analogue, so a lab file pinned to `COM7` is less stable than a Linux by-id path;
    the number can move on re-enumeration.
- **hdmicap capture + build deps.**
  - *macOS:* our AVFoundation layer (`hdmicap/src/capture_avf.m`); no packages
    beyond the Xcode toolchain.
  - *Linux:* V4L2 with a raw-MJPEG tee. Building needs `build-essential pkg-config
    libclang-dev clang` (V4L2 bindgen via `v4l2-sys-mit`) plus `cmake nasm` (the
    `turbojpeg` crate builds a vendored libjpeg-turbo: Debian's libturbojpeg is too
    old for its pkg-config path, and its `require-simd` default makes nasm
    mandatory on x86-64). `make install` fails early with a hint if any are missing
    (`check-deps` in the Makefile); `panio setup` prints a reminder.
    `CaptureStream::next` returns the whole mmap'd buffer, sized once at the
    format's worst-case `sizeimage` (for MJPG the *uncompressed* frame, 4,147,200
    bytes at 1080p), so `frame()` must trim it to the `Metadata.bytesused` the
    driver filled. Skipping the trim is invisible to consumers (JPEG decoders stop
    at EOI) but ships a 216 KB frame inside a 4.1 MB part, 94.8% zeros.
  - *Windows:* our Media Foundation layer (`hdmicap/src/capture_mf.rs`), delivering
    NV12 so it reuses the macOS pixel path. It enumerates the device's *native*
    media types and selects one explicitly (as with AVFoundation), which is why an
    Openterface captures at 3840x2160 instead of a rescaled 1080p. MJPEG-only
    devices are not supported yet. Samples can pad rows to a stride wider than the
    visible width, so `frame()` takes the real pitch from `IMF2DBuffer::Lock2D`
    when available, else the media type's `MF_MT_DEFAULT_STRIDE` (read once in
    `open()`), else `width`; `pixel::compact_nv12` then strips the padding into
    tight Y/CbCr planes before anything indexes by `width`. A negative pitch
    (bottom-up) is explicitly unsupported.
