// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! `netif mode off` and `netif down-hard` report address-release failures.
//!
//! Runs the built binary against fake `sudo`, `ip` (Linux) or `ifconfig`
//! (macOS) commands first on PATH. The fakes keep the interface's addresses in
//! a state file, log every argv, and fail per environment variable, so each
//! case is a real execution of the removal-and-probe path.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const IFACE: &str = "ethtest0";
const LAB: &str = "[targets.nuc]\n[targets.nuc.netboot]\ninterface = \"ethtest0\"\n";
/// panio's default netboot host IP (`DEFAULT_HOST_IP`).
const V4: &str = "192.168.99.1";
const V6: &str = "fe80::1";

const SUDO: &str = "#!/bin/sh\nexec \"$@\"\n";

/// Fake Linux `ip`: argv logged; state file `addrs` holds CIDRs, one per line.
const FAKE_IP: &str = r#"#!/bin/sh
echo "ip $*" >> "$FAKE_DIR/argv.log"
state="$FAKE_DIR/addrs"
case "$*" in
  "route show default")
    echo "default via 192.0.2.1 dev ethprimary"; exit 0 ;;
  "-brief addr show dev "*)
    case "$FAKE_PROBE" in
      missing) echo "Device \"$5\" does not exist." >&2; exit 1 ;;
      broken) echo "Cannot open netlink socket: Operation not permitted" >&2; exit 1 ;;
    esac
    printf '%s UP' "$5"
    while read a; do printf ' %s' "$a"; done < "$state"
    echo
    exit 0 ;;
  "-6 addr del "*)
    if [ "$FAKE_FAIL_V6" = 1 ]; then
      echo "RTNETLINK answers: Cannot assign requested address" >&2; exit 2
    fi
    grep -v -x -F "$4" "$state" > "$state.new"; mv "$state.new" "$state"; exit 0 ;;
  "addr del "*)
    if [ "$FAKE_FAIL_V4" = 1 ]; then
      echo "RTNETLINK answers: Cannot assign requested address" >&2; exit 2
    fi
    grep -v -x -F "$3" "$state" > "$state.new"; mv "$state.new" "$state"; exit 0 ;;
esac
exit 0
"#;

const FAKE_ETHTOOL: &str = "#!/bin/sh\necho \"ethtool $*\" >> \"$FAKE_DIR/argv.log\"\nexit 0\n";

/// Fake macOS `ifconfig`: state file `addrs` holds bare addresses.
const FAKE_IFCONFIG: &str = r#"#!/bin/sh
echo "ifconfig $*" >> "$FAKE_DIR/argv.log"
state="$FAKE_DIR/addrs"
if [ $# -eq 1 ]; then
  case "$FAKE_PROBE" in
    missing) echo "ifconfig: interface $1 does not exist" >&2; exit 1 ;;
    broken) echo "ifconfig: socket: Operation not permitted" >&2; exit 1 ;;
  esac
  echo "$1: flags=8863<UP,BROADCAST> mtu 1500"
  while read a; do
    case "$a" in
      *:*) printf '\tinet6 %s%%%s prefixlen 64 scopeid 0x4\n' "$a" "$1" ;;
      *) printf '\tinet %s netmask 0xffffff00\n' "$a" ;;
    esac
  done < "$state"
  echo "	status: active"
  exit 0
fi
case "$2" in
  inet6)
    if [ "$FAKE_FAIL_V6" = 1 ]; then
      echo "ifconfig: ioctl (SIOCDIFADDR): Can't assign requested address" >&2; exit 1
    fi
    grep -v -x -F "$3" "$state" > "$state.new"; mv "$state.new" "$state"; exit 0 ;;
  inet)
    if [ "$FAKE_FAIL_V4" = 1 ]; then
      echo "ifconfig: ioctl (SIOCDIFADDR): Can't assign requested address" >&2; exit 1
    fi
    grep -v -x -F "$3" "$state" > "$state.new"; mv "$state.new" "$state"; exit 0 ;;
esac
exit 0
"#;

const FAKE_ROUTE: &str = "#!/bin/sh\necho '   interface: enprimary'\nexit 0\n";
const FAKE_NETWORKSETUP: &str = "#!/bin/sh\nexit 0\n";

struct Fixture {
    dir: PathBuf,
}

/// What the fake interface does.
#[derive(Default, Clone, Copy)]
struct Scenario {
    held_v4: bool,
    held_v6: bool,
    fail_v4: bool,
    fail_v6: bool,
    probe: &'static str,
}

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    fn new(name: &str, sc: Scenario) -> Fixture {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("netif_off")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.join("run")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::write(dir.join("lab.toml"), LAB).unwrap();
        write_exec(&bin.join("sudo"), SUDO);
        let mac = cfg!(target_os = "macos");
        if mac {
            write_exec(&bin.join("ifconfig"), FAKE_IFCONFIG);
            write_exec(&bin.join("route"), FAKE_ROUTE);
            write_exec(&bin.join("networksetup"), FAKE_NETWORKSETUP);
        } else {
            write_exec(&bin.join("ip"), FAKE_IP);
            write_exec(&bin.join("ethtool"), FAKE_ETHTOOL);
        }
        let (v4, v6) = if mac {
            (V4.to_string(), V6.to_string())
        } else {
            (format!("{V4}/24"), format!("{V6}/64"))
        };
        let mut addrs = String::new();
        if sc.held_v4 {
            addrs.push_str(&format!("{v4}\n"));
        }
        if sc.held_v6 {
            addrs.push_str(&format!("{v6}\n"));
        }
        std::fs::write(dir.join("addrs"), addrs).unwrap();
        std::fs::write(dir.join("argv.log"), "").unwrap();
        Fixture { dir }
    }

    fn panio(&self, sc: Scenario, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_panio"))
            .arg("--lab")
            .arg(self.dir.join("lab.toml"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.dir.join("home"))
            .env("PANIO_RUNTIME_BASE", self.dir.join("run"))
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env_remove("PANIO_LAB")
            .env_remove("PANIO_JSON_ERRORS")
            .env("FAKE_DIR", &self.dir)
            .env("FAKE_PROBE", sc.probe)
            .env("FAKE_FAIL_V4", if sc.fail_v4 { "1" } else { "0" })
            .env("FAKE_FAIL_V6", if sc.fail_v6 { "1" } else { "0" })
            .output()
            .unwrap()
    }

    fn argv_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("argv.log")).unwrap()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Whether the log holds the removal command for the IPv6 (`v6`) or IPv4
/// address.
fn removal_logged(log: &str, v6: bool) -> bool {
    let prefix = match (cfg!(target_os = "macos"), v6) {
        (true, true) => format!("ifconfig {IFACE} inet6 {V6} -alias"),
        (true, false) => format!("ifconfig {IFACE} inet {V4} -alias"),
        (false, true) => format!("ip -6 addr del {V6}/64 dev {IFACE}"),
        (false, false) => format!("ip addr del {V4}/24 dev {IFACE}"),
    };
    log.lines().any(|l| l == prefix)
}

fn admin_down_logged(log: &str) -> bool {
    if cfg!(target_os = "macos") {
        log.lines().any(|l| l == format!("ifconfig {IFACE} down"))
    } else {
        log.lines()
            .any(|l| l == format!("ip link set {IFACE} down"))
    }
}

#[test]
fn mode_off_succeeds_when_the_removals_work() {
    let sc = Scenario {
        held_v4: true,
        held_v6: true,
        ..Default::default()
    };
    let fx = Fixture::new("clean", sc);
    let out = fx.panio(sc, &["netif", "mode", "off", "nuc"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(removal_logged(&fx.argv_log(), true));
    assert!(removal_logged(&fx.argv_log(), false));
}

/// The old code discarded the removal result and exited 0.
#[test]
fn mode_off_fails_when_a_failed_removal_leaves_the_address() {
    let sc = Scenario {
        held_v4: true,
        fail_v4: true,
        ..Default::default()
    };
    let fx = Fixture::new("still_held", sc);
    let out = fx.panio(sc, &["netif", "mode", "off", "nuc"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains(V4), "{err}");
    assert!(err.contains(IFACE), "{err}");
    assert!(err.contains("assign requested address"), "{err}");
}

#[test]
fn mode_off_accepts_a_failed_removal_when_the_address_is_absent() {
    let sc = Scenario {
        fail_v4: true,
        fail_v6: true,
        ..Default::default()
    };
    let fx = Fixture::new("already_absent", sc);
    let out = fx.panio(sc, &["netif", "mode", "off", "nuc"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn mode_off_succeeds_when_the_interface_does_not_exist() {
    let sc = Scenario {
        held_v4: true,
        held_v6: true,
        fail_v4: true,
        fail_v6: true,
        probe: "missing",
    };
    let fx = Fixture::new("missing_iface", sc);
    let out = fx.panio(sc, &["netif", "mode", "off", "nuc"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

/// A probe that fails for any reason but "does not exist" proves nothing.
#[test]
fn mode_off_fails_when_the_probe_itself_fails() {
    let sc = Scenario {
        held_v4: true,
        held_v6: true,
        fail_v4: true,
        fail_v6: true,
        probe: "broken",
    };
    let fx = Fixture::new("broken_probe", sc);
    let out = fx.panio(sc, &["netif", "mode", "off", "nuc"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("could not confirm"), "{err}");
    assert!(err.contains(V4) && err.contains(V6), "{err}");
}

#[test]
fn mode_off_still_removes_the_other_address_when_one_removal_fails() {
    let sc = Scenario {
        held_v4: true,
        held_v6: true,
        fail_v6: true,
        ..Default::default()
    };
    let fx = Fixture::new("one_fails", sc);
    let out = fx.panio(sc, &["netif", "mode", "off", "nuc"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains(V6), "{err}");
    assert!(!err.contains(V4), "v4 was released: {err}");
    let log = fx.argv_log();
    assert!(removal_logged(&log, true), "{log}");
    assert!(removal_logged(&log, false), "{log}");
}

#[test]
fn down_hard_still_admin_downs_when_a_removal_fails() {
    let sc = Scenario {
        held_v4: true,
        held_v6: true,
        fail_v4: true,
        ..Default::default()
    };
    let fx = Fixture::new("down_hard", sc);
    let out = fx.panio(sc, &["netif", "down-hard", "nuc"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains(V4), "{}", stderr(&out));
    let log = fx.argv_log();
    assert!(admin_down_logged(&log), "{log}");
    if !cfg!(target_os = "macos") {
        assert!(log.contains("ethtool -s ethtest0 wol d"), "{log}");
    }
}
