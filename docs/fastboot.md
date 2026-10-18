<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Guarded RAM-only USB fastboot staging

Use an existing device-scoped, download-only USB fastboot client when a target's
firmware can accept bytes into RAM but Ethernet delivery is unavailable. Wrap
each artifact in one guarded operation: enter the firmware service, observe
the intended USB device, transfer once, return to the serial loader, and verify
the bytes. Repeat the complete operation for every artifact.

**Terms** (more in the [glossary](glossary.md)):

- **RAM staging** — placing an artifact in temporary memory without flashing
  storage or executing it.
- **guarded staging runner** — a board-specific program that enforces the
  prerequisites, deadlines, and evidence requirements below as one operation.
- **service-entry witness** — a complete, fresh firmware response for this
  artifact's entry into the download service.
- **ownership reservation** — exclusive control of the target and its shared
  hardware for the entire run, assigned by the lab's coordinator.
- **staging receipt** — the retained evidence of success, failure, or partial
  progress for one artifact.

This guide defines the supported integration contract for an external runner;
panio does not ship a fastboot channel or guarded staging helper. Its
[`usb` channel](usb.md) switches physical media, and its [adb channel](adb.md)
talks to Android; neither implements this workflow. Use panio's existing
serial commands for the console. Do not open its managed serial port directly.

## Establish the board profile and owner

Before a run, the board profile must identify the exact client and version,
its download-only invocation, the expected device identity, the firmware
entry command and complete success response, a RAM-preserving return to the
loader, and the target's checksum command and algorithm. If any of these are
unknown, stop before delivery. Do not substitute a flash command for a missing
download-only client operation.

If no guarded runner can enforce the readiness checks and terminate the sender
in its privilege domain on the USB host, do not stage or call the client directly.

Fastboot's protocol distinguishes `download` (write memory), `flash` (write a
partition), and `boot` (execute the downloaded image). Those protocol names
are not promises that a particular host CLI exposes a `download FILE` verb.
Verify the client's actual interface and restrict the runner to the necessary
read-only queries, RAM transfer, and reviewed return operation.
See the [fastboot protocol](https://android.googlesource.com/platform/system/core/+/refs/heads/main/fastboot/README.md).

Record the RAM buffer address, capacity, artifact destination and length, and
memory ranges that must be preserved. Check for overflow, overlap with the
loader or earlier artifacts, and a changing input file before sending. The
download buffer is firmware-specific; for example, [U-Boot configures its
address and size](https://docs.u-boot.org/en/v2025.10/android/fastboot.html#general-configuration).
Pin the artifact bytes, byte count, and hash for the transfer and verification.

One owner holds the target, its serial input, power/reset controls, and the
selected USB device across the entire multi-artifact run. Other agents,
dashboards and operators must not send input or reset it. A coordinator or
exclusive bench checkout must enforce that reservation across control hosts;
an informal assumption that nobody else is using it is insufficient.

Panio's [serial transaction lock](serial.md#requestresponse-transactions)
holds only during a command/reply exchange. Its [action/power hook lock](actions.md#how-an-action-runs)
holds only around hooks. Neither reserves the complete staging sequence or
excludes another USB client. A runner must not treat either as a global lock.

## One guarded operation per artifact

The runner performs these steps in order under the same reservation. A direct
sender invocation cannot bypass them. A witness is consumed by one send
attempt and cannot be supplied from a previous receipt.

1. **Start a new receipt and establish the loader state.** Record the owner,
   artifact identity, expected USB identity, startup identity, and serial
   capture epoch, connection generation and receive boundary using
   [`serial status`](serial.md#read-only-health): inspect the active interface's
   `connected`, `health.epoch`, `health.connection_generation`,
   `health.last_rx_seq` and `health.evidence_error`. Missing required evidence
   is unknown, not ready. Observe the
   current loader before entering its download service. A changed startup,
   reconnect, daemon restart or conflicting control action invalidates earlier
   observations.
2. **Enter the service and validate a fresh response.** Use
   [`serial transact`](serial.md#requestresponse-transactions) with the
   profile's entry command, complete service-entry readiness marker as `--until`,
   byte caps and finite timeout. This marker is not the interactive loader
   prompt: a foreground service normally holds the console until it exits.
   Reject an error or return to the loader before or after the marker.
   Check the JSON status and actual response, not just exit 0:
   `matched` means bytes matched, not that firmware accepted the command.
   Reject an echoed command, generic prompt, partial response, stale marker,
   timeout, cap, gap or capture fault. Save the exact response and its
   epoch/generation/RX sequence evidence. Where firmware supports it, use a
   per-operation nonce to distinguish replies. Without a trustworthy complete
   response and exclusive sequencing, do not start the sender.
3. **Observe USB readiness separately.** Firmware service entry does not prove
   that the host has enumerated the USB device. Start an explicit monotonic
   readiness deadline after entry; run bounded read-only discovery on the USB
   control host. Save every observation, including empty output. An exit-0
   discovery with zero devices is pending: wait the configured poll interval
   and observe again within the same deadline. A discovery error, malformed
   output, wrong device, ambiguous identity, or expired deadline fails the
   operation. Never restart the deadline or choose the first available device.
4. **Bind the observed device and send once.** Require exactly one expected
   match using the profile's stable identity and, where necessary, physical
   USB topology and VID/PID. VID/PID alone is not unique. After discovery and
   immediately before sending, recheck `serial status` and inspect new raw
   evidence after the entry result's `through_rx_seq` in the same epoch.
   Require journal coverage through the status snapshot's `last_rx_seq` within
   the remaining deadline; pending capture is not proof that nothing happened. A
   reconnect, capture fault, firmware error or return to the loader invalidates
   entry. Revalidate identity when opening the USB device; a disappeared or
   replaced device is a failure, not permission to search for another.
   Pass the exact observed selector to the client with implicit wait-for-device
   behavior disabled. The client must verify the opened identity matches; a
   selector string alone is insufficient if it can bind a replacement device.
   If the client cannot enforce this, the profile is unsuitable. The sender
   must have no automatic resend and start only while both readiness witnesses
   remain valid. Consume the service witness before starting the transfer,
   including a failed start.
5. **Record delivery, then return and verify.** Retain sender byte counts,
   timing, stdout, stderr and the target's final protocol response. Host-side
   bytes written alone do not prove complete delivery. Use only the profile's
   RAM-preserving return procedure. Capture a new serial receive boundary
   before that operation and require a complete fresh loader response after
   it; USB disappearance alone does not prove return. Query the checksum of
   the exact staged RAM range and compare it with the same algorithm applied
   to the pinned artifact bytes. If bytes must be copied before the next
   download overwrites the buffer, copy
   through the loader and verify the destination range too. Mark success only
   after return and verification succeed.

For a serial-triggered return, use `serial transact` with the profile's return
command and loader response marker. If USB or another control triggers the
return, save `health.epoch` and `health.last_rx_seq` from `serial status`
before triggering it, then use [`serial wait`](serial.md#bounded-fresh-wait-and-follow)
with that `--epoch`, `--since` and the loader response as `--pattern`. Inspect
the full response and reject incomplete evidence as at entry. Run checksum and
RAM-copy commands through `serial transact`, checking their replies explicitly.

The next artifact starts again at step 1, even after a successful download.
Returning to the loader to copy or verify bytes ends the previous service
session. Reusing its entry witness is the sequencing error this guard prevents.
Reobserve device identity after every startup; do not trust a cached USB bus
address or a device list from the previous artifact.

Do not use `flash`, `erase`, `update`, unlocking, slot changes, or persistent
environment saves in this RAM-only operation. Image execution is a separate
explicit action after staging and verification. Query only firmware variables
needed for identity, capacity or this transfer; a broad `getvar all` or Android
partition/A-B inventory is not a readiness check.

## Bound every phase where it runs

The profile supplies finite limits for ownership acquisition, service entry,
each discovery call, total USB readiness, transfer, return, verification and
the whole artifact operation. Also specify polling interval, output limits,
termination grace and final termination deadline. These are chosen budgets,
not measurements inferred from one device's enumeration delay.

Use monotonic deadlines; each subcall is limited by the remaining phase and
overall time. Only successful empty discovery may be polled automatically.
Errors and uncertain transfer results stop the operation without another send.

The watchdog must run on the USB control host with authority to terminate the
sender and all descendants. If USB access requires elevated privilege, the
watchdog must have that authority too. Timing out a local SSH process or an
unprivileged wrapper around a privileged sender does not prove the remote
transfer stopped. Arrange supervision before launching the sender, including
what happens if SSH disconnects or the controlling process exits.

On a deadline, retain output already collected, request termination, escalate
after the bounded grace, and confirm exit and release of device handles. If
termination cannot be established by the final deadline, finish the receipt
as **unknown / possibly still active** and keep the hardware reserved or
quarantined. Do not wait forever, release it for another owner, retry, or
automatically reset the board. Recovery requires a separate explicit decision
after the live process and target state have been inspected.

## Retain a receipt for every attempt

Write evidence as each phase runs, before moving to the next. Preserve failure
and partial-delivery receipts; do not replace them with a later successful run.
Save raw records outside the public source tree with owner-only access. Real
USB identifiers, host names and private paths must not enter issues or commits.

| Evidence | Required contents |
|---|---|
| Run and input | Owner/reservation, artifact ID, immutable input hash and size, board profile and client versions, chosen budgets |
| Firmware entry | Command, fresh complete response, startup identity, capture epoch, connection generation and receive boundary; any gaps, caps or faults |
| USB observations | Every discovery's start/end time, exit status, stdout/stderr and selected identity, including empty results and rejected candidates |
| Transfer | Explicit device selection and invocation, start/end times and elapsed duration, requested and reported byte counts, final response, stdout/stderr, failure or timeout |
| Termination | Signals/actions taken, deadlines, exit status and confirmation that descendants and handles are gone, or explicit unknown state |
| Return and verification | New receive boundary, fresh loader response, exact RAM ranges, target checksum and comparison, copies and their verification |
| Verdict | Last completed phase, whether a send was attempted, known or unknown partial delivery, success/failure reason and reservation disposition |

Use wall-clock timestamps for correlation and monotonic durations for limits.
If a client cannot report a byte count, record it as unknown rather than
inventing a completed count; the profile must provide the evidence required
to establish full delivery. A failed evidence write or truncated required
response prevents advancement. Panio's [raw serial evidence](serial.md#immutable-receive-evidence)
preserves receive times independently of when the runner reads it.

## Acceptance checks for an integration

Exercise these with simulated client/firmware results before using a runner on
shared hardware. They describe required behavior, not tests of a bundled helper.

| Condition | Required result |
|---|---|
| Entry witness missing, stale, incomplete, or for another artifact/startup | Sender never starts |
| Entry matched only an echo or generic prompt; serial evidence has a gap/cap/fault | Sender never starts; retain rejected response |
| Firmware ready; several successful empty USB observations; expected device appears before deadline | Retain every observation; start one transfer after selection |
| Discovery error, wrong device, ambiguous identity, malformed output or deadline expiry | Stop; no sender and no device substitution |
| Device disappears or changes between selection and open | Fail the binding; no search or resend |
| Firmware aborts or returns to the loader during USB enumeration | Invalidate entry; sender never starts |
| Partial transfer, sender error, timeout or lost control connection | Retain partial evidence; bounded termination; no automatic retry |
| Watchdog lacks termination authority, or cannot confirm termination | Refuse launch when known beforehand; otherwise quarantine and report unknown |
| Return response absent or stale; checksum differs | Do not mark the artifact staged successfully or begin the next one |
| Second artifact follows a successful transfer and return to loader | Require a new complete service entry and new USB observation |
| Competing owner or control action | Refuse competing access; invalidate readiness if exclusivity was lost |
