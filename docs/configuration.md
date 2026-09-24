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

Sessions are independent: a host being down means its session retries with backoff and heals the moment the host answers — the others never notice. Sessions targeting the same host share one SSH connection. The keepalives below catch a dead network; an agent that is alive but has stopped answering is caught too. It reports every few seconds on work in progress, and a request that goes 60 seconds with neither an answer nor any progress fails the connection, naming the request. Its sessions then reconnect as they would after a dropped connection. Slow work, such as scanning a huge tree or hashing a huge file, keeps reporting and is never cut off.

Your `ssh_config` applies to these connections, except for a few options autobahn always sets, which win over it: `-T` (no terminal on the protocol stream), `ForwardAgent=no`, `ForwardX11=no`, `ClearAllForwardings=yes`, `PermitLocalCommand=no`, `ConnectTimeout=20`, `BatchMode=yes`, `ServerAliveInterval=15`, `ServerAliveCountMax=4` and `Compression=no`. The connections stay open for days, so forwarding your agent or a port for their lifetime is not something to inherit by accident. Host-key checking is left to your configuration: with `BatchMode`, an unknown host is refused. `AUTOBAHN_SSH` replaces the `ssh` program itself.

## Top level

Eight keys. Unknown keys are refused at startup, not ignored — here and in every section.

| Key | Type | Default | What it is |
|---|---|---|---|
| `on_alert` | string | — | Shell command run when a session needs a person. The only hook. `autobahn init` writes an experimental example at `~/.autobahn/on-alert.sh` to point it at. See [Alerts](./alerts.md). |
| `disabled_hosts` | list of hosts | `[]` | Hosts excluded everywhere. A disabled beta drops that beta; a disabled *alpha* drops the whole group. `autobahn disable --host <host>` edits it for you. |
| `log` | string | `"normal"` | `quiet`, `normal`, or `debug`. See [The log](./logging.md). |
| `reload` | bool | `true` | Whether the running supervisor re-reads this file and applies an edit in place. See [Editing it while it runs](#editing-it-while-it-runs). |
| `[defaults]` | table | — | Session settings every group inherits. Same keys as a group, minus the endpoints. |
| `[groups.name]` | table of tables | — | The sync groups, keyed by a name you choose. The name appears in status, alerts, and `resolve`. |
| `[advanced.alerts]` | table | — | Alerter timing. Correct as shipped. See [Alerts](./alerts.md). |
| `[advanced.peering-dangerously-experimental]` | table | — | Peering timing: `ttl`, `failover_after`. Correct as shipped. See [Peering](./peering.md). |
| `advanced.allow_root` | bool | `false` | Let `watch`, `sync`, `resolve`, `install` and `start` run as root, as `--allow-root` does. Root with a `$HOME` owned by someone else (`sudo`) is refused regardless. |

Why `defaults` is a table and `log` is not: TOML requires bare keys to appear before the first table header. Every `defaults` key is *also* a valid group key, so a bare `mode = …` written after `[groups.x]` would silently become that group's mode — legal, so no error. `on_alert`, `disabled_hosts`, `log` and `reload` are valid nowhere else, so the same slip is caught. (`disabled` on its own is a *group* key, and means something else: that one group, off.)

## Session settings

Fourteen keys live in both `[defaults]` and any group, with the group winning. Five exist only on a group.

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
| `ignore_mounts` | both | `true` | Leave alone any directory mounted inside a root — another disk, a network share, a `tmpfs` — as `rsync -x` and `du -x` do. It is left out on *both* sides, so a real directory at the same path on the other side is neither filled from the mount nor emptied to match it; and when the mount goes away (a drive unplugged) its empty mount point stays left out, so nothing moves. `false` synchronizes what is mounted as part of the tree, and halts rather than deletes when a mount that held content comes back empty. |
| `staging` | both | `"state"` | Where in-flight content lives: `state`, `beside-root` (same filesystem as the root — guarantees rename-speed publishing), or `inside-root` (for roots that are the only writable place on their host). |
| `default_owner` / `default_group` | both | — | Ownership for created entries (`name`, `1000`, or `id:1000`), resolved on each endpoint's own host. Needs chown rights, which in practice means an agent running as root — the one case an agent accepts root. That turns the scanner and transition races in [RETAINED §2](./correctness/RETAINED.md) into privilege escalation, so set it only where root is really meant. |
| `durability` | both | `"process"` | `process` survives a crashed process; `power` additionally syncs each journal append to stable storage, trading a little latency for power-loss durability. Records that announce a transition are synced either way whenever a remote endpoint is involved. |
| `acknowledge_secrets` | group | `false` | Silences the warning that a local root holds credentials — `.ssh`, `.aws`, `.gnupg`, `.config/gcloud` or `.kube` that its ignores leave in, or any of them when the root is your home directory. `sync` and `watch` print that warning once per root when they start, because every destination would receive those files. Set it on a group that means to synchronize them; otherwise add them to `ignores`. |
| `agent_command` | group | ssh | Advanced: reach remote endpoints through this command (whitespace-split argv) instead of SSH. Testing and custom transports. |

### What is refused

Beyond keys and values it does not know, a configuration is refused at startup, with the session named, when:

- **a session's two sides are one tree, or one is inside the other.** The session would consume its own output — in a mirroring mode, delete the alpha through the beta path. Roots are compared as resolved, so a symbolic link alias or a trailing `/.` is the same tree. The same check guards `autobahn sync ALPHA BETA`. One session's beta feeding another's alpha (a relay) stays legal; two sessions writing nested roots do not — see [Overlapping and nested roots](./nesting.md).
- **a local root is, or holds, autobahn's own directories**: the state root (`~/.autobahn`, or `--state-root`), the default state root when another is in use, or the directory the configuration file lives in. Synchronized, they would change under the session using them, and a peer that edits its copy of `config.toml` or `on-alert.sh` would choose what runs here next. A root that holds one is accepted when its ignores keep it out — `ignores = [".autobahn"]` for a root of `~`. `watch` checks each edit the same way and keeps the last configuration if one fails.
- **the command is running as root** without `--allow-root` or `advanced.allow_root`, or as root under someone else's home (`sudo`), which is refused regardless.

A root that holds credentials is warned about, not refused: see `acknowledge_secrets` above.

### Symbolic links

In the default `raw` mode a symbolic link is synchronized as what it is — a link — with its target copied verbatim, even when the target is absolute or points outside the root. Autobahn never follows a link, neither when it scans nor when it writes, so a link aimed outside the root cannot make autobahn read or write there. Other tools that walk the synchronized tree may follow it, though: a backup, an indexer, or a build on the other machine will see whatever that target names *there*. For trees other tools will walk, set `symlink_mode = "portable"`: a link must then be relative and stay inside the root, and one that is not is reported as a problem and left out rather than copied. `ignore` leaves every link out.

## Running it

```sh
autobahn watch              # every configured session, here, until Ctrl-C
```

On a terminal, `watch` is a live `autobahn status` that repaints as sessions report; piped to a file it logs one line per event instead. To keep syncing when no terminal is:

```sh
autobahn install            # register a login service, and start it
autobahn stop               # stop it (it returns at the next login)
autobahn start
autobahn restart            # after upgrading
autobahn uninstall          # stop it, and unregister it
```

The service is launchd on macOS and a systemd user unit on Linux — inspect it with `launchctl` or `systemctl --user` like any other — and it logs to `~/.autobahn/service.log`. There is no daemon of autobahn's own and nothing backgrounds itself: `start` with no service installed says so and points at `install` or `watch`.

A restart does not ask the supervisor to stop: the service manager kills it and starts it again. That is safe — writes are staged and published by rename, and the ancestor journal is built to survive a crash at any point (see [Safety](./safety.md)) — but a cycle in flight is abandoned and redone. An upgrade needs one; an edit to the configuration does not.

## Editing it while it runs

The running supervisor reads the file every two seconds and acts on an edit once it has read the same bytes twice, so a file caught half-written is read again rather than refused. The edit gets the checks `start` makes — a key it does not know, a mode it does not have, a group with no sessions — and one of two things happens:

- **It loads.** The sessions wind down between cycles and start again under the new configuration, in the same process: groups added start, groups removed stop, and a group whose settings changed starts over from its kept state. `status` and `mi` show the new sessions; `autobahn watch` on a terminal repaints with them.
- **It is refused.** The sessions keep running under the configuration that last loaded, and the refusal is said everywhere the supervisor speaks: the log, `status` (a line above the sessions), `mi` (a line under the sign), the tray (a line in the menu, and a notification), and `on_alert` (one firing, `AUTOBAHN_STATES=config`, `AUTOBAHN_EVENT=config`). The line stands until the file loads again. Nothing is retried in the meantime, and the next edit is judged on its own.

`reload = false` at the top level turns the watch off, and an edit lands on `restart` as before. An edit that *sets* it is the last one applied in place; one that sets it back lands on `restart`. A peer (a machine following a leader's configuration) has no file of its own to watch, and the alpha of a peering group watches its file only while it leads — an edit made while a beta leads is found when the lead comes back.

## See also

- [Modes](./modes.md) — which to pick, and what each does case by case
- [Peering](./peering.md) — the peering modes, dangerously experimental
- [Ignores](./ignores.md) — pattern semantics, ignore files, and negations
- [Alerts](./alerts.md) — the one hook, and when it fires
- [Overlapping and nested roots](./nesting.md) — what is refused and why
- [Commands](./commands.md) — asking a running supervisor things
