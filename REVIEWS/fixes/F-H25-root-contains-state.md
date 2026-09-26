# F-H25: A sync root never includes autobahn's own state or config

**Findings:** H-25 (KIMI ABN-H11).
**Status:** proposed. High; fix before v1.

## Problem

`Config::plans()` refuses overlapping endpoints, but never compares an endpoint with the state root (`~/.autobahn`, or `--state-root`) or with the configuration file. `run_sync` doesn't either. The scanner hides only `.autobahn-tmp*` names (`src/scan/mod.rs:59`).

So `alpha = "~"`, a natural setup for syncing dotfiles or a whole home directory, syncs all of `~/.autobahn` as ordinary files:
- `config.toml`;
- `on-alert.sh`;
- the ancestors, locks and status files;
- the installed agent bundle.

Consequences:

- **Remote code execution from a compromised peer.** The peer edits `.autobahn/config.toml` in its copy. The next cycle writes it locally, the live reloader loads it, and the attacker's `agent_command` runs at the next connection. A simpler route: rewrite `on-alert.sh` and wait for an alert.
- **Self-corruption with no attacker at all.** The session's own ancestor journal and locks change underneath it while it syncs them.
- **The agent side has the same problem.** A remote root of `~` includes the remote host's `~/.autobahn`, with its installed agents and peering state.

## Proposed resolution

- **Refuse at planning time.** Where the state root and config path are known, in `Supervisor::new`, `run_sync_config`, `run_sync` and `check_startable`, refuse any *local* endpoint whose resolved identity equals or contains:
  - the resolved state root;
  - the resolved config file's directory, when that isn't the state root.

  Put this in the shared topology check from F-H2, so every entry point gets it. The message: "the root <path> contains autobahn's own state at <state root>; add it to ignores, or choose a narrower root."
- **Or accept it when ignored.** If the state root is covered by one of the group's ignore patterns, as in `ignores = [".autobahn"]`, accept the root. The home-directory sync then stays possible, with one line of config.
- **Exclude it in the scanner as a backstop.** The scanner always treats the state root as excluded when it falls inside a root, whatever the configuration says. It is recorded as untracked, so it is never synced and never deleted. The agent does the same for its own state directory, `~/.autobahn` on the remote host.
- **Remote endpoints.** The controller can't see the remote state root's resolved path. The agent-side backstop covers that side.

## Tests

- `alpha = "~"` with the default state root is refused, with the message above.
- The same with `ignores = [".autobahn"]` is accepted, and after a sync the beta has no `.autobahn` directory.
- A custom `--state-root` inside the root is refused.
- **Backstop:** with the planning check bypassed through a test hook, a scan of a root containing the state root doesn't list it.
- **Agent side:** a remote root equal to the agent's home, with the check disabled, doesn't scan `~/.autobahn`.
