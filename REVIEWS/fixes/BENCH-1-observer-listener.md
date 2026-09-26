# BENCH-1: The benchmark observer listens on loopback and writes only under its root

**Findings:** H-14 (ASTRA F09).
**Status:** proposed. This is a fault under the bench standard: it can harm the machine it runs on.

## Problem

`benchmark observer <port>` listens on every interface (`bench/harness/src/observer.rs:50`). Its `floor_arm` request takes any path and payload, and `floor_write` writes them (`:83`, `:112`). There is no authentication and no limit on where it writes.

Anyone who can reach the port can overwrite any file the benchmark user can write, such as shell startup files or `~/.ssh/authorized_keys`. The local smoke test (`bench/smoke.sh:73`) starts this listener and connects only to `127.0.0.1`, yet it still exposes the listener to the whole network. On EC2 the fleet needs a network listener, because load generators on other hosts connect to it. The fleet's security group limits who can reach it.

## Proposed resolution

- **Listen on loopback by default.** Add `--listen <addr>`, defaulting to `127.0.0.1`. `orchestrate.py` passes `--listen 0.0.0.0` on the fleet (`:844`). `smoke.sh` needs no change.
- **Confine writes.** Add a required `--root <dir>` to the observer. `floor_arm` refuses any path that is absolute or contains `..`, and joins what remains under the root. Open the file with `create_new` or `O_NOFOLLOW` so a symlink inside the root cannot redirect the write. The existing `floor --dest-root` already names this directory, so the orchestrator and `smoke.sh` pass the same value to the observer.
- **Bound requests.** Cap the payload size and the number of connections. It is cheap, and it stops one bad client from filling the disk.
- **Not included:** token authentication. The loopback default and the root confinement close the local risk, and the fleet is limited by its security group.

## Tests

- The observer refuses `floor_arm` for `/etc/x`, for `../x`, and for a path through a symlink inside the root.
- With no `--listen`, the observer is not reachable on a non-loopback address.
- `smoke.sh` passes unchanged.
