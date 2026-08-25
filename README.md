# Autobahn

Fast, safe, SSH-focused bidirectional file synchronization.

Autobahn keeps a local directory synchronized with another directory — local
or on the far side of an SSH connection — using three-way reconciliation
against a persisted ancestor, rsync-style delta transfer, and a
safety-first transition engine that refuses to destroy content it didn't
expect to find.

It is a from-scratch Rust distillation of the architecture that emerged from
a deep memory/performance overhaul of [Mutagen]'s synchronization engine
(see `../mutagen`): enum-based trees with name-sorted, copy-on-write shared
children; scan metadata resident on the nodes themselves rather than in a
path-keyed cache; linear-merge reconciliation; and streaming transfers end
to end. What required convention and adversarial review to keep safe in Go —
immutable shared hierarchies, copy-on-write without aliasing bugs — the
borrow checker and `Arc::make_mut` enforce structurally here.

## Usage

```sh
# Bidirectional sync between a local directory and a remote one.
autobahn sync ./project user@host:/home/user/project

# Watch continuously; mirror exactly; ignore build artifacts.
autobahn sync ./project user@host:/srv/project \
    --watch --mode one-way-replica --ignore target --ignore '*.log'
```

Remote roots use scp-style `[user@]host:path` syntax and require `autobahn`
(the same version) to be installed on the remote host: the CLI runs
`ssh host autobahn agent` and speaks a framed, version-checked protocol over
stdio. There is no daemon; state (the synchronization ancestor and staged
content) lives under `~/.autobahn/sessions/<session-id>`.

### Synchronization modes

| Mode | Behavior |
|---|---|
| `two-way-safe` (default) | Propagates changes both ways; conflicts are reported and left in place. |
| `two-way-resolved` | Both ways; conflicts resolve in alpha's (the first root's) favor. |
| `one-way-safe` | Alpha → beta only; beta-side changes are never overwritten or reverse-propagated. |
| `one-way-replica` | Beta is an exact mirror of alpha. |

### Safety

- Deleting or emptying a synchronization root halts the session rather than
  propagating the deletion.
- Transitions verify on-disk state against what was scanned before
  replacing or removing anything; concurrent modifications become reported
  problems, never data loss.
- A corrupt ancestor is an error, not a silent reset (a reset would
  resurrect deletions).

## Scope

Unix only. SSH (or any stdio subprocess) transport only — no Docker, no
daemon, no forwarding. Polling-based watching.

## Development

```sh
cargo test          # unit + end-to-end suites (e2e spawns real agent subprocesses)
cargo clippy --all-targets
```

[Mutagen]: https://github.com/mutagen-io/mutagen
