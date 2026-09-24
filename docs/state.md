# State

Everything autobahn keeps lives under one directory, `~/.autobahn`, on every machine it touches. Removing it is a full uninstall (aside from the binary itself).

`AUTOBAHN_HOME` moves that directory, whole: the installer puts the agent bundle there, and every command reads its configuration and state from there. `autobahn install` writes the variable into the login service it registers, since a service inherits nothing from the shell it was installed from; re-run it after changing the variable. The variable is the controller's own — a remote host keeps its agent under its own `~/.autobahn` regardless. `--state-root` and `--config` on a command still override it.

| path | holds |
|---|---|
| `config.toml` | the configuration — the source of truth |
| `sessions/<id>/` | each session's ancestor and journal: what was last agreed between its two roots |
| `status/<id>.json` | what each session is doing, or last did; what `status` reads |
| `staging/` | in-flight content, held aside until verified, then renamed into place; swept at the end of every cycle, so a version that changed while in flight does not linger |
| `endpoint-locks/` | one lock per pair of roots, so two sessions never write one tree from independent ancestors |
| `agents/` | the agent bundle — binaries for platforms other than this one |
| `bin/autobahn-<version>-<digest>` | on a *remote* host: the agent this controller streamed there, named by its version and its content |
| `ignores/` | ignore files, named from the config — see [Ignores](./ignores.md) |
| `peering/` | experimental: the lease, this host's name in the star, the pushed configuration, and the ancestor copies a leader keeps here — see [Peering](./peering.md). `clean` leaves it alone |
| `service.log` | the supervisor's log — see [The log](./logging.md) |
| `control.sock` | the running supervisor's control socket, through which `flush`, `reset`, `verify` and `pause` reach live sessions |
| `config-notice.json` | present while the running supervisor is refusing an edit to `config.toml` — what `status`, `mi` and the menu bar app show for it; removed when the file loads again, or a supervisor starts — see [Editing it while it runs](./configuration.md#editing-it-while-it-runs) |
| `icon.png` | autobahn's icon, for notifiers |
| `on-alert.sh`, `open-status` | the example alert hook `init` writes, and what a click on its notification opens — see [Alerts](./alerts.md). Yours to edit; never replaced |

## Sessions outlive the config

Removing a group from the config stops its sessions but keeps their state, so that adding the group back later resumes from memory rather than re-merging two drifted trees. `clean` is how that state is eventually let go:

```sh
autobahn clean --dry-run   # what state belongs to sessions no longer in the config
autobahn clean             # remove it
```

It removes ancestors, status records, staged content, and endpoint locks for any session the config no longer describes. Anything a running session holds is skipped, and the files in the synchronized trees are never touched.

Staged content this machine holds *as an agent* for sessions driven from other machines cannot be attributed from here, so it is left alone unless `--agent-staging-older-than DAYS` asks for it by age.

## Agents

Remote hosts need nothing pre-installed. Connections run the agent this controller would install for the host's platform, by a path naming its version and a digest of its bytes (`~/.autobahn/bin/autobahn-0.4.0+e13-613662c7aad6`) — the platform is read on the host, in the same command, so it costs no extra round trip. When that path is missing — a fresh host, your first connect after upgrading, or a rebuild at the same version — the controller streams the matching agent into place and retries. Upgrades therefore roll out host by host, automatically, on first contact, and the agent running is always exactly the build the controller holds.

The binary to stream is looked for in `AUTOBAHN_AGENTS_DIR`, then `~/.autobahn/agents`, then an `agents` directory beside the running executable — and, when the remote platform matches the local one, the running executable itself. A fleet on one platform needs no bundle at all.

Old versions are kept, so an older controller reconnecting finds its agent already there. Nothing removes them, so a host accumulates one binary (about 5 MB) per version that has ever contacted it:

```sh
autobahn clean --agents                  # prune, keeping the version in use + 1
autobahn clean --agents --keep-agents 3  # more rollback headroom
```

The version in use is never a candidate and never spends a `--keep-agents` slot. It is off by default because everything else `clean` does is local and this reaches out over SSH; a host that cannot be reached is reported and stepped over rather than failing the run. Removal is by exact name under the one directory, never a glob.

One thing to know: "in use" means the version of the binary *running `clean`*, which is normally the supervisor's version too. If you have built a newer binary but not yet restarted, they differ, and the default `--keep-agents 1` is what protects the running supervisor's agent.

## Compatibility epochs

The agent's version must match the controller's exactly. A change that breaks the wire protocol, or one that makes the two sides disagree about a tree — a scan rule, an ignore rule — bumps a compatibility epoch that rides inside the version string (`0.4.0+e8`). A mismatched agent fails the handshake, and the installer places the new agent at a path the old one never occupied, so both sides are enforced with no protocol change.

A *stale bundle* — an `agents/` binary left over from an older build — is refused before upload when the bundle has a `MANIFEST`, which every released bundle does: the message names the bundle, the build it is for, and `autobahn update` as the fix. A bundle built by hand has no manifest; its stale binary is uploaded, the handshake refuses it, and the message names the bundle and its age.

## See also

- [Commands](./commands.md) — `clean`, `reset`, `verify`
- [Ignores](./ignores.md) — the `ignores/` directory
- [The log](./logging.md) — `service.log`
- [Safety](./safety.md) — why an unreadable ancestor is rebuilt only when both sides match, and never reset
