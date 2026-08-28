The journal is the right minimal fix. A scan cache cannot substitute for the ancestor, and an in-memory generation fence only moves the revert hole to crash/restart. Pointer equality itself is safe as an equality proof; the risks come from drawing conclusions stronger than “these immutable directory contents are identical.”

## 1. Ancestor persistence: journal versus cache or generation fence

### Use a journal/WAL for the minimal change

The durable state to record is the cycle’s resulting `ancestor_changes`, after applying them and validating the result. The cycle already has exactly that seam:

1. Reconciliation produces ancestor changes.
2. Achieved transition results are added to them.
3. `apply()` constructs the new ancestor.
4. The new ancestor is validated.
5. Only then is it persisted and installed in memory.

That sequence is at `src/session/mod.rs:394-442`.

A practical format is:

```text
checkpoint generation G: full ancestor
journal record G+1: length, previous_generation, checksum, ancestor_changes
journal record G+2: ...
```

For each cycle:

- Build and validate `new_ancestor` first.
- Append a complete length-delimited record containing the changes needed to reproduce it.
- Flush/sync that record.
- Only after the append succeeds, set `self.ancestor = new_ancestor` and let the cycle complete.
- Compact asynchronously by writing a new full checkpoint, publishing it atomically, then retiring journal records covered by that checkpoint.

`apply()` is well suited to replay: it deliberately uses only each change’s `new` side and preserves unchanged storage (`src/tree/apply.rs:7-18`, `src/tree/apply.rs:29-70`). Include a monotonically increasing generation and base-generation check so a journal can never be replayed against the wrong checkpoint. A torn final record may be ignored only when framing/checksums prove it is an incomplete tail; corruption in an allegedly committed record should fail closed, just as `load_ancestor` currently fails on an unreadable or invalid ancestor (`src/session/mod.rs:686-701`).

A persistent content-addressed/Merkle tree with a small atomic root pointer is the cleaner long-term alternative: write only new root-to-leaf nodes, then commit a root ID. It avoids journal compaction, but it is substantially more machinery. The append journal is the right first implementation.

### Do not derive the ancestor from the scan cache

The scan cache cannot recover provenance:

- The ancestor is computed from reconciliation decisions plus the **achieved** results of both sides’ transitions (`src/session/mod.rs:394-420`).
- A scan cache represents one root observation. It may include untracked or problematic content and is validated with `validate(false)`, whereas an ancestor must contain only synchronizable content and is validated with `validate(true)` (`src/endpoint/observer.rs:416-420`, `src/session/mod.rs:421-423`).
- Scan-cache loss and write failure are explicitly acceptable optimizations (`src/persist.rs:1-18`). Ancestor corruption is fatal because discarding it could resurrect deletions (`src/session/mod.rs:686-700`).
- The new shared observer has one cache per root, while ancestors legitimately remain per alpha/beta session (`src/endpoint/observer.rs:15-20`, `src/endpoint/observer.rs:124-138`). One cache cannot reconstruct N different session histories.

You could use a scan snapshot as the physical encoding of a checkpoint only if a separate durable session record says, in effect, “this exact tree is ancestor generation G.” At that point it is an ancestor checkpoint, not an ordinary scan cache.

### A generation fence works only if it is durable and fail-closed

A volatile fence could allow the full write to run in the background while subsequent cycles use the new in-memory ancestor. It is safe until the process crashes. After a crash, `load_ancestor()` sees the old full file and has no way to reconstruct the missing reconciliation decision; that is exactly the revert hole.

The observer’s current generation is deliberately in-memory (`src/endpoint/observer.rs:79-101`), so it cannot solve ancestor restart consistency.

A small durable marker could say “checkpoint generation G is stale; generation G+1 was acknowledged.” On restart, `load_ancestor` would then have to refuse synchronization until the missing ancestor is recovered. That avoids silent loss but turns an ordinary crash into a blocked session. Recording the changes alongside the generation makes recovery automatic—which is the journal design.

One important correction: current `save_ancestor` is synchronous in the sense that serialization, write, and rename finish before return, but it is not power-loss durable. It calls `fs::write` and `fs::rename` without syncing the temporary file or containing directory (`src/session/mod.rs:678-683`). A journal using `fsync` would strengthen the contract, so benchmark it separately; it is not an apples-to-apples replacement for the current write.

## 2. Does beta share storage with the ancestor?

Sometimes, but `ScanUnchanged` alone does not establish that sharing.

`ScanUnchanged` does preserve beta storage across beta scans: the controller returns `self.last_snapshot.clone()`, which clones the directory Arcs (`src/endpoint/remote.rs:176-190`). Thus:

```text
previous beta snapshot ──shares──> next unchanged beta snapshot
```

It does not imply:

```text
beta snapshot ──shares──> session ancestor
```

On startup those are independent:

- The ancestor is deserialized from its session file (`src/session/mod.rs:164-181`, `src/session/mod.rs:689-700`).
- The first remote `Response::Scan(snapshot)` is decoded from the wire and stored separately (`src/endpoint/remote.rs:177-181`).
- Deserializing from disk or wire creates fresh Arc allocations; the tree API explicitly warns that equal decoded hierarchies share nothing (`src/tree/mod.rs:374-377`).

The brief’s benchmark assumes the ideal case by passing the same `settled.root` as both ancestor and beta (`examples/cycle_cost.rs:44-69`). Its 2,499/2,501 result measures sharing between generations of one local scan, not normal production sharing between a persisted ancestor and a remote beta.

### Transitions can establish selective beta/ancestor sharing

A remote transition is the exception:

1. The decoded `TransitionOutcome` is folded into `RemoteEndpoint.last_snapshot` (`src/endpoint/remote.rs:243-257`).
2. The session uses that same in-process outcome to construct achieved ancestor changes (`src/session/mod.rs:394-412`).
3. Both folds clone directory-valued result nodes, preserving their Arcs (`src/endpoint/mod.rs:102-143`, `src/tree/apply.rs:49-58`).

Therefore a directory subtree returned as one transition result can become shared between the controller’s beta model and the new ancestor.

For an ordinary single-file transition, however, the result is a file node with no child Arc. Folding it into beta and ancestor independently copy-on-writes each root-to-leaf directory path, so those newly allocated parent vectors do not become cross-shared. Untouched subtrees remain cross-shared only if beta and ancestor already shared them before that edit.

So the accurate answer is:

- Beta-to-previous-beta sharing: strong, including `ScanUnchanged`.
- Beta-to-ancestor sharing after restart or an independently scanned seed: none.
- Beta-to-ancestor sharing after directory-valued transition grafts: potentially substantial.
- Beta-to-ancestor sharing from routine file transitions: little or none unless inherited from earlier grafts.

Consequently, the benchmark does overstate how much pointer pruning is immediately available in production. Instrument actual reconcile inputs and count three relationships separately: ancestor–alpha, ancestor–beta, and alpha–beta.

## 3. Correctness risks of pointer pruning

### Pointer equality has no false-positive problem here

For two corresponding directory nodes, identical `Arc<Vec<Node>>` storage is a stronger fact than content equality:

- The child names and node values are literally the same immutable vector.
- `Arc::make_mut` clones before modification, so applying a change cannot mutate an already shared subtree (`src/tree/apply.rs:29-40`).
- Both Arcs are live during comparison, so allocator address reuse cannot produce an ABA false match.
- Content equality ignores file scan metadata, whereas pointer equality necessarily includes the identical recorded metadata (`src/tree/mod.rs:202-230`).

Deserialization only creates false negatives: content may be equal without sharing. `nodes_share_storage` is explicitly documented as proof of agreement, never proof of difference (`src/tree/mod.rs:366-392`).

The fact that the ancestor is built by `apply()` does not invalidate that proof. `apply()` intentionally preserves unchanged base subtrees and grafted `change.new` subtrees (`src/tree/apply.rs:7-13`). Pointer identity can therefore mean “grafted from the same construction,” not necessarily “both came from scanning,” but reconciliation needs content identity, not common observation provenance.

### The real risks are over-pruning

These shortcuts are unsafe:

- **Ancestor equals alpha, therefore return.** This proves alpha is unchanged relative to the ancestor; beta may still have changes that need propagation or conflict handling.
- **Alpha equals beta, therefore return.** Both sides may have made the same change, in which case the ancestor must be advanced. The current reconciler explicitly emits ancestor updates when the sides agree but the ancestor does not (`src/tree/reconcile.rs:107-127`).
- **Pointer inequality means content differs.** Equal decoded trees frequently have different storage.
- **Ancestor pointer identity proves the filesystem is unchanged.** It proves two in-memory trees agree. It does not replace transition-time validation against the exact scan and current filesystem.
- **Any shared subtree is already ancestor-valid.** A subtree shared with a scan or transition outcome may contain untracked/problematic content. Only sharing with the previous ancestor—already validated using `validate(true)`—carries that validation provenance.

The simplest unquestionably safe reconcile pruning is:

```rust
if nodes_share_storage(ancestor, alpha)
    && nodes_share_storage(ancestor, beta)
{
    return;
}
```

That proves all three reconciliation inputs are identical at the entire subtree. More ambitious two-of-three shortcuts require preserving the existing mode-specific semantics, not merely returning.

### Incremental ancestor validation is safe with a narrow trust rule

For the 13.8 ms structural validation, compare `new_ancestor` only against the previous, already validated ancestor:

- If corresponding directory nodes share storage, skip that subtree.
- At every changed directory vector, still check all immediate child names for emptiness, separators, sorting, and uniqueness—the invariants currently checked at `src/tree/mod.rs:316-335`.
- Recursively validate every inserted/replaced subtree that is not shared with the previous ancestor.
- Continue rejecting untracked/problematic content and empty symlink targets (`src/tree/mod.rs:338-360`).

This is safe inductively because every in-memory ancestor is fully validated before installation (`src/session/mod.rs:419-442`), and every loaded ancestor is validated before use (`src/session/mod.rs:695-700`).

Do not generalize that to “shared with alpha or beta means validated.” `apply()` can graft `change.new` directly, and the full validation is explicitly the safety net against a reconciliation defect reaching persistent state (`src/session/mod.rs:415-424`). New/grafted storage must be checked unless its validation provenance is tracked explicitly.

In short: pointer equality is a safe, constant-time content proof. The danger is treating it as a proof of three-way reconciliation completion, filesystem recency, or ancestor validation provenance when it proves only the first of those.
