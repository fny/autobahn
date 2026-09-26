# T1-2: Validate `Initialize.session` and `Initialize.side` before any filesystem use

**Tier 1 fix for:** the session and side half of H-18 (DEEPSEEK F2, KIMI ABN-H1, GLM M5). The same validator is reusable for H-22.

**Not covered here:** `Initialize.root` in peering attach mode (KIMI ABN-H4). Pinning the root is part of the peering trust decision.

**Status:** proposed, not implemented. Line numbers are from the working tree on 2026-09-24.

## The problem

`create_endpoint` (`src/transport/mod.rs:918`) builds the agent's endpoint from the controller's `Initialize` frame. Two of its strings reach the filesystem unchecked:

```rust
let _ = std::fs::remove_dir_all(staging_area.join(&initialize.session));      // :929
let state_staging = staging_area.join(format!("{}-{}", initialize.session, initialize.side));
let staging_root = crate::endpoint::local::staging_root_for(
    initialize.staging, &root, state_staging, &initialize.session, &initialize.side)?;
```

The session string also reaches the ancestor copy through `open_copy` (`:985`) and `crate::peering::ancestor_copy_path` (`src/peering.rs:290`), which joins it as `ancestors/<session>/ancestor`.

Consequences, when the controller is hostile:

- `session = ".."` makes line 929 delete `~/.autobahn`, including config, every ancestor, and the agent bundle. `"../../.."` reaches `$HOME`. It happens on channel open, before any sync request.
- The staging directory escapes `~/.autobahn/staging`.
- `ancestor_copy_path` can write over another session's real ancestor store.

The controller is hostile only in peering attach mode or against a forced-command agent. Tier 1 still requires that neither case can reach outside the session's own state.

## What legitimate values look like

The code allows an exact rule instead of a loose charset:

- **`side`** is always `"alpha"` or `"beta"`. See `open_endpoints` at `src/supervisor/mod.rs:1521-1522` and `:1652-1653`.
- **`session`** always comes from `session_identifier` (`src/session/mod.rs:260`). That function hashes both endpoint specs with BLAKE3 and hex-encodes the first 16 bytes, giving 32 lowercase hex characters. Configured plans call it at `src/config.rs:1259` and manual sync at `src/main.rs:901`. Peering followers reuse the leader's pushed identifier (`src/peering.rs:574`), which the leader got from the same function.

## The fix

### 1. One validator

Put it next to `Initialize` in `src/protocol.rs`, so both ends can use it:

```rust
/// Whether a string is a session identifier as `session_identifier`
/// produces it: 32 lowercase hex characters.
pub fn is_session_identifier(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Initialize {
    /// Refuses identifiers that could not have come from a genuine
    /// controller. Runs before anything touches the filesystem.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            is_session_identifier(&self.session),
            "refusing session identifier {:?}", self.session
        );
        anyhow::ensure!(
            matches!(self.side.as_str(), "alpha" | "beta"),
            "refusing side {:?}", self.side
        );
        Ok(())
    }
}
```

### 2. Call it first in `create_endpoint`

```rust
fn create_endpoint(initialize: &Initialize) -> Result<LocalEndpoint> {
    initialize.validate()?;
    let home = ...
```

Every later use of `session` and `side` in this function, in `staging_root_for`, and in `open_copy` comes after this line. One call covers all of them.

### 3. Delete the legacy `remove_dir_all`

Line 929 retires staging directories from agent versions that keyed staging by session alone. Once the session is validated it is harmless, but removing a directory named by wire input is the worst sink in the file. It is also migration code for an old layout. Delete it and let `autobahn clean` handle leftovers, or restrict it to a path that `is_session_identifier` accepted and that `symlink_metadata` shows to be a real directory owned by the current user.

### 4. Defensive check in `ancestor_copy_path`

`ancestor_copy_path` is public and takes a `&str`. Make it return `Result<PathBuf>` and check `is_session_identifier` itself. That way a future caller cannot bypass `create_endpoint`. This is also the check H-22 needs for the pushed `sessions/<group>` content, once the peering decision is made.

## Compatibility

- Genuine controllers already send 32-hex sessions and `alpha` or `beta`, so real traffic is unaffected.
- Mixed versions are unaffected, because the handshake already pins an exact version.
- Tests need updating. They use `"session-1"` (`src/endpoint/remote.rs:925`, `:1040`) and `mux-test-{root.len()}` in `src/transport/mux.rs`. Replace them with `session_identifier(..)` over the test's own paths. This also fixes OPUS's finding that all eight mux tests share one id (M-53).

## Tests

1. **Traversal session is refused with no filesystem effect.** Under an isolated `HOME`, create `~/.autobahn/sentinel`. Call `create_endpoint` with `session = ".."`. Assert an error and that the sentinel still exists.
2. **Separator and absolute sessions are refused.** `"a/b"`, `"/tmp/x"`, `""`, and 31 or 33 hex characters.
3. **Uppercase hex is refused.** `session_identifier` never produces it.
4. **Unknown side is refused.** `"gamma"`, `"../alpha"`, `""`.
5. **A genuine identifier is accepted.** `session_identifier("a", "b")` with side `"beta"`.
6. **Through the wire.** Using `serve_agent`, open a channel with a traversal session. Assert the open fails and the sentinel survives.
