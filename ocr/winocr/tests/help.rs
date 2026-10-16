// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

use std::process::{Command, Stdio};

#[test]
fn help_exits_successfully_without_reading_an_image() {
    for flag in ["--help", "-h"] {
        let output = Command::new(env!("CARGO_BIN_EXE_winocr"))
            .args(["--json", "missing-image.png", flag])
            .stdin(Stdio::null())
            .output()
            .expect("run winocr");
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        for expected in ["Usage: winocr", "PATH", "stdin", "--json", "-h, --help"] {
            assert!(help.contains(expected), "missing {expected}: {help}");
        }
    }
}
