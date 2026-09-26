## Your job: replay lane 3b's finished work onto the current code

Lane `3b` finished its tickets on branch `lane-3b`, which started from tag `cf70a91`. Since then, other lanes' work has landed on `integration`, and replaying `lane-3b` onto it conflicts. Your worktree is a new branch `lane-3br`, starting from the current `integration`.

1. Read `/home/ubuntu/lanes/3b.done.md` and `/home/ubuntu/lanes/3b.notes.md`, and the tickets they name.
2. Replay the commits of `git rev-list --reverse cf70a91..lane-3b` one at a time, with `git cherry-pick -x <commit>`.
3. **On a conflict, resolve it so both sides' intent survives.**
   - Read the conflicting commit on `integration`, which is another lane's ticket work (`git log integration -- <file>`), and its ticket in `REVIEWS/fixes/`. The other lane's behaviour must be kept. Your commit's behaviour must be added on top of it.
   - When both lanes changed the same function differently, combine them by hand, then re-run both lanes' tests for that code.
   - Never drop another lane's change to make yours apply.
4. **After each commit,** run `cargo fmt`, clippy, and the tests for the touched modules, using `isolated lane-3br cargo test --release <filter>`. If a resolution needs a follow-up fix, amend it into that cherry-pick's commit.
5. **Finish** with the full suite: `isolated lane-3br cargo test --release --no-fail-fast`. Write `/home/ubuntu/lanes/3br.done.md`, listing each replayed commit, how each conflict was resolved, and the final test results.

Everything else in the common rules applies. Your edits may touch any file a conflict involves, but only to resolve that conflict.
