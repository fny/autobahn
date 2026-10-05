# Synchronizing Git Checkouts

With appropriate configurations, Autobahn can synchronize active Git repositories across hosts, including branch refs, commit objects, and tags, without requiring intermediate `git push` or `git pull` operations.

## Configuration

To carry the history, do not exclude `.git` wholesale. The patterns that must come out are the same for every repository, so they live in one file rather than in each group:

```toml
# ~/.autobahn/config.toml

[groups.work]
mode = "two-way-conflict"
primary = "~/Workspace"
replicas = ["dev@build.audi.de:/home/dev/workspace"]
ignores = [
  "file:Essential.gitignore",   # written by `autobahn init`
  "target",
  "node_modules",
]
```

`Essential.gitignore` excludes `.git` whole by default, which is right for most people — a checkout arrives as a working tree, and the history comes from the remote. Uncomment its Git block to do what this page describes instead: carry objects, refs, `HEAD` and config, and leave out only the bookkeeping that cannot cross — the index, the locks, the reflog, the scratch files, the worktree links, and any half-finished merge or rebase.

### Excluded vs. Synchronized Git State

- **Synchronized:** Objects, packfiles, branch refs (`refs/heads/*`), tags (`refs/tags/*`), `HEAD`, configuration, and hooks.
- **Excluded (`.git/index`):** The Git index stores device-specific file metadata (inodes, devices, modification timestamps). Synchronizing the index between different machines causes false modification detections and infinite index rewrite loops. Excluding the index allows each host to track its own working tree stat cache.

## Operational Considerations

### Working Tree and Index Alignment

When a branch checkout occurs on the primary (`git checkout feature`), the changed working tree files and `HEAD` reference synchronize to the replica within milliseconds. However, because `.git/index` is excluded, Git on the replica still has the prior commit's cached stat entries.

- Running `git status` on the replica may temporarily display modified files.
- Run `git reset` (mixed reset) on the replica to refresh the local index against `HEAD`.

### Garbage Collection (`git gc`)

`git gc` repacks loose objects into unified packfiles. While Autobahn safely propagates packfiles, run `git gc` on one host at a time, avoiding concurrent active commits on opposing endpoints.

### Concurrent Branch Commits

If both the primary and the replica commit to the same branch simultaneously during a single synchronization interval, a conflict is flagged on the corresponding ref file under `.git/refs/heads/<branch>`. Resolve the ref via `autobahn resolve`, then align the losing endpoint with `git reset --hard`.

### Git Linked Worktrees

Linked worktrees created via `git worktree add` embed absolute filesystem paths into `.git/worktrees/`. To synchronize linked worktrees across machines with differing directory paths, enable relative worktree paths (requires Git 2.48+):

```sh
git config worktree.useRelativePaths true
```

## See Also

- [Ignores](./ignores.md): Pattern syntax, ignore files, and precedence
- [Configuration](./configuration.md): Group definitions and ignore settings
- [Modes](./modes.md): Direction and conflict policies for synchronized checkouts
- [Conflicts](./conflicts.md): How to inspect and resolve competing changes
