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

fn main() {
    println!("cargo:rerun-if-env-changed=PANIO_VERSION");
}
