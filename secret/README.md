<!--
Copyright 2026 Curtis Galloway

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0
-->

# secret

A small library crate that the panio helpers share by path dependency. It
reads a helper's secret (a password or token) from the first source that is
set:

1. an environment variable, `X`;
2. a file, `--x-file <path>` (one trailing newline dropped; a warning if the
   file is readable by group or others);
3. a command, `--x-command <cmd>`, run by `sh -c` (`cmd /C` on Windows) with no
   stdin, whose stdout is the secret; killed after 30 s.

So any secret manager plugs in without a wrapper script, and the secret itself
never appears in a flag, the lab file, or `ps`. Every failure is a
`NotConfigured` error, which a helper maps to exit status 3.

```rust
const PASSWORD: secret::Spec = secret::Spec {
    what: "AMT password",
    env: "AMT_PASSWORD",
    file_flag: "--password-file",
    command_flag: "--password-command",
};
let pw = secret::require(&PASSWORD, &secret::Sources { file, command })?;
```

The helper declares the two flags itself (see `amt/src/main.rs`). Used by
`amt`; see `docs/dev/adding-power-helpers.md` for the convention.
