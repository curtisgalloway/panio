#!/usr/bin/env bash
# Copyright 2026 Curtis Galloway
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

# Reproduce the Linux GitHub CI jobs (.github/workflows/ci.yml) locally, so you
# can catch failures before pushing. Run it inside a Linux environment:
#
#   # On a Mac, via a Lima VM (Linux on macOS):
#   limactl shell <instance> -- bash -l <repo>/scripts/ci-local.sh
#
#   # Or directly on a Linux box / dev container:
#   bash scripts/ci-local.sh
#
# It mirrors every Linux crate job in ci.yml; scripts/ci-coverage-check.sh
# enforces that the two stay in sync. The lint jobs (shellcheck, zigplug,
# evals-check, actions-pinned, release-guards) are deliberately not mirrored: none needs a
# Linux box, so run them on the host -- "Before opening a PR" in AGENTS.md
# has the one-line command for each.
#
# It installs the toolchain if missing (rustup + uv + apt build deps) and copies
# the working tree to a VM-local dir before building, so nothing is written to a
# shared host mount (a virtiofs/9p mount rejects setuptools' editable egg-info
# and would clobber a host cargo target/). The macOS-only CI job (hdmicap
# AVFoundation + visionocr) is not covered here — run it on the host.
# Like the skippable CI jobs, it deletes every doc-only file from the copy before
# building (scripts/ci-doc-only.sh strip), so a crate that reads one fails here.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$(cd "$SCRIPT_DIR/.." && pwd)"
DST="${PANIO_CI_DIR:-$HOME/.cache/panio-ci-src}"
export DEBIAN_FRONTEND=noninteractive

echo "### [setup] system deps"
# Wait for the dpkg lock rather than failing on it: a freshly booted VM usually
# has unattended-upgrades holding it for the first few minutes. And abort if the
# install still fails -- without these libraries every serialport crate (cli,
# serialcap, cambrionix, ch9329, hidrig, dsdble) and hdmicap fail to build, which reads
# as six code failures instead of one missing dependency.
APT=(-o DPkg::Lock::Timeout=300)
if ! sudo apt-get "${APT[@]}" update -qq \
  || ! sudo apt-get "${APT[@]}" install -y -qq pkg-config libudev-dev libdbus-1-dev build-essential \
       libclang-dev clang cmake nasm libturbojpeg0-dev curl ca-certificates rsync >/dev/null
then
  echo "FATAL: could not install the system build dependencies; aborting." >&2
  echo "       Re-run once apt is free, or install them by hand." >&2
  exit 2
fi

# Installers are downloaded to a file and run from there, never piped into
# sh: the file is what actually ran, so its path and sha256 are logged and it
# can be read or diffed against a known copy while the script is running.
INSTALLERS="$(mktemp -d)"
trap 'rm -rf "$INSTALLERS"' EXIT
fetch_installer () {
  local url="$1" out="$INSTALLERS/$2"
  curl --proto '=https' --tlsv1.2 -fsSL -o "$out" "$url" || return 1
  echo "###   fetched $url"
  sha256sum "$out"
}

if ! command -v cargo >/dev/null 2>&1; then
  echo "### [setup] rustup (stable, minimal + clippy + rustfmt)"
  fetch_installer https://sh.rustup.rs rustup-init.sh \
    && sh "$INSTALLERS/rustup-init.sh" -y --profile minimal \
         --component clippy --component rustfmt >/dev/null
fi
# rustup writes ~/.cargo/env at install time; it is not in the repo, so there
# is nothing for the SC1091 source-follow to read.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

if ! command -v uv >/dev/null 2>&1; then
  echo "### [setup] uv"
  fetch_installer https://astral.sh/uv/install.sh uv-install.sh \
    && sh "$INSTALLERS/uv-install.sh" >/dev/null 2>&1
fi
export PATH="$HOME/.local/bin:$PATH"

if ! command -v cargo >/dev/null 2>&1; then
  echo "FATAL: cargo still not on PATH after setup; aborting." >&2
  exit 2
fi
echo "### toolchain: $(cargo --version) | $(uv --version 2>/dev/null || echo 'uv missing')"

echo "### [setup] copy working tree to local disk ($DST)"
# rsync --delete erases whatever else is in $DST, so refuse unless it is
# clearly ours: absent, empty, or carrying the marker this script leaves
# behind after its first sync. A PANIO_CI_DIR typo that lands on a real
# directory then fails loudly instead of emptying it.
MARKER=".panio-ci-local"
if [ -e "$DST" ]; then
  if [ ! -d "$DST" ]; then
    echo "FATAL: $DST exists and is not a directory; aborting." >&2
    exit 2
  fi
  if [ -n "$(ls -A "$DST")" ] && [ ! -f "$DST/$MARKER" ]; then
    echo "FATAL: $DST is not empty and has no $MARKER marker; refusing to" >&2
    echo "       rsync --delete into it. Point PANIO_CI_DIR at an empty or" >&2
    echo "       previously synced directory, or clear this one by hand." >&2
    exit 2
  fi
fi
mkdir -p "$DST"
rsync -a --delete \
  --exclude 'target' --exclude '.venv' --exclude '*.egg-info' \
  --exclude '_site' --exclude 'site' --exclude '.git' \
  --exclude "$MARKER" \
  "$SRC/" "$DST/"
touch "$DST/$MARKER"

# Same invariant as the skippable CI jobs: delete every doc-only file from the
# build copy, so a crate that starts to read one fails here too. The copy has
# no .git, so the script lists files with find.
echo "### [setup] strip doc-only files (scripts/ci-doc-only.sh)"
( cd "$DST" && PANIO_STRIP_NO_GIT=1 bash scripts/ci-doc-only.sh strip ) \
  || { echo "FATAL: strip failed; aborting." >&2; exit 2; }

declare -A RES

# Go: the jetkvm helper is not a Cargo crate. Needs Go >= the version in
# jetkvm/go.mod; a distro golang-go is usually too old, so install the
# official tarball under $HOME when `go` is missing.
ensure_go () {
  if command -v go >/dev/null 2>&1; then return 0; fi
  if [ -x "$HOME/.cache/panio-go/go/bin/go" ]; then
    export PATH="$HOME/.cache/panio-go/go/bin:$PATH"
    return 0
  fi
  echo "### [setup] go"
  local arch
  case "$(uname -m)" in
    x86_64) arch=amd64 ;;
    aarch64|arm64) arch=arm64 ;;
    *) echo "unsupported architecture for the Go install" >&2; return 1 ;;
  esac
  fetch_installer "https://go.dev/dl/go1.24.7.linux-$arch.tar.gz" go.tar.gz || return 1
  mkdir -p "$HOME/.cache/panio-go" \
    && tar -C "$HOME/.cache/panio-go" -xzf "$INSTALLERS/go.tar.gz" || return 1
  export PATH="$HOME/.cache/panio-go/go/bin:$PATH"
}

go_job () {
  local name="$1" dir="$2"
  echo
  echo "===== $name ====="
  (
    ensure_go || exit 91
    cd "$DST/$dir" || exit 90
    unformatted="$(gofmt -l .)"
    if [ -n "$unformatted" ]; then
      echo "gofmt would change:" >&2
      echo "$unformatted" >&2
      exit 1
    fi
    go vet ./... && go test -race ./...
  )
  RES["$name"]=$?
  echo "----- $name exit ${RES[$name]} -----"
}

# fmt + clippy (-D warnings) + a final build/test, mirroring each crate's CI job.
crate_job () {
  local name="$1" dir="$2" lastcmd="$3"
  echo
  echo "===== $name ====="
  (
    cd "$DST/$dir" || exit 90
    cargo fmt --check \
      && cargo clippy --all-targets -- -D warnings \
      && $lastcmd
  )
  RES["$name"]=$?
  echo "----- $name exit ${RES[$name]} -----"
}

crate_job "cli"        "cli"        "cargo test"
crate_job "serialcap"  "serialcap"  "cargo test"
crate_job "netbootd"   "netbootd"   "cargo test"
crate_job "hdmicap"    "hdmicap"    "cargo build"
crate_job "cambrionix" "cambrionix" "cargo test"
crate_job "dsdrelay"   "dsdrelay"   "cargo test"
crate_job "dsdble"     "dsdble"     "cargo test"
crate_job "ch9329"     "ch9329"     "cargo test"
crate_job "hidrig"     "hidrig"     "cargo test"
crate_job "shellyplug" "shellyplug" "cargo test"
crate_job "switchport" "switchport" "cargo test"
crate_job "amt"        "amt"        "cargo test"
crate_job "secret"     "secret"     "cargo test"
go_job    "jetkvm"     "jetkvm"

echo
echo "########## LOCAL CI SUMMARY ##########"
fail=0
for k in "cli" "serialcap" "netbootd" "hdmicap" "cambrionix" "dsdrelay" "dsdble" "ch9329" \
         "hidrig" "shellyplug" "switchport" "amt" "secret" "jetkvm"; do
  c="${RES[$k]:-NA}"
  if [ "$c" = "0" ]; then printf 'PASS       %s\n' "$k"; else printf 'FAIL(%s)  %s\n' "$c" "$k"; fail=1; fi
done
echo "######################################"
exit "$fail"
