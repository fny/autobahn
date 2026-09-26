# MAC-1: A detached mount under `ignore_mounts = false` reports "synchronized"

**Findings:** new, from MAC-BENCH 4 (step 6). **Status:** proposed, Medium.

## Problem

With `ignore_mounts = false`, a volume mounted inside the alpha syncs like any other directory — correct, and section 4 confirms it. Detaching that volume then leaves the mount point behind as an empty directory, and the session reports **`synchronized`, `error: null`, no conflicts, no blocked paths**, while the two sides plainly differ: alpha's `mnt/` is empty and beta's still holds every file.

Nothing is deleted, which is the important half — the beta keeps its copy. But the state word claims the sides agree when they do not, and no message names the mount. A person watching `status`, the shop or the menu bar has no way to know that half a tree stopped being synchronized.

Measured on macOS 26.5.1, Apple M4, commit `d7c2e21` (epoch 14), 2026-09-24: 50 MB APFS image at `~/mb-synced/a/mnt`, `two-way-conflict`, both sides local. After `hdiutil detach` and a forced `flush`, 22 cycles in, `status --json` still read `{'state': 'synchronized', 'cycles': 22, 'error': None, 'conflicts': [], 'blocked': []}` with `a/mnt` empty and `b/mnt` holding `own.txt` and `secret.txt`.

The likely mechanism: `sessions/<id>/mounts` still records `{"alpha":["mnt"],"beta":[]}` after the flag is turned off, so the boundary keeps protecting the content while the state word is decided by something that no longer knows a mount was ever there.

Expected, per `MAC-BENCH.md` section 4 step 6: the session halts with *mnt on alpha was a mount point and is now empty or gone*.

## Proposed resolution

Decide first whether a recorded mount boundary should survive `ignore_mounts = false` at all. Either:

- the boundary is dropped with the flag, and the empty mount point is then an emptied directory like any other, reaching the existing guards; or
- the boundary is kept, and the session reports it: a halt naming the mount, as the section expects, rather than a state word that says the sides agree.

Whichever is chosen, a session whose two sides differ must not read `synchronized`.

## Tests

- The steps above as an end-to-end test using a loopback or disk image, asserting the state word and that the beta keeps its files.
- `ignore_mounts = true` (the default) keeps its current behaviour: nothing carried, nothing deleted, no halt.
- Turning the flag off while a boundary is recorded, then detaching, reaches whichever of the two behaviours is chosen.
