//! `paniolo mcp`: paniolo's runtime commands as MCP tools over stdio.
//!
//! A Model Context Protocol server speaking newline-delimited JSON-RPC 2.0 on
//! stdin/stdout, hand-written rather than built on an SDK (design and
//! dependency decision: `notes/mcp-server.md`). It implements the tools-only
//! subset an agent harness needs — `initialize`, `ping`, `tools/list`,
//! `tools/call` — and ignores notifications.
//!
//! **stdout is the protocol channel.** Nothing else may write to it, so a tool
//! never runs a command handler in-process (handlers print, and `video shot`
//! exits the process). A tool that drives a channel runs this same binary as
//! a child with stdin closed and stdout captured, which also gives it the
//! CLI's remote dispatch unchanged. Read-only lab queries (`target_list`) run
//! in-process because they print nothing.
//!
//! The server holds no state of its own: the daemons hold the real state, so
//! a restart loses nothing.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::error::Kind;
use serde_json::{json, Value};

/// MCP revisions this server accepts. The tools-only subset is identical
/// across them; an `initialize` naming one is answered with that one, and
/// anything else with the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Longest `timeout_ms` a `video_shot` call may ask for. The call blocks the
/// server (requests are handled one at a time), so it is bounded.
const MAX_SHOT_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_SHOT_TIMEOUT_MS: u64 = 2_000;

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Run the server on this process's stdin/stdout until stdin closes.
pub fn serve(lab_flag: Option<&str>) -> Result<()> {
    let server = Server {
        exe: std::env::current_exe()?,
        lab: lab_flag.map(str::to_string),
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    server.run(stdin.lock(), stdout.lock())
}

struct Server {
    /// The paniolo binary tools run as a child.
    exe: PathBuf,
    /// The `--lab` the server was started with, passed on to every child.
    lab: Option<String>,
}

impl Server {
    fn run(&self, input: impl BufRead, mut output: impl Write) -> Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(error_reply(
                    Value::Null,
                    PARSE_ERROR,
                    &format!("parse error: {e}"),
                )),
            };
            if let Some(reply) = reply {
                // serde_json never emits a raw newline, so one message stays
                // one line, as the stdio transport requires.
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// One incoming message → its reply, or `None` for a notification.
    fn handle(&self, msg: &Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            // Includes JSON-RPC batches, which MCP no longer allows.
            return Some(error_reply(
                Value::Null,
                INVALID_REQUEST,
                "expected one JSON-RPC object",
            ));
        };
        let method = obj.get("method").and_then(Value::as_str);
        // No id: a notification (`notifications/initialized`,
        // `notifications/cancelled`, …). Nothing to answer.
        let id = obj.get("id")?.clone();
        let Some(method) = method else {
            return Some(error_reply(id, INVALID_REQUEST, "missing method"));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => result_reply(id, initialize(&params)),
            "ping" => result_reply(id, json!({})),
            "tools/list" => result_reply(id, json!({ "tools": tool_list() })),
            "tools/call" => match self.call_tool(&params) {
                Ok(result) => result_reply(id, result),
                Err(message) => error_reply(id, INVALID_PARAMS, &message),
            },
            other => error_reply(id, METHOD_NOT_FOUND, &format!("unknown method: {other}")),
        })
    }

    /// `tools/call`. `Err` is a protocol error (unknown tool, bad arguments);
    /// a tool that ran and failed is `Ok` with `isError: true`, so the agent
    /// sees the failure as a result it can act on.
    fn call_tool(&self, params: &Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tools/call needs a tool name")?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        match name {
            "target_list" => Ok(self.target_list()),
            "video_shot" => {
                let req = ShotRequest::from_args(&args)?;
                Ok(self.video_shot(&req))
            }
            other => {
                let call = cli_call(other, &args)?;
                Ok(self.run_cli_tool(&call))
            }
        }
    }

    fn target_list(&self) -> Value {
        let lab = match crate::load_for_read(self.lab.as_deref()) {
            Ok(lab) => lab,
            Err(e) => return tool_error(&format!("{e:#}")),
        };
        let targets: Vec<Value> = lab
            .targets
            .keys()
            .filter_map(|name| lab.resolved_target(name))
            .map(|rt| {
                let channels: Vec<Value> = rt
                    .channels
                    .iter()
                    .map(|c| json!({ "kind": c.kind.as_str(), "name": c.name, "host": c.host }))
                    .collect();
                json!({
                    "name": rt.name,
                    "description": rt.description,
                    "hosts": rt.hosts(),
                    "channels": channels,
                })
            })
            .collect();
        tool_text(&serde_json::to_string_pretty(&targets).unwrap_or_default())
    }

    fn video_shot(&self, req: &ShotRequest) -> Value {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => return tool_error(&format!("creating a temp dir for the shot: {e}")),
        };
        let png = dir.path().join("shot.png");
        let out = match self.run_child(&req.argv(&png)) {
            Ok(out) => out,
            Err(e) => return tool_error(&format!("running paniolo video shot: {e}")),
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let code = crate::error::shell_code(out.status);
            return tool_error(&format!(
                "paniolo video shot exited {code}\n{}",
                stderr.trim_end()
            ));
        }
        let bytes = match std::fs::read(&png) {
            Ok(b) => b,
            Err(e) => return tool_error(&format!("reading the captured PNG: {e}")),
        };
        // hdmicap prints `signal=<s>  hash=<h>[  (timeout)]`; pass it through
        // so the agent has the hash for its next `changed_since`.
        let status = stderr
            .lines()
            .find(|l| l.contains("hash="))
            .unwrap_or("")
            .trim();
        json!({
            "content": [
                { "type": "image", "data": base64(&bytes), "mimeType": "image/png" },
                { "type": "text", "text": status },
            ],
            "isError": false,
        })
    }

    /// Run a tool that maps onto one paniolo command and reports its output
    /// as text. stderr rides along after stdout when there is any, because
    /// it carries warnings an agent should see (a stale serial log, say).
    fn run_cli_tool(&self, call: &CliCall) -> Value {
        let out = match self.run_child(&call.argv) {
            Ok(out) => out,
            Err(e) => return tool_error(&format!("running paniolo: {e}")),
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let verb = call.verb();
        if !out.status.success() {
            let code = crate::error::shell_code(out.status);
            let unknown = call.writes
                && [Kind::Unreachable, Kind::Timeout]
                    .iter()
                    .any(|k| k.exit_code() == code);
            let mut text = format!("paniolo {verb} exited {code}");
            if unknown {
                text.push_str(
                    "\nOutcome unknown: the command may or may not have run. \
                     Check the target's state before repeating it.",
                );
            }
            for part in [stdout.trim_end(), stderr.trim_end()] {
                if !part.is_empty() {
                    text.push('\n');
                    text.push_str(part);
                }
            }
            return tool_error(&text);
        }
        let mut text = stdout.trim_end().to_string();
        if text.is_empty() {
            text = format!("paniolo {verb}: ok");
        }
        if !stderr.trim().is_empty() {
            text.push_str("\nstderr:\n");
            text.push_str(stderr.trim_end());
        }
        tool_text(&text)
    }

    /// Run this binary with `args`, never letting it near the protocol
    /// stream: stdin closed (it must not read our requests), stdout and
    /// stderr captured.
    fn run_child(&self, args: &[String]) -> std::io::Result<std::process::Output> {
        let mut cmd = Command::new(&self.exe);
        if let Some(lab) = &self.lab {
            cmd.arg("--lab").arg(lab);
        }
        cmd.arg("--json-errors")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }
}

/// One tool call translated into a paniolo argv.
#[derive(Debug, PartialEq)]
struct CliCall {
    argv: Vec<String>,
    /// The command changes the target (input, power). A write whose outcome
    /// is unknown must not be retried blindly, so its error says so.
    writes: bool,
}

impl CliCall {
    /// The command's words before its first flag, for messages.
    fn verb(&self) -> String {
        self.argv
            .iter()
            .take_while(|a| !a.starts_with('-'))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Typed access to a tool's `arguments` object. Every error names the tool
/// and the argument, and comes back to the agent as a JSON-RPC error.
struct Args<'a> {
    tool: &'a str,
    v: &'a Value,
}

impl Args<'_> {
    fn target(&self) -> Result<String, String> {
        self.v
            .get("target")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .ok_or(format!("{} needs a target", self.tool))
    }

    fn opt_str(&self, name: &str) -> Result<Option<String>, String> {
        match self.v.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("{}: {name} must be a string", self.tool)),
        }
    }

    fn opt_bool(&self, name: &str) -> Result<Option<bool>, String> {
        match self.v.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("{}: {name} must be a boolean", self.tool)),
        }
    }

    fn opt_u64(&self, name: &str, max: u64) -> Result<Option<u64>, String> {
        match self.v.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_u64().filter(|n| *n <= max).map(Some).ok_or(format!(
                "{}: {name} must be an integer from 0 to {max}",
                self.tool
            )),
        }
    }
}

/// Longest serial log window one call may ask for.
const MAX_LOG_LINES: u64 = 5_000;
/// The CLI's own cap on `serial send --pace-ms`.
const MAX_PACE_MS: u64 = 10_000;

/// Translate a tool that maps onto one paniolo command. Every target is
/// passed as `-t`, never positionally, so a name can't be read as an option.
fn cli_call(tool: &str, args: &Value) -> Result<CliCall, String> {
    let a = Args { tool, v: args };
    let s = |x: &str| x.to_string();
    let read = |argv: Vec<String>| {
        Ok(CliCall {
            argv,
            writes: false,
        })
    };
    let write = |argv: Vec<String>| Ok(CliCall { argv, writes: true });
    match tool {
        "video_read" => {
            let mut argv = vec![s("video"), s("read"), s("-t"), a.target()?];
            let timeout = a
                .opt_u64("timeout_ms", MAX_SHOT_TIMEOUT_MS)?
                .unwrap_or(DEFAULT_SHOT_TIMEOUT_MS);
            argv.extend([s("--timeout"), timeout.to_string()]);
            if a.opt_bool("stable")?.unwrap_or(false) {
                argv.push(s("--stable"));
            }
            read(argv)
        }
        "serial_status" => {
            let mut argv = vec![s("serial"), s("status"), s("-t"), a.target()?];
            if let Some(i) = a.opt_str("interface")? {
                argv.extend([s("-i"), i]);
            }
            read(argv)
        }
        "serial_evidence" => {
            let mut argv = vec![s("serial"), s("evidence"), s("-t"), a.target()?];
            if let Some(i) = a.opt_str("interface")? {
                argv.extend([s("-i"), i]);
            }
            let epoch = a.opt_str("epoch")?;
            let since = a.opt_u64("since", u64::MAX)?;
            if since.is_some() && epoch.is_none() {
                return Err("serial_evidence: since requires epoch".into());
            }
            if let Some(epoch) = epoch {
                argv.extend([s("--epoch"), epoch]);
            }
            if let Some(since) = since {
                argv.extend([s("--since"), since.to_string()]);
            }
            if let Some(max) = a.opt_u64("max_bytes", crate::serial::MAX_EVIDENCE_BYTES)? {
                if max == 0 {
                    return Err("serial_evidence: max_bytes must be positive".into());
                }
                argv.extend([s("--max-bytes"), max.to_string()]);
            }
            read(argv)
        }
        "serial_log" => {
            let mut argv = vec![s("serial"), s("log"), s("-t"), a.target()?];
            if let Some(i) = a.opt_str("interface")? {
                argv.extend([s("-i"), i]);
            }
            for (name, flag) in [
                ("tail", "-n"),
                ("since", "--since"),
                ("from", "--from"),
                ("to", "--to"),
            ] {
                let max = if name == "tail" {
                    MAX_LOG_LINES
                } else {
                    u64::MAX
                };
                if let Some(n) = a.opt_u64(name, max)? {
                    argv.extend([s(flag), n.to_string()]);
                }
            }
            read(argv)
        }
        "serial_send" => {
            let target = a.target()?;
            let text = a.opt_str("text")?.ok_or(format!(
                "{tool} needs text (\"\" with newline sends just Enter)"
            ))?;
            let mut argv = vec![s("serial"), s("send"), s("-t"), target];
            if let Some(i) = a.opt_str("interface")? {
                argv.extend([s("-i"), i]);
            }
            if let Some(p) = a.opt_u64("pace_ms", MAX_PACE_MS)? {
                argv.extend([s("--pace-ms"), p.to_string()]);
            }
            if !a.opt_bool("newline")?.unwrap_or(true) {
                argv.push(s("--no-newline"));
            }
            // `--` so text that starts with `-` stays text.
            argv.extend([s("--"), text]);
            write(argv)
        }
        "hid_send" => {
            let target = a.target()?;
            let words: Vec<String> = match args.get("command") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|w| w.as_str().map(str::to_string))
                    .collect::<Option<_>>()
                    .ok_or(format!("{tool}: command must be an array of strings"))?,
                _ => return Err(format!("{tool} needs command, an array of strings")),
            };
            match words.first() {
                None => return Err(format!("{tool}: command is empty")),
                Some(w) if w.is_empty() || w.starts_with('-') => {
                    return Err(format!("{tool}: command must start with a verb, got {w:?}"))
                }
                Some(_) => {}
            }
            let mut argv = vec![s("hid"), s("send"), s("-t"), target];
            argv.extend(words);
            write(argv)
        }
        "power_state" => read(vec![s("power-state"), s("-t"), a.target()?]),
        "power_on" => write(vec![s("power"), s("on"), s("-t"), a.target()?]),
        "power_off" => write(vec![s("power"), s("off"), s("-t"), a.target()?]),
        "power_cycle" => write(vec![s("power-cycle"), s("-t"), a.target()?]),
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Validated `video_shot` arguments.
#[derive(Debug, PartialEq)]
struct ShotRequest {
    target: String,
    changed_since: Option<String>,
    stable: bool,
    timeout_ms: u64,
}

impl ShotRequest {
    fn from_args(args: &Value) -> Result<Self, String> {
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or("video_shot needs a target")?
            .to_string();
        let changed_since = match args.get("changed_since") {
            None | Some(Value::Null) => None,
            Some(Value::String(h)) => Some(h.clone()),
            Some(_) => return Err("changed_since must be a string".into()),
        };
        let stable = match args.get("stable") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("stable must be a boolean".into()),
        };
        let timeout_ms = match args.get("timeout_ms") {
            None | Some(Value::Null) => DEFAULT_SHOT_TIMEOUT_MS,
            Some(v) => v
                .as_u64()
                .filter(|t| *t <= MAX_SHOT_TIMEOUT_MS)
                .ok_or(format!(
                    "timeout_ms must be an integer from 0 to {MAX_SHOT_TIMEOUT_MS}"
                ))?,
        };
        Ok(Self {
            target,
            changed_since,
            stable,
            timeout_ms,
        })
    }

    /// The `paniolo` argv (after the global flags) for this shot.
    fn argv(&self, out: &Path) -> Vec<String> {
        // `-t` rather than a positional, so a target named like an option
        // can never be read as one.
        let mut a = vec![
            "video".to_string(),
            "shot".to_string(),
            "-t".to_string(),
            self.target.clone(),
            "--timeout".to_string(),
            self.timeout_ms.to_string(),
            "--out".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        if self.stable {
            a.push("--stable".to_string());
        }
        if let Some(h) = &self.changed_since {
            a.push("--changed-since".to_string());
            a.push(h.clone());
        }
        a
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "paniolo", "version": crate::VERSION },
        "instructions": "Drive embedded target machines managed by paniolo. \
            Call target_list first to see the targets and their channels. \
            To see the screen, video_shot returns it as an image plus a hash; after \
            acting (hid_send, serial_send), pass that hash back as changed_since to \
            wait for the screen to change. video_read returns the screen's text, and \
            serial_log the console output. A write tool (hid_send, serial_send, \
            power_*) whose outcome is unknown says so: check state before retrying it.",
    })
}

fn tool_list() -> Value {
    json!([
        {
            "name": "target_list",
            "description": "List the lab's targets: each target's name, description, \
                the control hosts it lives on, and its channels (video, serial, hid, \
                power, …) with the host each runs on.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "video_shot",
            "description": "Capture the target's current screen from its HDMI-capture \
                (video) channel. Returns the PNG and a text line `signal=<s>  hash=<h>`. \
                The video daemon must already be running (`paniolo video watch <target>`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Target name, as listed by target_list." },
                    "changed_since": {
                        "type": "string",
                        "description": "Wait until the frame's hash differs from this one (the hash a previous video_shot returned)."
                    },
                    "stable": { "type": "boolean", "description": "Wait until the signal is stable before capturing." },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": MAX_SHOT_TIMEOUT_MS,
                        "description": "How long to wait for changed_since/stable, in ms (default 2000)."
                    }
                },
                "required": ["target"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "video_read",
            "description": "Read the text on the target's screen: OCR of the current \
                video frame (BIOS menus, boot messages, console text). Cheaper than a \
                screenshot when only the words matter. Small console fonts can confuse \
                1/l/I; check a critical string with video_shot.",
            "inputSchema": schema(&[
                ("stable", json!({ "type": "boolean", "description": "Wait until the signal is stable before reading." })),
                ("timeout_ms", json!({ "type": "integer", "minimum": 0, "maximum": MAX_SHOT_TIMEOUT_MS, "description": "Longest stable wait, in ms (default 2000)." })),
            ]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "serial_status",
            "description": "Read configured and active serial identity, process liveness, port connection, capture epoch and RX/TX progress as JSON. Never starts or restarts a daemon. Driver acceptance and RX traffic do not prove target command execution. Older daemons may omit health fields; missing evidence is unknown.",
            "inputSchema": schema(&[("interface", json!({"type": "string"}))]),
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "serial_evidence",
            "description": "Read immutable receive chunks from the offline journal as JSON. Exact byte arrays, original RX timestamps, epoch and connection generation are preserved. Reading never opens hardware. Includes cap, epoch and gap/corruption diagnostics; RX does not prove execution. since requires epoch; max_bytes caps raw bytes, with bounded extra JSON overhead.",
            "inputSchema": schema(&[
                ("interface", json!({"type":"string"})),
                ("epoch", json!({"type":"string"})),
                ("since", json!({"type":"integer", "minimum":0})),
                ("max_bytes", json!({"type":"integer", "minimum":1, "maximum": crate::serial::MAX_EVIDENCE_BYTES})),
            ]),
            "annotations": {"readOnlyHint":true},
        },
        {
            "name": "serial_log",
            "description": "Read the target's serial console output (boot log, kernel \
                messages, shell output) from the capture log. Each line is \
                `[time] #<seq> <text>`; pass the last seq back as `since` to get only \
                newer lines. With no window it returns the last 200 lines. The serial \
                daemon must be running (`paniolo serial watch <target>`), or the log may \
                be stale (a warning says so).",
            "inputSchema": schema(&[
                ("interface", json!({ "type": "string", "description": "Serial interface name, for a target with several (see target_list)." })),
                ("tail", json!({ "type": "integer", "minimum": 0, "maximum": MAX_LOG_LINES, "description": "Only the most recent N lines." })),
                ("since", json!({ "type": "integer", "minimum": 0, "description": "Only lines with a seq greater than this." })),
                ("from", json!({ "type": "integer", "minimum": 0, "description": "Lowest seq to return (inclusive)." })),
                ("to", json!({ "type": "integer", "minimum": 0, "description": "Highest seq to return (inclusive)." })),
            ]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "serial_send",
            "description": "Type text into the target's serial console, followed by Enter \
                unless newline is false. Use serial_log afterwards to read the reply.",
            "inputSchema": schema_requiring(&["text"], &[
                ("text", json!({ "type": "string", "description": "Text to send. \"\" with newline true sends just Enter." })),
                ("interface", json!({ "type": "string", "description": "Serial interface name, for a target with several." })),
                ("newline", json!({ "type": "boolean", "description": "Append Enter (a carriage return). Default true." })),
                ("pace_ms", json!({ "type": "integer", "minimum": 0, "maximum": MAX_PACE_MS, "description": "Delay between bytes, for slow polled consoles (default 0)." })),
            ]),
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false },
        },
        {
            "name": "hid_send",
            "description": "Send keyboard or mouse input to the target through its USB HID \
                injector. command is one HID command as words, for example \
                [\"type\", \"hello\"], [\"key\", \"ENTER\"], [\"combo\", \"CONTROL\", \"ALT\", \"DELETE\"], \
                [\"moveabs\", \"16384\", \"16384\"] (0..32767 across the screen, not pixels), \
                [\"click\", \"left\"], [\"scroll\", \"-3\"]. After acting, use video_shot with \
                changed_since to see the result. The full vocabulary is in \
                `paniolo skill kvm-puppeting`.",
            "inputSchema": schema_requiring(&["command"], &[
                ("command", json!({ "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "The verb, then its arguments, one string each." })),
            ]),
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false },
        },
        {
            "name": "power_state",
            "description": "Report whether the target is powered on: prints on or off.",
            "inputSchema": schema(&[]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "power_on",
            "description": "Switch the target's power on.",
            "inputSchema": schema(&[]),
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
        },
        {
            "name": "power_off",
            "description": "Switch the target's power off (a hard power-off, not a shutdown).",
            "inputSchema": schema(&[]),
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
        },
        {
            "name": "power_cycle",
            "description": "Power-cycle the target: off, then on (a hard reset). When the \
                target has a power state reader, the call returns once power is back on; \
                otherwise only once the cycle was requested. If the call fails with an \
                unknown outcome, check power_state before trying again.",
            "inputSchema": schema(&[]),
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false },
        },
    ])
}

/// An input schema with a required `target` plus `extra` optional properties.
fn schema(extra: &[(&str, Value)]) -> Value {
    schema_requiring(&[], extra)
}

/// [`schema`], with `required` listed as required alongside `target`.
fn schema_requiring(required: &[&str], extra: &[(&str, Value)]) -> Value {
    let mut props = serde_json::Map::new();
    props.insert(
        "target".into(),
        json!({ "type": "string", "description": "Target name, as listed by target_list." }),
    );
    for (name, prop) in extra {
        props.insert((*name).into(), prop.clone());
    }
    let mut req = vec!["target"];
    req.extend(required);
    json!({ "type": "object", "properties": props, "required": req, "additionalProperties": false })
}

fn result_reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_text(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn tool_error(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// Standard base64 (RFC 4648, padded) — the one encoding MCP image content
/// needs, and too small to justify a dependency.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(exe: PathBuf, lab: Option<String>) -> Server {
        Server { exe, lab }
    }

    /// Drive the server over in-memory stdin/stdout, one request per line.
    fn exchange(s: &Server, requests: &[Value]) -> Vec<Value> {
        let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
        let mut output = Vec::new();
        s.run(input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn req(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "{input:?}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn initialize_echoes_a_known_version_and_offers_tools() {
        let s = server(PathBuf::from("/nonexistent"), None);
        let r = exchange(
            &s,
            &[req(
                1,
                "initialize",
                json!({ "protocolVersion": "2025-06-18" }),
            )],
        );
        assert_eq!(r[0]["id"], 1);
        assert_eq!(r[0]["result"]["protocolVersion"], "2025-06-18");
        assert!(r[0]["result"]["capabilities"]["tools"].is_object());
        assert_eq!(r[0]["result"]["serverInfo"]["name"], "paniolo");
    }

    #[test]
    fn initialize_answers_an_unknown_version_with_the_newest() {
        let s = server(PathBuf::from("/nonexistent"), None);
        let r = exchange(
            &s,
            &[req(
                1,
                "initialize",
                json!({ "protocolVersion": "1999-01-01" }),
            )],
        );
        assert_eq!(r[0]["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    /// Notifications get no reply, and blank lines are skipped, so the reply
    /// stream lines up one-to-one with the requests that carry an id.
    #[test]
    fn notifications_and_blank_lines_get_no_reply() {
        let s = server(PathBuf::from("/nonexistent"), None);
        let input = format!(
            "{}\n\n{}\n",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            req(7, "ping", json!({}))
        );
        let mut output = Vec::new();
        s.run(input.as_bytes(), &mut output).unwrap();
        let replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], 7);
        assert_eq!(replies[0]["result"], json!({}));
    }

    #[test]
    fn protocol_errors_carry_json_rpc_codes() {
        let s = server(PathBuf::from("/nonexistent"), None);
        let mut output = Vec::new();
        s.run("not json\n[1,2]\n".as_bytes(), &mut output).unwrap();
        let replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(replies[1]["error"]["code"], INVALID_REQUEST);

        let r = exchange(
            &s,
            &[
                req(1, "resources/list", json!({})),
                req(2, "tools/call", json!({ "name": "no_such_tool" })),
                req(
                    3,
                    "tools/call",
                    json!({ "name": "video_shot", "arguments": {} }),
                ),
            ],
        );
        assert_eq!(r[0]["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(r[1]["error"]["code"], INVALID_PARAMS);
        assert_eq!(r[2]["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn tools_list_names_every_tool_with_object_schemas() {
        let s = server(PathBuf::from("/nonexistent"), None);
        let r = exchange(&s, &[req(1, "tools/list", json!({}))]);
        let tools = r[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "target_list",
                "video_shot",
                "video_read",
                "serial_status",
                "serial_evidence",
                "serial_log",
                "serial_send",
                "hid_send",
                "power_state",
                "power_on",
                "power_off",
                "power_cycle"
            ]
        );
        for t in tools {
            let name = t["name"].as_str().unwrap();
            assert_eq!(t["inputSchema"]["type"], "object", "{name}");
            if name != "target_list" {
                assert!(
                    t["inputSchema"]["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("target")),
                    "{name}"
                );
            }
            // A tool that changes the target must not advertise itself as
            // read-only, or a harness may run it without asking.
            let writes = [
                "serial_send",
                "hid_send",
                "power_on",
                "power_off",
                "power_cycle",
            ]
            .contains(&name);
            assert_eq!(t["annotations"]["readOnlyHint"], !writes, "{name}");
        }
    }

    /// One valid call per CLI-backed tool, with every optional argument set.
    fn sample_calls() -> Vec<(&'static str, Value)> {
        vec![
            (
                "serial_evidence",
                json!({"target":"pi5", "interface":"console", "epoch":"capture-1", "since":0, "max_bytes":4096}),
            ),
            (
                "serial_status",
                json!({"target": "pi5", "interface": "console"}),
            ),
            (
                "video_read",
                json!({ "target": "pi5", "stable": true, "timeout_ms": 500 }),
            ),
            (
                "serial_log",
                json!({ "target": "pi5", "interface": "uart0", "tail": 50, "since": 7, "from": 1, "to": 9 }),
            ),
            (
                "serial_send",
                json!({ "target": "pi5", "text": "-rf /", "interface": "uart0", "newline": false, "pace_ms": 5 }),
            ),
            ("serial_send", json!({ "target": "pi5", "text": "" })),
            (
                "hid_send",
                json!({ "target": "pi5", "command": ["move", "50", "-30"] }),
            ),
            (
                "hid_send",
                json!({ "target": "pi5", "command": ["type", "--help"] }),
            ),
            ("power_state", json!({ "target": "pi5" })),
            ("power_on", json!({ "target": "pi5" })),
            ("power_off", json!({ "target": "pi5" })),
            ("power_cycle", json!({ "target": "pi5" })),
        ]
    }

    /// Every argv a tool builds must be one the real CLI parser accepts, with
    /// the global flags the server adds in front. Text and HID words that look
    /// like options (`-rf /`, `-30`, `--help`) are the cases that would break.
    #[test]
    fn every_tool_argv_parses_with_the_real_cli() {
        use clap::Parser;
        for (tool, args) in sample_calls() {
            let call = cli_call(tool, &args).unwrap();
            let mut argv = vec!["paniolo", "--lab", "/l.toml", "--json-errors"];
            argv.extend(call.argv.iter().map(String::as_str));
            if let Err(e) = crate::Cli::try_parse_from(&argv) {
                panic!("{tool}: {argv:?} rejected:\n{e}");
            }
        }
    }

    #[test]
    fn serial_evidence_requires_epoch_and_rejects_unbounded_caps() {
        use clap::Parser;
        for args in [
            json!({"target":"pi5", "since":1}),
            json!({"target":"pi5", "max_bytes":0}),
            json!({"target":"pi5", "max_bytes":1048577}),
        ] {
            assert!(cli_call("serial_evidence", &args).is_err());
        }
        assert!(crate::Cli::try_parse_from([
            "paniolo", "serial", "evidence", "pi5", "--since", "1"
        ])
        .is_err());
        assert!(crate::Cli::try_parse_from([
            "paniolo",
            "serial",
            "evidence",
            "pi5",
            "--epoch",
            "one",
            "--since",
            "1",
            "--max-bytes",
            "4096"
        ])
        .is_ok());
    }

    #[test]
    fn cli_calls_mark_writes_and_map_arguments() {
        let send = cli_call(
            "serial_send",
            &json!({ "target": "pi5", "text": "ls", "newline": false }),
        )
        .unwrap();
        assert!(send.writes);
        assert_eq!(
            send.argv,
            ["serial", "send", "-t", "pi5", "--no-newline", "--", "ls"]
        );
        assert_eq!(send.verb(), "serial send");

        let log = cli_call("serial_log", &json!({ "target": "pi5", "since": 42 })).unwrap();
        assert!(!log.writes);
        assert_eq!(log.argv, ["serial", "log", "-t", "pi5", "--since", "42"]);

        let cycle = cli_call("power_cycle", &json!({ "target": "pi5" })).unwrap();
        assert!(cycle.writes);
        assert_eq!(cycle.argv, ["power-cycle", "-t", "pi5"]);
        assert!(
            !cli_call("power_state", &json!({ "target": "pi5" }))
                .unwrap()
                .writes
        );
    }

    #[test]
    fn cli_calls_reject_bad_arguments() {
        for (tool, args) in [
            ("power_on", json!({})),
            ("serial_send", json!({ "target": "pi5" })),
            ("serial_send", json!({ "target": "pi5", "text": 7 })),
            (
                "serial_send",
                json!({ "target": "pi5", "text": "x", "pace_ms": MAX_PACE_MS + 1 }),
            ),
            (
                "serial_log",
                json!({ "target": "pi5", "tail": MAX_LOG_LINES + 1 }),
            ),
            ("serial_log", json!({ "target": "pi5", "since": -1 })),
            ("hid_send", json!({ "target": "pi5" })),
            ("hid_send", json!({ "target": "pi5", "command": [] })),
            ("hid_send", json!({ "target": "pi5", "command": "type hi" })),
            (
                "hid_send",
                json!({ "target": "pi5", "command": ["type", 1] }),
            ),
            (
                "hid_send",
                json!({ "target": "pi5", "command": ["-t", "other"] }),
            ),
            ("video_read", json!({ "target": "pi5", "stable": "yes" })),
            ("no_such_tool", json!({ "target": "pi5" })),
        ] {
            assert!(cli_call(tool, &args).is_err(), "{tool} {args}");
        }
    }

    #[test]
    fn shot_request_validates_and_builds_argv() {
        let r = ShotRequest::from_args(&json!({
            "target": "pi5", "changed_since": "abc", "stable": true, "timeout_ms": 5000
        }))
        .unwrap();
        assert_eq!(
            r.argv(Path::new("/tmp/x.png")),
            [
                "video",
                "shot",
                "-t",
                "pi5",
                "--timeout",
                "5000",
                "--out",
                "/tmp/x.png",
                "--stable",
                "--changed-since",
                "abc"
            ]
        );
        let d = ShotRequest::from_args(&json!({ "target": "pi5" })).unwrap();
        assert_eq!(d.timeout_ms, DEFAULT_SHOT_TIMEOUT_MS);
        assert!(!d.stable && d.changed_since.is_none());

        for bad in [
            json!({}),
            json!({ "target": "" }),
            json!({ "target": "pi5", "timeout_ms": MAX_SHOT_TIMEOUT_MS + 1 }),
            json!({ "target": "pi5", "timeout_ms": -1 }),
            json!({ "target": "pi5", "stable": "yes" }),
            json!({ "target": "pi5", "changed_since": 7 }),
        ] {
            assert!(ShotRequest::from_args(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn target_list_reads_the_lab() {
        let dir = tempfile::tempdir().unwrap();
        let lab = dir.path().join("lab.toml");
        std::fs::write(
            &lab,
            "[targets.pi5]\ndescription = \"bench pi\"\n[targets.pi5.video]\ndevice = \"cap0\"\n",
        )
        .unwrap();
        let s = server(
            PathBuf::from("/nonexistent"),
            Some(lab.to_string_lossy().into_owned()),
        );
        let r = exchange(
            &s,
            &[req(1, "tools/call", json!({ "name": "target_list" }))],
        );
        let result = &r[0]["result"];
        assert_eq!(result["isError"], false, "{result}");
        let targets: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(targets[0]["name"], "pi5");
        assert_eq!(targets[0]["description"], "bench pi");
        assert_eq!(targets[0]["channels"][0]["kind"], "video");
    }

    /// A fake `paniolo` that behaves like `video shot`: writes PNG bytes to
    /// the `--out` path and the status line to stderr. Proves the child is
    /// really run with the global flags first, its file read back, and its
    /// stdin closed (the script would block on `read` otherwise).
    ///
    /// The script is written by a short-lived `sh` child, never by this
    /// process. Tests run in parallel threads, and a thread that forks while
    /// this one holds the file open for writing gives its child a copy of
    /// that descriptor; exec'ing the script then fails with ETXTBSY ("Text
    /// file busy") until the child execs. That failed CI on main after #253.
    /// A pipe is the only thing this process holds, so no fork can inherit a
    /// writer on the script itself.
    #[cfg(unix)]
    fn fake_paniolo(dir: &Path, body: &str) -> PathBuf {
        use std::io::Write;
        let p = dir.join("paniolo");
        let mut writer = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(&p)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .unwrap();
        assert!(writer.wait().unwrap().success());
        p
    }

    #[cfg(unix)]
    #[test]
    fn video_shot_runs_the_cli_and_returns_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let args_log = dir.path().join("args");
        let exe = fake_paniolo(
            dir.path(),
            &format!(
                r#"echo "$@" > {log}
read -r _ && exit 9
while [ $# -gt 0 ]; do [ "$1" = --out ] && printf 'PNGDATA' > "$2"; shift; done
echo 'signal=ok  hash=00ff' >&2"#,
                log = args_log.display()
            ),
        );
        let s = server(exe, Some("/some/lab.toml".into()));
        let r = exchange(
            &s,
            &[req(
                1,
                "tools/call",
                json!({ "name": "video_shot", "arguments": { "target": "pi5" } }),
            )],
        );
        let result = &r[0]["result"];
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(result["content"][0]["type"], "image");
        assert_eq!(result["content"][0]["data"], base64(b"PNGDATA"));
        assert_eq!(result["content"][0]["mimeType"], "image/png");
        assert_eq!(result["content"][1]["text"], "signal=ok  hash=00ff");
        let args = std::fs::read_to_string(&args_log).unwrap();
        assert!(
            args.starts_with("--lab /some/lab.toml --json-errors video shot -t pi5"),
            "{args}"
        );
    }

    #[cfg(unix)]
    fn call(s: &Server, name: &str, arguments: Value) -> Value {
        let r = exchange(
            s,
            &[req(
                1,
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )],
        );
        r[0]["result"].clone()
    }

    #[cfg(unix)]
    #[test]
    fn cli_tool_returns_stdout_and_stderr_and_runs_the_argv() {
        let dir = tempfile::tempdir().unwrap();
        let args_log = dir.path().join("args");
        let exe = fake_paniolo(
            dir.path(),
            &format!(
                r#"echo "$@" > {log}
read -r _ && exit 9
echo '[2026-10-02T00:00:00.000Z] #12      login:'
echo 'warning: serialcap is not running; the log may be stale' >&2"#,
                log = args_log.display()
            ),
        );
        let s = server(exe, None);
        let result = call(&s, "serial_log", json!({ "target": "pi5", "tail": 5 }));
        assert_eq!(result["isError"], false, "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("[2026-10-02T00:00:00.000Z] #12      login:"),
            "{text}"
        );
        assert!(
            text.contains("stderr:\nwarning: serialcap is not running"),
            "{text}"
        );
        let args = std::fs::read_to_string(&args_log).unwrap();
        assert_eq!(args.trim(), "--json-errors serial log -t pi5 -n 5");
    }

    #[cfg(unix)]
    #[test]
    fn empty_output_reports_ok() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(fake_paniolo(dir.path(), "exit 0"), None);
        let result = call(&s, "power_on", json!({ "target": "pi5" }));
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "paniolo power on: ok");
    }

    /// An unreachable host or a timeout leaves a write's outcome unknown, and
    /// the error must say so; the same exit from a read needs no such warning,
    /// and neither does a write that failed for a known reason.
    #[cfg(unix)]
    #[test]
    fn unknown_outcome_is_flagged_only_for_writes_that_may_have_run() {
        let dir = tempfile::tempdir().unwrap();
        for (code, tool, flagged) in [
            (4, "power_cycle", true),
            (22, "hid_send", true),
            (4, "power_state", false),
            (3, "power_cycle", false),
        ] {
            let exe = fake_paniolo(dir.path(), &format!("echo 'ssh failed' >&2; exit {code}"));
            let s = server(exe, None);
            let args = if tool == "hid_send" {
                json!({ "target": "pi5", "command": ["key", "ENTER"] })
            } else {
                json!({ "target": "pi5" })
            };
            let result = call(&s, tool, args);
            assert_eq!(result["isError"], true, "{tool} {code}");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(&format!("exited {code}")), "{text}");
            assert!(text.contains("ssh failed"), "{text}");
            assert_eq!(
                text.contains("Outcome unknown"),
                flagged,
                "{tool} {code}: {text}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn video_shot_failure_is_a_tool_error_with_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_paniolo(dir.path(), "echo 'no video daemon running' >&2; exit 6");
        let s = server(exe, None);
        let r = exchange(
            &s,
            &[req(
                1,
                "tools/call",
                json!({ "name": "video_shot", "arguments": { "target": "pi5" } }),
            )],
        );
        let result = &r[0]["result"];
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("exited 6"), "{text}");
        assert!(text.contains("no video daemon running"), "{text}");
    }
}
