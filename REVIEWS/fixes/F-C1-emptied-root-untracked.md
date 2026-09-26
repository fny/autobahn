# F-C1: An emptied root is emptied, even with an ignored file left in it

**Findings:** C-1 (OPUS C1, reproduced).
**Status:** proposed. Critical; fix before v1.

## Problem

The emptied-root halt (invariant I7) exists for the case that "an unmounted disk is far more likely than a deliberate wipe." It decides that a side is "gone" by checking for no children at all (`src/session/mod.rs:1351-1354`):

```rust
let gone = |side: Option<&Node>| match side {
    None => true,
    Some(node) => node.children().is_empty(),
};
```

The scanner records ignored entries as `Content::Untracked` children, and so it does for FIFOs, sockets, oversized files, and symlinks under the `ignore` mode. `.DS_Store` and `.git` are in the default ignore list. So a root that has lost everything that syncs, but still holds one ignored entry, is not "gone". The halt doesn't fire. Reconciliation then sees every ancestor entry as deleted on that side and deletes every file on the other side. This happens in every mode, including `two-way-paranoid`.

OPUS reproduced it: an ancestor with 20 files, alpha holding only an untracked `.DS_Store`, beta unchanged, in `two-way-paranoid`. The result was no halt, and 20 deletions for beta.

Realistic triggers:
- an unmounted volume whose bare mount point keeps a `.DS_Store` or `.git`;
- a wipe that leaves `.git` or `node_modules`;
- a restore tool that recreates only dotfiles.

The paranoid mode's per-directory guard has the same flaw (`src/tree/reconcile.rs:204-209`). It checks `children().is_empty()` for a subdirectory, so a directory emptied down to one ignored file isn't treated as emptied either.

## Proposed resolution

- **One helper.** Add `Node::holds_synchronizable()` in `src/tree/mod.rs`. It is true when any child's content is `synchronizable()`: a directory, file or symlink.
- **The halt.** In `one_side_emptied_root`, `gone` becomes `None => true, Some(node) => !node.holds_synchronizable()`.
- **The paranoid guard.** Its `empty` check uses the same helper.
- **Leave the ancestor count alone.** `entries_below(ancestor) >= 2` stays as it is. The ancestor records only synchronizable content, so its count is already right.
- **Deliberately not changed.** A root emptied down to one empty *directory* still counts as not gone. That is a much rarer shape, and changing it would need a threshold rather than a yes/no. Note it in the I7 boundary text.

## Tests

- **`emptied_root_detection`.** Add three cases:
  - a side holding only an `Untracked` `.DS_Store` counts as gone;
  - a side holding only an `Untracked` `.git` directory counts as gone;
  - a side holding one real file plus an untracked entry does not.
- **Session level.** An ancestor with 20 files, alpha with only an untracked `.DS_Store`, and beta unchanged gives `SafetyHalt::RootEmptied` in every mode, with no beta transitions.
- **Paranoid mode.** A subdirectory emptied down to one untracked entry is reported as a conflict, as a truly empty one is.
- **Property test.** Teach the reconcile proptest generator to place `Untracked` nodes at every depth, including the root, in every mode. Add a "no silent loss" property: every change a side made since the ancestor either propagates or surfaces as a conflict. This covers the class, which also includes H-8 and M-33.
- **Mutation check.** Revert the helper in `one_side_emptied_root`, and confirm the session-level test fails.

## Docs

`docs/correctness/INVARIANTS.md` I7: say that emptiness is judged by synchronizable content, and name the new test in "Checked by".
