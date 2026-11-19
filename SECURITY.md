<!--
Copyright 2026 The Panio Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# Security policy

## Supported versions

Panio is pre-1.0 and ships from a single release line. Security fixes go into
the next release on that line; older releases are not patched.

| Version | Supported |
|---|---|
| Latest release (currently 0.16.x) | Yes |
| Anything older | No: upgrade to the latest release |
| Unreleased `main` | Best effort |

## Reporting a vulnerability

**Please do not open a public issue, pull request, or discussion for a
security problem.**

Report it privately through GitHub instead:

1. Open the repository's [Security tab](https://github.com/curtisgalloway/panio/security).
2. Choose **Report a vulnerability**.
3. Describe the problem, the affected version (`panio --version`), the
   platform, and the steps to reproduce it. A proof of concept helps.

Only the maintainers can see the report. Please leave out anything that
identifies your own network or hardware (hostnames, private addresses, MAC
addresses, serial numbers) unless it is needed to reproduce the problem.

## What to expect

Panio is maintained by volunteers, so these are goals, not guarantees:

- An acknowledgement within 7 days.
- An initial assessment (accepted, needs more information, or not a
  vulnerability) within 14 days.
- For an accepted report, a fix in a release, then a published GitHub security
  advisory that credits you unless you ask to stay anonymous.

Please give us a reasonable chance to ship a fix before you disclose the issue
publicly. We aim to do that within 90 days of the report and will keep you
informed if it takes longer.

## Scope

In scope is the code in this repository and the packages built from it
(Homebrew, `.deb` and the apt repository, the Windows zip, and the source
install). Areas where a bug is most likely to matter:

- **Privileged code**: the setuid-root `netbootd-bpf-helper` on macOS, and the
  privilege dropping in `netbootd`.
- **Network listeners**: the netboot DHCP, TFTP and HTTP servers, and the
  daemon HTTP and WebSocket endpoints (`hdmicap`, `serialcap`, the HID daemon,
  the dashboard), including their bearer tokens and dashboard tickets.
- **Secrets handling**: the `secret` crate and the helpers that use it (for
  example `amt`).
- **Remote labs**: commands run on control hosts over SSH from a lab file.
- **Release and supply chain**: `.github/workflows`, package signing, and the
  apt repository.

Out of scope:

- Vulnerabilities in third-party dependencies that are already public; these
  are tracked through Dependabot. Do report them if Panio uses the dependency
  in a way that makes the problem worse.
- Attacks that need an attacker who already has root or the same user account
  on the control host.
- Physical access to the lab hardware or the target machine. Panio controls
  targets on purpose: an agent that can run `panio` can power-cycle, netboot,
  and type into its targets, and that is the intended behaviour.
