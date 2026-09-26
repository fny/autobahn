# BENCH-3: The orchestrator never runs remote output through a shell

**Findings:** M-57 (KIMI ABN-L15).
**Status:** proposed. This is a fault under the bench standard: it can harm the machine it runs on.

## Problem

`bench/orchestrate.py` runs every command through `subprocess.run(command, shell=True)` (`:352-357`). Some of those command strings include values read back from the fleet instances, for example host keys (`:490`, `:743`). They are escaped with `json.dumps`, which does not escape single quotes. `bench/verify/launch.py:62,121` has the same pattern.

A tampered instance can return a value containing `'` and run commands on the operator's workstation, which holds AWS credentials.

## Proposed resolution

- **Pass argument lists.** Change `run()` to take a list and call `subprocess.run(args)` without a shell.
- **Quote whatever remains a shell string.** Commands that really need a shell, such as the remote strings passed to `ssh`, build that string with `shlex.quote` on every value. The local call still receives a list.
- **Treat returned values as data.** Values read from an instance are checked against the shape they should have. A host key, for example, must be one line starting with a known key type.

## Tests

- A unit test feeds a host-key value containing `'; touch /tmp/pwned; '`. Nothing runs, and the value is rejected.
- A syntax check (`python3 -m py_compile`) on both files.
