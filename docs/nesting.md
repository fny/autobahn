# Overlapping and nested roots

Several sessions may share a root exactly. That is the fan-out, star, and relay shape, and it is ordinary: those sessions share one watcher and one scan of the root, and each write is validated against the scan it was reconciled from.

*Nesting* is different, and is refused when either endpoint is written:

```
sessions 'dist@/web/dist' and 'project@/backup/project': endpoint
/srv/project/dist is nested inside /srv/project and at least one of
them is written; two sessions cannot safely write one tree region from
independent ancestors. Add it to the outer group's `ignores` if the
outer session should leave that subtree alone
```

## Why

The reason is the ancestor. Two sessions writing one region each keep their own record of what was last agreed, so each reads the other's writes as user edits and propagates them back — indefinitely, with neither able to notice. Sharing a root exactly avoids this because the sessions share one observation of it; nesting gives them genuinely separate views, so it cannot.

"Written" is the test, not the mode. An alpha is written only in the two-way modes; a beta is written in every mode. So two one-way sources reading overlapping trees are legal — nothing writes the shared region — while any nesting involving a destination, or a two-way source, is not.

## Unless the outer session ignores the inner root

Then they do not overlap at all: the outer never scans, records, or writes into that path. This is how you synchronize a project and ship its build output somewhere else:

```toml
[groups.project]
alpha = "~/project"
mode = "two-way-conflict"
ignores = ["dist"]          # the outer session leaves it alone
betas = ["build.example.com"]

[groups.dist]
alpha = "~/project/dist"    # nested, but excluded above
mode = "one-way-alpha"
betas = ["web.example.com:/srv/www"]
```

Both run side by side: the first destination receives the project without `dist`, the second receives `dist`. Remove the `ignores` line and the configuration is refused again.

## What an ignore does not protect against

One thing worth knowing before you arrange it this way: **deleting the directory above an ignored path takes the ignored path with it.** If `~/project` is deleted, `dist` goes too, and the inner session then finds its root missing. An ignore says which files synchronization carries, not which files exist, and a deletion is an instruction about the directory — obeying it halfway would leave a tree that is neither deleted nor synchronized and that nothing can ever clear.

The inner session stops there rather than passing the loss on: a missing source root is an error, so `web.example.com` keeps its copy and waits for a person. That is the protection — not that the inner tree cannot be deleted, but that its deletion never travels.

## Two configurations cannot see each other

The check sees only one configuration load. Two autobahn processes with separate config files can still nest their endpoints, because neither can see the other — see [Support boundaries](./support-boundaries.md).

## See also

- [Ignores](./ignores.md) — ignored means absent, and what that does not cover
- [Safety](./safety.md) — a missing source root halts rather than empties
- [Modes](./modes.md) — which side is written in each
