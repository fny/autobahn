# PEER-4: Pushed `agent_command` and pushed roots, and locked-down SSH keys

**Findings:** H-20 (KIMI ABN-H2, DEEPSEEK I2), H-21 (KIMI ABN-H3, GLM L4).
**Status:** decided as a documented boundary for now. The feature below is deferred and not in v1.

## Problem

`derive_star` builds each follower's groups with `..group.clone()` (`src/peering.rs:533-536`). The pushed `agent_command`, ignores, modes and owners therefore carry over unchanged. A follower runs that `agent_command` when it takes the lead. It also takes its own root from the path part of the pushed `name` (`src/peering.rs:517-520`).

In the default setup this grants nothing new. A leader already has an SSH login, and so a shell, on every peer. It becomes a real escalation in two cases:
- through PEER-2, on the alpha;
- when a user locks keys with `command="autobahn agent"`.

Locked keys do not contain the ordinary agent either. A controller can pick any root in `Initialize`, for example `$HOME`, and write `~/.bashrc`.

## Proposed resolution

The decision for now is documentation. `docs/peering.md` says every peer that can lead is trusted like a shell, and that locked-down keys are not supported with peering.

If locked-down keys are to be supported later, the feature needs three parts:
- **An agent-side root allowlist,** configured locally on each server. `create_endpoint` refuses any root outside it. rsync's `rrsync` wrapper solves the same problem the same way.
- **No pushed commands.** `derive_star` sets `agent_command = None` on every group it derives. A follower that needs a custom command must configure it locally.
- **Pinned beta roots.** The first push of `name` pins the follower's own root. A later push that changes it is refused until a local command resets the pin.

## Tests, for the feature

- A root outside the allowlist is refused by `create_endpoint`.
- A pushed `agent_command` is not run at takeover.
- A changed pushed `name` is refused once a root is pinned.
