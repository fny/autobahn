//! The local (in-process) endpoint.
//!
//! This is the only module in autobahn that mutates a user's filesystem, and
//! every design decision here follows from that: the endpoint refuses and
//! reports rather than guesses, it never follows a symbolic link while
//! validating or removing content, and it publishes new content only by
//! renaming a fully written temporary into place.
//!
//! Three pieces of state make the whole thing work:
//!
//! - **The last scan.** [`scan`](Endpoint::scan) retains its snapshot, which
//!   serves both as the digest cache for the next scan (node-resident
//!   metadata — see [`crate::scan`]) and as the record that transitions
//!   validate the filesystem against. The controller's cycle guarantees the
//!   ordering that makes the latter sound: the transitions handed to
//!   [`transition`](Endpoint::transition) were reconciled from the snapshot
//!   returned by the immediately preceding scan, so "matches the last scan"
//!   is exactly "unchanged since reconciliation decided this was safe".
//! - **Content-addressed staging.** Received (and locally reused) file
//!   content lands at `staging_root/<digest-hex>` once it has been verified.
//!   Staging is therefore idempotent, resumable across interrupted cycles,
//!   and deduplicated between paths that share content, all for free.
//! - **Temporaries.** Every intermediate file this module creates is named
//!   with the [`TEMPORARY_PREFIX`] that scans skip, so an in-flight (or
//!   abandoned) transition is never mistaken for synchronizable content.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::rsync::{self, Signature};
use crate::scan::{
    self, recompose, validate_portable_target, FilesystemBehavior, IgnoreSet, SymlinkMode,
};
use crate::tree::{path_join, Change, Content, Digest, FileMetadata, Node, Problem, Snapshot};

/// The name prefix shared by every temporary file this module creates. It
/// matches the prefix that scanning skips, so temporaries are invisible to
/// the synchronization hierarchy no matter which directory they live in.
const TEMPORARY_PREFIX: &str = ".autobahn-tmp";

/// The default permission bits applied to created directories. The default
/// is deliberately conservative (owner-only, matching Mutagen): synchronized
/// trees frequently hold credentials, and a too-tight mode is an
/// inconvenience while a too-loose one is an exposure.
const DEFAULT_DIRECTORY_MODE: u32 = 0o700;

/// The default permission bits applied to created non-executable files.
const DEFAULT_FILE_MODE: u32 = 0o600;

/// The size of the buffer used to stream local staging copies.
const COPY_BUFFER_SIZE: usize = 64 * 1024;

/// The counter that uniquifies temporary file names within a process.
static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The policy options governing a local endpoint's behavior.
#[derive(Debug, Default)]
pub struct EndpointOptions {
    /// The ignore set applied to scans.
    pub ignores: IgnoreSet,
    /// The treatment of symbolic links.
    pub symlink_mode: SymlinkMode,
    /// The permission bits for created non-executable files (`None` for the
    /// default).
    pub file_mode: Option<u32>,
    /// The permission bits for created directories (`None` for the default).
    pub directory_mode: Option<u32>,
}

/// A local filesystem endpoint.
pub struct LocalEndpoint {
    /// The synchronization root (which need not exist).
    root: PathBuf,
    /// The directory holding staged content and staging temporaries.
    staging_root: PathBuf,
    /// The ignore set applied to scans.
    ignores: IgnoreSet,
    /// The treatment of symbolic links.
    symlink_mode: SymlinkMode,
    /// The permission bits for created non-executable files.
    file_mode: u32,
    /// The permission bits for created directories.
    directory_mode: u32,
    /// The probed behavior of the root's filesystem, determined at the
    /// first scan that finds the root present and cached for the endpoint's
    /// lifetime.
    behavior: Option<FilesystemBehavior>,
    /// The most recent scan, used as the digest cache for the next scan, as
    /// the local-content index for staging, and as the record that
    /// transitions validate against.
    last_snapshot: Option<Snapshot>,
    /// The open supply stream, if any.
    supply: Option<SupplyState>,
    /// The receive state established by the last [`stage_begin`], if any.
    ///
    /// [`stage_begin`]: Endpoint::stage_begin
    receive: Option<ReceiveState>,
    /// The filesystem watcher, created lazily at the first
    /// [`await_change`](Endpoint::await_change) that finds the root present.
    watcher: Option<ChangeWatcher>,
}

/// A recursive filesystem watcher over the synchronization root, delivering
/// events through a channel.
struct ChangeWatcher {
    /// The watcher itself, retained for its lifetime side effect.
    _watcher: notify::RecommendedWatcher,
    /// The event stream.
    receiver: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
}

impl ChangeWatcher {
    /// Establishes a recursive watch over `root`.
    fn new(root: &Path) -> Result<ChangeWatcher> {
        use notify::Watcher;
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })
        .context("unable to create a filesystem watcher")?;
        watcher
            .watch(root, notify::RecursiveMode::Recursive)
            .with_context(|| format!("unable to watch {}", root.display()))?;
        Ok(ChangeWatcher {
            _watcher: watcher,
            receiver,
        })
    }
}

impl LocalEndpoint {
    /// Creates a local endpoint for the specified synchronization root,
    /// with staging state isolated under `staging_root` (which will be
    /// created if needed).
    ///
    /// The synchronization root itself is neither created nor required to
    /// exist: a missing root is a legitimate synchronization state (and one
    /// that a transition may resolve by creating it).
    pub fn new(
        root: PathBuf,
        staging_root: PathBuf,
        options: EndpointOptions,
    ) -> Result<LocalEndpoint> {
        fs::create_dir_all(&staging_root).with_context(|| {
            format!(
                "unable to create staging directory {}",
                staging_root.display()
            )
        })?;
        Ok(LocalEndpoint {
            root,
            staging_root,
            ignores: options.ignores,
            symlink_mode: options.symlink_mode,
            file_mode: options.file_mode.unwrap_or(DEFAULT_FILE_MODE) & 0o777,
            directory_mode: options.directory_mode.unwrap_or(DEFAULT_DIRECTORY_MODE) & 0o777,
            behavior: None,
            last_snapshot: None,
            supply: None,
            receive: None,
            watcher: None,
        })
    }

    /// Returns the path at which content with the specified digest lives once
    /// it has been fully received and verified.
    fn staged_path(&self, digest: &Digest) -> PathBuf {
        staged_path(&self.staging_root, digest)
    }

    /// Builds an index from content digest to root-relative path over the
    /// last scan's file nodes, enabling requests to be satisfied by content
    /// that already exists somewhere in the root.
    fn digest_index(&self) -> HashMap<Digest, String> {
        fn collect(node: &Node, path: &str, index: &mut HashMap<Digest, String>) {
            match &node.content {
                Content::File { digest, .. } => {
                    // The first path recorded for a digest wins; any of them
                    // would do, and this keeps the walk allocation-free for
                    // duplicated content.
                    index.entry(*digest).or_insert_with(|| path.to_owned());
                }
                Content::Directory(children) => {
                    for child in children.iter() {
                        collect(child, &path_join(path, &child.name), index);
                    }
                }
                _ => {}
            }
        }
        let mut index = HashMap::new();
        if let Some(root) = self.last_snapshot.as_ref().and_then(|s| s.root.as_ref()) {
            collect(root, "", &mut index);
        }
        index
    }

    /// Attempts to satisfy a content request from a file that already exists
    /// in the root, streaming it into staging while digesting it. Returns
    /// whether the content was staged: a digest mismatch (the file changed
    /// since the scan that indexed it) is an ordinary negative result, not an
    /// error, and leaves the request to be transferred normally.
    fn stage_locally(&self, source: &str, digest: &Digest) -> Result<bool> {
        let source_path = self.root.join(source);
        let temporary = self.staging_root.join(temporary_name("copy"));
        let copied = match copy_verifying(&source_path, &temporary, digest) {
            Ok(copied) => copied,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        };
        if !copied {
            let _ = fs::remove_file(&temporary);
            return Ok(false);
        }
        let staged = self.staged_path(digest);
        if let Err(error) = fs::rename(&temporary, &staged) {
            let _ = fs::remove_file(&temporary);
            return Err(error)
                .with_context(|| format!("unable to publish staged content {}", staged.display()));
        }
        Ok(true)
    }

    /// Buffers the complete delta for the supply stream's current need.
    ///
    /// Deltas are generated one file at a time, in full, because
    /// [`rsync::deltify`] streams to a callback while
    /// [`supply_pull`](Endpoint::supply_pull) must return bounded batches.
    /// The buffer therefore holds one file's operations (whose total size is
    /// the size of that file's *delta*, not the file), never more: the next
    /// file is only deltified once the previous one has been fully drained.
    fn buffer_delta(&self, need: &StagingNeed, pending: &mut VecDeque<TransferFrame>) {
        let path = self.root.join(&need.request.path);
        let error = match File::open(&path) {
            Err(error) => Some(format!("unable to open {}: {error}", need.request.path)),
            Ok(file) => match rsync::deltify(file, &need.signature, &mut |op| {
                pending.push_back(TransferFrame::Op(op));
                Ok(())
            }) {
                Ok(()) => None,
                Err(error) => Some(format!(
                    "unable to compute a delta for {}: {error:#}",
                    need.request.path
                )),
            },
        };
        // A failed supply still terminates the file's stream, so that the
        // receiver discards its partial content and moves on rather than
        // desynchronizing from the need list.
        pending.push_back(TransferFrame::EndOfFile { error });
    }

    /// Applies a batch of transfer frames to the receive state.
    fn push_frames(&self, state: &mut ReceiveState, frames: Vec<TransferFrame>) -> Result<()> {
        for frame in frames {
            let Some(need) = state.needs.get(state.current) else {
                bail!("received transfer frames beyond the end of the staging need list");
            };
            match frame {
                TransferFrame::Op(op) => {
                    if state.file.is_none() {
                        state.file = Some(self.open_receive_file(need)?);
                    }
                    let file = state
                        .file
                        .as_mut()
                        .expect("the receive file was just opened");
                    if let Err(error) =
                        rsync::patch(&mut file.base, &need.signature, &op, &mut file.writer)
                    {
                        // Patching only fails on a malformed delta or on real
                        // I/O trouble (a full or failing disk), neither of
                        // which the next operation would survive either.
                        if let Some(file) = state.file.take() {
                            file.discard();
                        }
                        return Err(error)
                            .with_context(|| format!("unable to stage {}", need.request.path));
                    }
                }
                TransferFrame::EndOfFile { error } => {
                    let file = state.file.take();
                    if error.is_some() {
                        // The source couldn't supply this file. That isn't a
                        // failure of this cycle: the file simply isn't staged,
                        // so the transition reports it missing and the next
                        // cycle retries it.
                        if let Some(file) = file {
                            file.discard();
                        }
                    } else {
                        // An empty file arrives as an end-of-file frame with
                        // no preceding operations, and still has to be staged.
                        let file = match file {
                            Some(file) => file,
                            None => self.open_receive_file(need)?,
                        };
                        self.finish_receive(file, need)?;
                    }
                    state.current += 1;
                }
            }
        }
        Ok(())
    }

    /// Opens a temporary receive file for a need, along with the base content
    /// its delta operations apply against (the current content at the need's
    /// path, or an empty base when there is nothing usable there).
    fn open_receive_file(&self, need: &StagingNeed) -> Result<ReceiveFile> {
        let temporary = self.staging_root.join(temporary_name("recv"));
        let output = File::create(&temporary)
            .with_context(|| format!("unable to create {}", temporary.display()))?;
        let target = self.root.join(&need.request.path);
        let base = match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_file() => match File::open(&target) {
                Ok(file) => PatchBase::File(file),
                Err(_) => PatchBase::empty(),
            },
            _ => PatchBase::empty(),
        };
        Ok(ReceiveFile {
            temporary,
            writer: DigestingWriter::new(output),
            base,
        })
    }

    /// Completes a received file: flushes it, compares the digest accumulated
    /// during patching against the requested one, and publishes the content
    /// into staging on a match. A mismatch means the file changed on the
    /// source mid-transfer, which is an ordinary occurrence — the partial
    /// content is discarded and the next cycle transfers the new content.
    fn finish_receive(&self, file: ReceiveFile, need: &StagingNeed) -> Result<()> {
        let ReceiveFile {
            temporary,
            mut writer,
            ..
        } = file;
        if let Err(error) = writer.flush() {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("unable to flush {}", temporary.display()));
        }
        let digest = writer.digest();
        drop(writer);
        if digest != need.request.digest {
            let _ = fs::remove_file(&temporary);
            return Ok(());
        }
        let staged = self.staged_path(&digest);
        if let Err(error) = fs::rename(&temporary, &staged) {
            let _ = fs::remove_file(&temporary);
            return Err(error)
                .with_context(|| format!("unable to publish staged content {}", staged.display()));
        }
        Ok(())
    }
}

impl Endpoint for LocalEndpoint {
    fn scan(&mut self) -> Result<Snapshot> {
        // Filesystem behavior is probed at the first scan that finds the
        // root present, then cached: the properties are per-volume, and the
        // volume doesn't change under a live endpoint.
        if self.behavior.is_none() && fs::symlink_metadata(&self.root).is_ok() {
            self.behavior = Some(scan::probe(&self.root));
        }
        let behavior = self.behavior.unwrap_or_default();

        // The watcher must exist *before* the scan, not lazily at the first
        // await: a change landing between the scan and a later
        // watcher creation would be invisible to
        // [`await_change`](Endpoint::await_change), and a caller relying on
        // change signals (rather than a tight heartbeat) would never learn
        // of it. Established here, events queue from the moment the
        // snapshot's view of the world is taken.
        if self.watcher.is_none() {
            self.watcher = ChangeWatcher::new(&self.root).ok();
        }

        // The retained snapshot is the scanner's baseline, which is what
        // turns a rescan into a walk of what changed rather than a re-read of
        // everything. Cloning it is cheap: directory children are shared
        // through `Arc`.
        let snapshot = scan::scan(
            &self.root,
            self.last_snapshot.as_ref(),
            &self.ignores,
            &behavior,
            self.symlink_mode,
        )
        .with_context(|| format!("unable to scan {}", self.root.display()))?;
        self.last_snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>> {
        fs::create_dir_all(&self.staging_root).with_context(|| {
            format!(
                "unable to create staging directory {}",
                self.staging_root.display()
            )
        })?;

        // Any receive state left over from a previous staging operation
        // belongs to a stream that will never be continued.
        if let Some(state) = self.receive.take() {
            state.discard();
        }

        // The local-content index is only built if a request actually misses
        // staging, and only once per staging operation.
        let mut index: Option<HashMap<Digest, String>> = None;
        let mut needs = Vec::new();
        for request in files {
            // Already staged, either by an earlier request in this batch or
            // by an interrupted previous cycle.
            if fs::symlink_metadata(self.staged_path(&request.digest)).is_ok() {
                continue;
            }

            // Identical content elsewhere in the root is faster to copy (and
            // verify) than to transfer.
            let source = index
                .get_or_insert_with(|| self.digest_index())
                .get(&request.digest)
                .cloned();
            if let Some(source) = source {
                // A digest mismatch (the file changed since the scan that
                // indexed it) or a read failure just means the content has to
                // come from the source endpoint after all.
                if let Ok(true) = self.stage_locally(&source, &request.digest) {
                    continue;
                }
            }

            // The content has to be transferred, so describe whatever base
            // content exists at the target path for the source to delta
            // against.
            let signature = base_signature(&self.root.join(&request.path));
            needs.push(StagingNeed { request, signature });
        }

        self.receive = Some(ReceiveState {
            needs: needs.clone(),
            current: 0,
            file: None,
        });
        Ok(needs)
    }

    fn supply_open(&mut self, needs: Vec<StagingNeed>) -> Result<()> {
        self.supply = Some(SupplyState {
            needs,
            current: 0,
            pending: VecDeque::new(),
        });
        Ok(())
    }

    fn supply_pull(&mut self, max_frames: usize) -> Result<Vec<TransferFrame>> {
        let Some(mut state) = self.supply.take() else {
            bail!("no supply stream is open");
        };
        // A zero-frame request would otherwise be indistinguishable from
        // exhaustion, which would silently truncate the transfer.
        let limit = max_frames.max(1);

        let mut frames = Vec::new();
        while frames.len() < limit {
            if state.pending.is_empty() {
                if state.current >= state.needs.len() {
                    break;
                }
                // Split the borrow: the delta for one need is buffered into
                // the pending queue, and only then is the cursor advanced.
                let need = &state.needs[state.current];
                self.buffer_delta(need, &mut state.pending);
                state.current += 1;
            }
            while frames.len() < limit {
                match state.pending.pop_front() {
                    Some(frame) => frames.push(frame),
                    None => break,
                }
            }
        }

        // An empty batch signals exhaustion, at which point the stream is
        // closed rather than left open for a pull that will never come.
        if !frames.is_empty() {
            self.supply = Some(state);
        }
        Ok(frames)
    }

    fn stage_push(&mut self, frames: Vec<TransferFrame>) -> Result<()> {
        let Some(mut state) = self.receive.take() else {
            bail!("no staging operation is in progress");
        };
        let result = self.push_frames(&mut state, frames);
        self.receive = Some(state);
        result
    }

    fn await_change(&mut self, timeout: std::time::Duration) -> Result<bool> {
        // The watcher is established lazily (the root may not exist yet) and
        // re-established after failures. A root that can't be watched
        // degrades to waiting out the timeout — the caller's heartbeat still
        // cycles, so watching failures cost latency, never correctness.
        if self.watcher.is_none() {
            match ChangeWatcher::new(&self.root) {
                Ok(watcher) => self.watcher = Some(watcher),
                Err(_) => {
                    std::thread::sleep(timeout);
                    return Ok(false);
                }
            }
        }
        let watcher = self.watcher.as_ref().expect("the watcher was just created");
        match watcher.receiver.recv_timeout(timeout) {
            Ok(_) => {
                // Coalesce whatever else is already queued; one wake covers
                // any number of events.
                while watcher.receiver.try_recv().is_ok() {}
                Ok(true)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(false),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // The watcher backend died; drop it (to be re-established)
                // and report a change so the caller rescans.
                self.watcher = None;
                Ok(true)
            }
        }
    }

    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
        // Validation is performed against the last scan, which the
        // controller's cycle guarantees is the very scan these transitions
        // were reconciled from. The snapshot is deliberately *not* updated
        // here: the next scan is what re-establishes the record, and a
        // half-updated record would be worse than a stale one.
        let mut transitioner = Transitioner {
            root: &self.root,
            staging_root: &self.staging_root,
            scanned: self.last_snapshot.as_ref().and_then(|s| s.root.as_ref()),
            behavior: self.behavior.unwrap_or_default(),
            symlink_mode: self.symlink_mode,
            file_mode: self.file_mode,
            directory_mode: self.directory_mode,
            problems: Vec::new(),
            missing_staged_files: false,
        };
        let mut results = Vec::with_capacity(transitions.len());
        for change in &transitions {
            // Each change is applied independently: a refusal at one path
            // must never abort the rest of the transition.
            results.push(transitioner.apply(change));
        }
        Ok(TransitionOutcome {
            results,
            problems: transitioner.problems,
            missing_staged_files: transitioner.missing_staged_files,
        })
    }
}

/// The state of an open supply stream: the needs being supplied, the index of
/// the next need to deltify, and the frames buffered for the need currently
/// being drained.
struct SupplyState {
    /// The needs to supply, in order.
    needs: Vec<StagingNeed>,
    /// The index of the next need to deltify.
    current: usize,
    /// The frames buffered for the current need.
    pending: VecDeque<TransferFrame>,
}

/// The state of an in-progress staging operation, running in parallel with
/// the need list returned by the last `stage_begin`: the controller pushes
/// exactly the frames the source pulls, in need order, so the current index
/// alone identifies the file each frame belongs to.
struct ReceiveState {
    /// The needs reported to the controller, in order.
    needs: Vec<StagingNeed>,
    /// The index of the need currently being received.
    current: usize,
    /// The file currently being written, if any operations have arrived for
    /// the current need.
    file: Option<ReceiveFile>,
}

impl ReceiveState {
    /// Discards any partially received content.
    fn discard(self) {
        if let Some(file) = self.file {
            file.discard();
        }
    }
}

/// A partially received file: the staging temporary being written (through a
/// digesting writer, so that verification costs nothing beyond the write it
/// already performs) and the base its delta applies against.
struct ReceiveFile {
    /// The temporary being written.
    temporary: PathBuf,
    /// The digesting writer over the temporary.
    writer: DigestingWriter<File>,
    /// The base content for block operations.
    base: PatchBase,
}

impl ReceiveFile {
    /// Discards the partially received content, best-effort.
    fn discard(self) {
        let temporary = self.temporary;
        drop(self.writer);
        let _ = fs::remove_file(temporary);
    }
}

/// The base content that delta operations are applied against: the current
/// content at a need's path, or an empty stream when the path holds nothing
/// usable.
enum PatchBase {
    /// An open regular file.
    File(File),
    /// An empty base.
    Empty(Cursor<&'static [u8]>),
}

impl PatchBase {
    /// Creates an empty base.
    fn empty() -> PatchBase {
        PatchBase::Empty(Cursor::new(b""))
    }
}

impl Read for PatchBase {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            PatchBase::File(file) => file.read(buffer),
            PatchBase::Empty(cursor) => cursor.read(buffer),
        }
    }
}

impl Seek for PatchBase {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            PatchBase::File(file) => file.seek(position),
            PatchBase::Empty(cursor) => cursor.seek(position),
        }
    }
}

/// A writer that digests everything it passes through, so that received
/// content is verified without a second pass over it.
struct DigestingWriter<W: Write> {
    /// The underlying writer.
    inner: W,
    /// The digest of everything written so far.
    hasher: blake3::Hasher,
}

impl<W: Write> DigestingWriter<W> {
    /// Wraps a writer in a digester.
    fn new(inner: W) -> DigestingWriter<W> {
        DigestingWriter {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    /// Returns the digest of everything written so far.
    fn digest(&self) -> Digest {
        *self.hasher.finalize().as_bytes()
    }
}

impl<W: Write> Write for DigestingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // Only the bytes the underlying writer accepted are digested, so a
        // short write can't desynchronize the digest from the content.
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The state of one transition operation: the endpoint's paths and scanned
/// record, plus the problems and flags accumulated across changes.
struct Transitioner<'a> {
    /// The synchronization root.
    root: &'a Path,
    /// The staging directory holding content to be applied.
    staging_root: &'a Path,
    /// The last scan's hierarchy, which all validation is performed against.
    scanned: Option<&'a Node>,
    /// The behavior of the root's filesystem, governing how on-disk names
    /// are matched against the hierarchy's (NFC, case-exact) names.
    behavior: FilesystemBehavior,
    /// The treatment of symbolic links.
    symlink_mode: SymlinkMode,
    /// The permission bits for created non-executable files.
    file_mode: u32,
    /// The permission bits for created directories.
    directory_mode: u32,
    /// The problems accumulated so far.
    problems: Vec<Problem>,
    /// Whether or not any staged content was found missing.
    missing_staged_files: bool,
}

impl Transitioner<'_> {
    /// Records a problem at a root-relative path.
    fn problem(&mut self, path: &str, message: impl Into<String>) {
        self.problems.push(Problem {
            path: path.to_owned(),
            message: message.into(),
        });
    }

    /// Applies one change, returning the content actually achieved at its
    /// path: the target content on success, the surviving old content on
    /// refusal, or a partial hierarchy where only part of the change landed.
    fn apply(&mut self, change: &Change) -> Option<Node> {
        // Paths arrive from a peer, so they're checked before they're allowed
        // anywhere near the filesystem. Scanning can't produce a component
        // that escapes the root, so a path that contains one is either a
        // defect or an attack; either way it isn't acted upon.
        if let Err(message) = validate_path(&change.path) {
            self.problem(
                &change.path,
                format!("refusing to act on this path: {message}"),
            );
            return sanitize(change.old.clone());
        }
        let result = match (&change.old, &change.new) {
            (None, None) => None,
            (None, Some(new)) => self.create_change(&change.path, new),
            (Some(old), None) => self.remove_change(&change.path, old),
            (Some(old), Some(new)) => self.replace_change(&change.path, old, new),
        };
        sanitize(result)
    }

    /// Resolves the on-disk directory containing `path`, descending only real
    /// directories: every component is verified with `symlink_metadata`, so a
    /// symbolic link anywhere along the way is a refusal rather than a
    /// redirection.
    fn resolve_parent<'p>(&mut self, path: &'p str) -> Option<(PathBuf, &'p str)> {
        let (parent, name) = match path.rfind('/') {
            Some(index) => (&path[..index], &path[index + 1..]),
            None => ("", path),
        };
        let mut current = self.root.to_path_buf();
        if let Err(message) = verify_directory(&current) {
            self.problem(path, format!("unable to resolve path: {message}"));
            return None;
        }
        if !parent.is_empty() {
            for component in parent.split('/') {
                current.push(component);
                if let Err(message) = verify_directory(&current) {
                    self.problem(path, format!("unable to resolve path: {message}"));
                    return None;
                }
            }
        }
        Some((current, name))
    }

    /// Returns the node the last scan recorded at a root-relative path.
    fn scanned_node(&self, path: &str) -> Option<&Node> {
        let mut current = self.scanned?;
        if path.is_empty() {
            return Some(current);
        }
        for component in path.split('/') {
            current = current.child(component)?;
        }
        Some(current)
    }

    /// Validates that the content on disk at `path` is the regular file the
    /// last scan recorded there, and that the scan recorded the expected
    /// content. This is the check that stands between a stale transition and
    /// somebody else's data: the digest ties the file to what reconciliation
    /// decided about, and the metadata ties that decision to content that
    /// hasn't moved since.
    fn validate_file(
        &self,
        path: &str,
        metadata: &Metadata,
        expected: &Digest,
    ) -> Result<(), String> {
        if !metadata.file_type().is_file() {
            return Err("expected a regular file, but found other content".into());
        }
        let Some(node) = self.scanned_node(path) else {
            return Err("the last scan recorded no content at this path".into());
        };
        let Content::File {
            digest,
            metadata: recorded,
            ..
        } = &node.content
        else {
            return Err("the last scan did not record a regular file at this path".into());
        };
        if digest != expected {
            return Err("the content differs from the expected content".into());
        }
        if file_metadata(metadata) != *recorded {
            return Err("the file has been modified since the last scan".into());
        }
        Ok(())
    }

    /// Applies a creation.
    fn create_change(&mut self, path: &str, new: &Node) -> Option<Node> {
        // Creating the synchronization root itself: the root has no parent to
        // resolve within, and it's the one path where creating over an
        // existing (empty) directory is expected rather than suspicious.
        if path.is_empty() {
            let Content::Directory(children) = &new.content else {
                self.problem(
                    path,
                    "refusing to create a non-directory synchronization root",
                );
                return None;
            };
            let root = self.root;
            if let Err(error) = fs::create_dir_all(root) {
                self.problem(
                    path,
                    format!("unable to create the synchronization root: {error}"),
                );
                return None;
            }
            let created = self.create_children(path, root, children);
            return Some(Node::directory(new.name.clone(), created));
        }

        let (parent, name) = self.resolve_parent(path)?;
        // Creating over existing content would destroy something nobody
        // asked to destroy: the change carries no expectation about what's
        // there, so there's nothing to validate it against.
        if fs::symlink_metadata(parent.join(name)).is_ok() {
            self.problem(path, "refusing to create over existing content");
            return None;
        }
        self.create_node(path, &parent, name, new)
    }

    /// Creates one node inside an already-resolved directory, recursing into
    /// directory contents. The returned node describes what was actually
    /// created, which for a partially created directory is a partial
    /// hierarchy.
    fn create_node(&mut self, path: &str, parent: &Path, name: &str, node: &Node) -> Option<Node> {
        let target = parent.join(name);
        match &node.content {
            Content::Directory(children) => {
                if let Err(error) = fs::create_dir(&target) {
                    self.problem(path, format!("unable to create directory: {error}"));
                    return None;
                }
                if let Err(error) =
                    fs::set_permissions(&target, Permissions::from_mode(self.directory_mode))
                {
                    // The directory exists and is usable; only its mode is
                    // off, so this is reported without abandoning its
                    // contents.
                    self.problem(
                        path,
                        format!("unable to set directory permissions: {error}"),
                    );
                }
                let created = self.create_children(path, &target, children);
                Some(Node::directory(name, created))
            }
            Content::File {
                digest, executable, ..
            } => {
                let metadata = self.publish_file(path, parent, &target, digest, *executable)?;
                Some(Node {
                    name: name.to_owned(),
                    content: Content::File {
                        digest: *digest,
                        executable: *executable,
                        metadata,
                    },
                })
            }
            Content::Symlink { target: link } => {
                // Symlink policy is enforced on creation as well as at scan
                // time: content arriving from a peer must satisfy the same
                // rules this endpoint's own scans would apply.
                match self.symlink_mode {
                    SymlinkMode::Ignore => {
                        self.problem(
                            path,
                            "refusing to create a symbolic link: symbolic links are ignored \
                             by configuration",
                        );
                        return None;
                    }
                    SymlinkMode::Portable => {
                        if let Err(message) = validate_portable_target(path, link) {
                            self.problem(
                                path,
                                format!("refusing to create a symbolic link: {message}"),
                            );
                            return None;
                        }
                    }
                    SymlinkMode::Raw => {}
                }
                if let Err(error) = symlink(link, &target) {
                    self.problem(path, format!("unable to create symbolic link: {error}"));
                    return None;
                }
                Some(Node {
                    name: name.to_owned(),
                    content: node.content.clone(),
                })
            }
            Content::Untracked | Content::Problematic { .. } => {
                self.problem(path, "refusing to create unsynchronizable content");
                None
            }
        }
    }

    /// Creates a directory's children, returning those that were actually
    /// created. A child that can't be created is reported and skipped, so
    /// that its siblings still land.
    fn create_children(&mut self, path: &str, directory: &Path, children: &[Node]) -> Vec<Node> {
        let mut created = Vec::with_capacity(children.len());
        // On a case-insensitive volume, sibling names that differ only by
        // case denote a single on-disk entry; creating the second would
        // corrupt the first, so it's refused up front.
        let mut folded: HashMap<String, ()> = HashMap::new();
        for child in children {
            let child_path = path_join(path, &child.name);
            if let Err(message) = validate_name(&child.name) {
                self.problem(
                    &child_path,
                    format!("refusing to create this name: {message}"),
                );
                continue;
            }
            if self.behavior.case_insensitive
                && folded.insert(child.name.to_lowercase(), ()).is_some()
            {
                self.problem(
                    &child_path,
                    "refusing to create this entry: its name collides with a sibling's on \
                     this case-insensitive filesystem",
                );
                continue;
            }
            if let Some(node) = self.create_node(&child_path, directory, &child.name, child) {
                created.push(node);
            }
        }
        created
    }

    /// Publishes staged content at a target path: copy the staged file to a
    /// temporary beside the target, set its permissions, then rename it into
    /// place. Copying (rather than moving) keeps the staged content available
    /// for other paths that share it, and the rename makes the target's
    /// transition from old content to new atomic.
    fn publish_file(
        &mut self,
        path: &str,
        parent: &Path,
        target: &Path,
        digest: &Digest,
        executable: bool,
    ) -> Option<FileMetadata> {
        let staged = staged_path(self.staging_root, digest);
        if fs::symlink_metadata(&staged).is_err() {
            // The content was staged (or should have been) and has since
            // vanished, or the source couldn't supply it. Either way the
            // controller runs another cycle immediately.
            self.missing_staged_files = true;
            self.problem(
                path,
                "staged content is unavailable; it will be retransferred on the next cycle",
            );
            return None;
        }

        let temporary = parent.join(temporary_name("apply"));
        if let Err(error) = fs::copy(&staged, &temporary) {
            let _ = fs::remove_file(&temporary);
            self.problem(path, format!("unable to stage content into place: {error}"));
            return None;
        }
        let mode = creation_mode(self.file_mode, executable);
        if let Err(error) = fs::set_permissions(&temporary, Permissions::from_mode(mode)) {
            let _ = fs::remove_file(&temporary);
            self.problem(path, format!("unable to set file permissions: {error}"));
            return None;
        }
        if let Err(error) = fs::rename(&temporary, target) {
            let _ = fs::remove_file(&temporary);
            self.problem(path, format!("unable to publish content: {error}"));
            return None;
        }

        // The metadata recorded on the result node comes from the file as it
        // now exists, so that the ancestor (and any scan warmed by it)
        // describes reality rather than intent.
        match fs::symlink_metadata(target) {
            Ok(metadata) => Some(file_metadata(&metadata)),
            Err(error) => {
                self.problem(path, format!("unable to probe the created file: {error}"));
                Some(FileMetadata::default())
            }
        }
    }

    /// Applies a removal.
    fn remove_change(&mut self, path: &str, expectation: &Node) -> Option<Node> {
        if path.is_empty() {
            // The session layer halts before a root deletion reaches an
            // endpoint; this is the second lock on the same door.
            self.problem(path, "refusing to remove the synchronization root");
            return Some(expectation.clone());
        }
        let Some((parent, name)) = self.resolve_parent(path) else {
            return Some(expectation.clone());
        };
        self.remove_entry(path, &parent.join(name), expectation)
    }

    /// Removes one entry, validating it against the last scan first and
    /// recursing bottom-up through directories. Returns the content that
    /// survived: `None` when the entry is gone, and a partial hierarchy when
    /// some of it had to be left in place.
    fn remove_entry(&mut self, path: &str, target: &Path, expectation: &Node) -> Option<Node> {
        let metadata = match fs::symlink_metadata(target) {
            Ok(metadata) => metadata,
            // Already absent: the intended state, reached by other means.
            Err(error) if error.kind() == ErrorKind::NotFound => return None,
            Err(error) => {
                self.problem(path, format!("unable to probe content: {error}"));
                return Some(expectation.clone());
            }
        };

        match &expectation.content {
            Content::File { digest, .. } => {
                if let Err(message) = self.validate_file(path, &metadata, digest) {
                    self.problem(path, format!("refusing to remove this file: {message}"));
                    return Some(expectation.clone());
                }
                match fs::remove_file(target) {
                    Ok(()) => None,
                    Err(error) => {
                        self.problem(path, format!("unable to remove file: {error}"));
                        Some(expectation.clone())
                    }
                }
            }
            Content::Symlink { target: expected } => {
                if !metadata.file_type().is_symlink() {
                    self.problem(
                        path,
                        "refusing to remove this entry: expected a symbolic link, but found other content",
                    );
                    return Some(expectation.clone());
                }
                match fs::read_link(target) {
                    Ok(actual) if actual.to_str() == Some(expected.as_str()) => {}
                    Ok(_) => {
                        self.problem(
                            path,
                            "refusing to remove this symbolic link: it has been retargeted since the last scan",
                        );
                        return Some(expectation.clone());
                    }
                    Err(error) => {
                        self.problem(path, format!("unable to read symbolic link: {error}"));
                        return Some(expectation.clone());
                    }
                }
                match fs::remove_file(target) {
                    Ok(()) => None,
                    Err(error) => {
                        self.problem(path, format!("unable to remove symbolic link: {error}"));
                        Some(expectation.clone())
                    }
                }
            }
            Content::Directory(_) => self.remove_directory(path, target, &metadata, expectation),
            Content::Untracked | Content::Problematic { .. } => {
                self.problem(path, "refusing to remove unsynchronizable content");
                Some(expectation.clone())
            }
        }
    }

    /// Removes a directory bottom-up. Every entry present on disk must be
    /// accounted for by the expectation: content that reconciliation never
    /// saw is content nobody decided to delete, so it's left in place (which
    /// necessarily leaves its parents in place too).
    fn remove_directory(
        &mut self,
        path: &str,
        target: &Path,
        metadata: &Metadata,
        expectation: &Node,
    ) -> Option<Node> {
        if !metadata.file_type().is_dir() {
            self.problem(
                path,
                "refusing to remove this entry: expected a directory, but found other content",
            );
            return Some(expectation.clone());
        }
        let entries = match fs::read_dir(target) {
            Ok(entries) => entries,
            Err(error) => {
                self.problem(path, format!("unable to list directory: {error}"));
                return Some(expectation.clone());
            }
        };

        let mut survivors = Vec::new();
        let mut unexpected = false;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.problem(path, format!("unable to list directory: {error}"));
                    unexpected = true;
                    continue;
                }
            };
            let raw_name = entry.file_name();
            let Some(name) = raw_name.to_str() else {
                // Scanning records non-UTF-8 names under a marked, lossy name
                // that can't be matched back to a directory entry, so such an
                // entry is never something this removal expected.
                self.problem(
                    path,
                    "refusing to remove this directory: it contains an entry whose name is not valid UTF-8",
                );
                unexpected = true;
                continue;
            };
            // On a decomposing volume the on-disk name is NFD while the
            // expectation (like every hierarchy name) is NFC; recompose
            // before matching, or every non-ASCII name would read as
            // unexpected content.
            let name = if self.behavior.decomposes_unicode {
                recompose(name)
            } else {
                name.to_owned()
            };
            let name = name.as_str();
            let child_path = path_join(path, name);
            match expectation.child(name) {
                Some(child) => {
                    if let Some(survivor) = self.remove_entry(&child_path, &entry.path(), child) {
                        survivors.push(survivor);
                    }
                }
                None => {
                    self.problem(
                        &child_path,
                        "refusing to remove unexpected content that appeared since the last scan",
                    );
                    unexpected = true;
                }
            }
        }

        // Anything known to have survived keeps the directory itself alive.
        if !survivors.is_empty() || unexpected {
            return Some(Node::directory(expectation.name.clone(), survivors));
        }
        match fs::remove_dir(target) {
            Ok(()) => None,
            Err(error) => {
                // The directory acquired content between the listing and the
                // removal; `remove_dir` refuses to recurse, so nothing was
                // lost.
                self.problem(path, format!("unable to remove directory: {error}"));
                Some(Node::directory(expectation.name.clone(), Vec::new()))
            }
        }
    }

    /// Applies a replacement.
    fn replace_change(&mut self, path: &str, old: &Node, new: &Node) -> Option<Node> {
        if path.is_empty() {
            // A root replacement would require removing the root, which is
            // never permitted. Reconciliation can't produce one (both sides'
            // roots are directories, which agree shallowly), so this is a
            // guard rather than a case.
            self.problem(
                path,
                "refusing to replace the synchronization root, which would require removing it",
            );
            return Some(old.clone());
        }
        let Some((parent, name)) = self.resolve_parent(path) else {
            return Some(old.clone());
        };
        let target = parent.join(name);

        // File-to-file replacements are performed in place, which is both
        // faster and safer than a removal followed by a creation: the path
        // never transiently ceases to exist.
        if let (
            Content::File {
                digest: old_digest, ..
            },
            Content::File {
                digest: new_digest,
                executable,
                ..
            },
        ) = (&old.content, &new.content)
        {
            let metadata = match fs::symlink_metadata(&target) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.problem(path, format!("unable to probe content: {error}"));
                    return Some(old.clone());
                }
            };
            if let Err(message) = self.validate_file(path, &metadata, old_digest) {
                self.problem(path, format!("refusing to replace this file: {message}"));
                return Some(old.clone());
            }

            if old_digest == new_digest {
                // Only executability differs, so the content is left entirely
                // alone: this is a permission change, not a rewrite.
                let mode = creation_mode(self.file_mode, *executable);
                if let Err(error) = fs::set_permissions(&target, Permissions::from_mode(mode)) {
                    self.problem(path, format!("unable to set file permissions: {error}"));
                    return Some(old.clone());
                }
                let metadata = match fs::symlink_metadata(&target) {
                    Ok(metadata) => file_metadata(&metadata),
                    Err(error) => {
                        self.problem(path, format!("unable to probe the modified file: {error}"));
                        FileMetadata::default()
                    }
                };
                return Some(Node {
                    name: name.to_owned(),
                    content: Content::File {
                        digest: *new_digest,
                        executable: *executable,
                        metadata,
                    },
                });
            }

            let Some(metadata) = self.publish_file(path, &parent, &target, new_digest, *executable)
            else {
                return Some(old.clone());
            };
            return Some(Node {
                name: name.to_owned(),
                content: Content::File {
                    digest: *new_digest,
                    executable: *executable,
                    metadata,
                },
            });
        }

        // Everything else (type changes, and anything involving a directory)
        // is a validated removal followed by a creation. If the removal
        // refuses, the creation must not proceed — the old content is still
        // there.
        if let Some(survivor) = self.remove_entry(path, &target, old) {
            self.problem(
                path,
                "refusing to create replacement content: the existing content could not be removed",
            );
            return Some(survivor);
        }
        self.create_node(path, &parent, name, new)
    }
}

/// Filters unsynchronizable content out of a transition result.
///
/// Results become ancestor content, and the ancestor may only ever contain
/// synchronizable content — the session validates this and treats a violation
/// as fatal to the whole cycle. Reconciliation never puts unsynchronizable
/// content into a transition's expectation, so this is a guard rather than a
/// transformation; it exists so that a defect anywhere upstream degrades to a
/// path the ancestor simply doesn't describe, rather than to a failed cycle.
fn sanitize(result: Option<Node>) -> Option<Node> {
    result.as_ref().and_then(Node::synchronizable_subtree)
}

/// Computes the permission bits for a created file: the configured file
/// mode, with executability granted (where readability already is) for
/// executable files.
fn creation_mode(file_mode: u32, executable: bool) -> u32 {
    if executable {
        file_mode | ((file_mode & 0o444) >> 2)
    } else {
        file_mode
    }
}

/// Returns the staging path for content with the specified digest.
fn staged_path(staging_root: &Path, digest: &Digest) -> PathBuf {
    use std::fmt::Write;
    let mut name = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(name, "{byte:02x}");
    }
    staging_root.join(name)
}

/// Generates a unique temporary file name carrying the scan-invisible prefix.
/// Names are unique within a process and, through the process identifier,
/// between concurrent processes sharing a staging directory.
fn temporary_name(purpose: &str) -> String {
    let count = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{TEMPORARY_PREFIX}-{purpose}-{}-{count}",
        std::process::id()
    )
}

/// Streams a file into a temporary while digesting it, returning whether the
/// content matched the expected digest. A mismatch means the file changed
/// since the scan that indexed its content.
fn copy_verifying(source: &Path, temporary: &Path, digest: &Digest) -> Result<bool> {
    let mut input =
        File::open(source).with_context(|| format!("unable to open {}", source.display()))?;
    let mut output = File::create(temporary)
        .with_context(|| format!("unable to create {}", temporary.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    loop {
        let count = input
            .read(&mut buffer)
            .with_context(|| format!("unable to read {}", source.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .with_context(|| format!("unable to write {}", temporary.display()))?;
    }
    output
        .flush()
        .with_context(|| format!("unable to flush {}", temporary.display()))?;
    Ok(hasher.finalize().as_bytes() == digest)
}

/// Computes the rsync signature of whatever base content exists at a path.
///
/// Anything other than a readable regular file yields an empty signature,
/// which is exactly right: with no usable base, delta generation degenerates
/// to streaming the content, and correctness never depends on the base being
/// what the destination expected.
fn base_signature(path: &Path) -> Signature {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Signature::default();
    };
    if !metadata.file_type().is_file() {
        return Signature::default();
    }
    let Ok(file) = File::open(path) else {
        return Signature::default();
    };
    rsync::signature(file, rsync::optimal_block_size(metadata.len())).unwrap_or_default()
}

/// Verifies that a path is a real directory, without following symbolic
/// links.
fn verify_directory(path: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    Ok(())
}

/// Extracts the metadata recorded on file nodes, matching what the scanner
/// records so that the two can be compared directly.
fn file_metadata(metadata: &Metadata) -> FileMetadata {
    FileMetadata {
        mtime_seconds: metadata.mtime(),
        mtime_nanos: metadata.mtime_nsec() as u32,
        size: metadata.size(),
        inode: metadata.ino(),
        mode: metadata.mode(),
    }
}

/// Validates a single path component: it must be a name that scanning could
/// have produced, which in particular excludes anything that would escape the
/// synchronization root.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty path component".into());
    }
    if name == "." || name == ".." {
        return Err("dot path component".into());
    }
    if name.contains('/') || name.contains('\0') {
        return Err("path component contains a separator or NUL".into());
    }
    Ok(())
}

/// Validates a root-relative path component by component. The empty path (the
/// synchronization root itself) is valid.
fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    for component in path.split('/') {
        validate_name(component)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::symlink;
    use tempfile::{tempdir, TempDir};

    use crate::rsync::Op;
    use crate::session::transition_dependencies;
    use crate::tree::diff;

    /// A pair of endpoints over two roots, with isolated staging.
    struct Fixture {
        _keep: TempDir,
        alpha_root: PathBuf,
        beta_root: PathBuf,
        alpha: LocalEndpoint,
        beta: LocalEndpoint,
    }

    impl Fixture {
        fn new() -> Fixture {
            let keep = tempdir().expect("temporary directory should be creatable");
            let alpha_root = keep.path().join("alpha");
            let beta_root = keep.path().join("beta");
            fs::create_dir_all(&alpha_root).expect("alpha root should be creatable");
            fs::create_dir_all(&beta_root).expect("beta root should be creatable");
            let alpha = endpoint(&alpha_root, &keep.path().join("staging-alpha"));
            let beta = endpoint(&beta_root, &keep.path().join("staging-beta"));
            Fixture {
                _keep: keep,
                alpha_root,
                beta_root,
                alpha,
                beta,
            }
        }

        /// Scans both endpoints and returns the changes that would bring beta
        /// into agreement with alpha.
        fn beta_transitions(&mut self) -> Vec<Change> {
            let alpha = self.alpha.scan().expect("alpha scan should succeed");
            let beta = self.beta.scan().expect("beta scan should succeed");
            diff(beta.root.as_ref(), alpha.root.as_ref())
        }

        /// Drives a complete staging exchange from alpha into beta, exactly
        /// as the session controller does (with a deliberately small batch
        /// size, so that per-file buffers are drained across several pulls).
        fn stage(&mut self, transitions: &[Change]) -> Vec<StagingNeed> {
            let requests = transition_dependencies(transitions);
            let needs = self
                .beta
                .stage_begin(requests)
                .expect("staging should begin");
            if needs.is_empty() {
                return needs;
            }
            self.alpha
                .supply_open(needs.clone())
                .expect("supply should open");
            loop {
                let frames = self.alpha.supply_pull(3).expect("supply should pull");
                if frames.is_empty() {
                    break;
                }
                self.beta.stage_push(frames).expect("staging should accept");
            }
            needs
        }
    }

    fn endpoint(root: &Path, staging: &Path) -> LocalEndpoint {
        LocalEndpoint::new(
            root.to_path_buf(),
            staging.to_path_buf(),
            EndpointOptions::default(),
        )
        .expect("endpoint should be creatable")
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("parent should be creatable");
        }
        fs::write(&full, contents).expect("file should be writable");
    }

    fn read(root: &Path, path: &str) -> String {
        fs::read_to_string(root.join(path)).expect("file should be readable")
    }

    fn executable(root: &Path, path: &str) -> bool {
        fs::symlink_metadata(root.join(path))
            .expect("file should exist")
            .mode()
            & 0o111
            != 0
    }

    fn inode(root: &Path, path: &str) -> u64 {
        fs::symlink_metadata(root.join(path))
            .expect("file should exist")
            .ino()
    }

    /// Returns the node at a root-relative path within a snapshot.
    fn node_at(snapshot: &Snapshot, path: &str) -> Node {
        let mut current = snapshot.root.as_ref().expect("root should exist");
        if !path.is_empty() {
            for component in path.split('/') {
                current = current
                    .child(component)
                    .unwrap_or_else(|| panic!("{path} should exist in the snapshot"));
            }
        }
        current.clone()
    }

    /// Generates deterministic pseudo-random content.
    fn pseudo_random(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut data = Vec::with_capacity(length + 8);
        while data.len() < length {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.extend_from_slice(&state.to_le_bytes());
        }
        data.truncate(length);
        data
    }

    #[test]
    fn staging_and_transition_round_trip() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "top.txt", "top level");
        write(&fixture.alpha_root, "dir/inner.txt", "inner content");
        write(&fixture.alpha_root, "dir/tool.sh", "#!/bin/sh\n");
        write(&fixture.alpha_root, "dir/empty.txt", "");
        fs::set_permissions(
            fixture.alpha_root.join("dir/tool.sh"),
            Permissions::from_mode(0o755),
        )
        .expect("permissions should be settable");
        symlink("inner.txt", fixture.alpha_root.join("dir/link"))
            .expect("symlink should be creatable");
        fs::create_dir_all(fixture.alpha_root.join("empty"))
            .expect("directory should be creatable");

        let transitions = fixture.beta_transitions();
        let needs = fixture.stage(&transitions);
        // Every file needs transferring: beta is empty, so nothing can be
        // satisfied locally. (The empty file is a need too, and arrives as a
        // bare end-of-file frame.)
        assert_eq!(needs.len(), 4);
        assert!(needs.iter().all(|need| need.signature.is_empty()));

        let outcome = fixture
            .beta
            .transition(transitions.clone())
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(!outcome.missing_staged_files);
        assert_eq!(outcome.results.len(), transitions.len());
        assert!(outcome.results.iter().all(Option::is_some));

        // The destination now matches the source, byte for byte.
        assert_eq!(read(&fixture.beta_root, "top.txt"), "top level");
        assert_eq!(read(&fixture.beta_root, "dir/inner.txt"), "inner content");
        assert_eq!(read(&fixture.beta_root, "dir/empty.txt"), "");
        assert!(executable(&fixture.beta_root, "dir/tool.sh"));
        assert!(!executable(&fixture.beta_root, "dir/inner.txt"));
        assert_eq!(
            fs::read_link(fixture.beta_root.join("dir/link")).expect("link should be readable"),
            Path::new("inner.txt")
        );
        assert!(fixture.beta_root.join("empty").is_dir());

        // A rescan of beta agrees with alpha's hierarchy, and a further
        // reconciliation has nothing left to do.
        let further = fixture.beta_transitions();
        assert!(further.is_empty(), "{further:?}");
    }

    #[test]
    fn missing_root_is_created_by_transition() {
        let mut fixture = Fixture::new();
        fs::remove_dir_all(&fixture.beta_root).expect("beta root should be removable");
        write(&fixture.alpha_root, "dir/inner.txt", "inner");

        let transitions = fixture.beta_transitions();
        assert_eq!(transitions.len(), 1);
        assert!(transitions[0].path.is_empty());
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "dir/inner.txt"), "inner");
    }

    #[test]
    fn delta_transfer_reuses_the_destination_base() {
        let mut fixture = Fixture::new();
        let shared = pseudo_random(200_000, 0x1234);
        let mut alpha_content = shared.clone();
        alpha_content.extend_from_slice(b"alpha tail");
        let mut beta_content = shared.clone();
        beta_content.extend_from_slice(b"beta tail, which differs");
        fs::write(fixture.alpha_root.join("big.bin"), &alpha_content)
            .expect("file should be writable");
        fs::write(fixture.beta_root.join("big.bin"), &beta_content)
            .expect("file should be writable");

        let transitions = fixture.beta_transitions();
        assert_eq!(transitions.len(), 1);
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        assert_eq!(needs.len(), 1);
        // The destination's existing content is described, so the transfer
        // can be a delta rather than a copy.
        assert!(!needs[0].signature.is_empty());
        assert!(!needs[0].signature.hashes.is_empty());

        fixture
            .alpha
            .supply_open(needs)
            .expect("supply should open");
        let mut blocks = 0u64;
        let mut data = 0usize;
        loop {
            let frames = fixture.alpha.supply_pull(3).expect("supply should pull");
            if frames.is_empty() {
                break;
            }
            assert!(frames.len() <= 3);
            for frame in &frames {
                match frame {
                    TransferFrame::Op(Op::Blocks { count, .. }) => blocks += count,
                    TransferFrame::Op(Op::Data(bytes)) => data += bytes.len(),
                    TransferFrame::EndOfFile { error } => assert!(error.is_none()),
                }
            }
            fixture
                .beta
                .stage_push(frames)
                .expect("staging should accept");
        }
        // The shared prefix transferred as block references, not as data.
        assert!(blocks > 0, "expected block reuse");
        assert!(data < shared.len() / 2, "expected a small literal payload");

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(
            fs::read(fixture.beta_root.join("big.bin")).expect("file should be readable"),
            alpha_content
        );
    }

    #[test]
    fn identical_content_elsewhere_is_staged_locally() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "new/copy.txt", "shared content");
        write(&fixture.alpha_root, "original.txt", "shared content");
        write(&fixture.beta_root, "original.txt", "shared content");

        let transitions = fixture.beta_transitions();
        let requests = transition_dependencies(&transitions);
        assert_eq!(requests.len(), 1);
        let digest = requests[0].digest;
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        // The content already exists in beta's root, so nothing is needed
        // from alpha at all.
        assert!(needs.is_empty(), "{needs:?}");
        assert!(fixture.beta.staged_path(&digest).exists());

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "new/copy.txt"), "shared content");
    }

    #[test]
    fn refuses_to_remove_a_file_modified_since_the_scan() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "keep.txt", "original content");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "keep.txt");

        // The file changes after the scan that the transition was reconciled
        // from, which is precisely the race the validation exists for.
        write(&fixture.beta_root, "keep.txt", "content changed underneath");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "keep.txt".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");

        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].path, "keep.txt");
        assert!(
            outcome.problems[0]
                .message
                .contains("modified since the last scan"),
            "{}",
            outcome.problems[0].message
        );
        // The content survives, and the result reflects that.
        assert_eq!(
            read(&fixture.beta_root, "keep.txt"),
            "content changed underneath"
        );
        assert!(outcome.results[0].is_some());
    }

    #[test]
    fn refuses_to_remove_a_directory_containing_unexpected_content() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "dir/known.txt", "known");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "dir");

        // Content that reconciliation never saw appears after the scan.
        write(&fixture.beta_root, "dir/extra.txt", "created concurrently");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "dir".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");

        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].path, "dir/extra.txt");
        assert!(
            outcome.problems[0].message.contains("unexpected content"),
            "{}",
            outcome.problems[0].message
        );
        // The unexpected file and its directory both survive; the expected
        // child is gone, and the result says so.
        assert!(fixture.beta_root.join("dir").is_dir());
        assert_eq!(
            read(&fixture.beta_root, "dir/extra.txt"),
            "created concurrently"
        );
        assert!(!fixture.beta_root.join("dir/known.txt").exists());
        let result = outcome.results[0].as_ref().expect("the directory survives");
        assert!(matches!(result.content, Content::Directory(_)));
        assert!(result.children().is_empty());
    }

    #[test]
    fn executability_only_changes_are_applied_in_place() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "tool.sh", "#!/bin/sh\n");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let old = node_at(&snapshot, "tool.sh");
        let before = inode(&fixture.beta_root, "tool.sh");

        let Content::File {
            digest, metadata, ..
        } = old.content
        else {
            panic!("expected a file");
        };
        let new = Node {
            name: "tool.sh".into(),
            content: Content::File {
                digest,
                executable: true,
                metadata,
            },
        };
        let old = Node {
            name: "tool.sh".into(),
            content: Content::File {
                digest,
                executable: false,
                metadata,
            },
        };

        // A pure executability change carries no content dependency at all.
        let transitions = vec![Change {
            path: "tool.sh".into(),
            old: Some(old),
            new: Some(new),
        }];
        assert!(transition_dependencies(&transitions).is_empty());

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(executable(&fixture.beta_root, "tool.sh"));
        assert_eq!(read(&fixture.beta_root, "tool.sh"), "#!/bin/sh\n");
        // The inode proves the file was chmod'ed rather than rewritten.
        assert_eq!(inode(&fixture.beta_root, "tool.sh"), before);
    }

    #[test]
    fn file_content_replacement_validates_the_old_content() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "file.txt", "alpha content");
        write(&fixture.beta_root, "file.txt", "beta content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);

        // A concurrent modification between staging and transitioning must
        // stop the replacement.
        write(&fixture.beta_root, "file.txt", "beta content, edited again");
        let outcome = fixture
            .beta
            .transition(transitions.clone())
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(
            outcome.problems[0].message.contains("refusing to replace"),
            "{}",
            outcome.problems[0].message
        );
        assert_eq!(
            read(&fixture.beta_root, "file.txt"),
            "beta content, edited again"
        );

        // Rescanning legitimizes the current content, after which the same
        // transition (whose expectation now matches) applies.
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "file.txt"), "alpha content");
    }

    #[test]
    fn missing_staged_content_is_reported_and_creation_is_partial() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "dir/present.txt", "present");
        write(&fixture.alpha_root, "dir/absent.txt", "absent");
        let transitions = fixture.beta_transitions();
        let requests = transition_dependencies(&transitions);
        fixture.stage(&transitions);

        // Remove one file's staged content, simulating content that vanished
        // (or was never supplied) between staging and transitioning.
        let absent = requests
            .iter()
            .find(|request| request.path == "dir/absent.txt")
            .expect("the request should exist");
        fs::remove_file(fixture.beta.staged_path(&absent.digest))
            .expect("staged content should be removable");

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.missing_staged_files);
        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].path, "dir/absent.txt");

        // The rest of the directory was still created, and the result
        // describes exactly what landed.
        assert_eq!(read(&fixture.beta_root, "dir/present.txt"), "present");
        assert!(!fixture.beta_root.join("dir/absent.txt").exists());
        let result = outcome.results[0]
            .as_ref()
            .expect("the directory should have been created");
        let names: Vec<&str> = result.children().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["present.txt"]);
    }

    #[test]
    fn refuses_to_create_over_existing_content() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "file.txt", "alpha content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);

        // Content appears at the target path after reconciliation decided
        // there was nothing there.
        write(&fixture.beta_root, "file.txt", "appeared concurrently");
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(
            outcome.problems[0]
                .message
                .contains("refusing to create over"),
            "{}",
            outcome.problems[0].message
        );
        assert!(outcome.results[0].is_none());
        assert_eq!(
            read(&fixture.beta_root, "file.txt"),
            "appeared concurrently"
        );
    }

    #[test]
    fn refuses_root_deletion_and_unsafe_paths() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "file.txt", "content");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let root = node_at(&snapshot, "");
        let file = node_at(&snapshot, "file.txt");

        let outcome = fixture
            .beta
            .transition(vec![
                Change {
                    path: String::new(),
                    old: Some(root),
                    new: None,
                },
                Change {
                    path: "../escape".into(),
                    old: None,
                    new: Some(file),
                },
            ])
            .expect("transition should succeed");

        assert_eq!(outcome.problems.len(), 2);
        assert!(
            outcome.problems[0]
                .message
                .contains("refusing to remove the synchronization root"),
            "{}",
            outcome.problems[0].message
        );
        assert!(
            outcome.problems[1]
                .message
                .contains("refusing to act on this path"),
            "{}",
            outcome.problems[1].message
        );
        assert_eq!(read(&fixture.beta_root, "file.txt"), "content");
        assert!(outcome.results[0].is_some());
    }

    #[test]
    fn symbolic_links_are_never_followed_when_removing() {
        let mut fixture = Fixture::new();
        // A file outside the root, reachable through a symbolic link inside
        // it. Removing the link must never touch the target.
        let outside = fixture
            .beta_root
            .parent()
            .expect("parent")
            .join("outside.txt");
        fs::write(&outside, "outside content").expect("file should be writable");
        symlink(&outside, fixture.beta_root.join("link")).expect("symlink should be creatable");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "link");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "link".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(outcome.results[0].is_none());
        assert!(!fixture.beta_root.join("link").exists());
        assert_eq!(
            fs::read_to_string(&outside).expect("the target should survive"),
            "outside content"
        );
    }

    #[test]
    fn a_retargeted_symbolic_link_is_not_removed() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "a.txt", "a");
        write(&fixture.beta_root, "b.txt", "b");
        symlink("a.txt", fixture.beta_root.join("link")).expect("symlink should be creatable");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "link");

        fs::remove_file(fixture.beta_root.join("link")).expect("link should be removable");
        symlink("b.txt", fixture.beta_root.join("link")).expect("symlink should be creatable");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "link".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(
            outcome.problems[0].message.contains("retargeted"),
            "{}",
            outcome.problems[0].message
        );
        assert!(fixture.beta_root.join("link").exists());
    }

    #[test]
    fn supply_reports_unreadable_files_without_failing_the_stream() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "gone.txt", "content");
        write(&fixture.alpha_root, "present.txt", "content that stays");
        let transitions = fixture.beta_transitions();
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        assert_eq!(needs.len(), 2);

        // The file disappears from the source between staging and supply.
        fs::remove_file(fixture.alpha_root.join("gone.txt")).expect("file should be removable");
        fixture
            .alpha
            .supply_open(needs.clone())
            .expect("supply should open");
        let mut errors = 0;
        loop {
            let frames = fixture.alpha.supply_pull(2).expect("supply should pull");
            if frames.is_empty() {
                break;
            }
            for frame in &frames {
                if let TransferFrame::EndOfFile { error: Some(_) } = frame {
                    errors += 1;
                }
            }
            fixture
                .beta
                .stage_push(frames)
                .expect("staging should accept");
        }
        assert_eq!(errors, 1);

        // The surviving file is staged; the vanished one simply isn't, and
        // the transition reports it as missing rather than failing.
        let gone = needs
            .iter()
            .find(|need| need.request.path == "gone.txt")
            .expect("the need should exist");
        let present = needs
            .iter()
            .find(|need| need.request.path == "present.txt")
            .expect("the need should exist");
        assert!(!fixture.beta.staged_path(&gone.request.digest).exists());
        assert!(fixture.beta.staged_path(&present.request.digest).exists());

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.missing_staged_files);
        assert_eq!(
            read(&fixture.beta_root, "present.txt"),
            "content that stays"
        );
    }

    #[test]
    fn staged_content_survives_an_interrupted_cycle() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "file.txt", "content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);

        // A second staging pass (as a fresh cycle would perform) finds the
        // content already staged and asks for nothing.
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        assert!(needs.is_empty(), "{needs:?}");
    }

    #[test]
    fn type_changing_replacements_remove_then_create() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "entry", "now a file");
        write(&fixture.beta_root, "entry/inner.txt", "was a directory");

        let transitions = fixture.beta_transitions();
        assert_eq!(transitions.len(), 1);
        assert!(transitions[0].old.is_some() && transitions[0].new.is_some());
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "entry"), "now a file");
    }

    #[test]
    fn creation_modes_default_conservatively_and_are_configurable() {
        // Defaults: 0600 files, 0700 directories, 0700 executables.
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "dir/plain.txt", "content");
        write(&fixture.alpha_root, "dir/tool.sh", "#!/bin/sh\n");
        fs::set_permissions(
            fixture.alpha_root.join("dir/tool.sh"),
            Permissions::from_mode(0o755),
        )
        .expect("permissions should be settable");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        let mode = |path: &str| {
            fs::symlink_metadata(fixture.beta_root.join(path))
                .expect("entry should exist")
                .mode()
                & 0o777
        };
        assert_eq!(mode("dir"), 0o700);
        assert_eq!(mode("dir/plain.txt"), 0o600);
        assert_eq!(mode("dir/tool.sh"), 0o700);

        // Configured modes: 0644/0755, with executability derived (0755).
        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("beta");
        fs::create_dir_all(&root).expect("root should be creatable");
        let mut beta = LocalEndpoint::new(
            root.clone(),
            keep.path().join("staging"),
            EndpointOptions {
                file_mode: Some(0o644),
                directory_mode: Some(0o755),
                ..EndpointOptions::default()
            },
        )
        .expect("endpoint should be creatable");
        beta.scan().expect("scan should succeed");
        let digest = *blake3::hash(b"content").as_bytes();
        fs::write(beta.staged_path(&digest), b"content").expect("staged content");
        let outcome = beta
            .transition(vec![Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory(
                    "d",
                    vec![Node {
                        name: "run.sh".into(),
                        content: Content::File {
                            digest,
                            executable: true,
                            metadata: FileMetadata::default(),
                        },
                    }],
                )),
            }])
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        let mode = |path: &str| {
            fs::symlink_metadata(root.join(path))
                .expect("entry should exist")
                .mode()
                & 0o777
        };
        assert_eq!(mode("d"), 0o755);
        assert_eq!(mode("d/run.sh"), 0o755);
    }

    #[test]
    fn symlink_policy_is_enforced_at_creation() {
        let mut fixture = Fixture::new();
        fixture.beta.symlink_mode = SymlinkMode::Portable;
        fixture.beta.scan().expect("scan should succeed");
        let link = |name: &str, target: &str| Change {
            path: name.into(),
            old: None,
            new: Some(Node {
                name: name.into(),
                content: Content::Symlink {
                    target: target.into(),
                },
            }),
        };
        let outcome = fixture
            .beta
            .transition(vec![link("good", "file.txt"), link("bad", "/etc/passwd")])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1, "{:?}", outcome.problems);
        assert!(outcome.problems[0].message.contains("absolute"));
        assert!(fixture.beta_root.join("good").is_symlink());
        assert!(!fixture.beta_root.join("bad").is_symlink());

        // Ignore mode refuses symlink creation outright.
        fixture.beta.symlink_mode = SymlinkMode::Ignore;
        let outcome = fixture
            .beta
            .transition(vec![link("also-good", "file.txt")])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(outcome.problems[0]
            .message
            .contains("ignored by configuration"));
    }

    #[test]
    fn decomposed_on_disk_names_match_nfc_expectations() {
        let mut fixture = Fixture::new();
        // NFD on disk simulates a decomposing volume on our byte-preserving
        // test filesystem.
        write(&fixture.beta_root, "dir/cafe\u{0301}.txt", "content");
        fixture.beta.behavior = Some(FilesystemBehavior {
            decomposes_unicode: true,
            ..FilesystemBehavior::default()
        });
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        // The scan records the NFC spelling.
        let expectation = node_at(&snapshot, "dir");
        assert!(expectation.child("caf\u{00E9}.txt").is_some());

        // Removing the directory must match the NFD dirent against the NFC
        // expectation; without recomposition this would refuse with
        // "unexpected content".
        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "dir".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(!fixture.beta_root.join("dir").exists());
    }

    #[test]
    fn case_collisions_are_refused_on_case_insensitive_volumes() {
        let mut fixture = Fixture::new();
        fixture.beta.behavior = Some(FilesystemBehavior {
            case_insensitive: true,
            ..FilesystemBehavior::default()
        });
        fixture.beta.scan().expect("scan should succeed");

        let digest = *blake3::hash(b"content").as_bytes();
        let child = |name: &str| Node {
            name: name.into(),
            content: Content::File {
                digest,
                executable: false,
                metadata: FileMetadata::default(),
            },
        };
        // Stage the content so creation can proceed for the survivor.
        fs::create_dir_all(&fixture.beta.staging_root).expect("staging root");
        fs::write(fixture.beta.staged_path(&digest), b"content").expect("staged content");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory(
                    "d",
                    vec![child("File.txt"), child("file.txt")],
                )),
            }])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1, "{:?}", outcome.problems);
        assert!(
            outcome.problems[0].message.contains("case-insensitive"),
            "{}",
            outcome.problems[0].message
        );
        // Exactly one of the pair landed, and the result says which.
        let result = outcome.results[0].as_ref().expect("directory result");
        assert_eq!(result.children().len(), 1);
    }

    #[test]
    fn await_change_observes_writes() {
        use std::time::{Duration, Instant};

        let mut fixture = Fixture::new();
        // A quiet root waits out the timeout.
        assert!(!fixture
            .alpha
            .await_change(Duration::from_millis(50))
            .expect("await should succeed"));

        // A write arriving mid-wait is observed well before the timeout.
        let root = fixture.alpha_root.clone();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                fs::write(root.join("new.txt"), b"content").expect("write should succeed");
            });
            let start = Instant::now();
            assert!(fixture
                .alpha
                .await_change(Duration::from_secs(10))
                .expect("await should succeed"));
            assert!(start.elapsed() < Duration::from_secs(5));
        });
    }

    #[test]
    fn path_validation_rejects_escapes() {
        assert!(validate_path("").is_ok());
        assert!(validate_path("a/b/c.txt").is_ok());
        assert!(validate_path("..").is_err());
        assert!(validate_path("a/../b").is_err());
        assert!(validate_path("a//b").is_err());
        assert!(validate_path("a/./b").is_err());
    }
}
