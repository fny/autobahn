//! Durable storage for a session's ancestor.
//!
//! The ancestor records what both sides last agreed on, which is what
//! distinguishes "this side changed" from "the other side did". It must
//! reach disk before the cycle that produced it completes: content
//! deliberately reverted to an earlier state is indistinguishable from
//! content that never changed, so reconciling against a stale ancestor reads
//! a revert as "unchanged" while the peer reads "modified", and the peer's
//! content silently overwrites the revert — in every mode.
//!
//! Writing the whole hierarchy every cycle honours that at a cost
//! proportional to the tree rather than to the edit. On half a million
//! entries it meant encoding and writing about forty megabytes to record one
//! changed file, which measured at 165ms per edit and, because the write
//! happens after the transition, delayed the *next* cycle rather than the
//! current one: a second save 100ms after the first took 276ms longer than an
//! isolated one.
//!
//! So the hierarchy is written occasionally and the changes between are
//! appended. A cycle costs one small record; the full write happens on
//! compaction, amortised against the volume of change that provoked it.
//! The durability class is process-crash safety, matching the full rewrite
//! this replaced (which also never synced): nothing is installed in memory
//! until its record has been *written*, so a process crash cannot lose an
//! acknowledged cycle — but a power loss can, until the operating system
//! flushes its caches. Compaction alone is fsync-ordered, because a power
//! loss that kept the journal truncation while dropping the checkpoint
//! would roll back every generation the journal held. The window in which
//! the ancestor lags the transition shrinks from a full encode-and-write
//! to an append.
//!
//! # Format
//!
//! The checkpoint is the hierarchy, prefixed by a magic number and the
//! generation it represents. A checkpoint written by a build that predates
//! the journal has no magic and is read as generation zero.
//!
//! The journal is a sequence of records, each carrying the generation it
//! applies *to*, its length, a digest of its payload, and the payload: the
//! changes that advance the ancestor by one cycle. Replay stops at the first
//! record that does not follow the generation it holds, which is how a
//! journal left behind by a crash between publishing a checkpoint and
//! clearing the journal is recognised as spent rather than replayed twice.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::tree::{apply, Change, Node};

/// Marks a checkpoint as carrying a generation. Its absence means the file
/// was written before journalling existed and is generation zero.
const CHECKPOINT_MAGIC: [u8; 8] = *b"ABAHNANC";

/// The smallest journal worth compacting. Below this the full write costs
/// more than the reading it would save.
const MINIMUM_COMPACTION_SIZE: u64 = 1 << 20;

/// Compaction runs once the journal reaches this fraction of the
/// checkpoint's size, which bounds both the work replay can face and the
/// amortised cost of the full writes: a checkpoint is rewritten only after
/// roughly a quarter of its own size has accumulated in changes.
const COMPACTION_RATIO: u64 = 4;

/// A session's ancestor on disk: a checkpoint plus the changes since.
pub(crate) struct AncestorStore {
    checkpoint_path: PathBuf,
    journal_path: PathBuf,
    /// The generation the in-memory ancestor now stands at.
    generation: u64,
    /// The size of the checkpoint as last written or read, which sets the
    /// threshold at which appending stops being cheaper than rewriting.
    checkpoint_bytes: u64,
    journal_bytes: u64,
}

impl AncestorStore {
    /// Opens the store at `path`, returning it with the ancestor it holds:
    /// the checkpoint with every journalled change replayed onto it.
    ///
    /// A missing checkpoint is an absent ancestor, not an error — that is a
    /// session that has never completed a cycle. Corruption *is* an error:
    /// an ancestor that cannot be read must never be silently discarded,
    /// because starting from nothing would resurrect deletions.
    pub(crate) fn open(path: &Path) -> Result<(AncestorStore, Option<Node>)> {
        let journal_path = journal_path(path);
        let (mut generation, mut ancestor, checkpoint_bytes) = read_checkpoint(path)?;

        let (records, physical_bytes) = read_journal(&journal_path)?;
        // Replay applies every record that continues the lineage in hand and
        // skips the rest: spent records from a checkpoint that already
        // absorbed them (a crash can land between publishing the checkpoint
        // and clearing the journal), or records from another incarnation.
        // Skipping — rather than stopping at the first mismatch — is what
        // lets an acknowledged record behind a spent prefix survive.
        let mut applied = Vec::new();
        for record in records {
            if record.base_generation != generation {
                continue;
            }
            ancestor = apply(ancestor.as_ref(), &record.changes).map_err(|message| {
                anyhow::anyhow!("unable to replay ancestor journal: {message}")
            })?;
            generation += 1;
            applied.push(record.raw);
        }
        // The journal is normalized to exactly the applied records, so dead
        // bytes — spent prefixes, torn tails, partial headers — can never
        // sit in front of a future append and swallow it on the next load.
        // This also heals journals damaged before normalization existed.
        let applied_bytes: usize = applied.iter().map(Vec::len).sum();
        let journal_bytes = applied_bytes as u64;
        if applied_bytes as u64 != physical_bytes {
            let mut normalized = Vec::with_capacity(applied_bytes);
            for raw in &applied {
                normalized.extend_from_slice(raw);
            }
            fs::write(&journal_path, &normalized)
                .context("unable to normalize the ancestor journal")?;
        }

        if let Some(root) = &ancestor {
            root.validate(true)
                .map_err(|message| anyhow::anyhow!("persisted ancestor is invalid: {message}"))?;
        }

        Ok((
            AncestorStore {
                checkpoint_path: path.to_path_buf(),
                journal_path,
                generation,
                checkpoint_bytes,
                journal_bytes,
            },
            ancestor,
        ))
    }

    /// Discards the stored ancestor entirely, so the next cycle reconciles
    /// with no baseline.
    ///
    /// Every file the store owns goes together, and the *journal goes
    /// first*: a crash between the two removals then leaves a checkpoint
    /// alone — a state that was genuinely acknowledged once, read as "the
    /// reset has not happened yet" and simply retried. The other order
    /// leaves a journal whose records replay against the wrong base: a
    /// base-zero delta applied to nothing reconstructs a hierarchy that
    /// never existed on either side.
    pub(crate) fn reset(path: &Path) -> Result<()> {
        for path in [journal_path(path), path.to_path_buf()] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("unable to remove {}", path.display()))
                }
            }
        }
        Ok(())
    }

    /// Records the changes that advance the ancestor to `ancestor`, and
    /// does not return until they are written — surviving a process crash,
    /// though not a power loss (see the module documentation for the
    /// durability class).
    ///
    /// The caller must not install `ancestor` in memory before this returns:
    /// the ordering is the whole guarantee.
    pub(crate) fn record(&mut self, changes: &[Change], ancestor: Option<&Node>) -> Result<()> {
        let payload = bincode::serialize(changes).context("unable to encode ancestor changes")?;
        let mut record = Vec::with_capacity(payload.len() + RECORD_HEADER_SIZE);
        record.extend_from_slice(&self.generation.to_le_bytes());
        record.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        record.extend_from_slice(&digest(&payload));
        record.extend_from_slice(&payload);

        // A record approaching the size of a full rewrite is not worth
        // journalling: appending it would trip compaction immediately and
        // the hierarchy would be written twice. The first cycle of a session
        // is exactly this case — its single change carries the whole tree —
        // as is any bulk change, such as switching branches.
        if record.len() as u64 > self.compaction_threshold() {
            self.checkpoint(self.generation + 1, ancestor)?;
            self.generation += 1;
            return Ok(());
        }

        let mut journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)
            .context("unable to open the ancestor journal")?;
        if let Err(error) = journal.write_all(&record) {
            // A partial append — a full disk is the ordinary cause — must
            // not persist: bytes left at the tail would sit in front of the
            // next successful append and swallow it on the next load. Roll
            // the file back to the length every record before this one ends
            // at; if even that fails, the next open's normalization removes
            // the tear instead.
            let _ = journal.set_len(self.journal_bytes);
            return Err(error).context("unable to append to the ancestor journal");
        }

        self.generation += 1;
        self.journal_bytes += record.len() as u64;

        if self.journal_bytes > self.compaction_threshold() {
            self.checkpoint(self.generation, ancestor)?;
        }
        Ok(())
    }

    /// The journal size at which rewriting the hierarchy becomes the
    /// cheaper option. Scaling it to the checkpoint keeps the amortised
    /// cost of the full writes proportional to the change that provoked
    /// them, whatever the size of the tree.
    fn compaction_threshold(&self) -> u64 {
        MINIMUM_COMPACTION_SIZE.max(self.checkpoint_bytes / COMPACTION_RATIO)
    }

    /// Writes the hierarchy as a fresh checkpoint and clears the journal.
    ///
    /// The checkpoint is published before the journal is cleared; a crash
    /// between the two leaves spent records that replay skips and the next
    /// load's normalization retires. Unlike the per-cycle append — which
    /// deliberately stays in the process-crash durability class — the
    /// publish-then-truncate pair here is ordered with fsync: a power loss
    /// that kept the truncation but dropped the checkpoint would silently
    /// roll the ancestor back by every generation the journal held, and
    /// compaction is rare enough that the sync costs nothing anyone waits
    /// on.
    fn checkpoint(&mut self, generation: u64, ancestor: Option<&Node>) -> Result<()> {
        let payload =
            bincode::serialize(&ancestor).context("unable to encode the ancestor checkpoint")?;
        let mut data = Vec::with_capacity(payload.len() + 24);
        data.extend_from_slice(&CHECKPOINT_MAGIC);
        data.extend_from_slice(&generation.to_le_bytes());
        // The same truncated digest the journal's records carry: roughly
        // forty percent of a checkpoint's bytes are content digests, where
        // a flipped bit stays structurally valid bincode and directly
        // misclassifies a file during reconciliation.
        data.extend_from_slice(&digest(&payload));
        data.extend_from_slice(&payload);

        let temporary = self.checkpoint_path.with_extension("tmp");
        {
            let mut file =
                File::create(&temporary).context("unable to write the ancestor checkpoint")?;
            file.write_all(&data)
                .context("unable to write the ancestor checkpoint")?;
            file.sync_all()
                .context("unable to sync the ancestor checkpoint")?;
        }
        fs::rename(&temporary, &self.checkpoint_path)
            .context("unable to publish the ancestor checkpoint")?;
        if let Some(parent) = self.checkpoint_path.parent() {
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
        }

        // Truncation rather than removal: an empty journal and a missing one
        // mean the same thing to replay, and truncating cannot race a reader
        // into seeing the path vanish.
        File::create(&self.journal_path).context("unable to clear the ancestor journal")?;

        self.checkpoint_bytes = data.len() as u64;
        self.journal_bytes = 0;
        Ok(())
    }
}

/// The generation, length, and digest that precede each record's payload.
const RECORD_HEADER_SIZE: usize = 8 + 8 + 8;

/// The largest payload a record may claim, which bounds what a corrupt
/// length can make the loader allocate.
const MAXIMUM_RECORD_SIZE: u64 = 1 << 30;

struct Record {
    base_generation: u64,
    changes: Vec<Change>,
    /// The record's exact on-disk bytes, header included, so the journal
    /// can be rewritten as precisely the records that were applied.
    raw: Vec<u8>,
}

fn journal_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".journal");
    path.with_file_name(name)
}

/// Eight bytes of BLAKE3 over the payload — enough to catch the truncation
/// and tearing this is guarding against, without another dependency.
fn digest(payload: &[u8]) -> [u8; 8] {
    let hash = blake3::hash(payload);
    let mut digest = [0u8; 8];
    digest.copy_from_slice(&hash.as_bytes()[..8]);
    digest
}

/// Reads the checkpoint, returning its generation, hierarchy, and size.
fn read_checkpoint(path: &Path) -> Result<(u64, Option<Node>, u64)> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, None, 0)),
        Err(error) => return Err(error).context("unable to read ancestor"),
    };
    let size = data.len() as u64;
    // A checkpoint from a build that predates journalling is the bare
    // hierarchy. It reads as generation zero, and the first compaction
    // rewrites it in the current form.
    if !data.starts_with(&CHECKPOINT_MAGIC) {
        let ancestor: Option<Node> =
            bincode::deserialize(&data).context("unable to decode ancestor")?;
        return Ok((0, ancestor, size));
    }
    let body = &data[CHECKPOINT_MAGIC.len()..];
    if body.len() < 16 {
        bail!("the ancestor checkpoint is truncated");
    }
    let generation = u64::from_le_bytes(body[..8].try_into().expect("eight bytes"));
    let payload = &body[16..];
    if digest(payload) != body[8..16] {
        bail!("the ancestor checkpoint is corrupt");
    }
    let ancestor: Option<Node> =
        bincode::deserialize(payload).context("unable to decode ancestor")?;
    Ok((generation, ancestor, size))
}

/// Reads every intact record from the journal, in order, and reports how
/// many bytes of it are usable.
///
/// A record left incomplete by a crash mid-append can only be the last one,
/// and is discarded: it was never acknowledged, so the cycle that would have
/// produced it never completed either. A record that is complete but whose
/// payload does not match its digest is a different matter — something
/// claimed to be durable and is not — and fails the load rather than being
/// skipped.
fn read_journal(path: &Path) -> Result<(Vec<Record>, u64)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error).context("unable to read the ancestor journal")?,
    };
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .context("unable to read the ancestor journal")?;

    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset + RECORD_HEADER_SIZE <= data.len() {
        let header = &data[offset..offset + RECORD_HEADER_SIZE];
        let base_generation = u64::from_le_bytes(header[..8].try_into().expect("eight bytes"));
        let length = u64::from_le_bytes(header[8..16].try_into().expect("eight bytes"));
        if length > MAXIMUM_RECORD_SIZE {
            bail!("the ancestor journal declares a record of {length} bytes");
        }
        let start = offset + RECORD_HEADER_SIZE;
        let end = start + length as usize;
        if end > data.len() {
            // A torn tail. Everything before it stands.
            break;
        }
        let payload = &data[start..end];
        if digest(payload) != header[16..24] {
            bail!("the ancestor journal is corrupt at offset {offset}");
        }
        let changes: Vec<Change> = bincode::deserialize(payload)
            .context("unable to decode a record of the ancestor journal")?;
        records.push(Record {
            base_generation,
            changes,
            raw: data[offset..end].to_vec(),
        });
        offset = end;
    }
    // The *physical* length goes back, not the parsed one: the caller
    // compares it against what replay applied to decide whether the file
    // needs normalizing.
    Ok((records, data.len() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Content, Digest, FileMetadata};
    use std::sync::Arc;
    use tempfile::tempdir;

    /// `Node` has no `PartialEq` — content equivalence is the meaningful
    /// comparison, since names and scan metadata are not what the ancestor
    /// is being trusted for.
    fn same(left: &Option<Node>, right: &Option<Node>) -> bool {
        match (left, right) {
            (None, None) => true,
            (Some(left), Some(right)) => left.content_equal(right, true),
            _ => false,
        }
    }

    fn file(name: &str, byte: u8) -> Node {
        Node {
            name: name.into(),
            content: Content::File {
                digest: [byte; std::mem::size_of::<Digest>()],
                executable: false,
                metadata: FileMetadata::default(),
            },
        }
    }

    fn directory(children: Vec<Node>) -> Node {
        Node {
            name: String::new(),
            content: Content::Directory(Arc::new(children)),
        }
    }

    fn change(path: &str, new: Option<Node>) -> Change {
        Change {
            path: path.into(),
            old: None,
            new,
        }
    }

    /// Records must survive a reopen, which is the entire point.
    #[test]
    fn journalled_changes_reload() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");

        let (mut store, ancestor) = AncestorStore::open(&path).expect("opens");
        assert!(ancestor.is_none(), "a fresh session has no ancestor");

        let first = Some(directory(vec![file("a", 1)]));
        store
            .record(&[change("", first.clone())], first.as_ref())
            .expect("records");
        let second = Some(directory(vec![file("a", 1), file("b", 2)]));
        store
            .record(&[change("b", Some(file("b", 2)))], second.as_ref())
            .expect("records");

        let (_, reloaded) = AncestorStore::open(&path).expect("reopens");
        assert!(
            same(&reloaded, &second),
            "the reloaded ancestor must match what was recorded"
        );
    }

    /// A checkpoint written before journalling existed must still load.
    #[test]
    fn a_legacy_checkpoint_is_read_as_generation_zero() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let legacy = Some(directory(vec![file("a", 1)]));
        fs::write(&path, bincode::serialize(&legacy).expect("encodes")).expect("writes");

        let (mut store, ancestor) = AncestorStore::open(&path).expect("opens");
        assert!(same(&ancestor, &legacy));
        assert_eq!(store.generation, 0);

        // And it must accept records on top of itself.
        let next = Some(directory(vec![file("a", 1), file("b", 2)]));
        store
            .record(&[change("b", Some(file("b", 2)))], next.as_ref())
            .expect("records");
        let (_, reloaded) = AncestorStore::open(&path).expect("reopens");
        assert!(same(&reloaded, &next));
    }

    /// A crash part-way through an append leaves a record that was never
    /// acknowledged. It must be discarded, not replayed and not fatal.
    #[test]
    fn a_torn_final_record_is_discarded() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let (mut store, _) = AncestorStore::open(&path).expect("opens");
        let first = Some(directory(vec![file("a", 1)]));
        store
            .record(&[change("", first.clone())], first.as_ref())
            .expect("records");

        let journal = journal_path(&path);
        let mut data = fs::read(&journal).expect("reads");
        data.extend_from_slice(&7u64.to_le_bytes());
        data.extend_from_slice(&4096u64.to_le_bytes());
        data.extend_from_slice(&[0u8; 8]);
        data.extend_from_slice(b"partial");
        fs::write(&journal, &data).expect("writes");

        let (_, reloaded) = AncestorStore::open(&path).expect("opens despite the torn tail");
        assert!(
            same(&reloaded, &first),
            "the intact record must still apply"
        );
    }

    /// A complete record whose payload does not match its digest claimed to
    /// be durable and is not. Silently skipping it would lose a cycle's
    /// provenance, so it fails the load.
    #[test]
    fn a_corrupt_record_fails_the_load() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let (mut store, _) = AncestorStore::open(&path).expect("opens");
        let first = Some(directory(vec![file("a", 1)]));
        store
            .record(&[change("", first.clone())], first.as_ref())
            .expect("records");

        let journal = journal_path(&path);
        let mut data = fs::read(&journal).expect("reads");
        let last = data.len() - 1;
        data[last] ^= 0xff;
        fs::write(&journal, &data).expect("writes");

        let error = match AncestorStore::open(&path) {
            Ok(_) => panic!("a corrupt record must not load"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("corrupt"),
            "unexpected error: {error:#}"
        );
    }

    /// Compaction folds the journal into the checkpoint. Records written
    /// before it must not be applied a second time afterwards.
    #[test]
    fn compaction_retires_the_records_it_absorbed() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let (mut store, _) = AncestorStore::open(&path).expect("opens");

        let mut children = Vec::new();
        let mut ancestor = None;
        // Enough records to cross the compaction threshold.
        for index in 0..64 {
            let name = format!("f{index:03}");
            children.push(file(&name, index as u8));
            children.sort_by(|a, b| a.name.cmp(&b.name));
            ancestor = Some(directory(children.clone()));
            let payload = vec![change(&name, Some(file(&name, index as u8))); 512];
            store.record(&payload, ancestor.as_ref()).expect("records");
        }
        assert!(store.generation > 0);

        let (_, reloaded) = AncestorStore::open(&path).expect("reopens");
        assert!(
            same(&reloaded, &ancestor),
            "the ancestor must survive compaction unchanged"
        );

        // A stale journal left behind by a crash between publishing the
        // checkpoint and clearing it must be recognised as spent.
        let journal = journal_path(&path);
        let stale = fs::read(&journal).expect("reads");
        store
            .checkpoint(store.generation, ancestor.as_ref())
            .expect("checkpoints");
        fs::write(&journal, &stale).expect("restores the stale journal");
        let (_, after) = AncestorStore::open(&path).expect("reopens");
        assert!(
            same(&after, &ancestor),
            "records already folded into the checkpoint must not reapply"
        );
    }

    /// A reset must forget the ancestor, not merely the checkpoint. A
    /// surviving journal would replay onto the empty store and reinstate
    /// what the reset discarded.
    #[test]
    fn a_reset_leaves_nothing_to_replay() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let (mut store, _) = AncestorStore::open(&path).expect("opens");
        let recorded = Some(directory(vec![file("a", 1)]));
        store
            .record(&[change("", recorded.clone())], recorded.as_ref())
            .expect("records");

        AncestorStore::reset(&path).expect("resets");
        let (_, after) = AncestorStore::open(&path).expect("reopens");
        assert!(after.is_none(), "a reset ancestor must stay reset");
    }

    /// The point of the whole exercise: recording one changed file must not
    /// cost a rewrite of the hierarchy. This is the property that regressed
    /// silently before, because a full rewrite is correct — merely slow —
    /// and nothing but a benchmark would have noticed.
    #[test]
    fn one_changed_file_does_not_rewrite_the_hierarchy() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let (mut store, _) = AncestorStore::open(&path).expect("opens");

        // A hierarchy large enough that rewriting it would be conspicuous.
        let mut children: Vec<Node> = (0..30_000).map(|i| file(&format!("f{i:06}"), 1)).collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        let initial = Some(directory(children.clone()));
        let hierarchy_bytes = bincode::serialize(&initial).expect("encodes").len() as u64;
        assert!(
            hierarchy_bytes > MINIMUM_COMPACTION_SIZE,
            "the corpus must be worth journalling: {hierarchy_bytes} bytes"
        );
        store
            .record(&[change("", initial.clone())], initial.as_ref())
            .expect("records");

        // Total bytes on disk, whichever files the store chose to use.
        let written = |path: &Path| -> u64 {
            [path.to_path_buf(), journal_path(path)]
                .iter()
                .filter_map(|path| fs::metadata(path).ok())
                .map(|metadata| metadata.len())
                .sum()
        };
        let before = written(&path);

        // One file changes.
        let name = children[0].name.clone();
        children[0] = file(&name, 9);
        let next = Some(directory(children));
        store
            .record(&[change(&name, Some(file(&name, 9)))], next.as_ref())
            .expect("records");

        let growth = written(&path).saturating_sub(before);
        assert!(
            growth < hierarchy_bytes / 10,
            "one edit wrote {growth} bytes against a {hierarchy_bytes}-byte hierarchy"
        );

        let (_, reloaded) = AncestorStore::open(&path).expect("reopens");
        assert!(same(&reloaded, &next), "and the edit must survive");
    }

    /// Reads the store at `path` in a fresh copy directory, with the journal
    /// truncated to `length` bytes — the on-disk state a crash at that byte
    /// would leave under process-crash semantics.
    fn open_cut(
        checkpoint: &Path,
        journal_bytes: &[u8],
        length: usize,
    ) -> Result<(AncestorStore, Option<Node>)> {
        let keep = tempdir().expect("temporary directory");
        let target = keep.path().join("ancestor");
        if checkpoint.exists() {
            fs::copy(checkpoint, &target).expect("checkpoint copies");
        }
        fs::write(journal_path(&target), &journal_bytes[..length]).expect("journal writes");
        let result = AncestorStore::open(&target);
        // The store carries its paths; keep the directory alive alongside.
        std::mem::forget(keep);
        result
    }

    /// The invariant the whole store exists to provide: after a crash at any
    /// byte of the journal, reopening yields exactly the acknowledged state
    /// at that point — and the store must remain *appendable*: a fresh
    /// record made after recovery must survive its own reopen. The second
    /// half is what the original implementation lost: dead bytes left in
    /// the journal swallowed every later acknowledgment.
    #[test]
    fn every_journal_cut_reopens_and_stays_appendable() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let (mut store, _) = AncestorStore::open(&path).expect("opens");

        // Acknowledged history: after each record, the state and the journal
        // byte length it was acknowledged at.
        let mut boundaries: Vec<(u64, Option<Node>)> = vec![(0, None)];
        let mut state: Option<Node> = None;
        for index in 0..4u8 {
            let name = format!("f{index}");
            let mut children: Vec<Node> = state
                .as_ref()
                .map(|n| n.children().to_vec())
                .unwrap_or_default();
            children.push(file(&name, index + 1));
            let next = Some(directory(children));
            // The first record creates the root, as a session's first cycle
            // would; the rest are child changes onto the existing state.
            let advance = if state.is_none() {
                change("", next.clone())
            } else {
                change(&name, Some(file(&name, index + 1)))
            };
            store.record(&[advance], next.as_ref()).expect("records");
            let length = fs::metadata(journal_path(&path)).expect("journal").len();
            state = next;
            boundaries.push((length, state.clone()));
        }
        let journal = fs::read(journal_path(&path)).expect("journal reads");

        for cut in 0..=journal.len() {
            let expected = boundaries
                .iter()
                .rev()
                .find(|(length, _)| *length as usize <= cut)
                .expect("a boundary")
                .1
                .clone();
            let (mut reopened, loaded) = open_cut(&path, &journal, cut).unwrap_or_else(|error| {
                panic!(
                    "cut at {cut} of {} failed to open: {error:#}",
                    journal.len()
                )
            });
            assert!(
                same(&loaded, &expected),
                "cut at {cut}: reloaded state is not the acknowledged one"
            );
            // Recovery must leave the store appendable: a fresh record made
            // now must survive its own reopen.
            let mut children: Vec<Node> = loaded
                .as_ref()
                .map(|n| n.children().to_vec())
                .unwrap_or_default();
            children.retain(|c| c.name != "fresh");
            children.push(file("fresh", 99));
            let with_fresh = Some(directory(children));
            // Changes must fit the state they apply to, exactly as the
            // session's reconciler guarantees: a child change onto a
            // missing root cannot replay, so an empty state gets a
            // root-level creation instead.
            let fresh_change = if loaded.is_none() {
                change("", with_fresh.clone())
            } else {
                change("fresh", Some(file("fresh", 99)))
            };
            reopened
                .record(&[fresh_change], with_fresh.as_ref())
                .expect("recovered store accepts a record");
            let (_, after) =
                AncestorStore::open(&reopened.checkpoint_path).expect("reopens after append");
            assert!(
                same(&after, &with_fresh),
                "cut at {cut}: an acknowledgment made after recovery was lost"
            );
        }
    }

    /// A crash between publishing a checkpoint and clearing the journal
    /// leaves spent records. They must be skipped — and must not swallow
    /// records appended afterwards.
    #[test]
    fn a_spent_prefix_never_masks_later_acknowledgments() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");
        let (mut store, _) = AncestorStore::open(&path).expect("opens");
        let first = Some(directory(vec![file("a", 1)]));
        store
            .record(&[change("", first.clone())], first.as_ref())
            .expect("records");
        let stale = fs::read(journal_path(&path)).expect("journal");

        // The checkpoint publishes, then the crash lands before the journal
        // clears: simulated by putting the spent bytes back.
        store
            .checkpoint(store.generation, first.as_ref())
            .expect("checkpoints");
        fs::write(journal_path(&path), &stale).expect("restores the spent journal");

        let (mut reopened, loaded) = AncestorStore::open(&path).expect("opens");
        assert!(same(&loaded, &first));
        let second = Some(directory(vec![file("a", 1), file("b", 2)]));
        reopened
            .record(&[change("b", Some(file("b", 2)))], second.as_ref())
            .expect("records after recovery");
        let (_, after) = AncestorStore::open(&path).expect("reopens");
        assert!(
            same(&after, &second),
            "the acknowledgment behind the spent prefix was lost"
        );
    }

    /// A crash inside reset() must leave a state that was once acknowledged
    /// — never a fabrication built by replaying deltas against the wrong
    /// base. The dangerous residue is a removed checkpoint with a surviving
    /// journal, whose base-zero records then replay against nothing.
    #[test]
    fn an_interrupted_reset_cannot_fabricate_an_ancestor() {
        let keep = tempdir().expect("temporary directory");
        let path = keep.path().join("ancestor");

        // A legacy checkpoint (bare hierarchy, read as generation zero) with
        // a delta journalled on top: the exact shape codex flagged.
        let base = Some(directory(vec![file("a", 1), file("b", 2)]));
        fs::write(&path, bincode::serialize(&base).expect("encodes")).expect("writes");
        let (mut store, loaded) = AncestorStore::open(&path).expect("opens");
        assert!(same(&loaded, &base));
        let full = Some(directory(vec![file("a", 1), file("b", 2), file("c", 3)]));
        store
            .record(&[change("c", Some(file("c", 3)))], full.as_ref())
            .expect("records");

        // Reset crashes after its first removal. Whatever order the
        // implementation uses, the residue must reopen to a state that was
        // actually acknowledged: the full state, the checkpoint state, or
        // nothing. Removing the checkpoint first leaves the delta to replay
        // against None — fabrication or a load failure, both wrong.
        let residues: [&dyn Fn(); 2] = [
            &|| {
                let _ = fs::remove_file(&path);
            },
            &|| {
                let _ = fs::remove_file(journal_path(&path));
            },
        ];
        for (index, remove_first) in residues.iter().enumerate() {
            // Rebuild the pre-reset state each round.
            fs::write(&path, bincode::serialize(&base).expect("encodes")).expect("writes");
            let _ = fs::remove_file(journal_path(&path));
            let (mut store, _) = AncestorStore::open(&path).expect("opens");
            store
                .record(&[change("c", Some(file("c", 3)))], full.as_ref())
                .expect("records");

            // The crash: only the implementation's FIRST removal happened.
            // Residue 0 models checkpoint-first (the old order), residue 1
            // journal-first. The implementation controls which of these can
            // occur; both are asserted so the test outlives the choice.
            remove_first();
            let outcome = AncestorStore::open(&path);
            match outcome {
                Ok((_, state)) => {
                    let acknowledged =
                        same(&state, &full) || same(&state, &base) || state.is_none();
                    assert!(
                        acknowledged,
                        "residue {index}: reopened to a state never acknowledged"
                    );
                }
                // Residue 0 — checkpoint removed, journal surviving — is
                // the old removal order's crash state, unreachable now that
                // reset removes the journal first. If such a store is ever
                // encountered anyway, failing closed is acceptable;
                // fabricating a hierarchy is not. Residue 1 is the fixed
                // order's own crash state and must load.
                Err(error) => assert_eq!(
                    index, 0,
                    "the fixed order's residue must not brick the store: {error:#}"
                ),
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 24, ..Default::default()
        })]

        /// The deterministic cut test, generalized: random interleavings of
        /// records and checkpoints, then the same two invariants at every
        /// byte of the surviving journal — the acknowledged state is
        /// reproduced exactly, and a record made after recovery survives
        /// its own reopen.
        #[test]
        fn random_histories_survive_every_cut(
            operations in proptest::collection::vec(0u8..12, 1..9)
        ) {
            let keep = tempdir().expect("temporary directory");
            let path = keep.path().join("ancestor");
            let (mut store, _) = AncestorStore::open(&path).expect("opens");

            let mut state: Option<Node> = None;
            let mut boundaries: Vec<(u64, Option<Node>)> = vec![(0, None)];
            let mut counter = 0u8;
            for operation in operations {
                if operation >= 9 {
                    // A checkpoint absorbs the journal; the boundary map
                    // starts over from the checkpointed state.
                    store
                        .checkpoint(store.generation, state.as_ref())
                        .expect("checkpoints");
                    boundaries = vec![(0, state.clone())];
                    continue;
                }
                counter += 1;
                let name = format!("f{operation}");
                let mut children: Vec<Node> =
                    state.as_ref().map(|n| n.children().to_vec()).unwrap_or_default();
                children.retain(|c| c.name != name);
                children.push(file(&name, counter));
                children.sort_by(|a, b| a.name.cmp(&b.name));
                let next = Some(directory(children));
                let advance = if state.is_none() {
                    change("", next.clone())
                } else {
                    change(&name, Some(file(&name, counter)))
                };
                store.record(&[advance], next.as_ref()).expect("records");
                let length = fs::metadata(journal_path(&path))
                    .map(|m| m.len())
                    .unwrap_or(0);
                state = next;
                boundaries.push((length, state.clone()));
            }

            let journal = fs::read(journal_path(&path)).unwrap_or_default();
            for cut in 0..=journal.len() {
                let expected = boundaries
                    .iter()
                    .rev()
                    .find(|(length, _)| *length as usize <= cut)
                    .expect("a boundary")
                    .1
                    .clone();
                let (mut reopened, loaded) = open_cut(&path, &journal, cut)
                    .unwrap_or_else(|error| panic!("cut {cut}: {error:#}"));
                proptest::prop_assert!(
                    same(&loaded, &expected),
                    "cut {cut}: reloaded state was never acknowledged there"
                );
                let mut children: Vec<Node> =
                    loaded.as_ref().map(|n| n.children().to_vec()).unwrap_or_default();
                children.retain(|c| c.name != "fresh");
                children.push(file("fresh", 200));
                children.sort_by(|a, b| a.name.cmp(&b.name));
                let with_fresh = Some(directory(children));
                let advance = if loaded.is_none() {
                    change("", with_fresh.clone())
                } else {
                    change("fresh", Some(file("fresh", 200)))
                };
                reopened
                    .record(&[advance], with_fresh.as_ref())
                    .expect("recovered store accepts a record");
                let (_, after) =
                    AncestorStore::open(&reopened.checkpoint_path).expect("reopens");
                proptest::prop_assert!(
                    same(&after, &with_fresh),
                    "cut {cut}: an acknowledgment made after recovery was lost"
                );
            }
        }
    }
}
