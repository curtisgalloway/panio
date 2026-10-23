// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0
#![cfg(unix)]

//! Contract test: `panio mcp` driving the real `panio lease` CLI. Catches any
//! drift between what `lease acquire --json` prints and what the MCP tool
//! parses. `HOME` is a scratch directory.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const LAB: &str = "[targets.dut]\n\n[[targets.dut.serial]]\nname = \"console\"\ndevice = \"/dev/ttyUSB-test\"\nbaud = 115200\n";

fn base(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_panio"));
    cmd.arg("--lab")
        .arg(dir.join("lab.toml"))
        .env("HOME", dir.join("home"))
        .env("PANIO_RUNTIME_BASE", dir.join("run"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env_remove("PANIO_JSON_ERRORS")
        .env_remove("PANIO_LAB")
        .env_remove("PANIO_LEASE");
    cmd
}

fn mcp(dir: &Path, calls: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut child = base(dir)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for (i, c) in calls.iter().enumerate() {
        let msg = serde_json::json!({"jsonrpc":"2.0","id":i+1,"method":"tools/call","params":c});
        writeln!(stdin, "{msg}").unwrap();
    }
    drop(stdin);
    let out: Output = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn mcp_acquire_parses_the_real_cli_output_and_hides_the_token() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mcp_lease");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("run")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::write(dir.join("lab.toml"), LAB).unwrap();
    let call = |name: &str| serde_json::json!({"name": name, "arguments": {"target": "dut"}});
    let r = mcp(
        &dir,
        &[
            call("lease_acquire"),
            call("lease_acquire"),
            call("lease_status"),
            call("lease_release"),
        ],
    );
    for (i, reply) in r.iter().enumerate() {
        assert_eq!(reply["result"]["isError"], false, "call {i}: {reply}");
    }
    let acquire_text = r[0]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(acquire_text.contains("Lease held on dut"), "{acquire_text}");
    assert!(acquire_text.contains("expires in"), "{acquire_text}");
    let status = r[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(status.contains("\"held\""), "{status}");
    let o = base(&dir)
        .args(["lease", "status", "dut", "--json"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("\"held\":false"),
        "{o:?}"
    );
}
