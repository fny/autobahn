# F-M-SUP: A stuck agent, a panicking worker or a slow hook never fails silently

**Findings:**
- M-12: no timeouts on connection setup or requests (KIMI ABN-M9; OPUS M5, M6, M14).
- M-34: a worker panic kills a session silently, and logging can panic (OPUS M4).
- M-35: alert hooks can hang, and their stderr is lost (OPUS M12).
- M-38: a rejected configuration hides the running sessions (ASTRA F27).

**Status:** proposed, for v1. Decided 2026-09-24:
- **M-12:** both setup deadlines and silence detection, now. Silence detection changes the protocol, so this ticket brings a compatibility-epoch bump into v1.
- **M-34:** contain a panic to its own session.

## M-12: detect a stuck agent

### Problem

The mux waits with a plain `recv()` when opening a channel (`src/transport/mux.rs:253`) and for every response (`:357`). The only `recv_timeout` in the file is in a test (`:675`). SSH gets no `ConnectTimeout`. The handshake, the platform probe, the agent upload and the channel open have no deadlines, and the per-host pool lock is held across them (`:605-631`). SSH keepalives catch a dead *network*, but not an agent that is alive and silent: a wedged process, a login stuck in a slow NFS `.bashrc`, or a panic in an agent channel thread that never answers (M-5 in OPUS).

Any of these wedges every session to that host. No error is ever produced, so the supervisor never retries, never backs off, and never alerts. It is the worst failure mode a long-running sync can have.

### Proposed resolution, part A: deadlines on setup

- **Deadlines on each setup step.** Connect gets `-o ConnectTimeout=20`, from OPS-4. The handshake read, the platform probe, the agent upload and the channel open get `recv_timeout` or an I/O deadline. Suggested: 60 s each, and the upload scaled by size.
- **Fail the connection when one expires.** A missed deadline fails the whole connection, with a death reason such as "the agent did not answer the handshake within 60 s". Every waiting session then gets `ConnectionFailed` and goes through normal backoff and reconnection.
- **Release the pool lock.** Don't hold the per-host pool lock across network waits. Take the lock to claim a slot marked "connecting", release it, connect, then publish the result. Other sessions to that host wait on the slot with the same deadline, instead of on the mutex.

### Proposed resolution, part B: silence detection, with an epoch bump

A fixed deadline per response would break legitimate slow work: a scan of a 500k-file tree, hashing a 20 GB file, or a multi-gigabyte push. So detect *no progress*, not *slowness*.

- **Progress counters on the agent.** Each agent channel keeps an atomic progress counter. Scanning, hashing, transfer and apply code advance it as they work: entries walked, bytes hashed, frames handled. That code already loops per unit, so the cost is one relaxed atomic increment per unit.
- **A progress frame.** A new frame, `MuxResponse::Progress { channel, counter }`. Every few seconds the agent's writer sends one for each channel with a request in flight, *only if its counter moved*. It rides the existing multiplexed stream. The controller's router records the last progress time per channel.
- **Detect silence.** A request with no response and no progress for 60 s fails its connection, with a death reason naming the channel, the request, and how long it was silent. Why a separate counter rather than a heartbeat thread: a stuck worker thread stops advancing its counter, even if a heartbeat thread would still tick.
- **The same on the controller.** The controller's local endpoint serves the agent's requests in peering attach mode, so it runs the same counters. Both ends detect silence.
- **Compatibility.** Bump the epoch. The handshake already requires exact versions on both ends, so the change needs no negotiation. Ship it in the same bump as the other queued wire changes: the one-shot flag in `Initialize` (P-13) and any others ready by then.
- **Client calls to the control socket.** `status`, the shop and the tray talk to the supervisor over the control socket, and the tray does it on its event loop (`src/supervisor/control.rs:369-383`, `src/tray.rs:325-362`). Give those client calls a timeout of a few seconds, so a wedged supervisor shows up as "not responding" instead of freezing them.

### Tests

- **A**, using a fake agent that never completes the handshake: the session reports an error within the deadline, retries with backoff, and a second session to the same host isn't blocked meanwhile.
- **B**, using a fake agent that accepts a request and never answers or reports progress: the connection fails after the silence limit, and the session errors and reconnects.
- **B**, using a fake agent that works slowly but steadily, reporting progress every 2 s for 3 minutes: it is *not* failed.
- An agent channel thread that panics, through a test hook, fails its connection within the limit instead of hanging.
- The tray's refresh against a wedged control socket returns within its timeout.

## M-34: contain a panic to its session

### Problem

Session workers run under `thread::scope`, and their handles are joined only when the supervisor shuts down, with `.expect("session worker panicked")` (`src/supervisor/mod.rs:570`). Nothing uses `catch_unwind`. A panicking worker just stops. Its status file keeps its last state, often "synchronized", no alert fires, and the session never runs again until a restart.

Logging can cause that panic. `note!` and `complain!` use `println!` and `eprintln!` (`src/logging.rs:115`, `:128`, `:140`), which panic when stdout is a closed pipe, as with `watch | head`, or when the disk under `service.log` is full.

### Proposed resolution

- **Catch it per session.** Wrap each worker's cycle loop in `catch_unwind`. On a panic:
  - write the session's status as `errored: internal error: <panic message>`, sanitized per F-M-OUT;
  - raise the normal alert for `errored`;
  - restart that worker with the usual backoff;
  - leave the other sessions untouched.
- **Stop looping.** After several panics in a row, for example five within ten minutes, stop restarting that session. Mark it `halted: repeated internal errors`, pointing at the log.
- **Keep state consistent.** Before a restart, release and reacquire the session lock and ancestor store, so a restarted worker starts from what is on disk, as a process restart would. The crash-safety harnesses already cover recovery from an interruption at any point.
- **Make logging unable to panic.** Replace `println!` and `eprintln!` in the log macros with `writeln!` to a locked handle, ignoring errors. A failed log write costs the log line, never the session. Keep one "logging failed" flag, and show it in `status`.
- **Handle the join.** Join handles without `.expect`. Report a panic that escaped containment, and exit nonzero.

### Tests

- A session whose cycle panics, through a test hook, shows `errored: internal error`, fires one alert, restarts, and leaves another session's cycle count advancing.
- Five panics in a row give `halted`.
- `watch` with stdout closed, `autobahn watch | head -1`, keeps syncing after `head` exits.
- A full disk under the log, simulated with a writer that fails, doesn't stop a session.

## M-35: alert hooks within their deadline

### Problem

`run_hook` (`src/alerts.rs:~498-530`) writes the alert document to the hook's stdin *before* the timeout loop starts (`:510-512`). A hook that doesn't read stdin, given a report larger than the pipe buffer, blocks that write forever, and every later alert is skipped. The hook's stderr is piped (`:504`) but never read. `docs/alerts.md` says it goes to the log, but it is lost, and a hook that writes a lot to stderr blocks as well.

### Proposed resolution

- **Write stdin on its own thread,** then close it. The deadline covers the whole hook run. A hook that ignores stdin just gets a closed pipe.
- **Drain stderr on its own thread** into the log, one line per log entry, sanitized, and capped at a few KB per run with a "… truncated" marker.
- **On timeout,** kill the process group rather than just the shell, so a hook's child processes die too. Join both threads.

### Tests

- A hook `sleep 60`, handed a 1 MB document, is killed at the deadline. The next alert still runs.
- A hook that writes three lines to stderr has them logged.
- A hook that writes 10 MB to stderr is capped and doesn't block.

## M-38: status with a broken config

### Problem

`status` parses the configuration on disk to get the list of sessions before showing anything (`src/main.rs:3419`). After an edit the running supervisor refused, as a syntax error or an invalid mode, `status` fails with the parse error. It shows none of the sessions the supervisor deliberately kept running, and not the rejection notice either. The shop and tray behave the same way. An open shop also keeps its original list of sessions after a reload that succeeded.

### Proposed resolution

- **Ask the supervisor first.** When a supervisor is running, `status`, the shop and the tray get the session list from it, over a new control request, and read the config file only when no supervisor answers. The list includes each session's key (F-H12), display name, mode and state. It also carries the rejection notice, if any, so `status` can show "running the last configuration that loaded; your edit was refused: …".
- **Refresh the list.** The shop and tray refresh that list on every poll, so a reload shows up without restarting them.
- **When nothing is running,** `status` reads the saved status files for sessions it can't plan, and says the config is broken, instead of failing.

### Tests

- Break `config.toml` while a supervisor runs. `status` lists the running sessions and the rejection.
- A reload that adds a group shows up in an open shop within one poll.
- With no supervisor running and a broken config, `status` shows the saved status and the parse error.
