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

The defaults, then the group, each read straight down the list. Last match wins, so a group can re-include something the defaults excluded:

```
defaults.ignores  →  group.ignores
```

Within one list the order written is the order applied, including the files a `file:` entry pulls in.

## Ignore files

A useful ignore list for a language ecosystem runs to dozens of lines, which is more than belongs in a configuration file next to the hosts and the intervals. Keep those in `~/.autobahn/ignores` instead, one file per concern, and name the ones each group wants:

```toml
[defaults]
ignores = ["file:common.gitignore"]

[groups.work]
ignores = [
  "file:Rust.gitignore",
  "target/",
  "!target/keep-me",
  "file:~/dotfiles/node.gitignore",
]
```

An entry beginning `file:` names a file of patterns rather than being one. Its patterns are read in where the entry sits, so a pattern after it can re-include something it excluded — which the two separate keys this replaced could not express.

What follows `file:` is one of two things, decided by whether it looks like a path:

- A bare file name is a file in `~/.autobahn/ignores`, named exactly. Nothing is appended, and the directory is not searched for something close, so `"Rust"` does not find `Rust.gitignore`.
- Anything with a separator, or starting with `~`, is a path taken as written; `~/` expands against the home directory.

A relative path is refused. The supervisor runs under a login service, whose working directory is not the one the line was written in, so "relative to here" has no answer that stays right. The files themselves are gitignore syntax: comments, blank lines and all.

Naming files, rather than loading whatever the directory holds, is deliberate. Order *is* meaning: `!gradle-wrapper.jar` followed by `*.jar` is not the same list as the reverse. A directory scan would order them by whatever the filesystem returned, and dropping in a new file would silently change what the existing ones mean. A written list is an order someone chose and can see.

Bare file names use `$AUTOBAHN_HOME/ignores` when `AUTOBAHN_HOME` is set, otherwise `~/.autobahn/ignores`. A command's `--state-root` override does not move this shared pattern library. Ignore files are read when the configuration is loaded; changing only an ignore file does not trigger the config watcher. Restart the supervisor or also change `config.toml` to reload the patterns.

## Negations inside an ignored directory

A negation reaches inside an ignored directory. `vendor` followed by `!vendor/keep.txt` walks `vendor` after all, with everything in it ignored except `keep.txt` — the same result as `vendor/*` with the negation, so the two spellings agree.

This is one place autobahn is deliberately more forgiving than git, which refuses to re-include beneath an excluded directory. It is safe for every configuration that runs today: before this, such a list was refused at startup, so nothing that synchronizes now changes shape.

Only a negation without wildcards opens an ignored directory, because only it names the directories to walk. `vendor` with `!vendor/*.patch` re-includes nothing: the walk stops at `vendor` before the negation is ever asked. Loading the configuration warns about such a line. Ignore the directory's contents instead, `vendor/*` with `!vendor/*.patch`, or name what to keep without a wildcard.

Once a directory is walked this way, every negation applies inside it wherever it matches: `vendor`, `!vendor/keep.txt` and `!*.md` carry `vendor/README.md` too. A subdirectory that nothing opens stays pruned, so `vendor/sub/notes.md` is not carried. That is what git does with the `vendor/*` spelling.

An ignored directory with nothing re-included beneath it is still never opened, which is what keeps `node_modules` free. The directories a negation reaches are computed once from the patterns, so a list with no negations pays nothing.

## Dead negations

Combining files written independently has one failure the reader cannot see by looking at either file: a negation that can never take effect, because a later pattern ignores it again — `*.jar`, `!gradle-wrapper.jar`, then `*.jar` from another file. Those are refused at startup, named individually. A line that cannot ever do anything is a mistake, not a preference.

## Ignores and deletion

An ignore says which files synchronization carries, not which files exist. Deleting the directory above an ignored path takes the ignored path with it, because a deletion is an instruction about the directory and obeying it halfway would leave a tree that is neither deleted nor synchronized. Ignored content is never *overwritten*, though — "do not synchronize this" cannot become "replace it with the peer's copy". See [Overlapping and nested roots](./nesting.md) for what this means when an ignored path is another session's root.

This deletion rule applies to pattern-ignored content. Files excluded only by size, file type, or symlink mode are left standing with a problem reported. Unreadable content, and previously synchronized content that has since become excluded, can block the parent deletion as a conflict.

## See also

- [Configuration](./configuration.md) — the `ignores` key
- [Overlapping and nested roots](./nesting.md) — ignoring an inner root
- [Safety](./safety.md) — what is never removed
