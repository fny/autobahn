# MAC-4: Several commits on `main` do not compile, which blocks bisection and A/B work

**Findings:** new, from MAC-BENCH 3. **Status:** proposed, Info / process.

## Problem

`bench/ab.sh` needs an older binary to measure against. Building one from the history failed:

```
error[E0004]: non-exhaustive patterns: `Command::Update { .. }` not covered
   --> src/main.rs:547:24
```

at `c4e4109` (*perf(scan): the walk spreads over threads*) and at `f68b0a8` (*perf(apply): a transition spreads over threads*). The `Update` variant was added to the `Command` enum in one commit and the arm that handles it in another, so every commit in between builds neither the binary nor its tests.

The consequence is not cosmetic. `git bisect` cannot cross that range, and neither can the A/B gate the project relies on to accept or reject hot-path changes — which is how this was found: section 3 of the Mac bench could not run, because no valid baseline binary could be produced from the range just before the watch work.

Related but not the same: H-17 records that CI on `main` was red. This is about commits that never built at all, which CI would only catch if it ran on each of them.

## Proposed resolution

- Keep `main` building commit by commit: add the match arm in the same commit as the enum variant.
- If CI does not already build every pushed commit (rather than only the tip), make it do so, or say in `docs/development.md` that only the tip is guaranteed and bisection may need `--first-parent` or skips.

## Tests

- A CI job that builds each commit in a pushed range, or at least `cargo check` per commit.
- `git bisect run cargo check` over the last hundred commits completes without a skip.
