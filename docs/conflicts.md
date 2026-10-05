# Conflict Resolution

When concurrent, competing modifications occur across endpoints in a bidirectional synchronization session, Autobahn halts propagation on the affected paths, flags a conflict, and preserves both versions until resolved.

## Conflict Inspection Commands

Inspect outstanding issues, compare competing file contents, and isolate affected subdirectories:

```sh
# List all operational issues and conflicts across all groups
autobahn issues

# Enumerate conflicts for a specific group or directory
autobahn conflicts ~/Workspace/project
autobahn conflicts mygroup src/subfolder

# Aggregate conflicts by directory depth (useful for mass conflicts)
autobahn conflicts --depth 1

# Filter conflicts by filename or pattern
autobahn conflicts --filter vulns
autobahn conflicts --filter '*.ts'

# View unified diff between conflicting file versions
autobahn diff ./src/main.rs
autobahn diff mygroup src/main.rs
```

> [!NOTE]
> `autobahn conflicts` is an alias of `autobahn issues`. In addition to content discrepancies, the command reports mode/permission conflicts, symbolic link targets, and filesystem type mismatches (e.g., file vs. directory).

## Resolving Conflicts (`autobahn resolve`)

To resolve a conflict, specify which endpoint's version should prevail using `--keep`:

```sh
# Promote Primary's version as authoritative
autobahn resolve ./src/main.rs --keep primary

# Promote a specific remote host's version
autobahn resolve mygroup src/main.rs --keep remotehost

# Retain Primary's version, renaming the remote copy aside (e.g., main.rs.remotehost)
autobahn resolve mygroup src/main.rs --keep both

# Batch resolve all conflicts within a subtree
autobahn resolve mygroup src/subfolder --keep primary

# Resolve all conflicts across an entire group without interactive confirmation
autobahn resolve ~/Workspace/project --all --keep primary --yes
```

## Resolution Mechanics

1. **Retiring the Non-Authoritative Copy:** `autobahn resolve` does not perform a direct byte transfer immediately. Instead, it retires the non-winning copy on the opposing endpoint (or moves it to an adjacent backup name if `--keep both` is specified) and removes the path entry from the session ancestor database.
2. **Re-propagation:** Upon the subsequent synchronization cycle, the winning file is detected as a fresh creation and cleanly propagated across all endpoints in the group.
3. **Atomic Verification:** Removal transitions validate the path state against the latest scan. If a file changed after the `resolve` command was initiated, the deletion is rejected to protect against race conditions.
4. **Immediate Flush:** If a background supervisor is active, `resolve` automatically issues a flush over the supervisor control socket so the winning state propagates immediately. If no supervisor is active, execute `autobahn sync <group>` to complete propagation.

## Blocked Paths

A **blocked path** occurs when an endpoint cannot read or write an entry due to filesystem permissions, missing directory structures, or unsupported filename characters:

- `autobahn issues` prints the root cause and suggested remediation commands (e.g., `chmod` or `chown`).
- Once filesystem permissions or paths are corrected, the subsequent sync cycle clears the blocked status automatically.

## See Also

- [Modes](./modes.md): Which changes become conflicts and which propagate automatically
- [Commands](./commands.md): Session status and conflict commands
- [Desktop App](./app.md): The Conflicts pane and resolution controls
- [Terminal Interface](./shop.md): Conflict inspection and resolution in a terminal
- [Alerts](./alerts.md): Notifications for conflicts and blocked paths
- [Safety](./safety.md): Safeguards against silent overwrites and deletions
