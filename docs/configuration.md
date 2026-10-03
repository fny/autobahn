# Configuration

Everything autobahn does is described in one file, `~/.autobahn/config.toml`. There is no separate registry of sessions to drift out of date: what the file says is what runs.

`autobahn init` writes that file for you: the defaults, every mode explained in a comment, and one example group to edit. The rest of this page is every key it can hold.

## The shape

Each **group** fans one source root (the *primary*) out to any number of destinations (the *replicas*). Each (primary, replica) pair becomes its own session.

```toml
# ~/.autobahn/config.toml

on_alert = "~/.autobahn/on-alert.sh"

[defaults]                  # inherited by every group; any key can be
mode = "two-way-conflict"       # overridden per group
ignores = [".git"]
interval = 5                # heartbeat seconds between cycles

[groups.project]
primary = "~/project"         # the source root you edit
replicas = [                   # everywhere it fans out to
  "build.example.com",              # inherits the primary path (~/project
                                    # in *that* host's home)
  "user@lab.example.com:/srv/project",
  "/mnt/backup/project",            # local paths work too
]
ignores = ["target"]        # appended to the defaults' ignores

[groups.dotfiles]
primary = "~/.config/shell"
replicas = ["build.example.com"]
mode = "one-way-primary"
```

### Endpoint Syntax

Autobahn uses SSH key-based authentication, s you need to have your target machines in your `~/.ssh/config`.

- **Local Endpoints:** Paths beginning with `/`, `~`, or `./`.
- **Remote Endpoints:** Standard SSH syntax (`[user@]host[:path]`). If `:path` is omitted, the endpoint defaults to the same path as `primary` evaluated within the remote user's home directory.
- **SSH Transport:** Autobahn runs persistent SSH connections enforcing secure defaults (`ClearAllForwardings=yes`, `ForwardAgent=no`, `ForwardX11=no`, `PermitLocalCommand=no`, `Compression=no`, `ServerAliveInterval=15`). Custom SSH binaries can be configured using `AUTOBAHN_SSH`.

## Top-Level Settings

Top-level keys must precede section headers in the TOML document. Unknown keys are rejected on load.

| Key | Type | Default | Description |
| :--- | :---: | :---: | :--- |
| `on_alert` | string | `""` | Shell command executed when an alert condition persists. See [Alerts](alerts.md). |
| `disabled_hosts` | list of strings | `[]` | Excludes listed hosts across all groups. Disabling a primary host suspends its entire group. |
| `log_level` | string | `"normal"` | Supervisor logging verbosity: `"quiet"`, `"normal"`, or `"debug"`. |
| `power_saver_experimental` | boolean | `false` | When running on battery power, extends the full-walk audit interval from 2 minutes to 10 minutes. |
| `live_reload` | boolean | `true` | Automatically detects edits to `config.toml` and reconfigures active workers without restarting the process. |
| `experimental.allow_root` | boolean | `false` | Allows the supervisor to run under UID 0 (`root`). Requires matching home directory ownership. |
| `[defaults]` | table | — | Shared configuration inherited by all groups. |
| `[groups.<name>]` | table | — | Definition of a named synchronization group. |
| `[experimental.alerts]` | table | — | Tuning parameters for alert confirmation thresholds. |
| `[experimental.p2p-dangerously-experimental]` | table | — | Timing parameters (`ttl`, `failover_after`) for failover clusters. |

## Session and Group Settings

Settings defined in `[defaults]` are inherited by all groups. Group-level definitions take precedence over defaults, except `ignores`, which appends group rules to defaults.

| Setting | Scope | Default | Description |
| :--- | :---: | :---: | :--- |
| `primary` | Group | *(Required)* | Source root path (local path or `[user@]host:path`). |
| `replicas` | Group | `[]` | List of destination endpoints. |
| `mode` | Both | *(Required)* | Synchronization policy. See [Sync Modes](modes.md). |
| `ignores` | Both | `[]` | Gitignore-compatible exclusion patterns. |
| `interval` | Both | `5` | Fallback polling interval in seconds between idle synchronization checks. Minimum: 1. |
| `disabled` | Group | `false` | Suspends synchronization for this group while retaining session baselines. |
| `symlink_mode` | Both | `"raw"` | Symbolic link handling: `"raw"` (verbatim), `"portable"` (enforces intra-root relative targets), or `"ignore"`. |
| `file_mode` / `directory_mode` | Both | `"600"` / `"700"` | Octal permission strings applied to newly created files and directories. |
| `max_file_size` | Both | unlimited | Skips files exceeding this size threshold (e.g., `"100MB"`, `"2GiB"`) without deleting them. |
| `max_entry_count` | Both | unlimited | Halts the cycle if an endpoint scan exceeds this total file count. |
| `ignore_mounts` | Both | `false` | If `true`, ignores mounted subdirectories (e.g., NFS, external drives, `tmpfs`). If `false`, disappearing mounts trigger safety halts. |
| `guard_dir_deletes_over` | Both | unset | Converts mass directory deletions (above the threshold count) into conflicts requiring review. |
| `staging` | Both | `"state"` | In-flight staging directory location: `"state"` (`~/.autobahn/staging`), `"beside-root"` (guarantees same-filesystem atomic renames), or `"inside-root"`. |
| `default_owner` / `default_group` | Both | — | Explicit user/group ownership for created entries (requires root/elevated permissions). |
| `durability` | Both | `"process"` | Persistence level: `"process"` (survives application crashes) or `"power"` (syncs intent to disk, surviving power failure). |
| `acknowledge_secrets` | Group | `false` | Suppresses warnings when synchronizing directories containing sensitive tokens (`.ssh`, `.aws`, `.kube`). |



## Validation & Refusal Rules

Configurations are validated prior to execution. The supervisor halts or rejects changes under the following conditions:

1. **Self-Referential or Nested Roots:** A session's source and destination cannot resolve to the same filesystem tree, nor can one contain the other.
2. **State Directory Collisions:** Synchronized roots cannot encompass Autobahn's internal state directories (`~/.autobahn`), configuration files, or log directories unless explicitly excluded via `ignores = [".autobahn"]`.
3. **Privilege Escalation:** Running as root via `sudo` where `$HOME` belongs to a non-root user is explicitly rejected.

### Symbolic links

`raw` copies the link target verbatim, including absolute targets and targets outside the root. Autobahn treats links as entries and checks for linked parents before reads and writes. Local pathname check/use races remain. See [Retained Risks](./correctness/accepted-risks.md#2-pathname-toctou-outside-linux-creations).

Other software can follow these links on the destination machine. For trees used by backups, indexers, or builds, consider `symlink_mode = "portable"`. Portable mode requires relative targets that remain inside the root. It reports and excludes invalid links. `ignore` excludes all links.

## Live Reload Behavior

When `live_reload = true` (default), the supervisor monitors `~/.autobahn/config.toml` continuously:

- **Valid Changes:** The supervisor reconciles differences between the running configuration and the new file. Removed groups stop, newly added groups start, and modified groups reload state without interrupting unaffected sessions.
- **Syntax / Validation Errors:** The supervisor rejects the update and continues running under the previous valid configuration. The refusal is reported via `autobahn status`, service logs, and an alert event (`AUTOBAHN_STATES=config`).

## Host-Local Policy (`~/.autobahn/host.toml`)

To restrict which local directories remote controllers are allowed to access via Autobahn agents, define an immutable host policy on the target machine:

```toml
# ~/.autobahn/host.toml (on remote host)

# Whitelist directories the local agent will serve
roots = ["~/Workspace", "/srv/repositories"]
```
