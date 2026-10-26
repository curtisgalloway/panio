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

//! The press / observe / wait logic, behind small traits so tests drive it
//! with fakes (`Runner`, `Probe`, `Clock`). Nothing here touches the real
//! system; `sys.rs` holds the implementations that do.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};

/// Which computer the peripheral is connected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Host,
    Target,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Host => "host",
            Side::Target => "target",
        }
    }
}

/// Result of running a shell command with its stdout captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// Exit code, or `None` when the command died on a signal.
    pub code: Option<i32>,
    pub stdout: String,
}

/// Runs shell commands.
pub trait Runner {
    /// Run a button command (close or open). Its stdout must not reach this
    /// process's stdout. Fails on a non-zero exit.
    fn button(&mut self, cmd: &str) -> Result<()>;
    /// Run a command and capture its stdout. A non-zero exit is data, not an
    /// error; only failing to start the command is an `Err`.
    fn capture(&mut self, cmd: &str) -> Result<Captured>;
}

/// Is the peripheral visible on the control host's USB bus?
pub trait Probe {
    fn present(&mut self) -> Result<bool>;
}

/// Time, injectable so tests do not wait.
pub trait Clock {
    fn now(&self) -> Duration;
    fn sleep(&mut self, d: Duration);
}

/// How the position is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Probe the peripheral on the control host (`--device`).
    Device,
    /// Ask a command that prints `host` or `target` (`--state-cmd`).
    StateCmd(String),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub close_cmd: Option<String>,
    pub open_cmd: Option<String>,
    pub hold: Duration,
    pub settle: Duration,
    pub poll: Duration,
    pub source: Source,
    pub target_check: Option<String>,
}

/// One reading of the position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Host,
    /// On the target. `inferred` is true when the only evidence is that the
    /// device is absent from the host.
    Target {
        inferred: bool,
    },
    /// Absent from the host and not seen by `--target-check`.
    Neither,
}

impl Observed {
    fn describe(&self) -> &'static str {
        match self {
            Observed::Host => "the device is present on the host",
            Observed::Target { inferred: false } => "the device is on the target",
            Observed::Target { inferred: true } => {
                "the device is absent from the host (assumed on the target)"
            }
            Observed::Neither => "the device is seen on neither side",
        }
    }

    fn side(&self) -> Option<Side> {
        match self {
            Observed::Host => Some(Side::Host),
            Observed::Target { .. } => Some(Side::Target),
            Observed::Neither => None,
        }
    }
}

/// What a successful `state` / `usb` verb reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub side: Side,
    pub inferred: bool,
}

pub const INFERRED_WARNING: &str = "usbtoggle: WARNING: `target` is inferred only from the \
device being absent from this host. It could equally be unplugged or a hub could be dead. \
Add --target-check-cmd (or use --state-cmd) for a positive reading.";

/// Write the verb's result: the side on stdout, any warning on stderr.
pub fn emit(
    outcome: Outcome,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    if outcome.inferred {
        writeln!(err, "{INFERRED_WARNING}")?;
    }
    writeln!(out, "{}", outcome.side.as_str())
}

/// Parse the output of a `--state-cmd`.
pub fn parse_side(text: &str) -> Result<Side> {
    match text.trim() {
        "host" => Ok(Side::Host),
        "target" => Ok(Side::Target),
        other => bail!("--state-cmd printed {other:?}; expected `host` or `target`"),
    }
}

/// Take one reading of the position.
pub fn observe(cfg: &Config, runner: &mut dyn Runner, probe: &mut dyn Probe) -> Result<Observed> {
    if let Source::StateCmd(cmd) = &cfg.source {
        let cap = runner.capture(cmd)?;
        if cap.code != Some(0) {
            bail!("--state-cmd failed ({})", describe_code(cap.code));
        }
        return Ok(match parse_side(&cap.stdout)? {
            Side::Host => Observed::Host,
            Side::Target => Observed::Target { inferred: false },
        });
    }
    if probe.present()? {
        return Ok(Observed::Host);
    }
    match &cfg.target_check {
        None => Ok(Observed::Target { inferred: true }),
        Some(cmd) => {
            let cap = runner.capture(cmd)?;
            if cap.code == Some(0) {
                Ok(Observed::Target { inferred: false })
            } else {
                Ok(Observed::Neither)
            }
        }
    }
}

pub fn describe_code(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit status {c}"),
        None => "killed by a signal".to_string(),
    }
}

/// The `state` verb: one reading; `Neither` is an error.
pub fn state(cfg: &Config, runner: &mut dyn Runner, probe: &mut dyn Probe) -> Result<Outcome> {
    to_outcome(observe(cfg, runner, probe)?)
}

fn to_outcome(obs: Observed) -> Result<Outcome> {
    match obs {
        Observed::Host => Ok(Outcome {
            side: Side::Host,
            inferred: false,
        }),
        Observed::Target { inferred } => Ok(Outcome {
            side: Side::Target,
            inferred,
        }),
        Observed::Neither => bail!(
            "device seen on neither side: absent from this host and --target-check-cmd \
             did not succeed (unplugged, a dead hub, or the target is off)"
        ),
    }
}

/// Runs the open command when dropped (or explicitly through `finish`), so a
/// failed close, an error, or a panic between close and open can never leave
/// the button held down.
struct OpenGuard<'a> {
    runner: &'a mut dyn Runner,
    cmd: &'a str,
    armed: bool,
}

impl OpenGuard<'_> {
    /// Run the open command now and report its result.
    fn finish(mut self) -> Result<()> {
        self.armed = false;
        self.runner.button(self.cmd)
    }
}

impl Drop for OpenGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = self.runner.button(self.cmd) {
                eprintln!("usbtoggle: open-cmd failed after an earlier error: {e:#}");
            }
        }
    }
}

fn need<'a>(cmd: &'a Option<String>, flag: &str) -> Result<&'a str> {
    cmd.as_deref()
        .ok_or_else(|| anyhow!("{flag} is required to press the button"))
}

/// One button press: close, hold, open. The open command runs whenever the
/// close was attempted, whatever happened in between.
pub fn press(cfg: &Config, runner: &mut dyn Runner, clock: &mut dyn Clock) -> Result<()> {
    let close = need(&cfg.close_cmd, "--close-cmd")?;
    let open = need(&cfg.open_cmd, "--open-cmd")?;
    let guard = OpenGuard {
        runner,
        cmd: open,
        armed: true,
    };
    if let Err(e) = guard.runner.button(close) {
        // The guard releases the button as it drops.
        return Err(e.context("--close-cmd failed (--open-cmd was still run)"));
    }
    clock.sleep(cfg.hold);
    guard
        .finish()
        .map_err(|e| e.context("--open-cmd failed; the button may be left held"))
}

/// The `usb host` / `usb target` verbs.
pub fn switch_to(
    want: Side,
    cfg: &Config,
    runner: &mut dyn Runner,
    probe: &mut dyn Probe,
    clock: &mut dyn Clock,
) -> Result<Outcome> {
    // Refuse before reading anything if a press could not happen anyway.
    need(&cfg.close_cmd, "--close-cmd")?;
    need(&cfg.open_cmd, "--open-cmd")?;

    let first = observe(cfg, runner, probe)?;
    if first.side() == Some(want) {
        return to_outcome(first);
    }
    // `Neither` is neither side: do not press blind.
    if first == Observed::Neither {
        return to_outcome(first);
    }

    press(cfg, runner, clock)?;

    let start = clock.now();
    let mut last = format!("before the press: {}", first.describe());
    loop {
        match observe(cfg, runner, probe) {
            Ok(obs) => {
                if obs.side() == Some(want) {
                    return to_outcome(obs);
                }
                last = obs.describe().to_string();
            }
            Err(e) => last = format!("{e:#}"),
        }
        if clock.now().saturating_sub(start) >= cfg.settle {
            bail!(
                "timed out after {} ms waiting for the device to reach the {}; last \
                 observed: {last}. The button was pressed once and not pressed again",
                cfg.settle.as_millis(),
                want.as_str()
            );
        }
        clock.sleep(cfg.poll);
    }
}

#[cfg(test)]
pub mod fakes {
    use super::*;
    use std::collections::VecDeque;

    /// Records every command; scripted results.
    #[derive(Default)]
    pub struct FakeRunner {
        pub log: Vec<String>,
        pub fail_button: Vec<String>,
        /// Results for `capture`, by command; default exit 0 with no output.
        pub captures: Vec<(String, Captured)>,
    }

    impl Runner for FakeRunner {
        fn button(&mut self, cmd: &str) -> Result<()> {
            self.log.push(format!("button:{cmd}"));
            if self.fail_button.iter().any(|c| c == cmd) {
                bail!("{cmd} exited with status 1");
            }
            Ok(())
        }
        fn capture(&mut self, cmd: &str) -> Result<Captured> {
            self.log.push(format!("capture:{cmd}"));
            Ok(self
                .captures
                .iter()
                .find(|(c, _)| c == cmd)
                .map(|(_, r)| r.clone())
                .unwrap_or(Captured {
                    code: Some(0),
                    stdout: String::new(),
                }))
        }
    }

    /// A probe that answers from a script, then repeats its last answer.
    pub struct FakeProbe {
        pub answers: VecDeque<bool>,
        pub last: bool,
        pub reads: usize,
    }

    impl FakeProbe {
        pub fn new(answers: &[bool]) -> Self {
            FakeProbe {
                answers: answers.iter().copied().collect(),
                last: *answers.last().unwrap_or(&false),
                reads: 0,
            }
        }
    }

    impl Probe for FakeProbe {
        fn present(&mut self) -> Result<bool> {
            self.reads += 1;
            if let Some(a) = self.answers.pop_front() {
                self.last = a;
            }
            Ok(self.last)
        }
    }

    #[derive(Default)]
    pub struct FakeClock {
        pub t: Duration,
        pub slept: Vec<Duration>,
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            self.t
        }
        fn sleep(&mut self, d: Duration) {
            self.t += d;
            self.slept.push(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;

    fn cfg() -> Config {
        Config {
            close_cmd: Some("close".into()),
            open_cmd: Some("open".into()),
            hold: Duration::from_millis(200),
            settle: Duration::from_millis(1000),
            poll: Duration::from_millis(50),
            source: Source::Device,
            target_check: None,
        }
    }

    fn presses(r: &FakeRunner) -> usize {
        r.log.iter().filter(|l| *l == "button:close").count()
    }

    #[test]
    fn already_at_the_side_does_not_press() {
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        let mut p = FakeProbe::new(&[true]);
        let o = switch_to(Side::Host, &cfg(), &mut r, &mut p, &mut c).unwrap();
        assert_eq!(o.side, Side::Host);
        assert!(r.log.is_empty(), "{:?}", r.log);

        let mut p = FakeProbe::new(&[false]);
        let o = switch_to(Side::Target, &cfg(), &mut r, &mut p, &mut c).unwrap();
        assert_eq!(o.side, Side::Target);
        assert_eq!(presses(&r), 0);
    }

    #[test]
    fn toggle_then_verify_succeeds_with_one_press() {
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        // absent (first read), absent (still moving), present.
        let mut p = FakeProbe::new(&[false, false, true]);
        let o = switch_to(Side::Host, &cfg(), &mut r, &mut p, &mut c).unwrap();
        assert_eq!(o.side, Side::Host);
        assert_eq!(r.log, ["button:close", "button:open"]);
        assert_eq!(c.slept[0], Duration::from_millis(200), "hold between");
    }

    #[test]
    fn timeout_is_an_error_after_exactly_one_press() {
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        let mut p = FakeProbe::new(&[false]);
        let e = switch_to(Side::Host, &cfg(), &mut r, &mut p, &mut c).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("timed out after 1000 ms"), "{msg}");
        assert!(msg.contains("last observed"), "{msg}");
        assert_eq!(presses(&r), 1, "never press again: {:?}", r.log);
    }

    #[test]
    fn open_runs_even_when_close_fails() {
        let mut r = FakeRunner::default();
        r.fail_button.push("close".into());
        let mut c = FakeClock::default();
        let e = press(&cfg(), &mut r, &mut c).unwrap_err();
        assert!(format!("{e:#}").contains("--close-cmd failed"), "{e:#}");
        assert_eq!(r.log, ["button:close", "button:open"]);
    }

    #[test]
    fn open_failure_is_reported() {
        let mut r = FakeRunner::default();
        r.fail_button.push("open".into());
        let mut c = FakeClock::default();
        let e = press(&cfg(), &mut r, &mut c).unwrap_err();
        assert!(format!("{e:#}").contains("--open-cmd failed"), "{e:#}");
    }

    #[test]
    fn open_runs_when_the_hold_panics() {
        struct Boom;
        impl Clock for Boom {
            fn now(&self) -> Duration {
                Duration::ZERO
            }
            fn sleep(&mut self, _: Duration) {
                panic!("sleep blew up");
            }
        }
        let mut r = FakeRunner::default();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = press(&cfg(), &mut r, &mut Boom);
        }));
        assert!(res.is_err());
        assert_eq!(r.log, ["button:close", "button:open"]);
    }

    #[test]
    fn absent_with_failing_target_check_is_an_error_and_never_presses() {
        let mut cfg = cfg();
        cfg.target_check = Some("check".into());
        let mut r = FakeRunner::default();
        r.captures.push((
            "check".into(),
            Captured {
                code: Some(1),
                stdout: String::new(),
            },
        ));
        let mut p = FakeProbe::new(&[false]);
        let e = state(&cfg, &mut r, &mut p).unwrap_err();
        assert!(e.to_string().contains("neither side"), "{e}");

        let mut c = FakeClock::default();
        let e = switch_to(Side::Host, &cfg, &mut r, &mut p, &mut c).unwrap_err();
        assert!(e.to_string().contains("neither side"), "{e}");
        assert_eq!(presses(&r), 0, "do not press blind");
    }

    #[test]
    fn target_check_success_is_a_positive_target() {
        let mut cfg = cfg();
        cfg.target_check = Some("check".into());
        let mut r = FakeRunner::default();
        let mut p = FakeProbe::new(&[false]);
        let o = state(&cfg, &mut r, &mut p).unwrap();
        assert_eq!(
            o,
            Outcome {
                side: Side::Target,
                inferred: false
            }
        );
    }

    #[test]
    fn inferred_target_prints_the_warning_on_stderr_only() {
        let mut r = FakeRunner::default();
        let mut p = FakeProbe::new(&[false]);
        let o = state(&cfg(), &mut r, &mut p).unwrap();
        assert!(o.inferred);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        emit(o, &mut out, &mut err).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "target\n");
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("WARNING") && err.contains("inferred"), "{err}");

        // A positive reading is silent.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        emit(
            Outcome {
                side: Side::Host,
                inferred: false,
            },
            &mut out,
            &mut err,
        )
        .unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn state_cmd_mode_validates_what_it_prints() {
        let mut cfg = cfg();
        cfg.source = Source::StateCmd("st".into());
        let mut p = FakeProbe::new(&[]);
        let cap = |s: &str, code| Captured {
            code,
            stdout: s.into(),
        };

        let mut r = FakeRunner::default();
        r.captures.push(("st".into(), cap("target\n", Some(0))));
        assert_eq!(state(&cfg, &mut r, &mut p).unwrap().side, Side::Target);
        assert_eq!(p.reads, 0, "state-cmd mode never probes");

        let mut r = FakeRunner::default();
        r.captures.push(("st".into(), cap("banana\n", Some(0))));
        let e = state(&cfg, &mut r, &mut p).unwrap_err();
        assert!(e.to_string().contains("expected `host` or `target`"), "{e}");

        let mut r = FakeRunner::default();
        r.captures.push(("st".into(), cap("host\n", Some(2))));
        assert!(state(&cfg, &mut r, &mut p).is_err(), "non-zero exit fails");
    }

    #[test]
    fn missing_button_commands_refuse_before_any_press() {
        let mut cfg = cfg();
        cfg.close_cmd = None;
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        let mut p = FakeProbe::new(&[false]);
        let e = switch_to(Side::Host, &cfg, &mut r, &mut p, &mut c).unwrap_err();
        assert!(e.to_string().contains("--close-cmd"), "{e}");
        assert!(r.log.is_empty());
    }

    // Characterization tests: these pin documented limits, not evidence of
    // physical connectivity. In particular, false means only host absence.
    #[test]
    fn disconnected_target_noop_is_inferred_and_does_not_wait() {
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        let mut p = FakeProbe::new(&[false, true]);
        let o = switch_to(Side::Target, &cfg(), &mut r, &mut p, &mut c).unwrap();
        assert!(o.inferred);
        assert_eq!(p.reads, 1, "later enumeration is never observed");
        assert!(r.log.is_empty());
        assert!(c.slept.is_empty());
    }

    #[test]
    fn host_disappearance_after_press_is_only_inferred_success() {
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        let mut p = FakeProbe::new(&[true, false, true]);
        let o = switch_to(Side::Target, &cfg(), &mut r, &mut p, &mut c).unwrap();
        assert!(o.inferred);
        assert_eq!(p.reads, 2, "transient absence is sufficient");
        assert_eq!(r.log, ["button:close", "button:open"]);
    }

    #[test]
    fn misleading_initial_host_read_is_not_rechecked() {
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        let mut p = FakeProbe::new(&[true, false]);
        let o = switch_to(Side::Host, &cfg(), &mut r, &mut p, &mut c).unwrap();
        assert!(!o.inferred);
        assert_eq!(p.reads, 1);
        assert!(r.log.is_empty());
    }

    #[test]
    fn misleading_state_command_is_trusted_without_target_check() {
        let mut cfg = cfg();
        cfg.source = Source::StateCmd("st".into());
        cfg.target_check = Some("check".into());
        let mut r = FakeRunner::default();
        r.captures.push((
            "st".into(),
            Captured {
                code: Some(0),
                stdout: "target".into(),
            },
        ));
        r.captures.push((
            "check".into(),
            Captured {
                code: Some(1),
                stdout: String::new(),
            },
        ));
        let mut p = FakeProbe::new(&[false]);
        let o = switch_to(
            Side::Target,
            &cfg,
            &mut r,
            &mut p,
            &mut FakeClock::default(),
        )
        .unwrap();
        assert!(!o.inferred);
        assert_eq!(r.log, ["capture:st"]);
        assert_eq!(p.reads, 0);
    }

    struct CheckSequence {
        buttons: Vec<String>,
        codes: std::collections::VecDeque<i32>,
        last: i32,
        reads: usize,
    }

    impl CheckSequence {
        fn new(codes: &[i32]) -> Self {
            Self {
                buttons: Vec::new(),
                codes: codes.iter().copied().collect(),
                last: 1,
                reads: 0,
            }
        }
    }

    impl Runner for CheckSequence {
        fn button(&mut self, cmd: &str) -> Result<()> {
            self.buttons.push(cmd.into());
            Ok(())
        }
        fn capture(&mut self, _: &str) -> Result<Captured> {
            self.reads += 1;
            if let Some(code) = self.codes.pop_front() {
                self.last = code;
            }
            Ok(Captured {
                code: Some(self.last),
                stdout: String::new(),
            })
        }
    }

    #[test]
    fn target_check_waits_through_delayed_enumeration_after_press() {
        let mut cfg = cfg();
        cfg.target_check = Some("check".into());
        let mut r = CheckSequence::new(&[1, 1, 0]);
        let mut c = FakeClock::default();
        let mut p = FakeProbe::new(&[true, false]);
        let o = switch_to(Side::Target, &cfg, &mut r, &mut p, &mut c).unwrap();
        assert!(!o.inferred);
        assert_eq!(r.buttons, ["close", "open"]);
        assert_eq!(r.reads, 3);
        assert_eq!(c.t, cfg.hold + cfg.poll * 2);
    }

    #[test]
    fn disconnected_after_press_with_target_check_times_out() {
        let mut cfg = cfg();
        cfg.target_check = Some("check".into());
        let mut r = CheckSequence::new(&[1]);
        let mut p = FakeProbe::new(&[true, false]);
        let e = switch_to(
            Side::Target,
            &cfg,
            &mut r,
            &mut p,
            &mut FakeClock::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("neither side"));
        assert!(e.to_string().contains("timed out"));
        assert_eq!(r.buttons, ["close", "open"]);
    }

    #[test]
    fn initial_neither_refuses_both_directions_without_waiting() {
        for want in [Side::Host, Side::Target] {
            let mut cfg = cfg();
            cfg.target_check = Some("check".into());
            let mut r = CheckSequence::new(&[1, 0]);
            let mut c = FakeClock::default();
            let mut p = FakeProbe::new(&[false]);
            let e = switch_to(want, &cfg, &mut r, &mut p, &mut c).unwrap_err();
            assert!(e.to_string().contains("neither side"));
            assert!(r.buttons.is_empty());
            assert_eq!(r.reads, 1, "does not wait for delayed initial enumeration");
            assert!(c.slept.is_empty());
        }
    }

    #[test]
    fn relay_errors_abort_switch_even_if_next_read_would_match() {
        for failures in [vec!["close"], vec!["open"], vec!["close", "open"]] {
            let mut r = FakeRunner {
                fail_button: failures.into_iter().map(str::to_string).collect(),
                ..Default::default()
            };
            let mut p = FakeProbe::new(&[false, true]);
            assert!(switch_to(
                Side::Host,
                &cfg(),
                &mut r,
                &mut p,
                &mut FakeClock::default()
            )
            .is_err());
            assert_eq!(r.log, ["button:close", "button:open"]);
            assert_eq!(
                p.reads, 1,
                "never converts a failed relay command to success"
            );
        }
    }

    #[test]
    fn falsely_successful_target_check_can_produce_unwarned_noop() {
        let mut cfg = cfg();
        cfg.target_check = Some("check".into());
        let mut r = CheckSequence::new(&[0]);
        let mut p = FakeProbe::new(&[false]);
        let o = switch_to(
            Side::Target,
            &cfg,
            &mut r,
            &mut p,
            &mut FakeClock::default(),
        )
        .unwrap();
        assert!(
            !o.inferred,
            "exit zero is trusted, regardless of physical state"
        );
        assert!(r.buttons.is_empty());
    }

    #[test]
    fn late_matching_observation_is_accepted_before_deadline_check() {
        let mut cfg = cfg();
        cfg.settle = Duration::from_millis(25);
        let (mut r, mut c) = (FakeRunner::default(), FakeClock::default());
        let mut p = FakeProbe::new(&[false, false, true]);
        switch_to(Side::Host, &cfg, &mut r, &mut p, &mut c).unwrap();
        assert_eq!(c.t - cfg.hold, Duration::from_millis(50));
        assert_eq!(presses(&r), 1);
    }
}
