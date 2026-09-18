# Ignores

Ignore patterns say which files synchronization carries. Ignored content is treated as absent: nothing already synced is deleted, but new matching files silently never arrive — no conflict, no alert.

## Pattern semantics

Gitignore-style, over root-relative paths:

- a bare name (`target`) matches at any depth
- a leading `/` (`/dist`) anchors to the synchronization root
- a trailing `/` (`build/`) matches directories only
- `**` crosses directory boundaries; `*` and `?` stay within one component
- `!` re-includes something an earlier pattern excluded
- **the last matching pattern decides**

That last rule is what makes negations useful: `*.log` followed by `!keep.log` ignores every log file except one, while the reverse order ignores all of them.

Patterns are compiled once, at startup, and a bad one is a configuration error rather than a runtime failure discovered by the affected session.

## Where patterns come from

Widest first, and last match wins, so a group can re-include something a shared file excluded:

```
defaults.ignore_files  →  defaults.ignores  →  group.ignore_files  →  group.ignores
```

## Ignore files

A useful ignore list for a language ecosystem runs to dozens of lines, which is more than belongs in a configuration file next to the hosts and the intervals. Keep those in `~/.autobahn/ignores` instead, one file per concern, and name the ones each group wants:

```toml
[defaults]
ignore_files = ["common.gitignore"]

[groups.work]
ignore_files = ["Rust.gitignore", "~/dotfiles/node.gitignore"]
```

An entry is one of two things, decided by whether it looks like a path:

- A bare file name is a file in `~/.autobahn/ignores`, named exactly. Nothing is appended, and the directory is not searched for something close, so `"Rust"` does not find `Rust.gitignore`.
- Anything with a separator, or starting with `~`, is a path taken as written; `~/` expands against the home directory.

A relative path is refused. The supervisor runs under a login service, whose working directory is not the one the line was written in, so "relative to here" has no answer that stays right. The files themselves are gitignore syntax: comments, blank lines and all.

Naming files, rather than loading whatever the directory holds, is deliberate. Order *is* meaning: `!gradle-wrapper.jar` followed by `*.jar` is not the same list as the reverse. A directory scan would order them by whatever the filesystem returned, and dropping in a new file would silently change what the existing ones mean. A written list is an order someone chose and can see.

## Negations inside an ignored directory

A negation reaches inside an ignored directory. `vendor` followed by `!vendor/keep.txt` walks `vendor` after all, with everything in it ignored except `keep.txt` — the same result as `vendor/*` with the negation, so the two spellings agree.

This is one place autobahn is deliberately more forgiving than git, which refuses to re-include beneath an excluded directory. It is safe for every configuration that runs today: before this, such a list was refused at startup, so nothing that synchronizes now changes shape.

An ignored directory with nothing re-included beneath it is still never opened, which is what keeps `node_modules` free. The directories a negation reaches are computed once from the patterns, so a list with no negations pays nothing.

## Dead negations

Combining files written independently has one failure the reader cannot see by looking at either file: a negation that can never take effect, because a later pattern ignores it again — `*.jar`, `!gradle-wrapper.jar`, then `*.jar` from another file. Those are refused at startup, named individually. A line that cannot ever do anything is a mistake, not a preference.

## Ignores and deletion

An ignore says which files synchronization carries, not which files exist. Deleting the directory above an ignored path takes the ignored path with it, because a deletion is an instruction about the directory and obeying it halfway would leave a tree that is neither deleted nor synchronized. Ignored content is never *overwritten*, though — "do not synchronize this" cannot become "replace it with the peer's copy". See [Overlapping and nested roots](./nesting.md) for what this means when an ignored path is another session's root.

## See also

- [Configuration](./configuration.md) — the `ignores` and `ignore_files` keys
- [Overlapping and nested roots](./nesting.md) — ignoring an inner root
- [Safety](./safety.md) — what is never removed
