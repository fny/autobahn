# LOCAL-05: Control socket fallback directory

**Findings:** M-2 (DEEPSEEK F6; GLM L6; KIMI ABN-M12; OPUS).
**Status:** proposed.

## Problem

When the state root's path is too long for a Unix socket address, the control socket moves to `temp_dir()/autobahn-<uid>/<name>.sock` (`src/supervisor/control.rs:238-276`). The code creates that directory with `create_dir_all` and chmods it to `0700`, discarding the chmod's error. So a directory another user created first is accepted.

That user can then replace the socket and answer `status`, `flush`, the shop and the tray. The client never checks who is serving.

## Proposed resolution

- **A safer fallback location.** On Linux, prefer `$XDG_RUNTIME_DIR`, which the system already makes per-user and `0700`. Otherwise use `/tmp/autobahn-<uid>` through `private_dir` from LOCAL-01. If another user owns it, refuse with an error that names the directory.
- **Check the server from the client side.** After connecting, the client verifies with `getpeereid` or `SO_PEERCRED` that the server runs as the same uid. It refuses to send a request otherwise.

## Tests

- A fallback directory owned by another uid is refused. This test runs only as root.
- A fallback directory with `0777` permissions is tightened, or refused if it is owned by someone else.
- A client talking to a server under a different uid refuses the connection. This test runs only as root.
- With `$XDG_RUNTIME_DIR` set, the socket is placed there.
