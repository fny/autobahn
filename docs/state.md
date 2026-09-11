# State

Everything autobahn keeps lives under one directory, `~/.autobahn`, on
every machine it touches. Removing it is a full uninstall (aside from
the binary itself).

| path | holds |
|---|---|
| `config.toml` | the configuration — the source of truth |
| `sessions/<id>/` | each session's ancestor and journal: what was last agreed between its two roots |
| `status/<id>.json` | what each session is doing, or last did; what `status` reads |
| `staging/` | in-flight content, held aside until verified, then renamed into place; swept at the end of every cycle, so a version that changed while in flight does not linger |
| `endpoint-locks/` | one lock per pair of roots, so two sessions never write one tree from independent ancestors |
| `agents/` | the agent bundle — binaries for platforms other than this one |
| `bin/autobahn-<version>` | on a *remote* host: the agent this controller streamed there |
| `ignores/` | ignore files, named from the config — see [Ignores](./ignores.md) |
| `service.log` | the supervisor's log — see [The log](./logging.md) |
| `icon.png` | autobahn's icon, for notifiers |

## Sessions outlive the config

Removing a group from the config stops its sessions but keeps their
state, so that adding the group back later resumes from memory rather
than re-merging two drifted trees. `clean` is how that state is
eventually let go:

```sh
autobahn clean --dry-run   # what state belongs to sessions no longer in the config
autobahn clean             # remove it
```

It removes ancestors, status records, staged content, and endpoint locks
for any session the config no longer describes. Anything a running
session holds is skipped, and the files in the synchronized trees are
never touched.

Staged content this machine holds *as an agent* for sessions driven from
other machines cannot be attributed from here, so it is left alone unless
`--agent-staging-older-than DAYS` asks for it by age.

## Agents

Remote hosts need nothing pre-installed. Connections invoke a versioned
agent path (`~/.autobahn/bin/autobahn-<version>`); when it is missing —
a fresh host, or your first connect after upgrading — the controller
probes the platform, streams the matching agent into place over the same
SSH connection, and retries. Upgrades therefore roll out host by host,
automatically, on first contact.

The binary to stream is looked for in `AUTOBAHN_AGENTS_DIR`, then
`~/.autobahn/agents`, then an `agents` directory beside the running
executable — and, when the remote platform matches the local one, the
running executable itself. A fleet on one platform needs no bundle at
all.

Old versions are kept, so an older controller reconnecting finds its
agent already there. Nothing removes them, so a host accumulates one
binary (about 5 MB) per version that has ever contacted it:

```sh
autobahn clean --agents                  # prune, keeping the version in use + 1
autobahn clean --agents --keep-agents 3  # more rollback headroom
```

The version in use is never a candidate and never spends a `--keep-agents`
slot. It is off by default because everything else `clean` does is local
and this reaches out over SSH; a host that cannot be reached is reported
and stepped over rather than failing the run. Removal is by exact name
under the one directory, never a glob.

One thing to know: "in use" means the version of the binary *running
`clean`*, which is normally the supervisor's version too. If you have
built a newer binary but not yet restarted, they differ, and the default
`--keep-agents 1` is what protects the running supervisor's agent.

## Compatibility epochs

The agent's version must match the controller's exactly. A change that
breaks the wire protocol, or one that makes the two sides disagree about
a tree — a scan rule, an ignore rule — bumps a compatibility epoch that
rides inside the version string (`0.4.0+e8`). A mismatched agent fails
the handshake, and the installer places the new agent at a path the old
one never occupied, so both sides are enforced with no protocol change.

The one failure the installer cannot catch itself is a *stale bundle*:
an `agents/` binary left over from an older build gets uploaded under the
new name, and the handshake is the first thing to notice. The message
names the bundle and its age when that happens.

## See also

- [Commands](./commands.md) — `clean`, `reset`, `verify`
- [Ignores](./ignores.md) — the `ignores/` directory
- [The log](./logging.md) — `service.log`
- [Safety rules](./safety.md) — why a corrupt ancestor is an error, not a reset
