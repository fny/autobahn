# Ignores

Ignore patterns specify filesystem entries excluded from synchronization. Ignored files and directories are treated as absent: they do not trigger synchronization cycles, generate conflict warnings, or propagate to peers. Adding an ignore does not delete copies that already synchronized. New matching files do not synchronize and produce no conflict or alert.

## Pattern Syntax

Gitignore-style, over root-relative paths:

- a bare name (`target`) matches at any depth
- a slash anywhere but the end anchors to the synchronization root — `/dist`, and `.git/index` too
- a trailing `/` (`build/`) matches directories only, and does **not** anchor: `build/` still searches nested
- `**/` restores any depth to a path that would otherwise be anchored (`**/.git/index`)
- `**` crosses directory boundaries; `*` and `?` stay within one component
- `!` re-includes something an earlier pattern excluded
- **the last matching pattern decides**

That last rule is what makes negations useful: `*.log` followed by `!keep.log` ignores every log file except one, while the reverse order ignores all of them.

## Precedence Rules

- **Evaluation Order:** Patterns defined in `[defaults]` are evaluated first, followed sequentially by group-specific `ignores`.
- **Last Match Wins:** Later rules override earlier rules. For example, `*.log` followed by `!important.log` ignores all logs except `important.log`.
- **Dead Negation Validation:** If a negation (`!file`) is completely superseded by a subsequent catch-all rule, the configuration compiler rejects the rule set at load time as an unexecutable configuration error.

## Where patterns come from

The defaults, then the group, each read straight down the list. Last match wins, so a group can re-include something the defaults excluded:

```
defaults.ignores  →  group.ignores
```

Within one list the order written is the order applied, including the files a `file:` entry pulls in.

## Ignore files

Store reusable pattern lists in `~/.autobahn/ignores` and reference them from the configuration:

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

Resolution rules:

- **Bare filenames (`file:Rust.gitignore`):** Loaded from `~/.autobahn/ignores/` (or `$AUTOBAHN_HOME/ignores/` if set). No extensions are inferred.
- **Explicit paths (`file:~/path` or `file:/etc/path`):** Loaded from the specified location.
- **Relative paths:** Explicitly rejected to ensure deterministic behavior across daemon process contexts.

## Directory Pruning and Negations

1. **Performance Optimization:** By default, ignored directories (e.g., `node_modules/` or `.git/`) are pruned immediately during filesystem scanning. The scanner does not descend into them, conserving CPU and memory.
2. **Re-inclusion Inside Ignored Directories:** Unlike standard Git, Autobahn allows exact negations to open pruned parent directories. For example:
   ```toml
   ignores = ["vendor", "!vendor/critical.rs"]
   ```
   Autobahn inspects `vendor` solely to locate `critical.rs`, leaving all other sibling contents pruned.
3. **Wildcard Negation Restrictions:** Re-inclusions must name the exact target path without wildcards (e.g., `!vendor/*.patch` cannot un-prune `vendor` because directory walking stops at the ignored parent). To apply wildcard negations within a directory, ignore the directory's contents rather than the directory itself: `ignores = ["vendor/*", "!vendor/*.patch"]`.

## Interaction Between Ignores and Deletions

- **Ignored Content Creation:** Ignored files created locally are never copied to remote destinations.
- **Parent Directory Deletion:** Deleting an entire directory tree locally propagates the directory deletion to remote destinations, removing ignored children within that directory tree.
- **No Overwrite Guarantee:** Ignored files are never overwritten or replaced by synchronization passes.
