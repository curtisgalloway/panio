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

//! The HID serial protocol grammar (`docs/dev/hid-serial-protocol.md` §3), executed
//! against a CH9329 [`Session`] instead of forwarded to a microcontroller.
//!
//! [`execute_line`] is the single backend for both the CLI subcommands and
//! `run`-file lines, so a `ch9329` helper accepts exactly the command set the
//! KB2040 `hidrig` does. Command-file parsing (`parse_sequence`/[`Step`]) and
//! the `moveabs` clamp are ported from `hidrig/src/proto.rs`.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::keys::name_to_key;
use crate::session::{never_cancelled, Cancel, MuxSide, Session, MAX_TYPE_CHARS};

/// `version` reply data (the part after `OK`): protocol version, impl id, and
/// the optional capabilities this injector advertises. The CH9329 has a true
/// absolute pointer, so it offers `moveabs`. It deliberately does **not**
/// advertise `baud`: the protocol's `baud` is a *transient* link renegotiation
/// that reverts on power-cycle, but the CH9329 only has a *persistent*
/// flash-stored rate (the `baud` command below performs it, but a host should
/// not auto-invoke it expecting transient behavior).
pub const VERSION_REPLY: &str = "1 ch9329/0.1.0 moveabs";

/// How long a command line may take, the one bound both ends of `POST /send`
/// use so they agree (#328): the daemon waits this long for the owner thread's
/// answer, and the CLI client waits this plus [`CLIENT_MARGIN`].
///
/// A `type` costs roughly 50-60 ms per character (hold, pacing and two serial
/// round trips), so its bound grows with the text: [`TYPE_BASE`] plus
/// [`TYPE_PER_CHAR`] per character (at most [`MAX_TYPE_CHARS`] count; the
/// session refuses more). Everything else gets [`OTHER_BUDGET`], which covers
/// the slowest of them (a full-range `move`, a lazy link open, a mux switch).
/// Expiry is not just a report: the daemon drops the request, which cancels it
/// if it is already running.
pub fn budget(line: &str) -> Duration {
    let line = line.trim_start();
    let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
    if head.eq_ignore_ascii_case("type") {
        let chars = rest.chars().count().min(MAX_TYPE_CHARS) as u32;
        TYPE_BASE + TYPE_PER_CHAR * chars
    } else {
        OTHER_BUDGET
    }
}

/// Fixed part of a `type` command's [`budget`].
const TYPE_BASE: Duration = Duration::from_secs(10);
/// Per-character part of a `type` command's [`budget`]; roughly twice the
/// measured cost per character.
const TYPE_PER_CHAR: Duration = Duration::from_millis(100);
/// [`budget`] of every command except `type`.
const OTHER_BUDGET: Duration = Duration::from_secs(30);
/// How much longer than the daemon's [`budget`] the CLI client waits, so the
/// daemon normally answers (with its own timeout error) first.
pub const CLIENT_MARGIN: Duration = Duration::from_secs(5);

/// The `moveabs` logical maximum (matches hidrig's ABS_MAX).
pub const ABS_MAX: i32 = 32_767;

/// Clamp a value into the `moveabs` logical range.
pub fn clamp_abs(v: i32) -> i32 {
    v.clamp(0, ABS_MAX)
}

/// Execute one protocol command line. Returns the `OK` reply data (empty for a
/// bare `OK`, the capability string for `version`). Errors map to the `ERR`
/// the protocol requires.
///
/// The line is the frame: one trailing line terminator is stripped, and a
/// CR/LF anywhere else is refused rather than typed as Enter — this is the
/// direct path (a one-shot with no daemon running), so it must refuse an
/// embedded newline itself; the daemon's queue (`uart.rs`) refuses it too
/// before it ever reaches here. `type` text is the remainder after the one
/// separator, verbatim (trailing spaces included); every other verb takes
/// whitespace-separated tokens.
pub fn execute_line(s: &mut Session, line: &str) -> Result<String> {
    execute_line_cancellable(s, line, &never_cancelled)
}

/// [`execute_line`] for a requester that can go away: `cancel` is polled
/// between HID reports and inside long sleeps (see `session.rs`), and a
/// command it stops fails with [`crate::session::CANCELLED`] after releasing
/// whatever it pressed (#328). `delay`/`sleep` are not commands here; they
/// exist only in `run` files, where the client does the waiting.
pub fn execute_line_cancellable(s: &mut Session, line: &str, cancel: Cancel<'_>) -> Result<String> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.contains(['\r', '\n']) {
        return Err(anyhow!("command contains a newline: {line:?}"));
    }
    let line = line.trim_start();
    let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    match head.to_ascii_lowercase().as_str() {
        "type" => {
            // Everything after `type ` is literal text (spaces and # included).
            let text = line
                .strip_prefix(head)
                .unwrap_or("")
                .strip_prefix(' ')
                .unwrap_or("");
            s.type_text(text, cancel)?;
        }
        "key" => {
            let name = one_arg(rest, "key")?;
            s.tap(name_to_key(name)?, cancel)?;
        }
        "combo" => {
            let mut chord = Vec::new();
            for name in rest.split_whitespace() {
                chord.push(name_to_key(name)?);
            }
            if chord.is_empty() {
                bail!("combo needs at least one key name");
            }
            s.combo(&chord, cancel)?;
        }
        "down" => s.key_down(name_to_key(one_arg(rest, "down")?)?)?,
        "up" => s.key_up(name_to_key(one_arg(rest, "up")?)?)?,
        "releaseall" => s.release_all()?,
        "move" => {
            let (dx, dy) = two_ints(rest, "move")?;
            s.move_rel(dx, dy, cancel)?;
        }
        "moveabs" => {
            let (x, y) = two_ints(rest, "moveabs")?;
            s.move_abs(clamp_abs(x), clamp_abs(y))?;
        }
        "click" => s.click(button_or_default(rest), cancel)?,
        "mdown" => s.mouse_down(button_or_default(rest))?,
        "mup" => s.mouse_up(button_or_default(rest))?,
        "scroll" => {
            let amount: i32 = one_arg(rest, "scroll")?
                .parse()
                .map_err(|_| anyhow!("scroll amount must be an integer: {rest:?}"))?;
            s.scroll(amount, cancel)?;
        }
        "ping" => {
            s.get_info()?;
        }
        "version" => return Ok(VERSION_REPLY.to_string()),
        "info" => {
            // CH9329-specific extension (not part of the HID serial protocol):
            // chip firmware version, target-USB enumeration, lock LEDs, baud.
            let i = s.get_info()?;
            let baud = s.baud();
            return Ok(format!(
                "chip_version={:#04x} target_connected={} num_lock={} \
                 caps_lock={} scroll_lock={} baud={}",
                i.chip_version, i.target_connected, i.num_lock, i.caps_lock, i.scroll_lock, baud
            ));
        }
        "baud" => {
            let rate: u32 = one_arg(rest, "baud")?
                .parse()
                .map_err(|_| anyhow!("baud rate must be an integer: {rest:?}"))?;
            let note = s.set_baud(rate)?;
            let mut reply = format!(
                "link persisted to {rate} baud (CH9329 flash; auto-detect probes \
                 115200, 57600, then 9600 — for other rates reconnect with -b \
                 {rate})"
            );
            if let Some(n) = note {
                reply.push_str("; ");
                reply.push_str(&n);
            }
            return Ok(reply);
        }
        "usb" => {
            // Openterface USB-mux control, another CH9329-family extension
            // rather than part of the HID serial protocol: the shared microSD
            // reader is visible to exactly one side at a time. `state` reports
            // the current side; `host`/`target` drive it and verify it landed.
            // All three answer with the resulting side.
            let side = match one_arg(rest, "usb")? {
                "state" => s.usb_query()?,
                "host" => s.usb_set(MuxSide::Host)?,
                "target" => s.usb_set(MuxSide::Target)?,
                other => bail!("usb takes host, target, or state (got {other:?})"),
            };
            return Ok(side.as_str().to_string());
        }
        other => bail!("unknown command: {other}"),
    }
    Ok(String::new())
}

fn one_arg<'a>(rest: &'a str, verb: &str) -> Result<&'a str> {
    let arg = rest.split_whitespace().next();
    arg.ok_or_else(|| anyhow!("{verb} needs an argument"))
}

fn two_ints(rest: &str, verb: &str) -> Result<(i32, i32)> {
    let mut it = rest.split_whitespace();
    let a = it.next().and_then(|v| v.parse().ok());
    let b = it.next().and_then(|v| v.parse().ok());
    match (a, b) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(anyhow!("{verb} needs two integer arguments: {rest:?}")),
    }
}

fn button_or_default(rest: &str) -> &str {
    let b = rest.split_whitespace().next().unwrap_or("left");
    if b.is_empty() {
        "left"
    } else {
        b
    }
}

/// One step of a command file (ported from hidrig).
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// A protocol command line, executed verbatim.
    Cmd(String),
    /// A pause, in seconds.
    Delay(f64),
}

/// Longest pause a sequence file may ask for, in seconds (ported from hidrig).
const MAX_PAUSE_SECS: f64 = 3600.0;

/// Parse a command file into steps: non-blank, non-`#` lines are commands;
/// `delay <ms>` / `sleep <seconds>` are timing directives, each a finite value
/// between 0 and one hour. Command lines pass through with only their leading
/// whitespace removed (a `type` line's trailing spaces are part of its text).
pub fn parse_sequence(text: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if line.trim_end().is_empty() || line.starts_with('#') {
            continue;
        }
        let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
        let value = rest.split_whitespace().next().unwrap_or("");
        match head.to_ascii_lowercase().as_str() {
            "delay" => steps.push(Step::Delay(pause_secs(value, 1000.0, "delay", rest)?)),
            "sleep" => steps.push(Step::Delay(pause_secs(value, 1.0, "sleep", rest)?)),
            _ => steps.push(Step::Cmd(line.to_string())),
        }
    }
    Ok(steps)
}

/// A pause directive's value in seconds: `value` is in units of
/// `1/per_second` s, and must be finite, non-negative and at most
/// [`MAX_PAUSE_SECS`] — `Duration::from_secs_f64` panics on a negative,
/// infinite or NaN value, and an hours-long pause in a sequence is a typo.
fn pause_secs(value: &str, per_second: f64, what: &str, rest: &str) -> Result<f64> {
    let v: f64 = value
        .parse()
        .map_err(|_| anyhow!("invalid {what} value: {rest:?}"))?;
    let secs = v / per_second;
    if !secs.is_finite() || !(0.0..=MAX_PAUSE_SECS).contains(&secs) {
        return Err(anyhow!(
            "{what} must be between 0 and {MAX_PAUSE_SECS} seconds: {rest:?}"
        ));
    }
    // -0.0 is not < 0.0 but is sign-negative; normalise it.
    Ok(if secs == 0.0 { 0.0 } else { secs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_abs_bounds() {
        assert_eq!(clamp_abs(-1), 0);
        assert_eq!(clamp_abs(0), 0);
        assert_eq!(clamp_abs(ABS_MAX), ABS_MAX);
        assert_eq!(clamp_abs(ABS_MAX + 100), ABS_MAX);
    }

    #[test]
    fn parses_commands_and_directives() {
        let steps = parse_sequence(
            "# boot sequence\n\
             type root\n\
             key ENTER\n\
             delay 500\n\
             \n\
             sleep 1.5\n\
             move 300 -50\n",
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Cmd("type root".into()),
                Step::Cmd("key ENTER".into()),
                Step::Delay(0.5),
                Step::Delay(1.5),
                Step::Cmd("move 300 -50".into()),
            ]
        );
    }

    #[test]
    fn command_lines_pass_through_verbatim() {
        // `type` text may contain '#'; no inline-comment stripping on commands.
        let steps = parse_sequence("type issue #42\n").unwrap();
        assert_eq!(steps, vec![Step::Cmd("type issue #42".into())]);
    }

    #[test]
    fn two_ints_parses_negatives() {
        assert_eq!(two_ints("300 -50", "move").unwrap(), (300, -50));
        assert!(two_ints("300", "move").is_err());
    }

    #[test]
    fn button_defaulting() {
        assert_eq!(button_or_default(""), "left");
        assert_eq!(button_or_default("right"), "right");
    }

    /// A pause must be a finite, non-negative duration of at most an hour:
    /// `Duration::from_secs_f64` panics on a negative, infinite or NaN value,
    /// and a multi-hour sleep in a sequence file is a mistake, not a plan.
    #[test]
    fn delay_and_sleep_are_bounded() {
        for bad in [
            "delay -1",
            "sleep -0.5",
            "sleep inf",
            "delay inf",
            "sleep nan",
            "delay NaN",
            "sleep 3600.5",
            "delay 3600001",
        ] {
            assert!(parse_sequence(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_sequence("sleep 3600\ndelay 3600000\nsleep 0\n").unwrap(),
            vec![Step::Delay(3600.0), Step::Delay(3600.0), Step::Delay(0.0)]
        );
    }

    /// A `type` line's trailing spaces are part of its text; the parser strips
    /// only leading whitespace (blank and comment detection is unchanged).
    #[test]
    fn command_lines_keep_trailing_whitespace() {
        let steps = parse_sequence("  type hi  \n").unwrap();
        assert_eq!(steps, vec![Step::Cmd("type hi  ".into())]);
        assert!(parse_sequence("   \n\t\n").unwrap().is_empty());
    }

    /// The bound on a command grows with the text of a `type`, never shrinks,
    /// and gives a long one far more than the old fixed 30 s.
    #[test]
    fn budget_grows_with_the_text_of_a_type() {
        let mut prev = Duration::ZERO;
        for n in (0..=MAX_TYPE_CHARS + 100).step_by(50) {
            let b = budget(&format!("type {}", "x".repeat(n)));
            assert!(b >= prev, "budget fell at {n} characters");
            prev = b;
        }
        assert_eq!(budget("type "), Duration::from_secs(10));
        assert!(budget(&format!("type {}", "x".repeat(500))) > Duration::from_secs(30));
        assert!(
            budget(&format!("type {}", "x".repeat(MAX_TYPE_CHARS))) > Duration::from_secs(300),
            "a full-length type (about 4 minutes at 55 ms per character) must fit"
        );
        // Trailing spaces and multi-byte characters are characters, not bytes.
        assert_eq!(budget("type \u{e9}\u{e9}"), budget("type ab"));
        for other in [
            "key A",
            "click",
            "move 32767 32767",
            "releaseall",
            "usb host",
        ] {
            assert_eq!(budget(other), Duration::from_secs(30), "{other}");
        }
    }
}
