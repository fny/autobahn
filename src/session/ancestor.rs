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
//! compaction, amortised against the volume of change that provoked it. The
//! durability contract is unchanged — nothing is installed in memory until
//! its record is on disk — and the window in which the ancestor lags the
//! transition shrinks from the length of a full encode-and-write to the
//! length of an append.
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

        let (records, journal_bytes) = read_journal(&journal_path)?;
        for record in records {
            // A record that does not follow the generation in hand belongs
            // to a checkpoint that has already absorbed it (or to some other
            // lineage entirely). Either way it is not ours to apply.
            if record.base_generation != generation {
                break;
            }
            ancestor = apply(ancestor.as_ref(), &record.changes).map_err(|message| {
                anyhow::anyhow!("unable to replay ancestor journal: {message}")
            })?;
            generation += 1;
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
    /// Every file the store owns goes together. Removing the checkpoint
    /// alone would leave a journal whose first record expects generation
    /// zero — which is exactly what an empty store reports — so replay would
    /// reinstate the ancestor that was just discarded.
    pub(crate) fn reset(path: &Path) -> Result<()> {
        for path in [path.to_path_buf(), journal_path(path)] {
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

    /// Records the changes that advance the ancestor to `ancestor`, and does
    /// not return until they are on disk.
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
        journal
            .write_all(&record)
            .context("unable to append to the ancestor journal")?;

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
    /// The checkpoint is published before the journal is cleared. A crash
    /// between the two leaves records describing changes the checkpoint
    /// already contains; replay recognises them because their generation no
    /// longer follows the checkpoint's, and stops.
    fn checkpoint(&mut self, generation: u64, ancestor: Option<&Node>) -> Result<()> {
        let mut data = Vec::with_capacity(self.checkpoint_bytes as usize + 16);
        data.extend_from_slice(&CHECKPOINT_MAGIC);
        data.extend_from_slice(&generation.to_le_bytes());
        bincode::serialize_into(&mut data, &ancestor)
            .context("unable to encode the ancestor checkpoint")?;

        let temporary = self.checkpoint_path.with_extension("tmp");
        fs::write(&temporary, &data).context("unable to write the ancestor checkpoint")?;
        fs::rename(&temporary, &self.checkpoint_path)
            .context("unable to publish the ancestor checkpoint")?;

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
    if body.len() < 8 {
        bail!("the ancestor checkpoint is truncated");
    }
    let generation = u64::from_le_bytes(body[..8].try_into().expect("eight bytes"));
    let ancestor: Option<Node> =
        bincode::deserialize(&body[8..]).context("unable to decode ancestor")?;
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
        });
        offset = end;
    }
    Ok((records, offset as u64))
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
}
