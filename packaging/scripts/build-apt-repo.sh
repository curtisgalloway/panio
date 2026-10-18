#!/bin/bash
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

# Assemble the signed apt repository that docs.yml publishes to GitHub Pages
# under /apt/. Stateless by design: point it at a directory of .debs (the
# workflow downloads them from recent GitHub Releases) and it emits a complete
# pooled repo — pool/, per-arch Packages indexes, a GPG-signed
# Release/InRelease, and the public key as panio.asc — into the output dir.
# Nothing is kept between runs, so the live repo always mirrors the releases.
#
# The signing key must already be imported into the calling GNUPGHOME and must
# have no passphrase (CI runs non-interactively).
#
# Usage: build-apt-repo.sh <debs-dir> <out-dir> <signing-key-fingerprint>

set -euo pipefail

DEBS_DIR="$1"
OUT="$2"
KEY="$3"

SUITE=stable
COMPONENT=main
ARCHES="amd64 arm64"
BASE_URL="https://panio.dev/apt"

command -v dpkg-scanpackages >/dev/null 2>&1 || {
  echo "error: dpkg-scanpackages not found (apt-get install dpkg-dev)" >&2
  exit 1
}
command -v apt-ftparchive >/dev/null 2>&1 || {
  echo "error: apt-ftparchive not found (apt-get install apt-utils)" >&2
  exit 1
}
ls "$DEBS_DIR"/*.deb >/dev/null 2>&1 || {
  echo "error: no .deb files in $DEBS_DIR" >&2
  exit 1
}

# The fingerprint index.html shows is read from the key that signs, so the
# page cannot drift from the key docs.yml actually imported.
FPR="$(gpg --batch --with-colons --fingerprint "$KEY" \
  | awk -F: '/^fpr:/ {print $10; exit}')"
[ -n "$FPR" ] || {
  echo "error: no fingerprint for signing key $KEY (not imported?)" >&2
  exit 1
}

# apt refuses the repo once this passes, which bounds how long a stale or
# replayed index can be served. 30 days; docs.yml rebuilds weekly on a
# schedule as well as on every release, so a live repo never reaches it.
# apt-ftparchive derives the Valid-Until field from ValidTime (seconds after
# Date); it silently ignores a Valid-Until value given directly (verified
# against apt 2.8: the field was missing from the published Release).
VALID_SECONDS=$((30 * 24 * 3600))

POOL="pool/$COMPONENT/p/panio"
mkdir -p "$OUT/$POOL"
cp "$DEBS_DIR"/*.deb "$OUT/$POOL/"

cd "$OUT"

# --multiversion keeps every release in the index (the pool holds the last few
# releases so clients can pin or roll back); without it dpkg-scanpackages
# silently drops all but the newest.
for arch in $ARCHES; do
  bindir="dists/$SUITE/$COMPONENT/binary-$arch"
  mkdir -p "$bindir"
  dpkg-scanpackages --multiversion --arch "$arch" pool > "$bindir/Packages"
  gzip -9 -c "$bindir/Packages" > "$bindir/Packages.gz"
done

apt-ftparchive \
  -o "APT::FTPArchive::Release::Origin=panio" \
  -o "APT::FTPArchive::Release::Label=panio" \
  -o "APT::FTPArchive::Release::Suite=$SUITE" \
  -o "APT::FTPArchive::Release::Codename=$SUITE" \
  -o "APT::FTPArchive::Release::Architectures=$ARCHES" \
  -o "APT::FTPArchive::Release::Components=$COMPONENT" \
  -o "APT::FTPArchive::Release::ValidTime=$VALID_SECONDS" \
  release "dists/$SUITE" > "dists/$SUITE/Release"

gpg --batch --yes --local-user "$KEY" --armor --detach-sign \
  --output "dists/$SUITE/Release.gpg" "dists/$SUITE/Release"
gpg --batch --yes --local-user "$KEY" --clearsign \
  --output "dists/$SUITE/InRelease" "dists/$SUITE/Release"
gpg --batch --armor --export "$KEY" > panio.asc

cat > index.html <<EOF
<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>panio apt repository</title></head>
<body><h1>panio apt repository</h1>
<p>Signed apt repository for <a href="https://github.com/curtisgalloway/panio">panio</a>
(amd64/arm64, Debian 12+ and Raspberry Pi OS). To use it:</p>
<pre>
sudo install -d /etc/apt/keyrings
sudo curl -fsSL -o /etc/apt/keyrings/panio.asc $BASE_URL/panio.asc
sudo tee /etc/apt/sources.list.d/panio.sources &gt;/dev/null &lt;&lt;'SRC'
Types: deb
URIs: $BASE_URL
Suites: $SUITE
Components: $COMPONENT
Signed-By: /etc/apt/keyrings/panio.asc
SRC
sudo apt update &amp;&amp; sudo apt install panio
</pre>
<p>Signing key fingerprint: <code>$FPR</code></p>
</body></html>
EOF

echo "apt repo assembled in $OUT:"
find . -type f | sort
