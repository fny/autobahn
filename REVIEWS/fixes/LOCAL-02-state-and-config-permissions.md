# LOCAL-02: State root and config permissions

**Findings:** L-1 (DEEPSEEK F17, F18; GLM L2; KIMI ABN-L1; OPUS), L-2 (DEEPSEEK F16; KIMI ABN-M2).
**Status:** proposed.

## Problem

- **Readable state.** `~/.autobahn`, `sessions/`, `status/`, `service.log`, the ancestors and `config.toml` are created with the default umask, so other local users can usually read them. They reveal hosts, paths, digests and error text. The state root is `0700` today only as a side effect of `control::bind`, so a manual `sync` leaves ancestors at `0644`.
- **The config gets loosened.** `init`, `enable` and `disable` rewrite `config.toml` at the default umask, loosening a `0600` the user set.
- **No writability check.** `Config::load` does not check who can write the config, although it holds `on_alert` and `agent_command`, both commands autobahn runs.
- **Predictable temp names.** Atomic writes use `tmp.<pid>` names (`src/persist.rs:226`, `src/session/ancestor.rs:193`, `src/update.rs`).

## Proposed resolution

- **Private directories at startup.** Every entry point that touches state, including `watch`, `sync`, `status` and `init`, passes the state root and its `sessions/`, `status/`, `staging/` and `peering/` subdirectories through `private_dir` from LOCAL-01.
- **Safe atomic writes.** `write_atomically` creates its temporary with `private_file` and a random suffix. It then applies the mode of the file it replaces, or `0600` for a new file, before the rename.
- **A writability warning.** `Config::load` warns when the config or its directory is group- or world-writable, as ssh does. It warns rather than refuses in v1, to avoid breaking existing setups.
- **The same for other writers.** The ancestor writer and the updater use the same `private_file` temporaries.

## Tests

- After `init` under umask `022`, the state root is `0700` and `config.toml` is `0600`.
- `disable` on a `0600` config keeps it `0600`.
- A world-writable config loads with a warning.
- A manual `sync` leaves ancestors at `0600`.
