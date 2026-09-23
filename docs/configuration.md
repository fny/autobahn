# Configuration

Everything autobahn does is described in one file, `~/.autobahn/config.toml`. There is no separate registry of sessions to drift out of date: what the file says is what runs.

`autobahn init` writes that file for you: the defaults, every mode explained in a comment, and one example group to edit. The rest of this page is every key it can hold.

## The shape

Each **group** fans one source root (the *alpha*) out to any number of destinations (the *betas*). Each (alpha, beta) pair becomes its own session.

```toml
# ~/.autobahn/config.toml

on_alert = "terminal-notifier -title autobahn -message \"$AUTOBAHN_SUMMARY\""

[defaults]                  # inherited by every group; any key can be
mode = "two-way-conflict"       # overridden per group
ignores = [".git"]
interval = 5                # heartbeat seconds between cycles

[groups.project]
alpha = "~/project"         # the source root you edit
betas = [                   # everywhere it fans out to
  "build.example.com",              # inherits the alpha path (~/project
                                    # in *that* host's home)
  "user@lab.example.com:/srv/project",
  "/mnt/backup/project",            # local paths work too
]
ignores = ["target"]        # appended to the defaults' ignores

[groups.dotfiles]
alpha = "~/.config/shell"
mode = "one-way-alpha"
betas = ["build.example.com"]
```

Remote endpoints use the scp-style `[user@]host:path` syntax, key-based SSH auth, and *either* side of a group may be remote — you can pull from a build server, or relay between two remote hosts through your machine. An endpoint spec is treated as remote unless it visibly looks like a local path (starts with `.`, `/`, or `~`, or has a `/` before any `:`).

Sessions are independent: a host being down means its session retries with backoff and heals the moment the host answers — the others never notice. Sessions targeting the same host share one SSH connection.

## Top level

Seven keys. Unknown keys are refused at startup, not ignored — here and in every section.

| Key | Type | Default | What it is |
|---|---|---|---|
| `on_alert` | string | — | Shell command run when a session needs a person. The only hook. `autobahn init` writes an experimental example at `~/.autobahn/on-alert.sh` to point it at. See [Alerts](./alerts.md). |
| `disabled_hosts` | list of hosts | `[]` | Hosts excluded everywhere. A disabled beta drops that beta; a disabled *alpha* drops the whole group. `autobahn disable --host <host>` edits it for you. |
| `log` | string | `"normal"` | `quiet`, `normal`, or `debug`. See [The log](./logging.md). |
| `[defaults]` | table | — | Session settings every group inherits. Same keys as a group, minus the endpoints. |
| `[groups.name]` | table of tables | — | The sync groups, keyed by a name you choose. The name appears in status, alerts, and `resolve`. |
| `[advanced.alerts]` | table | — | Alerter timing. Correct as shipped. See [Alerts](./alerts.md). |
| `[advanced.peering-experimental]` | table | — | Peering timing: `ttl`, `failover_after`. Correct as shipped. See [Peering](./peering.md). |

Why `defaults` is a table and `log` is not: TOML requires bare keys to appear before the first table header. Every `defaults` key is *also* a valid group key, so a bare `mode = …` written after `[groups.x]` would silently become that group's mode — legal, so no error. `on_alert`, `disabled_hosts` and `log` are valid nowhere else, so the same slip is caught. (`disabled` on its own is a *group* key, and means something else: that one group, off.)

## Session settings

Thirteen keys live in both `[defaults]` and any group, with the group winning. Four exist only on a group.

| Key | Where | Default | What it does |
|---|---|---|---|
| `alpha` | group | required | The source root: a local path, or `[user@]host:path`. |
| `betas` | group | `[]` | Destinations: local paths and/or remote specs. A remote beta with no path inherits the alpha's path. |
| `disabled` | group | `false` | Turns the group off: no sessions, and its settings are not checked. State is kept, so turning it back on resumes. `autobahn disable --group <name>` sets it. Not to be confused with the top-level `disabled_hosts`. |
| `mode` | both | required | Synchronization mode. See [Modes](./modes.md). No default — direction is never guessed. |
| `ignores` | both | `[]` | Gitignore-style patterns. Defaults' patterns apply first, then the group's. See [Ignores](./ignores.md). |
| `ignore_files` | both | `[]` | Files of patterns, by name or path. See [Ignores](./ignores.md). |
| `interval` | both | `5` | Seconds between heartbeat cycles. Watching makes this a fallback, not the reaction time. Floored at 1. |
| `symlink_mode` | both | `"raw"` | `raw` (sync verbatim), `portable` (validate portability), or `ignore`. |
| `file_mode` / `directory_mode` | both | `600` / `700` | Octal permissions for created files and directories. |
| `max_file_size` | both | unlimited | Files larger than this (`"100MB"`, `"2GiB"`) stay on disk but are left out of syncing — never mistaken for deletions. |
| `max_entry_count` | both | unlimited | If a scan finds more entries than this, the cycle fails — a guard against pointing a session at the wrong directory. |
| `staging` | both | `"state"` | Where in-flight content lives: `state`, `beside-root` (same filesystem as the root — guarantees rename-speed publishing), or `inside-root` (for roots that are the only writable place on their host). |
| `default_owner` / `default_group` | both | — | Ownership for created entries (`name`, `1000`, or `id:1000`), resolved on each endpoint's own host. Needs chown rights. |
| `durability` | both | `"process"` | `process` survives a crashed process; `power` additionally syncs each journal append to stable storage, trading a little latency for power-loss durability. Records that announce a transition are synced either way whenever a remote endpoint is involved. |
| `agent_command` | group | ssh | Advanced: reach remote endpoints through this command (whitespace-split argv) instead of SSH. Testing and custom transports. |

## Running it

```sh
autobahn watch              # every configured session, here, until Ctrl-C
```

On a terminal, `watch` is a live `autobahn status` that repaints as sessions report; piped to a file it logs one line per event instead. To keep syncing when no terminal is:

```sh
autobahn install            # register a login service, and start it
autobahn stop               # stop it (it returns at the next login)
autobahn start
autobahn restart            # after editing the config, or upgrading
autobahn uninstall          # stop it, and unregister it
```

The service is launchd on macOS and a systemd user unit on Linux — inspect it with `launchctl` or `systemctl --user` like any other — and it logs to `~/.autobahn/service.log`. There is no daemon of autobahn's own and nothing backgrounds itself: `start` with no service installed says so and points at `install` or `watch`.

The supervisor holds the config in memory, so an edit takes effect on `restart`. A restart does not ask the supervisor to stop: the service manager kills it and starts it again. That is safe — writes are staged and published by rename, and the ancestor journal is built to survive a crash at any point (see [Safety](./safety.md)) — but a cycle in flight is abandoned and redone.

## See also

- [Modes](./modes.md) — which to pick, and what each does case by case
- [Peering](./peering.md) — the peering modes, experimental
- [Ignores](./ignores.md) — pattern semantics, ignore files, and negations
- [Alerts](./alerts.md) — the one hook, and when it fires
- [Overlapping and nested roots](./nesting.md) — what is refused and why
- [Commands](./commands.md) — asking a running supervisor things
