#!/bin/sh
set -e
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

# Advisory only — never reaps daemons. panio's daemons (serialcap, hdmicap,
# netbootd) are per-user processes, not packaged system services, so a root
# maintainer script can't reliably stop another user's running daemons (their
# state lives under per-user runtime dirs, and killing an in-flight netboot on
# a routine upgrade would be surprising). After an upgrade, any daemon still
# running keeps executing the OLD binary until restarted. The CLI detects this
# (`panio daemons` flags such daemons "stale") and heals it on demand.
#
# dpkg runs this as `postinst configure <previously-configured-version>`, and
# that second argument is empty on a fresh install (Debian Policy 6.5). It is
# the one thing that tells a first-time installer from an upgrader, and the
# two need different advice: the upgrader has daemons to restart, the
# first-timer has nothing running yet and wants to know where to start (#228).
# nfpm ships this script only in the .deb, so the rpm-style numeric argument
# convention does not apply.
case "$1" in
  configure)
    if [ -n "${2:-}" ]; then
      echo "panio: upgrade from $2 complete. Any capture daemons already running still use the"
      echo "         previous binary — restart them with: panio daemons restart --stale"
      echo "         (netbootd: restart via 'panio netboot start' when convenient)."
    else
      echo "panio: installed. Run 'panio setup' once (group membership for serial and USB"
      echo "         devices, the optional zigplug helper), then 'panio --help' to start;"
      echo "         'panio skill' prints the bundled agent instructions."
    fi
    ;;
esac

exit 0
