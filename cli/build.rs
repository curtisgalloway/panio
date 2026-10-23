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

//! `PANIO_VERSION` is read at compile time by `option_env!` in
//! `src/main.rs`; without this hint cargo would keep a stale stamp in an
//! incremental build after the variable changed.

//!
//! On Windows the main thread's stack is whatever the linker reserves,
//! 1 MiB by default, against 8 MiB on Linux and macOS. The debug build of
//! the full command set overflowed it before parsing finished (exit
//! 0xC00000FD on `panio --help`), so reserve 8 MiB there too.

fn main() {
    println!("cargo:rerun-if-env-changed=PANIO_VERSION");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    match (target_os.as_str(), target_env.as_str()) {
        ("windows", "msvc") => println!("cargo:rustc-link-arg=/STACK:8388608"),
        ("windows", "gnu") => println!("cargo:rustc-link-arg=-Wl,--stack,8388608"),
        _ => {}
    }
}
