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

# Documentation-only pull requests skip the expensive build and test jobs in
# .github/workflows/ci.yml. This script is the single definition of what
# "documentation-only" means, in two modes:
#
#   classify  Decide whether a change set is doc-only. Reads GITHUB_EVENT_NAME,
#             BASE_SHA and HEAD_SHA from the environment; prints `code=true` or
#             `code=false` and appends the same line to $GITHUB_OUTPUT when set.
#             `code=false` only for a pull request whose merge-base diff is
#             non-empty and made entirely of doc-allowlist paths. Every other
#             outcome -- a push, a missing SHA, a git error, an empty diff, any
#             other path -- prints `code=true`: the gate fails OPEN, because a
#             skipped required check counts as passing.
#
#   strip     Delete every tracked doc-allowlist file from the working tree.
#             Every skippable job runs this right after checkout, on every run.
#
# The invariant: no job that may be skipped on a doc-only PR can depend on a
# doc-only file. `strip` enforces it by execution rather than by review. If a
# build starts to read a Markdown file (include_str!, a doc-test, a script),
# that job fails on the PR that adds the dependency, not weeks later on the
# first doc-only PR that silently skipped it. A job that genuinely must read a
# doc file belongs in the always-run set in ci.yml, and this allowlist should
# then be revisited.
#
# The allowlist is deliberately small: *.md anywhere, everything under docs/,
# mkdocs.yml, LICENSE. Nothing under .github/ or scripts/, and no non-Markdown
# file under skills/.
#
# Must run under bash 3.2 (macOS runners), GNU and BSD userlands, and Git Bash
# on Windows: no mapfile, no GNU-only flags, NUL-delimited path lists.

set -u

is_doc() {
  case "$1" in
    *.md | docs/* | mkdocs.yml | LICENSE) return 0 ;;
    *) return 1 ;;
  esac
}

emit() {
  echo "code=$1"
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "code=$1" >>"$GITHUB_OUTPUT"
  fi
}

full_run() {
  echo "full run: $1"
  emit true
  exit 0
}

classify() {
  [ "${GITHUB_EVENT_NAME:-}" = "pull_request" ] || full_run "event is '${GITHUB_EVENT_NAME:-unset}', not pull_request"
  [ -n "${BASE_SHA:-}" ] || full_run "BASE_SHA missing"
  [ -n "${HEAD_SHA:-}" ] || full_run "HEAD_SHA missing"

  list="$(mktemp)" || full_run "mktemp failed"
  trap 'rm -f "$list"' EXIT

  # --no-renames lists both sides of a rename, so moving a code file to a
  # doc path cannot hide the deleted code path.
  git diff --name-only --no-renames -z "$BASE_SHA...$HEAD_SHA" >"$list" 2>/dev/null \
    || full_run "git diff $BASE_SHA...$HEAD_SHA failed"
  [ -s "$list" ] || full_run "diff is empty"

  total=0
  nondoc=0
  shown=0
  while IFS= read -r -d '' p; do
    total=$((total + 1))
    if ! is_doc "$p"; then
      nondoc=$((nondoc + 1))
      if [ "$shown" -lt 20 ]; then
        echo "  non-doc: $p"
        shown=$((shown + 1))
      fi
    fi
  done <"$list"

  if [ "$nondoc" -gt 0 ]; then
    echo "full run: $nondoc of $total changed paths are not documentation (first $shown listed)"
    emit true
    exit 0
  fi
  echo "doc-only: all $total changed paths are documentation"
  emit false
}

strip() {
  list="$(mktemp)" || { echo "mktemp failed" >&2; exit 1; }
  trap 'rm -f "$list"' EXIT
  # ci-local.sh runs this in a rsync copy that has no .git; it sets
  # PANIO_STRIP_NO_GIT=1 to list files with find instead.
  if [ -n "${PANIO_STRIP_NO_GIT:-}" ]; then
    find . -type f -not -path './.git/*' -not -path '*/target/*' \
      -not -path '*/.venv/*' -not -path '*/node_modules/*' -print0 >"$list" || exit 1
  else
    git ls-files -z >"$list" || { echo "git ls-files failed" >&2; exit 1; }
  fi
  n=0
  while IFS= read -r -d '' p; do
    p="${p#./}"
    if is_doc "$p"; then
      rm -f -- "$p"
      n=$((n + 1))
    fi
  done <"$list"
  echo "stripped $n doc-only files from the working tree"
}

case "${1:-}" in
  classify) classify ;;
  strip) strip ;;
  *)
    echo "usage: $0 classify|strip" >&2
    exit 2
    ;;
esac
