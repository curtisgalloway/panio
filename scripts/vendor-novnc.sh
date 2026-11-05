#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 The Panio Authors
# SPDX-License-Identifier: Apache-2.0
#
# Vendor noVNC as one bundled ES module for the hdmicap dashboard, so the page
# works on an isolated lab network (no CDN), like the vendored xterm.js.
#
# Pinned: @novnc/novnc 1.7.0 (published 2026-04-28, the newest release at
# least two weeks old when pinned; later npm versions are untagged snapshots).
# The tarball is checked against the npm integrity value below before use.
# Bundler: esbuild, pinned to an exact version and fetched through npx.
#
# Usage: scripts/vendor-novnc.sh   (needs node/npm, curl, openssl, tar)
set -euo pipefail

NOVNC_VERSION="1.7.0"
NOVNC_INTEGRITY="sha512-ucEJOx4T2avIRCleodk7YobZj5O2Ga2AeLfQ69A/yjG9HHba2+PDgwSkN3FttrmG+70ZGx21sElNFouK13RzyA=="
ESBUILD_VERSION="0.28.2"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
assets="$root/hdmicap/assets"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

tarball="$work/novnc.tgz"
curl -fsSL -o "$tarball" \
  "https://registry.npmjs.org/@novnc/novnc/-/novnc-${NOVNC_VERSION}.tgz"

actual="sha512-$(openssl dgst -sha512 -binary "$tarball" | openssl base64 -A)"
if [ "$actual" != "$NOVNC_INTEGRITY" ]; then
  echo "integrity mismatch for novnc-${NOVNC_VERSION}.tgz" >&2
  echo "  expected $NOVNC_INTEGRITY" >&2
  echo "  actual   $actual" >&2
  exit 1
fi
echo "integrity ok: $actual"

src="$work/src"
mkdir "$src"
tar -xzf "$tarball" -C "$src"

(
  cd "$work"
  npx --yes "esbuild@${ESBUILD_VERSION}" "$src/package/core/rfb.js" \
    --bundle --format=esm --minify --target=es2022 \
    --legal-comments=inline --outfile="$assets/novnc.js"
)

# noVNC's own notice, the full MPL-2.0 text it points at, and the MIT notice
# for the pako zlib port that the bundle includes.
{
  cat "$src/package/LICENSE.txt"
  printf '\n\n==== docs/LICENSE.MPL-2.0 ====\n\n'
  cat "$src/package/docs/LICENSE.MPL-2.0"
  printf '\n\n==== vendor/pako/LICENSE (MIT) ====\n\n'
  cat "$src/package/vendor/pako/LICENSE"
} > "$assets/novnc-LICENSE.txt"
echo "wrote $assets/novnc.js ($(wc -c < "$assets/novnc.js") bytes)"
