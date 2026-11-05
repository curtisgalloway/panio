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

# Enforce the supply-chain rule: every third-party GitHub Action is pinned to
# a full 40-hex commit SHA (Review M7).
#
# `uses: owner/repo@v3` resolves a movable tag at run time; whoever can move
# that tag (a compromised maintainer account, a hijacked repository) runs
# their code in this project's CI with its token. A commit SHA is immutable.
# The `actions-pinned` CI job runs this script; .github/dependabot.yml keeps
# the pins current by bumping the SHA and its `# vX.Y.Z` comment together.
#
# Accepted forms of a `uses:` line:
#   owner/repo[/path]@<40 hex>  # vX.Y.Z   the comment is required: it is how a
#                                          reader knows what the SHA is, and
#                                          how Dependabot knows what to bump
#   ./path/to/action                       an action in this repo (no ref)
#   docker://image@sha256:<64 hex>         a digest-pinned container image
#
# Run it anywhere: it needs nothing but a shell, find and grep.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

files=""
for dir in "$ROOT/.github/workflows" "$ROOT/.github/actions"; do
  [ -d "$dir" ] || continue
  found="$(find "$dir" -type f \( -name '*.yml' -o -name '*.yaml' \) | sort)"
  files="$files${found:+$found
}"
done
if [ -z "$files" ]; then
  echo "FATAL: no workflow files found under $ROOT/.github" >&2
  exit 2
fi

total=0
bad=0
while IFS= read -r file; do
  [ -n "$file" ] || continue
  rel="${file#"$ROOT"/}"
  # Every `uses:` line that is not itself a YAML comment.
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    lineno="${hit%%:*}"
    line="${hit#*:}"
    # The reference: the text after `uses:`, quotes stripped, comment dropped.
    ref="$(printf '%s' "$line" \
      | sed -E 's/^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]*//; s/[[:space:]]+#.*$//; s/^["'"'"']//; s/["'"'"']$//')"
    comment="$(printf '%s' "$line" | sed -nE 's/^.*[[:space:]]+#[[:space:]]*(.*)$/\1/p')"
    total=$((total + 1))
    case "$ref" in
      ./*)
        echo "  ok    $rel:$lineno $ref (local action)"
        ;;
      docker://*)
        if printf '%s' "$ref" | grep -qE '@sha256:[0-9a-f]{64}$'; then
          echo "  ok    $rel:$lineno $ref"
        else
          echo "  FAIL  $rel:$lineno $ref (container image without a sha256 digest)"
          bad=$((bad + 1))
        fi
        ;;
      *)
        if ! printf '%s' "$ref" \
            | grep -qE '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(/[^@[:space:]]+)?@[0-9a-f]{40}$'; then
          echo "  FAIL  $rel:$lineno $ref (not pinned to a 40-hex commit SHA)"
          bad=$((bad + 1))
        elif [ -z "$comment" ]; then
          echo "  FAIL  $rel:$lineno $ref (no trailing '# vX.Y.Z' comment naming the pinned version)"
          bad=$((bad + 1))
        else
          echo "  ok    $rel:$lineno $ref # $comment"
        fi
        ;;
    esac
  done < <(grep -nE '^[[:space:]]*(-[[:space:]]+)?uses:' "$file")
done < <(printf '%s' "$files")

echo
if [ "$bad" -ne 0 ]; then
  echo "FAIL: $bad of $total action references are not pinned to a commit SHA." >&2
  echo "Resolve the tag to its commit (gh api repos/<owner>/<repo>/git/ref/tags/<tag>;" >&2
  echo "dereference an annotated tag with git/tags/<sha>) and write the version in a" >&2
  echo "trailing comment:  uses: owner/repo@<40-hex sha> # vX.Y.Z" >&2
  exit 1
fi
echo "All $total action references are pinned to commit SHAs."
exit 0
