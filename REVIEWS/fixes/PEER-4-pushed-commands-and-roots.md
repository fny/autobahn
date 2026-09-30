# PEER-4: Pushed `agent_command` and pushed roots, and locked-down SSH keys

**Findings:** H-20 (KIMI ABN-H2, DEEPSEEK I2), H-21 (KIMI ABN-H3, GLM L4).
**Status:** planned, 2026-09-30, as the restricted-keys feature below. Not yet scheduled; until it lands, the documented boundary stands. PEER-1, PEER-2 and PEER-3 come first.

## Problem

`derive_star` builds each follower's groups with `..group.clone()` (`src/peering.rs:533-536`). The pushed `agent_command`, ignores, modes and owners therefore carry over unchanged. A follower runs that `agent_command` when it takes the lead. It also takes its own root from the path part of the pushed `name` (`src/peering.rs:517-520`).

In the default setup this grants nothing new. A leader already has an SSH login, and so a shell, on every peer. It becomes a real escalation in two cases:
- through PEER-2, on the alpha;
- when a user locks keys with `command="autobahn agent"`.

Locked keys do not contain the ordinary agent either. A controller can pick any root in `Initialize`, for example `$HOME`, and write `~/.bashrc`.

## Why it matters most for peering

Without peering, only the alpha holds keys to the betas, and a compromised beta holds none. Peering gives every beta a key with a full shell on every other beta, so that any of them can take over: one compromised beta is all of them. The feature below turns those keys into ones that can only run the agent, over folders each server allows, so a compromised beta can sync within those folders on the others and do nothing else.

## Plan: restricted keys

Until this lands, `docs/peering.md` says every peer that can lead is trusted like a shell, and that locked-down keys are not supported with peering.

1. **Allowed folders on each server.** A local setting on the server (for example `roots = ["~/Workspace"]`). The agent refuses any root outside it, resolved through symbolic links, as rsync's `rrsync` does. The same list bounds the server's own root when it leads, which replaces pinning the pushed `name`: a pushed name outside the list is refused. Useful without peering too.
2. **A gate for restricted keys.** The `authorized_keys` line forces every connection through one entry point, which reads `SSH_ORIGINAL_COMMAND` and lets through only the agent and `autobahn peering attach` (the alpha's connection to a leading beta). Installing and upgrading the agent today runs shell commands over SSH, which the gate refuses; the gate instead accepts an upgrade only as an officially signed release, verified against the minisign release key. Without that, upgrades are manual on each server.
3. **No pushed commands.** Once a leader has no shell on its followers, a pushed `agent_command` would be one. `derive_star` sets `agent_command = None` on every group it derives; a follower that needs one sets it locally. PEER-3's identifier check is a prerequisite.
4. **Automatic key setup.** When peering is turned on, the alpha sets the keys up on its first cycles, since it already reaches every beta:
   - It asks each beta's agent to make a peering key pair in `~/.autobahn/peering/` (mode `0600`) and return the public half. Private keys never leave the host that made them.
   - It collects each beta's SSH host key and pushes a peering `known_hosts`, so betas verify each other on first contact, with no prompt and no chance to be impersonated.
   - It installs every other beta's public key in each beta's `~/.ssh/authorized_keys`, each line forced through the gate, inside a marked block autobahn owns. Lines outside the block are never touched.
   - A beta taken out of the configuration has its key removed from the others on the next push. A `rekey` verb replaces them all.

   Constraints:
   - Opt-in. Where `authorized_keys` is managed centrally or read-only, the lines are printed for the user to install instead.
   - Only the alpha manages keys: its connection is the user's ordinary login. The gate refuses key management over a peering key, so a leading beta — compromised or not — cannot widen its own access.
   - `from=` restrictions are off by default. The address a beta connects to another from is not necessarily the one the alpha sees (private against public addresses), and a wrong one locks peers out of each other.
   - Worth doing only with the gate in place.

What stays exposed: a compromised beta can still change files inside the folders it syncs, as two-way sync allows; and the alpha's own keys stay as powerful as the user makes them (the gate applies to them too, if wanted).

Effort: four to five days for parts 1 to 3 (allowlist about a day, gate with signed upgrades one to two, no pushed commands about a day), one to two more for part 4, and docs.

## Tests, for the feature

- A root outside the allowlist is refused by `create_endpoint`, including one reached through a symbolic link.
- A pushed `name` whose root is outside the follower's allowlist is refused.
- The gate refuses a shell command, an install script, and key management; it runs the agent and `peering attach`.
- An upgrade offered through the gate is installed only when its signature verifies.
- A pushed `agent_command` is not run at takeover.
- Key setup: every beta ends with the others' keys inside its managed block and nothing outside it changed; a removed beta's key is gone after the next push; a peering key cannot run key management.
