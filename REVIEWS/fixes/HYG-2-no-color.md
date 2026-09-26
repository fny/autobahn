# HYG-2: Honour `NO_COLOR`, and colour only on terminals

**Findings:** L-39 (OPUS §4).
**Status:** proposed.

## Problem

ANSI colour codes are written by `src/main.rs`, `src/pager.rs` and `src/shop.rs`. Nothing checks the `NO_COLOR` environment variable, and some output is coloured even when it goes to a file or a pipe. Logs and scripts that read `status` or `issues` then get escape codes in their text.

## Proposed resolution

- **One decision point.** Add a single `colour_enabled()` function, for example in a small `src/style.rs`. Colour is on only when stdout is a terminal, `NO_COLOR` is unset or empty, and `TERM` isn't `dumb`.
- **Route every styled string through it,** so turning colour off leaves plain text with the same wording.
- **Use the standard terminal check.** Replace the five `unsafe { libc::isatty }` calls with `std::io::IsTerminal`, in this ticket or in HYG-4.
- **Keep the TUI styled.** The shop and the pager run in a terminal by definition. They still honour `NO_COLOR` by dropping colour, and keep bold and reverse video, which the `NO_COLOR` convention allows.
- **Override.** Consider `--color=always|never|auto` on commands that print reports.

## Tests

- `status` with stdout piped contains no escape bytes.
- `status` with `NO_COLOR=1` on a terminal contains no colour codes. Test it through a pseudo-terminal, or by forcing the terminal check through a test hook.
