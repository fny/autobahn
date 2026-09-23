# Syncing a git checkout

Two working copies of one repository, on two machines, kept in sync by autobahn — branches, commits and all, without pushing and pulling between them. It works, with a short ignore list. This page is what to write, what to expect, and what not to do.

## The configuration

```toml
[groups.project]
mode  = "two-way-conflict"
alpha = "~/Workspace/project"
betas = ["ubuntu@box:~/Workspace/project"]
ignores = [
  ".git/index",          # this machine's stat cache — never the same twice
  ".git/*.lock",         # taken and released in milliseconds
  ".git/**/*.lock",
  ".git/logs",           # the reflog: what *this* machine did
  ".git/gc.pid",
  ".git/FETCH_HEAD",
  ".git/ORIG_HEAD",
  ".git/COMMIT_EDITMSG",
  # ...and whatever the project ignores anyway:
  "target", "node_modules",
]
```

That is the whole setup. Everything else in `.git` synchronizes cleanly, because of what it is:

- **Objects and packs** are content-addressed and written once, by rename. Equal names mean equal bytes; a conflict is impossible.
- **Refs, `HEAD`, `packed-refs`, `config`, hooks** are small files written atomically. A commit on either side moves the branch on both within a cycle.

What the ignore list keeps out is everything that describes *one machine's* state rather than the repository's. The index is the important one: it caches the size, inode, and timestamps of every file in the working tree as this machine sees them, so on another machine every entry mismatches and `git status` there rehashes the whole tree and rewrites the index — which would then sync back, and the two sides would trade index writes indefinitely. Ignored, each side keeps its own.

## What to expect

Tested by `bench/git-sync.sh`, which keeps two clones in sync (the second over ssh), runs commits on either side, a branch and a checkout, a `gc`, a push and a fetch, and after each step checks that both repositories pass `git fsck` and agree on every ref. They do, with no conflicts and no halts.

The one wart, and it is the one the index predicts: **after a checkout on one side, `git status` on the other side shows the whole difference as staged.** The working tree and `HEAD` moved there — that is the sync doing its job — but the index still describes the old checkout, so git reads every changed file as "added to the index". Nothing is wrong. A `git reset` (the default, `--mixed`) refreshes the index to `HEAD` and status is clean again. The same happens after a commit made on the other side, for the files it touched.

`git gc` and `git repack` rewrite packs and prune loose objects in one burst: a large transfer, and a moment where one side's `.git` is mid-rewrite. It synchronizes fine — the script runs one — but run it on one side only, and not while the other side is committing.

## What not to do

- **Do not edit the same file on both sides between syncs**, any more than you would in any two-way group. It is a conflict like any other, reported by `autobahn mi`, settled with `autobahn resolve`. A conflict on a *ref* — both sides committed to the same branch in the same cycle — is the same thing one level up: resolve it, then `git reset --hard` the losing side to the branch.
- **Do not sync a worktree's `.git` file** (`git worktree add`) across machines unless the paths are identical on both: a linked worktree's `.git` is a one-line file holding an absolute path into the main repository's `.git/worktrees/`, and the main repository holds absolute paths back. `worktree.useRelativePaths` (git 2.48+) makes both relative, after which they carry across.
- **Do not point two groups at nested checkouts** (a submodule inside a synced repository is fine — it is just files — but a second group rooted inside the first is refused; see [Overlapping and nested roots](./nesting.md)).

## One direction

If the second machine only ever *reads* — a build box, a deploy target — `one-way-alpha` with the same ignores makes it a mirror: every commit and checkout on the alpha appears there, and nothing done there comes back. The index caveat still applies on the mirror; a `git reset` there after a checkout on the alpha is the whole cost.

## See also

- [Scope and support boundaries](./support-boundaries.md) — the general rules this page is a case of
- [Ignores](./ignores.md) — the pattern syntax
- [Conflicts](./conflicts.md) — settling one
