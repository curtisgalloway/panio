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

# Enforce the release-train profile's freshness rule: every path pinned in
# RELEASE-TRAIN.md's `## Sources` table still hashes to the blob id recorded
# beside it.
#
# The table exists because the profile states facts *derived* from those files
# -- which workflow job builds an arm, what the smoke contract can assume, how
# a version is bumped. When a source changes, the prose it feeds may have
# become wrong, and only a human re-reading it can say. The pin is how the next
# release knows to look.
#
# Nothing enforced this until now, and it drifted twice through green CI
# (GitHub #219): AGENTS.md in #209 and cli/src/daemons.rs in #213. The first
# thing to notice would otherwise be the release train itself, mid-release.
#
# Two callers. ci.yml runs it on every push and PR but only warns on exit 1,
# so a PR that touches a pinned file is not forced to re-pin a profile only a
# release reads. release.yml runs it as a blocking job ahead of every package
# build, so a stale profile stops the release before anything is built.
#
# Deliberately NOT the release-train skill's `profile_check.py`. That script
# lives in another repository and answers a broader question -- whether the
# profile is well-formed enough for the train to execute (paths, headings,
# channels, smoke subcommands). That belongs at release time, where the skill
# is being invoked anyway. CI needs one narrow question answered, and
# making this repo's CI clone a personal skills repo would break forks and add
# a cross-repo dependency to every run. The two are kept in agreement by using
# git's own blob id, which is a frozen format rather than a shared API.
#
# When this fails, re-read the sections the changed row feeds, then re-pin.
# The skill's `profile_check.py --update` rewrites the ids; so does:
#     git hash-object --no-filters <path> | cut -c1-12
#
# Run it anywhere: it needs a shell, sed, awk and git.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROFILE="${1:-$ROOT/RELEASE-TRAIN.md}"

if [ ! -f "$PROFILE" ]; then
  echo "FATAL: no profile at $PROFILE" >&2
  exit 2
fi

# The `## Sources` section only, so a backticked path in any other table is
# not mistaken for a pinned row.
section="$(sed -n '/^## Sources$/,/^## [^S]/p' "$PROFILE")"

# Data rows start with a backticked path, which excludes the header and the
# `|---|---|---|` separator without having to recognise them.
rows="$(printf '%s\n' "$section" | grep -E '^\|[[:space:]]*`' || true)"

if [ -z "$rows" ]; then
  echo "FATAL: no pinned rows found in the '## Sources' table of $PROFILE" >&2
  echo "       The table is how a release knows which prose to re-read; an" >&2
  echo "       empty one is a failure, not a pass." >&2
  exit 2
fi

total=0
bad=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  # shellcheck disable=SC2016  # the backticks are literal markdown, not a
  # command substitution: the single quotes are what keeps them that way.
  path="$(printf '%s' "$row" | sed -E 's/^\|[[:space:]]*`([^`]*)`.*/\1/')"
  pinned="$(printf '%s' "$row" | awk -F'|' '{gsub(/[`[:space:]]/, "", $3); print $3}')"
  feeds="$(printf '%s' "$row" | awk -F'|' '{sub(/^[[:space:]]*/, "", $4); sub(/[[:space:]]*$/, "", $4); print $4}')"
  total=$((total + 1))

  if [ -z "$pinned" ]; then
    echo "  FAIL  $path (no blob id in the table)"
    bad=$((bad + 1))
    continue
  fi
  if ! printf '%s' "$pinned" | grep -qE '^[0-9a-f]+$'; then
    echo "  FAIL  $path (blob id '$pinned' is not hex)"
    bad=$((bad + 1))
    continue
  fi
  if [ ! -f "$ROOT/$path" ]; then
    echo "  FAIL  $path (pinned but missing -- the fact it feeds is gone)"
    echo "        feeds: $feeds"
    bad=$((bad + 1))
    continue
  fi

  # --no-filters hashes the bytes on disk. Without it, git would apply any
  # .gitattributes eol/clean filter first and could disagree with the skill's
  # checker, which hashes the file as-is.
  actual="$(git -C "$ROOT" hash-object --no-filters "$path")"
  if [ "${actual:0:${#pinned}}" = "$pinned" ]; then
    echo "  ok    $path"
  else
    echo "  FAIL  $path (pinned $pinned, now ${actual:0:${#pinned}})"
    echo "        feeds: $feeds"
    bad=$((bad + 1))
  fi
done < <(printf '%s\n' "$rows")

echo
if [ "$bad" -ne 0 ]; then
  echo "FAIL: $bad of $total pinned sources have changed since the profile was written." >&2
  echo "" >&2
  echo "This is not a formatting nit. Each row names the part of RELEASE-TRAIN.md" >&2
  echo "that was written from that file; re-read those sections and correct them if" >&2
  echo "they have gone stale, THEN re-pin:" >&2
  echo "" >&2
  echo "    git hash-object --no-filters <path> | cut -c1-12" >&2
  echo "" >&2
  echo "Re-pinning without re-reading defeats the purpose of the table." >&2
  exit 1
fi
echo "All $total pinned sources match the profile."
exit 0
