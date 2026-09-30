# PEER-5: Harden the attach socket like the control socket

**Findings:** M-1 (DEEPSEEK F5, GLM M1, KIMI ABN-M1, OPUS S4).
**Status:** fixed, 2026-09-30. The socket is `0600` in a private `peering/`, a connection from another user is refused before anything is read, the greeting has a 10 s deadline and a 64-byte cap, and each connection is greeted on its own thread. The deadline is cleared once the alpha has greeted: the connection idles between cycles. Other local users could not reach the socket before either, since the state root and `peering/` are made `0700` at every start; what this adds is the control socket's checks, and a silent client no longer holds up the alpha. The other-user test runs only as root, and passes under `sudo`.

## Problem

The leading beta binds `~/.autobahn/peering/attach.sock` with default permissions (`src/supervisor/peer.rs:127-133`). It then authenticates a connection by the plain greeting `alpha`, read with an unbounded `read_line` on a single accept thread (`:206-227`). There is no peer-credential check and no timeout.

Any local process that can connect can therefore pose as the alpha. A silent client blocks the accept loop, and a client that never sends a newline grows memory without bound.

## Proposed resolution

Share the control socket's implementation (`src/supervisor/control.rs:262-336`):
- `0700` on the `peering/` directory and `0600` on the socket;
- a same-uid check with `SO_PEERCRED` or `getpeereid` before reading anything;
- read and write timeouts;
- a greeting length cap;
- one thread per connection.

## Tests

- A connection from another uid is refused. This test runs only as root.
- A silent client times out, and a second client is still accepted.
- An over-long greeting is refused.
