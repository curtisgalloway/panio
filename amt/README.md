<!--
Copyright 2026 Curtis Galloway

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0
-->

# amt

**Terms:** **AMT/ME** is Intel's management firmware; **WS-Management/SOAP**
is its XML management protocol; **TLS/HTTPS** encrypts traffic and verifies
the server; a **CA** signs certificates; **PEM** is their text file format.
See the [glossary](../docs/glossary.md).

Power control for **Intel AMT (vPro) machines** over the network, with no smart
plug. `amt` turns the host on, off, or power-cycles it, and reads its true
power state back. Pure Rust via [ureq](https://crates.io/crates/ureq); works on
**macOS and Linux**.

Commands talk to the **Management Engine** (ME), which runs on standby power, so
`state` is a real power sensor, even on bare metal with no OS.

- **Protocol:** WS-Management (SOAP over verified HTTPS) on port **16993** by default. It calls
  `CIM_PowerManagementService.RequestPowerStateChange` and reads back
  `CIM_AssociatedPowerManagementService.PowerState`.
- **Auth:** HTTP **Digest** (MD5, `qop=auth`) inside TLS. Digest alone does
  not encrypt traffic.
- **Trust:** built-in public roots, plus certificates from `--tls-ca-file
  <path>` (PEM). `AMT_TLS_CA_FILE` is the fallback; the flag wins. These are
  additive roots, not the operating system's certificate store. The file and
  fallback are read on the execution host; paniolo does not forward that
  environment variable over SSH. Invalid or empty files fail before requests.
- **Identity:** use the name in the server certificate; an IP needs a matching
  IP Subject Alternative Name (SAN). A CA file cannot fix a name mismatch.
  Self-signed certificates must pass standard verification. There is no
  custom verifier, pinning, or skip-verification mode.
- **Compatibility:** requires TLS 1.2+ with algorithms rustls supports; older
  firmware can fail even with TLS 1.2. No fallback to HTTP, no redirects, and
  no retries of TLS/certificate failures. Certificate provisioning, mutual
  TLS (client certificates), and encryption of separate RFB sessions are
  outside scope. KVM configuration commands do use the verified connection.

### Migrating HTTP deployments

Bare addresses now mean HTTPS/16993. Update **all four power hooks** together:
provision TLS, or add `--allow-insecure-http` to every hook for legacy HTTP.
That flag selects HTTP/16992 for bare or `http://` addresses and warns on
stderr every time. HTTP exposes the password exchange and allows command or
state tampering. `http://` without the flag is rejected; explicit `https://`
with the flag is also rejected. CA-file options, including the environment
fallback, cannot be combined with HTTP. The insecure flag has no environment
fallback.

An explicit port changes only the port: a bare `host:16992` now requests TLS
on 16992. Never use the HTTP opt-in automatically after a TLS failure.

## Credentials

The Digest password is never a flag's value or a config-file entry. It comes
from the first of these that is set:

1. the `AMT_PASSWORD` environment variable;
2. `--password-file <path>` (one trailing newline dropped);
3. `--password-command <cmd>`, run by `sh -c` (`cmd /C` on Windows), whose
   stdout is the password; killed after 30 s.

```bash
amt -d amt-host.example --password-command 'op read op://lab/amt/password' state
amt -d amt-host.example --password-file ~/.config/paniolo/amt state
op run --env-file .env -- bash -c 'amt state -d amt-host.example'
```

In the `op run` form keep the single quotes, or the parent shell expands
`$AMT_PASSWORD` before the wrapper sets it. When no source works, `amt` exits
3 (not configured). Pass the username (default `admin`) with `-u`. `kvm enable`
takes its RFB password the same way: `AMT_RFB_PASSWORD`,
`--rfb-password-file`, or `--rfb-password-command`.

## Usage

```bash
amt -d <host> status                # firmware identity + power state detail
amt -d <host> state                 # prints exactly "on" or "off"
amt -d <host> on                    # power on, confirm by read-back
amt -d <host> off                   # power off (hard, not a graceful shutdown)
amt -d <host> cycle [--delay-ms 3000]   # off → confirm → delay → on → confirm
```

- **`<host>`**: a hostname, IPv4 address, or bracketed IPv6 literal
  (`[fe80::1]`), optionally with `https://` and a `:port` (default 16993).
  HTTP requires `--allow-insecure-http` and defaults to 16992. Paths, queries,
  fragments, userinfo, and unbracketed IPv6 are rejected.
- **`state`** prints `on` only when running (PowerState 2). Sleep, hibernate
  and soft-off print `off`; any other state is an error.
- **`off`** is the CIM "Off - Soft" unconditional power-off, like holding the
  power button.
- **`cycle`** uses the same `--delay-ms` as the other paniolo power helpers. A
  sleeping or hibernating machine cold-boots instead of resuming.
- **`on`, `off`, `cycle`** exit non-zero if the read-back shows the machine did
  not comply.

## paniolo integration

`make install` / `paniolo setup` installs `amt` into paniolo's private libexec
dir; run it by hand with `paniolo helper amt …`. Wire the four power hooks:

```bash
paniolo power set -t <target> \
    --cycle-cmd "amt cycle -d <host> --delay-ms 5000" \
    --on-cmd    "amt on -d <host>" \
    --off-cmd   "amt off -d <host>" \
    --state-cmd "amt state -d <host>"
```

For private trust, add `--tls-ca-file /etc/paniolo/amt-ca.pem` to each hook.
Add `--password-file` or `--password-command` to each hook, or run
`paniolo power …` with `AMT_PASSWORD` set. See `docs/power.md` for the full
recipe, with systemd, 1Password and macOS Keychain examples.
