# OPS-4: Fixed SSH options for autobahn's connections

**Findings:** M-9 (OPUS S5; GLM L1; KIMI ABN-I4).
**Status:** proposed.

## Problem

autobahn sets only `BatchMode`, the `ServerAlive*` keepalives and `Compression` (`src/transport/mod.rs:73-84`). Everything else comes from the user's `ssh_config`. These connections stay open for days, so some settings meant for interactive use become risky or break things:

- **`ForwardAgent yes`** exposes the controller's SSH agent to every remote host for the supervisor's lifetime.
- **`RequestTTY force`** puts a terminal on the channel and corrupts the binary protocol.
- **`LocalForward` with `ExitOnForwardFailure`** breaks reconnects once the port is taken.
- **Nothing sets a connect timeout,** so one hung login blocks every session to that host. M-12 is the broader timeout problem.

## Proposed resolution

- **Add these to every SSH command autobahn builds:**
  - `-T`
  - `-o ForwardAgent=no`
  - `-o ForwardX11=no`
  - `-o ClearAllForwardings=yes`
  - `-o PermitLocalCommand=no`
  - `-o ConnectTimeout=20`
- **Leave host-key checking alone.** With `BatchMode=yes`, an unknown host is already refused. Setting `StrictHostKeyChecking=accept-new` would trust new hosts silently.
- **Keep the override.** `AUTOBAHN_SSH` still replaces the SSH program, as before.
- **Docs.** List the fixed options in `docs/configuration.md`, and say they win over `ssh_config`.

## Tests

- Extend the existing `ssh_argv` test to assert each option is present, and before the `--`.
