# OPS-6: Warn when a root holds credentials

**Findings:** I-2 (KIMI ABN-I2).
**Status:** proposed. The default ignores stay as they are.

## Problem

The default ignores are `.git`, `.DS_Store`, `node_modules` and `target`. A root that covers a home directory therefore syncs `.ssh`, `.aws` and `.gnupg` to every destination, and nothing says so.

Adding them to the default ignores would silently stop syncing files someone may have meant to sync.

## Proposed resolution

- **Warn when it matters.** When a plan is loaded, at `init`, `sync` and `watch` startup, warn once per root if any of the following is true, and the path isn't covered by an ignore:
  - the root is the user's home directory;
  - the root contains `.ssh`, `.aws`, `.gnupg`, `.config/gcloud` or `.kube`.
- **Say what to do.** The warning names the paths, and suggests adding them to `ignores`, or setting `acknowledge_secrets = true` on the group to silence it.
- **Don't nag.** It appears in `status` once, not on every cycle.

## Tests

- A root containing `.ssh` warns.
- The same root with `.ssh` ignored doesn't warn.
- The same root with the acknowledgement set doesn't warn.
