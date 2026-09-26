# F-M-OUT: Untrusted text is escaped before a terminal, a log or a shell sees it

**Findings:** M-6 (terminal escapes: GLM M8, KIMI ABN-M13, OPUS S11, DEEPSEEK F19, ASTRA), M-7 (log injection: KIMI ABN-M15, GLM L9), M-8 (remote `rm` built from a listing: DEEPSEEK F4, GLM M7, ASTRA, OPUS).
**Status:** proposed. Medium. The helpers here are also used by F-H27 and F-H29.

## Problem

File names, remote error text and relayed agent output are chosen by the other side. POSIX names may contain any byte except NUL and `/`.

- **M-6, terminal.** Tree- and peer-derived strings reach the terminal unescaped:
  - conflict and blocked paths in `status` and `issues` (`src/main.rs`);
  - rows and the ticker in the shop (`src/shop.rs`), where `width` and `shorten` count escape sequences as zero-width and keep them, and `strip` cleans only the selected row;
  - the pager (`src/pager.rs:144`), whose `truncate` deliberately passes escape sequences through, so autobahn's own colours survive;
  - relayed agent stderr (`src/transport/mod.rs:~130`).

  A name carrying OSC 52 can write the clipboard. OSC 8 can disguise a link. CSI and CR sequences can repaint the screen, for example to show a fake "settled" line before `resolve --yes`.
- **M-7, logs.** A newline in a name splits one log entry into several lines of the attacker's choosing, which have no timestamps (`src/logging.rs`, `note!` at `:111` and `complain!` at `:137`). Escape bytes fire in anyone's `tail -f`.
- **M-8, remote shell.** `prune_agents` lists `~/.autobahn/bin` on the remote host, keeps names starting with `autobahn-` that contain no `/`, and runs `rm -f {names}` through the remote shell (`src/transport/install.rs:462-481`). A file named `autobahn-x; curl … | sh` passes the filter and runs during routine pruning. The comment above it promises exact-name removal, but nothing enforces it.

## Proposed resolution

- **One display sanitizer.** Add `display_safe(s: &str) -> Cow<str>`, which replaces C0 and C1 control characters, ESC, BEL and DEL with visible escapes such as `\x1b` and `\n`. Apply it at the presentation boundary to every tree- or peer-derived string: paths, remote errors, relayed stderr, and the status fields the shop and tray read. Autobahn's own colour codes are added *after* sanitizing, so the escape-preserving width logic in the pager and shop only ever sees codes autobahn wrote. `--json` output keeps the raw bytes, correctly encoded as JSON.
- **Logs use the same escaping.** Apply `display_safe` to interpolated values in `note!` and `complain!`, or in the log line writer, so one event is always one line.
- **Prune with an allowlist and quoting.**
  - Accept only names matching `^autobahn-[0-9A-Za-z._+-]+$`.
  - Quote each path with `shell_quote` from F-H29, then pass the script through `sh -c` (OPS-3).
  - Log names that fail the allowlist once, and leave those files alone.
- **Cap relayed line length.** Relay agent stderr in pieces of at most a few KB. This bounds memory as well (M-13 is a tier 2 boundary, but the cap is free here).

## Tests

- A conflict path containing ESC, BEL, CR and an OSC 52 sequence is printed by `status`, `issues`, the shop row renderer and the pager with no raw control bytes. `--json` round-trips it exactly.
- A path with an embedded newline produces exactly one log line.
- `prune_agents` against a fake listing containing `autobahn-x; touch pwned` and `autobahn-0.4.0+e13` removes only the valid name, and the generated script passes `sh -n`.
- The pager and shop still render autobahn's own colours.
