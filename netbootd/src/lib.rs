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

//! Shared library for the `netbootd` binary and the `netbootd-bpf-helper`
//! setuid helper.
//!
//! Rust binaries in `src/bin/` are separate crate roots and cannot see modules
//! declared in `main.rs`, so anything both the daemon and the helper need lives
//! here: the raw-frame builder ([`frame`]) and the privileged BPF fd handoff
//! ([`handoff`]).

pub mod frame;
/// The privileged BPF fd handoff is built on `SCM_RIGHTS` over a Unix socket,
/// which has no Windows equivalent, so the module exists only on Unix.
#[cfg(unix)]
pub mod handoff;
/// The default-route lookup the setuid helper uses to refuse the primary NIC.
/// It runs `/sbin/route`, which only exists on macOS — the only platform with
/// the helper.
#[cfg(target_os = "macos")]
pub mod route;
