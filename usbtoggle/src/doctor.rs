// Copyright 2026 Curtis Galloway
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `doctor`: the read-only report panio reads (docs/dev/adding-power-helpers.md,
//! "Doctor check"). It never runs the close or open command.

use crate::logic::{describe_code, parse_side, Config, Probe, Runner, Side, Source};
use crate::sys::DeviceSpec;

#[derive(Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: &'static str,
    pub detail: String,
}

fn check(name: &'static str, status: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

pub fn checks(
    cfg: &Config,
    device: Option<&DeviceSpec>,
    runner: &mut dyn Runner,
    probe: &mut dyn Probe,
) -> Vec<Check> {
    let mut out = Vec::new();
    let mut side = None;

    match (&cfg.source, device) {
        (Source::StateCmd(cmd), _) => match runner.capture(cmd) {
            Ok(cap) if cap.code == Some(0) => match parse_side(&cap.stdout) {
                Ok(s) => {
                    out.push(check("state-cmd", "ok", format!("prints {}", s.as_str())));
                    side = Some(s);
                }
                Err(e) => out.push(check("state-cmd", "fail", format!("{e:#}"))),
            },
            Ok(cap) => out.push(check(
                "state-cmd",
                "fail",
                format!("failed ({})", describe_code(cap.code)),
            )),
            Err(e) => out.push(check("state-cmd", "fail", format!("{e:#}"))),
        },
        (Source::Device, Some(spec)) => match probe.present() {
            Ok(true) => {
                out.push(check("probe", "ok", "USB enumeration works"));
                out.push(check(
                    "device",
                    "ok",
                    format!("{spec} is present on this host (switch is on the host)"),
                ));
                side = Some(Side::Host);
            }
            Ok(false) => {
                out.push(check("probe", "ok", "USB enumeration works"));
                let detail = format!(
                    "{spec} is absent from this host: the switch is on the target, or the \
                     device is unplugged"
                );
                out.push(check("device", "warn", detail));
            }
            Err(e) => out.push(check("probe", "fail", format!("{e:#}"))),
        },
        (Source::Device, None) => out.push(check(
            "position",
            "fail",
            "neither --device nor --state-cmd given; the position cannot be read",
        )),
    }

    if let Some(cmd) = &cfg.target_check {
        match runner.capture(cmd) {
            Ok(cap) if cap.code == Some(0) => out.push(check(
                "target-check",
                "ok",
                "exit 0: the target sees the device",
            )),
            Ok(cap) => {
                let why = format!(
                    "{}: the target does not see the device",
                    describe_code(cap.code)
                );
                if side == Some(Side::Host) {
                    out.push(check(
                        "target-check",
                        "ok",
                        format!("{why} (expected while the switch is on the host)"),
                    ));
                } else {
                    out.push(check("target-check", "warn", why));
                }
            }
            Err(e) => out.push(check("target-check", "fail", format!("{e:#}"))),
        }
    }

    let missing: Vec<&str> = [
        ("--close-cmd", cfg.close_cmd.is_none()),
        ("--open-cmd", cfg.open_cmd.is_none()),
    ]
    .iter()
    .filter(|(_, m)| *m)
    .map(|(n, _)| *n)
    .collect();
    if missing.is_empty() {
        out.push(check(
            "buttons",
            "ok",
            "--close-cmd and --open-cmd are set (doctor does not run them)",
        ));
    } else {
        out.push(check(
            "buttons",
            "warn",
            format!(
                "{} not set: `usb host|target` and `press` will refuse",
                missing.join(" and ")
            ),
        ));
    }
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn to_json(checks: &[Check]) -> String {
    let items: Vec<String> = checks
        .iter()
        .map(|c| {
            format!(
                "{{\"name\": {}, \"status\": {}, \"detail\": {}}}",
                json_string(c.name),
                json_string(c.status),
                json_string(&c.detail)
            )
        })
        .collect();
    format!("{{\"checks\": [{}]}}", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::fakes::{FakeProbe, FakeRunner};
    use crate::logic::Captured;
    use std::time::Duration;

    fn cfg() -> Config {
        Config {
            close_cmd: Some("close".into()),
            open_cmd: Some("open".into()),
            hold: Duration::ZERO,
            settle: Duration::ZERO,
            poll: Duration::ZERO,
            source: Source::Device,
            target_check: None,
        }
    }

    fn spec() -> DeviceSpec {
        "046d:c08b".parse().unwrap()
    }

    #[test]
    fn doctor_never_runs_the_button_commands() {
        let mut r = FakeRunner::default();
        let mut p = FakeProbe::new(&[true]);
        let cs = checks(&cfg(), Some(&spec()), &mut r, &mut p);
        assert!(r.log.is_empty(), "{:?}", r.log);
        assert!(cs.iter().all(|c| c.status != "fail"), "{cs:?}");
        assert!(cs.iter().any(|c| c.name == "device" && c.status == "ok"));
    }

    #[test]
    fn absent_device_is_a_warning_and_target_check_runs() {
        let mut cfg = cfg();
        cfg.target_check = Some("check".into());
        let mut r = FakeRunner::default();
        r.captures.push((
            "check".into(),
            Captured {
                code: Some(0),
                stdout: String::new(),
            },
        ));
        let mut p = FakeProbe::new(&[false]);
        let cs = checks(&cfg, Some(&spec()), &mut r, &mut p);
        assert!(cs.iter().any(|c| c.name == "device" && c.status == "warn"));
        assert!(cs
            .iter()
            .any(|c| c.name == "target-check" && c.status == "ok"));
        assert_eq!(r.log, ["capture:check"]);
    }

    #[test]
    fn bad_state_cmd_output_fails() {
        let mut cfg = cfg();
        cfg.source = Source::StateCmd("st".into());
        let mut r = FakeRunner::default();
        r.captures.push((
            "st".into(),
            Captured {
                code: Some(0),
                stdout: "up\n".into(),
            },
        ));
        let mut p = FakeProbe::new(&[]);
        let cs = checks(&cfg, None, &mut r, &mut p);
        assert!(cs
            .iter()
            .any(|c| c.name == "state-cmd" && c.status == "fail"));
    }

    #[test]
    fn json_report_has_the_documented_shape() {
        let cs = vec![check("probe", "ok", "say \"hi\"\n")];
        assert_eq!(
            to_json(&cs),
            "{\"checks\": [{\"name\": \"probe\", \"status\": \"ok\", \"detail\": \"say \\\"hi\\\"\\n\"}]}"
        );
    }

    #[test]
    fn missing_buttons_warn() {
        let mut cfg = cfg();
        cfg.close_cmd = None;
        let mut r = FakeRunner::default();
        let mut p = FakeProbe::new(&[true]);
        let cs = checks(&cfg, Some(&spec()), &mut r, &mut p);
        assert!(cs.iter().any(|c| c.name == "buttons" && c.status == "warn"));
    }
}
