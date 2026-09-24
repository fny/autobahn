# LOCAL-08: Refuse to run as root by default

**Findings:** L-6 (KIMI ABN-L12). It also keeps the single-user assumption in `docs/correctness/RETAINED.md` §2 true by default.
**Status:** proposed.

## Problem

- **Nothing checks the effective uid.** Under `sudo` on macOS, `$HOME` stays the calling user's home. So `sudo autobahn watch` creates root-owned state in the user's `~/.autobahn`, which breaks every later unprivileged run. `sudo autobahn install` registers a root service that reads a user-writable config containing commands, a path from the user to root.
- **Root is already a supported deployment.** `default_owner` "needs chown rights", which in practice means the agent runs as root. RETAINED §2 names exactly that deployment as the one that turns its accepted races into privilege escalation.

## Reproduced on macOS, 2026-09-24 (MAC-BENCH 7g)

Commit `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4. `sudo autobahn watch --state-root ~/mb7/state`, with a throwaway config *in that state root*:

```
whoami: root
HOME:   /Users/faraz
supervising 10 session(s); status is available via `autobahn status`
2026-09-24 07:55:53 peering: leading as the alpha at term 1
```

Three things, in rising order of seriousness:

1. **Root-owned state where the user's state lives.** `~/mb7/state` gained `control.sock`, `sessions/`, `status/` and `supervisor/`, all owned by root, in a directory owned by the user.
2. **`--state-root` does not carry the configuration.** The state moved; the config did not. With `$HOME` preserved, it resolved to `~/.autobahn/config.toml`, so root did not supervise the throwaway pair at all — it started **ten sessions of the live fleet** as root, against the user's real roots and hosts. The endpoint locks refused each one, which is the only reason two supervisors did not write the same trees at once.
3. **It wrote into the live state root.** `~/.autobahn/peering/lease.json` is now owned by root, and the user's own supervisor can no longer renew it. The `shared` group took the lease at term 1 from a root process that has since exited. Repairing it needs `sudo chown`.

**Platform note.** Measured on both: macOS `sudo` keeps `HOME=/Users/faraz`, Ubuntu's `Defaults env_reset` sets `HOME=/root`. So on Linux the same command reads root's own (usually absent) config and writes root's own state, and the damage above does not appear *by default*. It is one flag away — `sudo -E`, or `sudo HOME=/home/user autobahn watch` — so the missing check is not macOS-specific, only its default path is.

## Proposed resolution

- **Controller.** `watch`, `sync`, `resolve`, `install` and `start` refuse when the effective uid is 0, unless the user passes `--allow-root` or sets `advanced.allow_root = true`. The refusal explains why.
- **Agent.** The agent refuses as root unless `Initialize` carries `default_owner` or `default_group`. That deployment is deliberate, and the controller's config already says so.
- **Always refused.** Effective uid 0 with a `$HOME` owned by a different uid, the `sudo` case, is refused even with the override.
- **Whatever the uid.** Refuse when the configuration or the state root about to be used is owned by a different user than the process, and say which. That is the platform-neutral form of the same rule, and it also catches `sudo -E` on Linux.
- **Docs.** `docs/configuration.md`, under `default_owner`, says that running the agent as root turns the scanner and transition races into privilege escalation. RETAINED §2 says the same.

## Tests

- As root, `watch` without the override is refused.
- As root with `$HOME` owned by another user, `watch` is refused even with the override.
- As root, the agent without `default_owner` is refused, and with it is accepted.

The first and third tests need root. Run them in CI containers.
