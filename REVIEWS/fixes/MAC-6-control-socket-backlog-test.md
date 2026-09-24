# MAC-6: Two control-socket tests assume a Linux listen backlog, and fail on macOS

**Findings:** new, from MAC-BENCH 7i (found while running the suite). **Status:** proposed, Low (blocks CI-09 alongside MAC-5).

## Problem

On macOS, `cargo test --release` fails two tests that have nothing to do with the code under test:

```
supervisor::control::tests::a_wedged_supervisor_is_unresponsive_within_the_client_timeout
supervisor::control::tests::a_status_report_against_a_wedged_supervisor_returns
panicked at src/supervisor/control.rs:1586: the backlog never filled
```

The fixture wedges a supervisor by filling the socket's accept backlog:

```rust
// Listening again sets the backlog; the smallest fills fast.
assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
…
assert!(full, "the backlog never filled");
```

`listen(fd, 0)` does not mean "queue nothing" on macOS: the kernel takes the value as a hint and keeps its own minimum, so 64 successive connects all succeed and the loop never observes a full queue. On Linux the queue fills as the fixture expects.

Both failures reproduce on untouched `main` (`f78238e`), so they are not caused by anything in this branch, and they are the remaining two of the 604/2 result on macOS 26.5.1, Apple M4, 2026-09-24.

Together with MAC-5 this is the second reason CI-09's macOS job would arrive red.

## Proposed resolution

Wedge the supervisor by a means that does not depend on the kernel's backlog arithmetic. Options, cheapest first:

- Hold the accept loop busy instead of the queue: connect one client and never speak, which is the state the test is really about — a supervisor that does not answer within the client timeout.
- Or keep the backlog trick and make the fixture adaptive: connect until a connect fails *or* a bound is reached, and skip the case with a clear message when the platform will not fill.

The second keeps the coverage on Linux while letting macOS run the rest of the test.

## Tests

- Both tests pass on macOS and on Linux, unchanged in what they assert about the client's timeout behaviour.
- The fixture fails loudly if it can no longer produce an unresponsive supervisor at all, rather than passing vacuously.
