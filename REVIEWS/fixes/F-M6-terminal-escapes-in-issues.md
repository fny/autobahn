# F-M6: `issues` prints filename escape sequences raw, and again inside a command to copy

**Findings:** M-6 (GLM M8; KIMI ABN-M13; OPUS S11; DEEPSEEK F19). **Status:** proposed, Medium. Reproduced on macOS, MAC-BENCH 7j.

## Problem

A file named with an OSC 52 clipboard write reaches the terminal intact. Two files named `x\x1b]52;c;cHduZWQ=\x07` with different contents, one conflict, `autobahn issues` piped through `cat -v` on macOS 26.5.1, commit `d7c2e21`:

```
    1 conflict
      x^[]52;c;cHduZWQ=^G
        alpha  10 B, modified 14s ago
        /Users/faraz/mb7/j8/b 9 B, modified 14s ago
      fix: autobahn resolve esc x^[]52;c;cHduZWQ=^G --keep alpha|…|both
```

`^[` is ESC, `^G` is BEL. In a terminal honouring OSC 52 — iTerm2 with "Applications in terminal may access clipboard" — that silently rewrites the clipboard. It appears **twice**: once in the listing, and once inside a `resolve` command the output invites the reader to copy and run.

The same name is *not* raw everywhere: `sync` printed it as `"x\u{1b}]52;c;cHduZWQ=\u{7}"`, escaped by Rust's `Debug`. So one path sanitises by accident and the other does not, which is M-6's point.

**Re-run on the merged build (`162bfdf`), 2026-09-26: fixed.** `issues` prints the name as visible text — `x\x1b]52;c;cHduZWQ=\x07` — so nothing reaches the terminal that it could act on.

Whether a terminal *would* have acted on it stays untested, and not for want of trying: a raw OSC 52 written straight to the tty changed nothing in Terminal.app, nor in iTerm2 3.7.3 with `AllowClipboardAccess` enabled and the application restarted. The control fails on this machine, so the end-to-end test cannot distinguish a fixed autobahn from a terminal that ignores the attack. The code-level evidence is what stands.

## Proposed resolution

As M-6 says: one sanitising helper for C0, C1, ESC, BEL and DEL, applied at the presentation layer to every tree- or peer-derived string — the listing, the suggested command, the shop, the tray and the log. Rendering the escape visibly (`\x1b`) keeps the name identifiable without handing it to the terminal.

## Tests

- A conflict on a name carrying OSC 52, OSC 8 and a bare CR: `issues`, `status`, `status --json`, the shop and the log all show it inert.
- The suggested `resolve` command round-trips: copying and running it settles that conflict and no other.
- A name with legitimate non-ASCII (accents, CJK, emoji) is unchanged.
