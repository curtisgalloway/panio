// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_amt"));
    command
        .env_remove("AMT_PASSWORD")
        .env_remove("AMT_TLS_CA_FILE");
    command
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn policy_validation_precedes_credential_resolution_and_network_access() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let output = command().args(["state", "-d", &address]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("requires --allow-insecure-http"));
    assert!(output.stdout.is_empty());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn ca_flag_overrides_environment_and_environment_is_used_when_flag_absent() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("flag.pem");
    let env = dir.path().join("environment.pem");
    for after_verb in [false, true] {
        let mut command = command();
        command
            .env("AMT_TLS_CA_FILE", &env)
            .args(["-d", "localhost"]);
        if after_verb {
            command.arg("state");
        }
        command.arg("--tls-ca-file").arg(&flag);
        if !after_verb {
            command.arg("state");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("flag.pem"));
        assert!(!stderr(&output).contains("environment.pem"));
    }
    let output = command()
        .env("AMT_TLS_CA_FILE", &env)
        .args(["-d", "localhost", "state"])
        .output()
        .unwrap();
    assert!(stderr(&output).contains("environment.pem"));
}

#[test]
fn insecure_http_has_visible_warning_and_clean_state_output() {
    for after_verb in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("localhost:{}", listener.local_addr().unwrap().port());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap();
            let len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            stream.read_exact(&mut vec![0; len]).unwrap();
            let body = "<PowerState>2</PowerState>";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let mut command = command();
        command
            .env("AMT_PASSWORD", "test-password")
            .args(["-d", &address]);
        if after_verb {
            command.arg("state");
        }
        command.arg("--allow-insecure-http");
        if !after_verb {
            command.arg("state");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(output.stdout, b"on\n");
        assert_eq!(stderr(&output).matches("WARNING:").count(), 1);
        assert!(stderr(&output).contains("unencrypted HTTP"));
        server.join().unwrap();
    }
}

#[cfg(unix)]
#[test]
fn rejected_transport_does_not_execute_password_command() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("password-command-ran");
    let cmd = format!("touch '{}'", marker.display());
    let output = command()
        .args([
            "-d",
            "http://localhost",
            "--password-command",
            &cmd,
            "state",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!marker.exists());
}
