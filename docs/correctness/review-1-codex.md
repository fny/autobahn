# Correctness defects, ranked

1. **The emptied-root safety halt fails open for a root with one immediate child, regardless of the size of that subtree.**

   **Location:** `src/session/mod.rs:334-344`, `src/session/mod.rs:584-605` (and the behavior is explicitly pinned by `src/session/mod.rs:743-748`).

   **Failure sequence:** The agreed ancestor and beta each contain one top-level directory, `data/`, holding the entire dataset. Alpha is a mount point; the mount disappears but the mount-point directory remains, so alpha scans as an existing empty directory. `one_side_emptied_root` returns false solely because `ancestor.children().len() < 2` (`src/session/mod.rs:592-597`). Reconciliation descends through the still-present root, treats alpha's missing `data/` as a deletion while beta is unchanged, and emits a beta transition deleting `data/` (`src/tree/reconcile.rs:207-226`). The root-deletion check only rejects a transition at the empty path (`src/session/mod.rs:355-363`), so the child deletion proceeds and removes the whole good copy. Counting immediate children is not a defensible proxy for “non-trivial”: one child can contain millions of files.

   **Category:** Data loss; safety check bypass/fail-open.

2. **A successful filesystem transition is committed before the new ancestor is persisted, leaving a crash/error window that reopens the deliberate-revert hole.**

   **Location:** `src/session/mod.rs:365-393` commits endpoint transitions; only afterward do `src/session/mod.rs:421-452` build and record the ancestor. The code itself states the stale-ancestor consequence at `src/session/mod.rs:435-449`.

   **Failure sequence:** Ancestor and beta contain `O`; alpha changes to `A`. The cycle publishes `A` to beta successfully. The process then crashes, loses the remote response, or gets an ancestor-write error before `AncestorStore::record` completes, so disk still says ancestor `O`. While autobahn is down/backed off, the user deliberately reverts alpha from `A` to `O`. On restart the scans are alpha `O`, beta `A`, ancestor `O`. In bidirectional reconciliation alpha is classified as unchanged and beta as modified (`src/tree/reconcile.rs:228-241`), so autobahn overwrites the deliberate alpha revert with `A`. The journal shortens this window; it does not close it.

   **Category:** Data loss/resurrection via stale provenance.

3. **Digest reuse treats forgeable/reusable stat fields as proof of content identity, so changed bytes can remain invisible even to a full scan and can pass transition validation.**

   **Location:** `src/scan/mod.rs:514-562`, especially `src/scan/mod.rs:650-667`; the destructive-side check repeats the same assumption at `src/endpoint/local.rs:1251-1283`.

   **Failure sequence:** Ancestor and both sides contain bytes `A`. Alpha's file is replaced with same-length bytes `B` while retaining the same mtime and mode, and either retaining or reusing the inode (an editor/tool can restore timestamps; inode reuse after delete/create is also possible). Every scan, including the periodic “full” scan, calls `reusable_digest` and keeps digest `A` without reading the file. If beta then legitimately changes `A` to `C`, reconciliation sees alpha as unchanged and emits an alpha transition carrying `C`. `validate_file` checks that the retained scan says digest `A` and that the current stat tuple matches; both checks pass, so publication overwrites the unobserved `B`. With no beta edit, the result is instead indefinite silent divergence: the model says `A` while alpha contains `B`, and future full walks continue reusing `A`.

   **Category:** Data loss; silent divergence; safety check bypass.

4. **File creation/replacement/removal checks are TOCTOU checks followed by overwrite/delete operations, so a concurrent save can be destroyed after it passed outside the check.**

   **Location:** Creation checks absence at `src/endpoint/local.rs:1323-1331` and publishes through `src/endpoint/local.rs:1361-1372`; file replacement validates at `src/endpoint/local.rs:1776-1814`; removal validates and then deletes at `src/endpoint/local.rs:1589-1607`; publication uses overwrite-capable `rename` at `src/endpoint/local.rs:1501-1503` and `src/endpoint/local.rs:1537-1541`.

   **Failure sequence:** A creation transition checks that `p` is absent. Before `publish_file` renames staged content onto `p`, an editor atomically saves new user content at `p`. Unix `rename` replaces that new file, even though the transition carried no old-content expectation. The replacement path has the same defect: it stats and validates old `A`, an editor installs `C`, then the later rename installs synchronized `B` over `C`. The removal path can likewise validate `A`, then unlink an atomically replaced `C`. Watcher invalidation does not serialize external writers and cannot make these check/use pairs atomic.

   **Category:** Data loss; safety check bypass.

5. **Path-component safety is also TOCTOU: verified directories are retained only as strings, allowing a concurrent symlink swap to redirect a transition outside the synchronization root.**

   **Location:** `src/endpoint/local.rs:1213-1236` verifies components with `symlink_metadata`; `src/endpoint/local.rs:1984-1992` shows that this is a one-time stat rather than a held directory handle. Later operations use the reconstructed pathname, for example `src/endpoint/local.rs:1589-1607` and `src/endpoint/local.rs:1501-1541`.

   **Failure sequence:** A scan records `root/d/file`. During a replacement, `resolve_parent` verifies that `root/d` is a real directory. A concurrent process then renames that exact directory to `/valuable/d` and places `root/d -> /valuable/d`. The subsequent stat still finds the same scanned file through the intermediate symlink, so metadata validation passes; the later rename or unlink follows the symlink and overwrites/deletes `/valuable/d/file`, outside the configured root. Avoiding this requires descriptor-relative traversal/operations; repeated pathname stats cannot close the race.

   **Category:** Data loss outside the synchronization root; safety check bypass.

6. **Existing staged content is trusted solely by its digest-shaped filename and is never rehashed before publication, so crash-damaged staging can be published and then modeled as correct.**

   **Location:** Received content is only flushed, not `sync_all`ed, before rename at `src/endpoint/local.rs:709-735` (the local-copy path likewise only flushes at `src/endpoint/local.rs:1937-1962`); `stage_begin` inventories bare names and skips transfer on a match at `src/endpoint/local.rs:768-799`; `publish_file` copies/renames without digest verification at `src/endpoint/local.rs:1471-1555`.

   **Failure sequence:** A transfer writes and hashes the right bytes, flushes userspace buffers, and renames the file to `<expected-digest>`. A power loss leaves the directory entry but loses/truncates the unsynced data (equivalently, an old staged file is corrupted after its original verification). On restart, `stage_begin` sees the filename and declares the request satisfied. `publish_file` installs the truncated/wrong bytes and returns metadata while constructing a result node carrying the *requested* digest (`src/endpoint/local.rs:1364-1371`). The folded baseline now pairs that false digest with the corrupt file's real metadata, so the metadata-reuse defect above can keep accepting it without hashing.

   **Category:** Corruption; silent divergence.

7. **A returned journal append is not durable: `write_all` is treated as “on disk” without `sync_data`/`sync_all`.**

   **Location:** The durability contract is claimed at `src/session/ancestor.rs:138-142`, but the append path only opens and calls `write_all` at `src/session/ancestor.rs:162-169`, then advances the generation and returns at `src/session/ancestor.rs:171-177`.

   **Failure sequence:** A cycle applies a transition, appends its ancestor record, returns success, and may report/settle the session. A power loss occurs while that append exists only in cache; the synchronized file data happens to survive but the journal tail does not. Restart loads the older ancestor. A later deliberate revert to that older content is then classified as “unchanged,” while the peer is “modified,” and is overwritten as in defect 2. `flush` on a Rust `File` would not be sufficient either; this requires an OS durability primitive.

   **Category:** Persisted state read back stale; data loss/resurrection.

8. **Checkpoint compaction has no durable ordering, so a power loss can preserve the journal truncation while losing the new checkpoint.**

   **Location:** `src/session/ancestor.rs:194-209` writes a temporary with `fs::write`, renames it, and immediately truncates the journal with `File::create`; it syncs neither file nor the containing directory.

   **Failure sequence:** Checkpoint `G` plus journal records through `H` are valid. Compaction writes/renames checkpoint `H`, then truncates the journal. The storage system persists the truncation but, because neither the new checkpoint contents nor the rename/directory entry was synced first, loses the new checkpoint publication on power failure. Restart therefore sees old checkpoint `G` and an empty journal, silently rolling provenance back to `G`. A later revert can then be overwritten. “Rename before truncate” is only program order; without fsync ordering it is not crash order.

   **Category:** Persisted state read back stale; data loss/resurrection.

9. **A journal left behind after checkpoint publication permanently masks later valid records, because replay stops at the stale prefix but append does not retire it.**

   **Location:** Replay breaks at the first generation mismatch at `src/session/ancestor.rs:82-98` but retains the journal and its byte count at `src/session/ancestor.rs:105-112`; new records append to that same file at `src/session/ancestor.rs:162-172`. The crash window is `src/session/ancestor.rs:201-209`.

   **Failure sequence:** The store has a below-threshold journal beginning at generation `G`. A large change takes the direct-checkpoint path (`src/session/ancestor.rs:151-159`) and publishes checkpoint `H`, then the process crashes before clearing the old journal. Restart correctly declines to replay its first base-`G` record because the checkpoint is already `H`, but does not truncate that spent prefix. A subsequent small, acknowledged base-`H` record is appended behind it and does not itself trigger compaction. On the next restart replay again stops at the first base-`G` record and never reaches the valid base-`H` record, so the ancestor silently loses an acknowledged cycle.

   **Category:** Persisted state read back stale; data loss/resurrection.

10. **A tolerated torn journal tail is not truncated, so a later valid append can sit behind the tear and be silently ignored.**

    **Location:** `src/session/ancestor.rs:270-317` returns the last usable offset after breaking on an incomplete record, but `AncestorStore::open` at `src/session/ancestor.rs:82-115` never truncates the physical file; later writes append at `src/session/ancestor.rs:162-169`.

    **Failure sequence:** A crash leaves a complete header for a large record but only a short prefix of its declared payload. Restart accepts the intact prefix and ignores the torn tail. The recovery cycle then appends and acknowledges a smaller current-generation record after those physical torn bytes. As long as the torn header's declared end is still beyond EOF, the next loader breaks at it and never parses the valid record behind it. The in-memory generation during the successful run advanced, but the next process silently reconstructs the older ancestor.

    **Category:** Persisted state read back stale; data loss/resurrection.

11. **Reset is a non-atomic checkpoint-first two-file deletion, so a crash can leave a generation-zero journal that resurrects the reset ancestor.**

    **Location:** `src/session/ancestor.rs:117-135` removes the checkpoint and journal in separate calls, in that order.

    **Failure sequence:** A small session's first ancestor is stored entirely as a base-generation-zero journal record (records below the 1 MiB minimum threshold are journalled, `src/session/ancestor.rs:52-60` and `src/session/ancestor.rs:151-169`), with no checkpoint. Reset observes/removes the absent checkpoint first and the process dies before removing the journal. On restart `read_checkpoint` reports `(generation 0, ancestor None)` (`src/session/ancestor.rs:244-249`), the surviving base-zero journal is accepted, and replay reconstructs the supposedly reset ancestor. The same lineage ambiguity applies to an orphan base-zero journal from a previous incarnation.

    **Category:** Resurrection; persisted state read back after reset.

12. **The checkpoint has no integrity checksum, so valid-looking bit corruption is accepted as an ancestor that was never written.**

    **Location:** `src/session/ancestor.rs:194-204` writes only magic, generation, and bincode payload; `src/session/ancestor.rs:244-267` deserializes it without an integrity check, and `src/session/ancestor.rs:100-102` validates only tree structure/synchronizability.

    **Failure sequence:** A sector/bit error changes a file digest or the generation in a checkpoint while leaving the bincode layout structurally decodable. `read_checkpoint` accepts it and `Node::validate(true)` cannot detect that a digest or generation differs from what was written. A changed generation can cause replay to skip valid journal records; a changed content digest can make three-way reconciliation classify the wrong side as unchanged and overwrite it. Journal payloads at least carry a truncated BLAKE3 check (`src/session/ancestor.rs:235-241`, `src/session/ancestor.rs:304-307`); checkpoints do not.

    **Category:** Persisted-state corruption; possible data loss.

## Considered and ruled out

- **`Node::validate_against` pointer pruning is sound under its actual call chain.** It skips only when two directory nodes contain the identical immutable `Arc<Vec<Node>>` (`src/tree/mod.rs:382-400`, `src/tree/mod.rs:440-465`). The prior ancestor is fully validated on load (`src/session/ancestor.rs:100-102`) and every replacement vector that is not pointer-identical has its names checked and its children matched by name before recursive validation (`src/tree/mod.rs:402-435`). `apply` uses `Arc::make_mut` on every modified ancestor path (`src/tree/apply.rs:18-70`), so an unvalidated changed vector cannot masquerade as prior validated storage. File/symlink/problematic nodes have no pointer shortcut and receive ordinary validation.

- **An `apply` error during journal replay does not leak a partly applied ancestor.** `apply` mutates a local clone and returns `Err` without returning that clone (`src/tree/apply.rs:18-72`); `AncestorStore::open` propagates the error and never installs the local value (`src/session/ancestor.rs:94-96`). That is a load failure, not silent partial state.

- **The shared observer's normal mid-scan invalidation is conservative, not a permanent missed change.** A scan tags itself with the generation read before walking (`src/endpoint/observer.rs:320-347`), published snapshots are reused only if that generation still equals current (`src/endpoint/observer.rs:229-235`), and transitions invalidate before their first write (`src/endpoint/local.rs:925-932`). Each endpoint also retains its own exact scan lease for transition validation (`src/endpoint/local.rs:740-751`, `src/endpoint/local.rs:936-942`). A scan that starts after invalidation but races the write can be stale until watcher activity or the forced 120-second full scan (`src/endpoint/observer.rs:56-58`, `src/endpoint/observer.rs:305-309`), but I did not find a standalone permanent-loss sequence without also invoking defects 3, 4, or 5. Observer teardown is protected by endpoint-held `Arc`s and a registry of `Weak`s (`src/endpoint/observer.rs:433-483`).

- **The split transport header/body writes do not interleave in the shipped paths.** The controller holds one writer mutex across the complete `send_frame` call (`src/transport/mux.rs:371-379`), and the agent similarly shares a locked output (`src/transport/mod.rs:267-284`, `src/transport/mod.rs:372-383`). Scratch is moved out of thread-local storage for the entire send, making same-thread reentrancy use independent empty scratch (`src/transport/mod.rs:629-640`). A truncated body is rejected by `read_exact` (`src/transport/mod.rs:718-740`); it is not decoded as a complete frame. With the blocking pipe/SSH streams used here, a partial write followed by a terminal error kills the exchange rather than allowing another frame to complete the first.

- **The on-disk formats are not explicitly schema-versioned, but I did not rank that alone as a demonstrated current data-corruption path.** A schema-incompatible bincode change will generally fail decoding, which is a stall and therefore outside the brief. It should still acquire an explicit format version before a future schema migration; otherwise a structurally compatible change could become a wrong-but-decodable ancestor.
