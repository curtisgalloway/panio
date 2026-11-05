// Copyright 2026 The Panio Authors
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

//! The interface carrying the host's default route — looked up in a way that
//! is safe to call from the setuid-root `netbootd-bpf-helper`.
//!
//! macOS only (the module is gated in `lib.rs`): `/sbin/route -n get default`
//! prints an `interface:` line naming the primary NIC. Because the helper runs
//! setuid, the command is spawned by **absolute path** with an **empty
//! environment** and no stdin, so whoever invoked the helper cannot steer the
//! lookup through `PATH`, `DYLD_*`, or anything else a child would inherit.
//! The daemon's own primary-NIC guard (`netcfg::is_primary_interface`) uses
//! this same lookup so the two can never disagree.

use std::process::{Command, Stdio};

/// The interface carrying the system default route, if any. `None` when there
/// is no default route (an offline bench) or `route` could not be run.
pub fn default_route_interface() -> Option<String> {
    let out = Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .ok()?;
    parse_route_get(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the interface name out of `route -n get default` output, which looks
/// like:
///
/// ```text
///    route to: default
/// destination: default
///        mask: default
///     gateway: 192.0.2.1
///   interface: en0
/// ```
///
/// When there is no default route the command prints nothing on stdout (its
/// complaint goes to stderr), so this yields `None`.
pub fn parse_route_get(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|l| l.trim().strip_prefix("interface:"))
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_interface_line() {
        let out = "   route to: default\n\
                   destination: default\n\
                   \x20      mask: default\n\
                   \x20   gateway: 192.0.2.1\n\
                   \x20 interface: en0\n\
                   \x20     flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>\n";
        assert_eq!(parse_route_get(out).as_deref(), Some("en0"));
    }

    #[test]
    fn no_interface_line_means_no_default_route() {
        assert_eq!(parse_route_get(""), None);
        assert_eq!(
            parse_route_get("route: writing to routing socket: not in table\n"),
            None
        );
        assert_eq!(parse_route_get("  interface: \n"), None);
    }
}
