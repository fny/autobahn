# Lane 3ar notes
- integration had 6 docs commits since cf70a91; only e159cd2 touches src (doc comments).
- 54271d3, 46b7d12, 6fe5757, cb45ef3 applied cleanly (auto-merged alongside HYG-3's doc-comment edits).
- b1b056e conflicted in src/main.rs above `run_resolve`: HYG-3 added a `///` doc to run_resolve; 3a added the cfg(test) `RESOLVE_FLUSHES` thread_local there. Kept both: thread_local first, then the doc directly on run_resolve (so the doc stays on its item, HYG-3's point).
- fmt, clippy -D warnings and the full suite are clean after the replay.
