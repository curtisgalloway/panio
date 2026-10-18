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

# Sync crate sources to the Windows bench host for a build there.
#
# Usage: BRIK_HOST=<ssh host> BRIK_DEST=<path on host> \
#          scripts/sync-brik.sh <crate> [crate...]
#
# BRIK_HOST is the ssh destination (a hostname or an ~/.ssh/config alias) and
# BRIK_DEST the checkout on it, in the forward-slash form the host's tar takes
# (e.g. C:/src/panio). Both come from the environment and have no default:
# the real values name private infrastructure, which does not belong in the
# repo (AGENTS.md, "Never commit private infrastructure").
#
# Excludes target/ deliberately: the build dirs are gigabytes and the transfer
# stalls the SSH session long before it finishes. Only sources, manifests and
# lockfiles cross the wire; the host keeps its own target/.
#
# BRIK_SSH_AGENT points ssh at a session-scoped agent (see the homelab-ssh
# skill) so a locked 1Password desktop app cannot interrupt a long build.
set -euo pipefail

if [ -z "${BRIK_HOST:-}" ] || [ -z "${BRIK_DEST:-}" ] || [ "$#" -eq 0 ]; then
  echo "usage: BRIK_HOST=<ssh host> BRIK_DEST=<path on host> $0 <crate> [crate...]" >&2
  echo "  BRIK_HOST  ssh destination of the Windows bench host (no default)" >&2
  echo "  BRIK_DEST  checkout path on it, forward slashes, e.g. C:/src/panio (no default)" >&2
  exit 2
fi
HOST="$BRIK_HOST"
DEST="$BRIK_DEST"
SSH_OPTS=(-o BatchMode=yes -o ServerAliveInterval=20)
[ -n "${BRIK_SSH_AGENT:-}" ] && SSH_OPTS+=(-o "IdentityAgent=${BRIK_SSH_AGENT}")
cd "$(dirname "$0")/.."
# $DEST is meant to expand here, on the client, into the remote command line.
# shellcheck disable=SC2029
tar czf - --exclude=target --exclude=.git "$@" \
  | ssh "${SSH_OPTS[@]}" "$HOST" "tar xzf - -C $DEST"
