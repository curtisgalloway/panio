#!/usr/bin/env bash
# Copyright 2026 The Panio Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# Fail when any committed Cargo.lock is out of sync with its Cargo.toml.
#
# No crate job builds with --locked, so cargo quietly rewrites a stale lockfile
# in the runner's checkout and the build goes green. That hid two problems:
# netbootd/Cargo.lock had no entry for its socket2 dependency, and Dependabot's
# lockfile-only batch #419 pinned windows-sys 0.61 and thiserror 2 while the
# manifests still required 0.59 and 1. A green run then said nothing about the
# lockfile that ships.
#
# `cargo metadata --locked` resolves every target's dependencies (the lockfile
# is platform-independent, so Windows- and macOS-only deps are checked here
# too) and fails if the lockfile would need any change. It downloads the
# index but builds nothing.
#
# Only directories with a committed Cargo.lock are checked. ocr/winocr has no
# lockfile; it is built fresh in the windows job.
#
# Fix a failure by running `cargo update --workspace` (or any cargo build) in
# that crate directory and committing the rewritten Cargo.lock.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 2

failed=()
while IFS= read -r lock; do
  dir="$(dirname "$lock")"
  if out="$(cd "$dir" && cargo metadata --locked --format-version 1 2>&1 >/dev/null)"; then
    echo "ok    $dir"
  else
    echo "FAIL  $dir"
    printf '%s\n' "$out" | sed 's/^/      /'
    failed+=("$dir")
  fi
done < <(git ls-files '*Cargo.lock')

if ((${#failed[@]})); then
  echo
  echo "Cargo.lock out of sync with Cargo.toml in: ${failed[*]}"
  echo "Run 'cargo update --workspace' in each and commit the lockfile."
  exit 1
fi
