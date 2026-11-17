#!/bin/bash
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

# Download the .debs of the newest <count> published GitHub Releases into
# <out-dir>, for build-apt-repo.sh to assemble into the apt pool docs.yml
# deploys. Either every expected .deb arrives or this exits non-zero.
#
# The pool is rebuilt from scratch and a Pages deploy replaces the whole
# site, so a .deb that fails to download is not a warning: it would publish a
# pool without that release, and apt clients would lose it (GitHub #407).
# Before this script the loop treated any `gh release download` failure as
# "no .deb assets", so an HTTP 503 on the newest release shipped a pool of
# only older ones.
#
# Each release's .deb names come from its metadata first. A release whose
# assets include no .deb is the only one skipped. Any other failure (listing
# releases, reading a release's assets, downloading) is retried a bounded
# number of times and then fails the run. After the downloads every expected
# file must be present and non-empty, so a partial download (one
# architecture missing) fails too.
#
# Usage: fetch-apt-debs.sh <out-dir> <owner/repo> <count>
#
# Environment (for tests; the defaults suit CI):
#   APT_FETCH_ATTEMPTS  tries per gh call (default 3)
#   APT_FETCH_BACKOFF   seconds before the first retry, doubled after each
#                       failure (default 10)
#
# Tested offline with a mock gh by scripts/tests/test_fetch_apt_debs.py.

set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <out-dir> <owner/repo> <count>" >&2
  exit 2
fi

OUT="$1"
REPO="$2"
COUNT="$3"
ATTEMPTS="${APT_FETCH_ATTEMPTS:-3}"
BACKOFF="${APT_FETCH_BACKOFF:-10}"

# Run "$@" up to $ATTEMPTS times, doubling the wait between tries. stdout is
# the successful attempt's only, so the caller can capture it.
retry() {
  local n=1 wait="$BACKOFF" out
  while :; do
    if out="$("$@")"; then
      [ -z "$out" ] || printf '%s\n' "$out"
      return 0
    fi
    if [ "$n" -ge "$ATTEMPTS" ]; then
      return 1
    fi
    echo "::warning::attempt $n of $ATTEMPTS failed: $*; retrying in ${wait}s" >&2
    sleep "$wait"
    n=$((n + 1))
    wait=$((wait * 2))
  done
}

mkdir -p "$OUT"

tags="$(retry gh release list --repo "$REPO" \
  --exclude-drafts --exclude-pre-releases \
  --limit "$COUNT" --json tagName --jq '.[].tagName')" || {
  echo "::error::could not list releases of $REPO" >&2
  exit 1
}
if [ -z "$tags" ]; then
  echo "::error::$REPO has no published releases" >&2
  exit 1
fi

expected=()
while IFS= read -r tag; do
  [ -n "$tag" ] || continue
  names="$(retry gh release view "$tag" --repo "$REPO" \
    --json assets --jq '.assets[].name | select(endswith(".deb"))')" || {
    echo "::error::could not read the assets of $tag" >&2
    exit 1
  }
  if [ -z "$names" ]; then
    echo "::notice::$tag has no .deb assets; skipped"
    continue
  fi
  # --clobber so a retry overwrites what a failed attempt left behind.
  retry gh release download "$tag" --repo "$REPO" \
    --pattern '*.deb' --dir "$OUT" --clobber >/dev/null || {
    echo "::error::could not download the .deb assets of $tag" >&2
    exit 1
  }
  while IFS= read -r name; do
    if [ -n "$name" ]; then expected+=("$name"); fi
  done <<<"$names"
done <<<"$tags"

if [ "${#expected[@]}" -eq 0 ]; then
  echo "::error::none of the newest $COUNT releases of $REPO has a .deb asset" >&2
  exit 1
fi

missing=0
for name in "${expected[@]}"; do
  if [ ! -s "$OUT/$name" ]; then
    echo "::error::$name is listed on its release but did not download" >&2
    missing=1
  fi
done
[ "$missing" -eq 0 ] || exit 1

echo "fetched ${#expected[@]} .deb files into $OUT"
