# HYG-3: Fix comments and docs that contradict the code

**Findings:** I-4 and I-5 (OPUS §4).
**Status:** proposed. This comes first among the cleanups, because the comments carry the design argument that reviewers rely on.

## Problem

**Doc comments on the wrong item, fused, or cut off mid-sentence.** There are about 15, apparently where functions were inserted between a doc comment and its item:

| File | Lines |
|---|---|
| `src/endpoint/local.rs` | 177-180, 509-510, 726-736, 2681-2686 |
| `src/endpoint/observer.rs` | 240-246, 301-305 |
| `src/scan/mod.rs` | 813-820 |
| `src/main.rs` | 1004, 1154, 2116, 2614-2633 (the doc for `run_clean` sits on `run_init`) |
| `src/tray.rs` | 748-762 |
| `src/session/mod.rs` | 96, 872 |
| `src/transport/mod.rs` | 86-88, 984-985 |
| `src/progress.rs` | — |

Line numbers are from the review, so recheck them. Examples noticed during the walkthrough: `snapshot_records_file` in `local.rs` sits under a doc about full-scan scheduling, and the `base_signature` doc is fused with a staged-file doc.

**Statements that say the opposite of the code:**
- the `blocking` comment in `src/tree/reconcile.rs`, which H-8's ticket also corrects;
- the `sanitize` comment ("reconciliation never puts unsynchronizable content into an expectation"), which is false for the one-way modes;
- `docs/how-it-works.md` Decision 4, the transition fold saving a rehash, which is P-3;
- the `ssh_argv` comment "autobahn never copies or bootstraps it";
- the `hold_paused` comment "a paused session holds no resources", although the pooled SSH connection is kept;
- `src/main.rs:2797`, which tells users to restart after editing the config, although reload is live;
- `docs/correctness/INVARIANTS.md:101`, which cites `oversized_frames_are_rejected_on_send`, a test removed in `84b5fc7`. The I9 text about outgoing and oversized messages also predates reassembly up to 4 GiB.

## Proposed resolution

- Move each misplaced doc comment to its item, or rewrite it. Delete fragments.
- Correct each contradicting statement to describe what the code does. Where the code is what's wrong, point to that ticket.
- In INVARIANTS I9:
  - drop the removed test's name;
  - say that "refused before allocation" holds per frame;
  - say that a message reassembles up to 4 GiB;
  - note T2-1's per-op and length checks once T2-1 lands.
- Turn on `#![warn(clippy::empty_line_after_doc_comments)]`, and `clippy::doc_lazy_continuation` if it helps, in `lib.rs` and `main.rs`, so CI (CI-01) catches the next misplaced doc comment.
- Open `cargo doc --no-deps` once and skim for docs attached to the wrong function.

## Tests

- Clippy passes with the new lints.
- `grep -rn oversized_frames_are_rejected_on_send docs` returns nothing.
