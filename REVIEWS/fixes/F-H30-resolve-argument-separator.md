# F-H30: The tray and shop pass file names after `--`

**Findings:** H-30 (OPUS S10).
**Status:** proposed. High, because a file name can trigger a data-changing command. OPUS rated it Medium.

## Problem

The tray (`src/tray.rs:731`) and the shop (`src/shop.rs:437`) run `autobahn resolve` as a child process, with conflict paths as positional arguments and no `--` separator:

```rust
command.args(["resolve", &group, &path, "--keep", &keep, "--yes"]);
```

The paths are names on disk, so the other side can choose them. A top-level file named `--all` is parsed as the `--all` flag and settles every conflict in the group. Any other flag name works the same way.

## Proposed resolution

- **Flags first, then `--`, then paths.** Build every `resolve` invocation from the tray and the shop as:
  `resolve --keep <keep> --yes <group> -- <path>...`.
  clap treats everything after `--` as positional. The group name also goes before `--`, so check that config parsing refuses group names starting with `-`; add that refusal if it's missing.
- **Everywhere else too.** Search for every other place that passes a tree-derived path as an argument to `autobahn` or another program (`diff`, `xdg-open`, `open`), and add `--` wherever the program supports it. The tray's `open` and `xdg-open` calls take a path we created, but use `--` anyway where the program accepts it.
- **One shared builder.** A single `resolve_command(group, keep, paths)` helper, used by both the tray and the shop, so the next caller can't forget.

## Tests

- Build the command for a path named `--all`, and for one named `-k`. Parse it with the real clap definition, and check that the path comes through as a positional and no flag is set.
- **Shop integration:** settle a conflict on a file named `--all` while a second conflict exists. Only the first is settled.
