//! The rsync delta-transfer algorithm: signatures, delta generation, and
//! patching. Never buffers whole files: signatures stream over the base,
//! deltas stream over the target, and patching streams operations to a
//! writer.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::tree::Digest;

/// The minimum block size selected for signatures.
pub const MINIMUM_BLOCK_SIZE: u32 = 1 << 10;
/// The maximum block size selected for signatures.
pub const MAXIMUM_BLOCK_SIZE: u32 = 1 << 16;
/// The maximum data payload carried by a single data operation.
pub const MAXIMUM_DATA_OPERATION_SIZE: usize = 1 << 16;

/// The block size used by [`signature`] when the caller doesn't specify one.
/// Signature computation only has a [`Read`] (not a [`Seek`]) over the base,
/// so it can't measure the base in order to derive an optimal block size —
/// callers that know the base length are expected to pass
/// [`optimal_block_size`] explicitly.
const DEFAULT_BLOCK_SIZE: u32 = 8192;

/// The modulus of each 16-bit component of the weak rolling checksum.
const WEAK_HASH_MODULUS: u32 = 1 << 16;
/// The mask corresponding to [`WEAK_HASH_MODULUS`].
const WEAK_HASH_MASK: u32 = WEAK_HASH_MODULUS - 1;

/// The hash of a single base block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockHash {
    /// The rolling (weak) checksum of the block.
    pub weak: u32,
    /// The BLAKE3 (strong) digest of the block.
    pub strong: Digest,
}

/// The signature of a base stream, enabling delta generation against it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Signature {
    /// The block size used (0 for an empty base).
    pub block_size: u32,
    /// The size of the final block (0 for an empty base; equal to
    /// `block_size` when the base divides evenly).
    pub last_block_size: u32,
    /// The per-block hashes.
    pub hashes: Vec<BlockHash>,
}

impl Signature {
    /// The block layout `signature` would give a base of `length` bytes,
    /// without reading it: the block size, the final block's size, and one
    /// placeholder hash per block. Enough to *apply* a delta — `patch`
    /// consults only the layout — and useless for computing one.
    pub fn layout(length: u64, block_size: u32) -> Signature {
        let block_size = if block_size == 0 {
            DEFAULT_BLOCK_SIZE
        } else {
            block_size
        };
        if length == 0 {
            return Signature::default();
        }
        let blocks = length.div_ceil(u64::from(block_size));
        let last_block_size = (length - (blocks - 1) * u64::from(block_size)) as u32;
        Signature {
            block_size,
            last_block_size,
            hashes: vec![
                BlockHash {
                    weak: 0,
                    strong: [0; 32],
                };
                blocks as usize
            ],
        }
    }

    /// Indicates whether or not this signature describes an empty base.
    pub fn is_empty(&self) -> bool {
        self.block_size == 0
    }

    /// Validates the signature's structural invariants.
    pub fn validate(&self) -> Result<()> {
        if self.is_empty() {
            // An empty base is described by a fully zeroed signature.
            if self.last_block_size != 0 {
                bail!(
                    "empty signature specifies a non-zero last block size ({})",
                    self.last_block_size
                );
            }
            if !self.hashes.is_empty() {
                bail!(
                    "empty signature carries {} block hash(es)",
                    self.hashes.len()
                );
            }
            return Ok(());
        }

        // The block size is one this module would choose: a peer's tiny
        // block size means a hash per byte, a huge one a huge buffer.
        if !(MINIMUM_BLOCK_SIZE..=MAXIMUM_BLOCK_SIZE).contains(&self.block_size) {
            bail!(
                "block size {} is outside {MINIMUM_BLOCK_SIZE}..={MAXIMUM_BLOCK_SIZE}",
                self.block_size
            );
        }

        // A non-empty base has at least one block, the last of which is
        // non-empty and no larger than the nominal block size.
        if self.last_block_size == 0 || self.last_block_size > self.block_size {
            bail!(
                "invalid last block size ({}) for block size {}",
                self.last_block_size,
                self.block_size
            );
        }
        if self.hashes.is_empty() {
            bail!("non-empty signature carries no block hashes");
        }
        Ok(())
    }

    /// Validates the signature as one of a base of a known length: its
    /// structure, and no more block hashes than such a base has blocks.
    /// A signature carries its own block count, so only a caller that knows
    /// the base's length can bound it.
    pub fn validate_for_base(&self, base_length: u64) -> Result<()> {
        self.validate()?;
        if self.is_empty() {
            return Ok(());
        }
        let blocks = base_length / u64::from(self.block_size) + 1;
        if self.hashes.len() as u64 > blocks {
            bail!(
                "signature carries {} block hashes for a base of {base_length} bytes",
                self.hashes.len()
            );
        }
        Ok(())
    }
}

/// A single delta operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op {
    /// Literal data to append to the output.
    Data(Vec<u8>),
    /// A run of consecutive base blocks to copy to the output.
    Blocks {
        /// The index of the first block.
        start: u64,
        /// The number of consecutive blocks.
        count: u64,
    },
}

/// Computes the optimal block size for a base of the specified length
/// (following the rsync thesis, clamped to the supported range).
pub fn optimal_block_size(base_length: u64) -> u32 {
    // The thesis minimizes the expected signature-plus-delta size at a block
    // size proportional to the square root of the file length. The clamp is
    // applied in floating point so that lengths whose square root exceeds the
    // integer range can't truncate on the way down.
    let optimal = (24.0 * base_length as f64).sqrt();
    optimal.clamp(MINIMUM_BLOCK_SIZE as f64, MAXIMUM_BLOCK_SIZE as f64) as u32
}

/// Computes the signature of a base stream using the specified block size
/// (or the default when zero).
///
/// The base is read in a single forward pass with a single block-sized
/// buffer. The final block may be short; its size is recorded separately so
/// that delta generation and patching can treat it correctly. An empty base
/// yields a zeroed (empty) signature.
pub fn signature<R: Read>(base: R, block_size: u32) -> Result<Signature> {
    let mut base = base;
    let block_size = if block_size == 0 {
        DEFAULT_BLOCK_SIZE
    } else {
        block_size
    };

    let mut buffer = vec![0u8; block_size as usize];
    let mut hashes = Vec::new();
    let mut last_block_size = 0u32;
    loop {
        // Fill a complete block, tolerating short reads, and stop once the
        // base is exhausted. A short fill means this is the final block.
        let filled = read_into(&mut base, &mut buffer).context("unable to read base")?;
        if filled == 0 {
            break;
        }
        let block = &buffer[..filled];
        hashes.push(BlockHash {
            weak: weak_hash(block, block_size),
            strong: *blake3::hash(block).as_bytes(),
        });
        last_block_size = filled as u32;
        if filled < buffer.len() {
            break;
        }
    }

    // An empty base carries no block size at all, so that delta generation
    // can recognize it without consulting the hash count.
    if hashes.is_empty() {
        return Ok(Signature::default());
    }
    Ok(Signature {
        block_size,
        last_block_size,
        hashes,
    })
}

/// The base length from which [`file_signature`] hashes on several threads.
const PARALLEL_SIGNATURE_MINIMUM: u64 = 64 << 20;
/// The most threads [`file_signature`] uses.
const SIGNATURE_THREADS_MAX: usize = 8;

/// The signature of a whole file: what [`signature`] computes reading it
/// from the start, with a large file's blocks read and hashed on several
/// threads, each taking a contiguous range of whole blocks. Measured on a
/// 4 GB base, one thread spent 2.7 s here while the rest of the machine
/// waited on it.
///
/// A file whose length changes under the parallel reads is signed again in
/// one pass, as `signature` would. Either way the result describes the file
/// as it was read, and nothing depends on that matching what it is later:
/// what a delta against it builds is checked against its digest.
///
/// `pulse` is called for every block read, from whichever thread read it,
/// so a caller can show a long signature is moving.
pub fn file_signature(
    file: &std::fs::File,
    block_size: u32,
    pulse: &(dyn Fn() + Sync),
) -> Result<Signature> {
    use std::os::unix::fs::FileExt;

    let block_size = if block_size == 0 {
        DEFAULT_BLOCK_SIZE
    } else {
        block_size
    };
    let length = file
        .metadata()
        .context("unable to read base metadata")?
        .len();
    let threads = std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .min(SIGNATURE_THREADS_MAX);
    if length < PARALLEL_SIGNATURE_MINIMUM || threads < 2 {
        return signature(Pulsed { inner: file, pulse }, block_size);
    }
    let block = u64::from(block_size);
    let blocks = length.div_ceil(block);
    let per_thread = blocks.div_ceil(threads as u64);
    let parts: Vec<Option<Vec<BlockHash>>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads as u64)
            .map(|thread| {
                let first = (thread * per_thread).min(blocks);
                let end = ((thread + 1) * per_thread).min(blocks);
                scope.spawn(move || {
                    let mut buffer = vec![0u8; block_size as usize];
                    let mut hashes = Vec::with_capacity((end - first) as usize);
                    for index in first..end {
                        let offset = index * block;
                        let size = block.min(length - offset) as usize;
                        let bytes = &mut buffer[..size];
                        file.read_exact_at(bytes, offset).ok()?;
                        pulse();
                        hashes.push(BlockHash {
                            weak: weak_hash(bytes, block_size),
                            strong: *blake3::hash(bytes).as_bytes(),
                        });
                    }
                    Some(hashes)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap_or(None))
            .collect()
    });
    let unchanged = file
        .metadata()
        .is_ok_and(|metadata| metadata.len() == length);
    if !unchanged || parts.iter().any(Option::is_none) {
        return signature(Pulsed { inner: file, pulse }, block_size);
    }
    Ok(Signature {
        block_size,
        last_block_size: (length - (blocks - 1) * block) as u32,
        hashes: parts.into_iter().flatten().flatten().collect(),
    })
}

/// A reader that calls `pulse` for every read that returned bytes.
struct Pulsed<'a, R> {
    inner: R,
    pulse: &'a (dyn Fn() + Sync),
}

impl<R: Read> Read for Pulsed<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            (self.pulse)();
        }
        Ok(read)
    }
}

/// Computes delta operations that reconstruct the target stream from a base
/// described by the provided signature, streaming operations to `emit`.
/// Adjacent block matches are coalesced; data operations are bounded by
/// [`MAXIMUM_DATA_OPERATION_SIZE`].
///
/// The target is scanned in a single forward pass using a rolling weak
/// checksum, with candidate matches confirmed by their strong digests. Only
/// full-size base blocks participate in rolling matches; a short final base
/// block can only align with the end of the target, so it's tested there.
/// Memory usage is bounded by the search window
/// ([`MAXIMUM_DATA_OPERATION_SIZE`] plus one block), regardless of target
/// size.
pub fn deltify<R: Read>(
    target: R,
    signature: &Signature,
    emit: &mut dyn FnMut(Op) -> Result<()>,
) -> Result<()> {
    signature
        .validate()
        .context("unable to deltify against an invalid signature")?;

    let mut target = target;
    let mut emitter = Emitter {
        emit,
        pending: None,
    };

    // With no base content there's nothing to match against, so the target
    // streams through as bounded data operations.
    if signature.is_empty() {
        let mut buffer = vec![0u8; MAXIMUM_DATA_OPERATION_SIZE];
        loop {
            let filled = read_into(&mut target, &mut buffer).context("unable to read target")?;
            if filled == 0 {
                break;
            }
            emitter.data(&buffer[..filled])?;
        }
        return Ok(());
    }

    let block_size = signature.block_size as usize;
    let last_block_size = signature.last_block_size as usize;
    let block_count = signature.hashes.len();

    // Index the full-size blocks by weak checksum. A short final block is
    // excluded: the rolling search only ever examines full-size windows, and
    // a short block that matched mid-target would leave the remainder of the
    // base misaligned anyway.
    let full_block_count = if last_block_size == block_size {
        block_count
    } else {
        block_count - 1
    };
    let mut weak_index: HashMap<u32, Vec<u64>> = HashMap::new();
    for (index, hash) in signature.hashes.iter().take(full_block_count).enumerate() {
        weak_index.entry(hash.weak).or_default().push(index as u64);
    }

    // The search buffer holds a maximal data operation plus one block, so
    // that unmatched bytes can always be flushed as a single data operation
    // while the window they precede stays resident.
    let capacity = MAXIMUM_DATA_OPERATION_SIZE + block_size;
    let mut buffer = vec![0u8; capacity];
    // Bytes valid in the buffer, the offset of the first byte not yet
    // emitted, and the offset of the search window (`start <= position`).
    let mut filled = 0usize;
    let mut start = 0usize;
    let mut position = 0usize;
    let mut exhausted = false;
    // The two 16-bit components of the window's rolling checksum, valid only
    // when `rolling` is set.
    let mut r1 = 0u32;
    let mut r2 = 0u32;
    let mut rolling = false;
    // The base block the next window is expected to be, if the target is
    // the base unchanged there: the first, and then the one after each
    // match. Checked once per match, so a changed stretch never pays it.
    let mut expected: Option<usize> = Some(0);

    loop {
        // Ensure the buffer holds a full window plus, unless the target is
        // exhausted, the lookahead byte needed to roll forward. Compaction
        // releases everything preceding the window; it only ever runs with a
        // non-empty prefix to release, so the loop always makes progress.
        while !exhausted && filled - position <= block_size {
            if filled == capacity {
                emitter.data(&buffer[start..position])?;
                buffer.copy_within(position..filled, 0);
                filled -= position;
                start = 0;
                position = 0;
            }
            let read =
                read_into(&mut target, &mut buffer[filled..]).context("unable to read target")?;
            filled += read;
            if filled < capacity {
                exhausted = true;
            }
        }
        if filled - position < block_size {
            break;
        }

        let window = &buffer[position..position + block_size];
        // Where the target is the base unchanged, the window just past a
        // match is the next base block: its strong digest settles that
        // without the weak checksum, whose byte-at-a-time sum would
        // otherwise cost more than the hash. Any other outcome falls through
        // to the search, so this changes nothing but the time taken.
        if !rolling {
            if let Some(index) = expected.take() {
                if index < full_block_count
                    && *blake3::hash(window).as_bytes() == signature.hashes[index].strong
                {
                    emitter.data(&buffer[start..position])?;
                    emitter.block(index as u64)?;
                    position += block_size;
                    start = position;
                    expected = Some(index + 1);
                    continue;
                }
            }
        }
        if !rolling {
            let (first, second) = weak_components(window, signature.block_size);
            r1 = first;
            r2 = second;
            rolling = true;
        }
        let weak = r1 + (r2 << 16);

        // Confirm weak-checksum candidates with their strong digests, which
        // is what actually establishes the match.
        let mut matched = None;
        if let Some(candidates) = weak_index.get(&weak) {
            let strong: Digest = *blake3::hash(window).as_bytes();
            matched = candidates
                .iter()
                .copied()
                .find(|&index| signature.hashes[index as usize].strong == strong);
        }
        if let Some(index) = matched {
            // Everything between the last emission and the match is literal
            // data, and it has to precede the match in the operation stream.
            emitter.data(&buffer[start..position])?;
            emitter.block(index)?;
            position += block_size;
            start = position;
            rolling = false;
            expected = Some(index as usize + 1);
            continue;
        }

        // No match here: the window slides forward by a single byte, which
        // the rolling checksum absorbs in constant time.
        if position + block_size >= filled {
            break;
        }
        let (first, second) = roll_weak_components(
            r1,
            r2,
            buffer[position],
            buffer[position + block_size],
            signature.block_size,
        );
        r1 = first;
        r2 = second;
        position += 1;
    }

    // The target is exhausted. A short final base block can only align with
    // the end of the target, so test it against the tail of what remains.
    if last_block_size != block_size && filled >= last_block_size {
        let tail_start = filled - last_block_size;
        if tail_start >= start {
            let tail = &buffer[tail_start..filled];
            let last = &signature.hashes[block_count - 1];
            if weak_hash(tail, signature.block_size) == last.weak
                && *blake3::hash(tail).as_bytes() == last.strong
            {
                emitter.data(&buffer[start..tail_start])?;
                emitter.block((block_count - 1) as u64)?;
                start = filled;
            }
        }
    }

    // Release the unmatched remainder and any coalescing block run.
    emitter.data(&buffer[start..filled])?;
    emitter.flush()
}

/// Applies a single delta operation against a seekable base, writing the
/// reconstructed content to the output.
///
/// Block operations are validated against the signature before any I/O
/// occurs, so a malformed or hostile delta can't induce an out-of-range read.
/// Block content is copied through a single reusable buffer.
pub fn patch<B: Read + Seek, W: Write>(
    base: &mut B,
    signature: &Signature,
    op: &Op,
    output: &mut W,
) -> Result<()> {
    signature
        .validate()
        .context("unable to patch against an invalid signature")?;

    let (start, count) = match op {
        Op::Data(data) => {
            return output
                .write_all(data)
                .context("unable to write data operation");
        }
        Op::Blocks { start, count } => (*start, *count),
    };

    // Validate the block range (guarding against overflow in the process)
    // before touching the base.
    let end = start
        .checked_add(count)
        .with_context(|| format!("block operation range overflows ({start} + {count})"))?;
    let block_count = signature.hashes.len() as u64;
    if end > block_count {
        bail!("block operation range {start}..{end} exceeds base block count {block_count}");
    }
    if count == 0 {
        return Ok(());
    }

    let block_size = signature.block_size as u64;
    let offset = start
        .checked_mul(block_size)
        .with_context(|| format!("block offset overflows (block {start} of size {block_size})"))?;
    base.seek(SeekFrom::Start(offset))
        .with_context(|| format!("unable to seek base to offset {offset}"))?;

    // The blocks in a run are consecutive, so a single seek suffices and the
    // copies proceed sequentially through one reusable buffer.
    let block_size = signature.block_size as usize;
    let mut buffer = vec![0u8; block_size.min(MAXIMUM_DATA_OPERATION_SIZE)];
    for index in start..end {
        let mut remaining = if index == block_count - 1 {
            signature.last_block_size as usize
        } else {
            block_size
        };
        while remaining > 0 {
            let length = remaining.min(buffer.len());
            base.read_exact(&mut buffer[..length])
                .with_context(|| format!("unable to read base block {index}"))?;
            output
                .write_all(&buffer[..length])
                .with_context(|| format!("unable to write base block {index}"))?;
            remaining -= length;
        }
    }
    Ok(())
}

/// Streams operations to a sink, coalescing consecutive block matches into
/// single [`Op::Blocks`] runs and chunking literal data to
/// [`MAXIMUM_DATA_OPERATION_SIZE`].
struct Emitter<'a> {
    /// The operation sink.
    emit: &'a mut dyn FnMut(Op) -> Result<()>,
    /// The block run being accumulated, as a (start, count) pair.
    pending: Option<(u64, u64)>,
}

impl Emitter<'_> {
    /// Emits literal data (chunked to the maximum operation size), first
    /// flushing any pending block run so that ordering is preserved. Empty
    /// data is ignored, and in particular doesn't break a block run.
    fn data(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.flush()?;
        for chunk in bytes.chunks(MAXIMUM_DATA_OPERATION_SIZE) {
            (self.emit)(Op::Data(chunk.to_vec()))?;
        }
        Ok(())
    }

    /// Records a block match, extending the pending run if the block follows
    /// it consecutively and starting a new run otherwise.
    fn block(&mut self, index: u64) -> Result<()> {
        match self.pending {
            Some((start, count)) if start + count == index => {
                self.pending = Some((start, count + 1));
            }
            Some(_) => {
                self.flush()?;
                self.pending = Some((index, 1));
            }
            None => self.pending = Some((index, 1)),
        }
        Ok(())
    }

    /// Emits any pending block run.
    fn flush(&mut self) -> Result<()> {
        if let Some((start, count)) = self.pending.take() {
            (self.emit)(Op::Blocks { start, count })?;
        }
        Ok(())
    }
}

/// Reads into the buffer until it's full or the reader is exhausted,
/// returning the number of bytes read. A short return therefore means (and
/// only means) that the reader hit its end.
fn read_into<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// Computes the two 16-bit components of the weak rolling checksum of a
/// block. The coefficients are derived from the *nominal* block size even for
/// a short final block, matching the classic rsync formulation (and thus the
/// coefficients that the rolling update maintains).
fn weak_components(block: &[u8], block_size: u32) -> (u32, u32) {
    // Both components are sums modulo 2^16, and wrapping `u32` arithmetic
    // is exact modulo 2^32 and so modulo 2^16: reducing once at the end
    // gives the same values as reducing at every step, and leaves a loop
    // without a carried mask, which the compiler vectorizes.
    let mut r1 = 0u32;
    let mut r2 = 0u32;
    for (index, &byte) in block.iter().enumerate() {
        r1 = r1.wrapping_add(byte as u32);
        let coefficient = block_size.wrapping_sub(index as u32);
        r2 = r2.wrapping_add(coefficient.wrapping_mul(byte as u32));
    }
    (r1 & WEAK_HASH_MASK, r2 & WEAK_HASH_MASK)
}

/// Computes the weak rolling checksum of a block: the low component in the
/// low 16 bits, the high component in the high 16 bits.
fn weak_hash(block: &[u8], block_size: u32) -> u32 {
    let (r1, r2) = weak_components(block, block_size);
    r1 + (r2 << 16)
}

/// Advances the weak checksum components by one byte: `outgoing` leaves the
/// window and `incoming` enters it. Every intermediate value stays below
/// 2^18, so the additions can't overflow `u32`.
fn roll_weak_components(
    r1: u32,
    r2: u32,
    outgoing: u8,
    incoming: u8,
    block_size: u32,
) -> (u32, u32) {
    // The modulus is added before each subtraction to keep the arithmetic in
    // the non-negative range without wrapping.
    let r1 = (r1 + WEAK_HASH_MODULUS - outgoing as u32 + incoming as u32) & WEAK_HASH_MASK;
    let departing = ((block_size & WEAK_HASH_MASK) * outgoing as u32) & WEAK_HASH_MASK;
    let r2 = (r2 + WEAK_HASH_MODULUS - departing + r1) & WEAK_HASH_MASK;
    (r1, r2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::DIGEST_SIZE;
    use std::io::Cursor;

    /// Computes a signature of the base, deltifies the target against it,
    /// patches every operation, and asserts that the reconstruction is exact
    /// and that every data operation respects the size bound.
    fn round_trip(base: &[u8], target: &[u8], block_size: u32) -> Vec<Op> {
        let signature = super::signature(Cursor::new(base), block_size).unwrap();
        signature.validate().unwrap();

        let mut ops = Vec::new();
        deltify(Cursor::new(target), &signature, &mut |op| {
            ops.push(op);
            Ok(())
        })
        .unwrap();

        let mut output = Vec::new();
        let mut cursor = Cursor::new(base);
        for op in &ops {
            if let Op::Data(data) = op {
                assert!(!data.is_empty(), "empty data operation");
                assert!(
                    data.len() <= MAXIMUM_DATA_OPERATION_SIZE,
                    "oversized data operation ({} bytes)",
                    data.len()
                );
            }
            patch(&mut cursor, &signature, op, &mut output).unwrap();
        }
        assert_eq!(output, target, "reconstruction mismatch");
        ops
    }

    /// Generates deterministic pseudo-random content with a linear
    /// congruential generator, taking the high (well-mixed) bytes of each
    /// state.
    fn pseudo_random(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut data = Vec::with_capacity(length + 4);
        while data.len() < length {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.extend_from_slice(&((state >> 32) as u32).to_le_bytes());
        }
        data.truncate(length);
        data
    }

    /// Returns the total number of blocks referenced by block operations.
    fn block_total(ops: &[Op]) -> u64 {
        ops.iter()
            .map(|op| match op {
                Op::Blocks { count, .. } => *count,
                Op::Data(_) => 0,
            })
            .sum()
    }

    #[test]
    fn optimal_block_size_clamps_to_the_supported_range() {
        assert_eq!(optimal_block_size(0), MINIMUM_BLOCK_SIZE);
        assert_eq!(optimal_block_size(1), MINIMUM_BLOCK_SIZE);
        assert_eq!(optimal_block_size(u64::MAX), MAXIMUM_BLOCK_SIZE);
        let middle = optimal_block_size(10_000_000);
        assert_eq!(middle, (24.0f64 * 10_000_000.0).sqrt() as u32);
        assert!(middle > MINIMUM_BLOCK_SIZE && middle < MAXIMUM_BLOCK_SIZE);
    }

    #[test]
    fn rolling_checksum_matches_direct_computation() {
        let data = pseudo_random(4096, 0x5EED);
        let block_size = 97u32;
        let (mut r1, mut r2) = weak_components(&data[..block_size as usize], block_size);
        for position in 1..(data.len() - block_size as usize) {
            let (next1, next2) = roll_weak_components(
                r1,
                r2,
                data[position - 1],
                data[position - 1 + block_size as usize],
                block_size,
            );
            r1 = next1;
            r2 = next2;
            let expected = weak_hash(&data[position..position + block_size as usize], block_size);
            assert_eq!(r1 + (r2 << 16), expected, "at position {position}");
        }
    }

    #[test]
    fn signature_describes_block_layout() {
        let empty = super::signature(Cursor::new(b""), 1024).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.block_size, 0);
        assert_eq!(empty.last_block_size, 0);
        assert!(empty.hashes.is_empty());

        // An exact multiple reports a full-size final block.
        let exact = super::signature(Cursor::new(vec![1u8; 4096]), 1024).unwrap();
        assert_eq!(exact.hashes.len(), 4);
        assert_eq!(exact.last_block_size, 1024);

        // A remainder reports a short final block.
        let short = super::signature(Cursor::new(vec![1u8; 4096 + 7]), 1024).unwrap();
        assert_eq!(short.hashes.len(), 5);
        assert_eq!(short.last_block_size, 7);

        // A zero block size falls back to the documented default.
        let defaulted = super::signature(Cursor::new(vec![1u8; 3]), 0).unwrap();
        assert_eq!(defaulted.block_size, DEFAULT_BLOCK_SIZE);
        assert_eq!(defaulted.last_block_size, 3);
    }

    #[test]
    fn validation_rejects_malformed_signatures() {
        let hash = BlockHash {
            weak: 0,
            strong: [0u8; DIGEST_SIZE],
        };
        assert!(Signature::default().validate().is_ok());
        assert!(Signature {
            block_size: 0,
            last_block_size: 1,
            hashes: Vec::new(),
        }
        .validate()
        .is_err());
        assert!(Signature {
            block_size: 0,
            last_block_size: 0,
            hashes: vec![hash.clone()],
        }
        .validate()
        .is_err());
        assert!(Signature {
            block_size: 1024,
            last_block_size: 0,
            hashes: vec![hash.clone()],
        }
        .validate()
        .is_err());
        assert!(Signature {
            block_size: 1024,
            last_block_size: 2048,
            hashes: vec![hash.clone()],
        }
        .validate()
        .is_err());
        assert!(Signature {
            block_size: 1024,
            last_block_size: 1024,
            hashes: Vec::new(),
        }
        .validate()
        .is_err());
        assert!(Signature {
            block_size: 1024,
            last_block_size: 1024,
            hashes: vec![hash.clone()],
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn validation_bounds_the_block_size_and_the_hash_count() {
        let hash = BlockHash {
            weak: 0,
            strong: [0u8; DIGEST_SIZE],
        };
        // A block size outside the module's own range is refused, however
        // well-formed the rest.
        for block_size in [1, MINIMUM_BLOCK_SIZE - 1, MAXIMUM_BLOCK_SIZE + 1, u32::MAX] {
            let signature = Signature {
                block_size,
                last_block_size: 1,
                hashes: vec![hash.clone()],
            };
            assert!(signature.validate().is_err(), "block size {block_size}");
        }
        for block_size in [MINIMUM_BLOCK_SIZE, MAXIMUM_BLOCK_SIZE] {
            let signature = Signature {
                block_size,
                last_block_size: 1,
                hashes: vec![hash.clone()],
            };
            assert!(signature.validate().is_ok(), "block size {block_size}");
        }

        // Against a known base length, no more hashes than it has blocks.
        let base = vec![3u8; 4096 + 7];
        let genuine = super::signature(Cursor::new(&base), 1024).unwrap();
        genuine.validate_for_base(base.len() as u64).unwrap();
        let mut padded = genuine.clone();
        padded.hashes.extend(vec![hash.clone(); 1000]);
        assert!(padded.validate().is_ok(), "structurally it is fine");
        assert!(padded.validate_for_base(base.len() as u64).is_err());
        assert!(Signature::default().validate_for_base(0).is_ok());
        assert!(genuine.validate_for_base(0).is_err());
    }

    #[test]
    fn empty_base_and_target_produce_no_operations() {
        let ops = round_trip(b"", b"", 0);
        assert!(ops.is_empty());
    }

    #[test]
    fn empty_base_streams_the_target_as_data() {
        let target = pseudo_random(MAXIMUM_DATA_OPERATION_SIZE * 2 + 13, 1);
        let ops = round_trip(b"", &target, 0);
        assert_eq!(ops.len(), 3);
        assert!(ops.iter().all(|op| matches!(op, Op::Data(_))));
    }

    #[test]
    fn empty_target_against_a_base_produces_no_operations() {
        let base = pseudo_random(5000, 2);
        let ops = round_trip(&base, b"", 1024);
        assert!(ops.is_empty());
    }

    #[test]
    fn identical_content_coalesces_into_a_single_block_run() {
        let base = pseudo_random(1024 * 16, 3);
        let ops = round_trip(&base, &base, 1024);
        assert!(ops.len() <= 2, "expected a coalesced run, got {ops:?}");
        assert_eq!(block_total(&ops), 16);

        // The same must hold when the final block is short.
        let ragged = pseudo_random(1024 * 16 + 300, 4);
        let ops = round_trip(&ragged, &ragged, 1024);
        assert!(ops.len() <= 2, "expected a coalesced run, got {ops:?}");
        assert_eq!(block_total(&ops), 17);
    }

    #[test]
    fn middle_mutation_reuses_surrounding_blocks() {
        let base = pseudo_random(1024 * 20, 5);
        let mut target = base.clone();
        for byte in target[1024 * 9..1024 * 9 + 64].iter_mut() {
            *byte ^= 0xFF;
        }
        let ops = round_trip(&base, &target, 1024);
        assert!(block_total(&ops) >= 18, "insufficient reuse: {ops:?}");
    }

    #[test]
    fn prefix_insertion_realigns_the_search() {
        let base = pseudo_random(1024 * 20, 6);
        let mut target = pseudo_random(777, 7);
        target.extend_from_slice(&base);
        let ops = round_trip(&base, &target, 1024);
        assert_eq!(block_total(&ops), 20, "expected full reuse: {ops:?}");
    }

    #[test]
    fn truncation_reuses_the_surviving_prefix() {
        let base = pseudo_random(1024 * 20, 8);
        let target = &base[..1024 * 7 + 11];
        let ops = round_trip(&base, target, 1024);
        assert_eq!(block_total(&ops), 7, "expected prefix reuse: {ops:?}");
    }

    #[test]
    fn appended_tail_reuses_the_entire_base() {
        let base = pseudo_random(1024 * 20, 9);
        let mut target = base.clone();
        target.extend_from_slice(&pseudo_random(3000, 10));
        let ops = round_trip(&base, &target, 1024);
        assert_eq!(block_total(&ops), 20, "expected full reuse: {ops:?}");
    }

    #[test]
    fn exact_block_multiple_bases_round_trip() {
        let base = pseudo_random(1024 * 8, 11);
        let signature = super::signature(Cursor::new(&base), 1024).unwrap();
        assert_eq!(signature.block_size, signature.last_block_size);
        let mut target = base.clone();
        target.extend_from_slice(b"tail");
        round_trip(&base, &target, 1024);
        round_trip(&base, &base[..1024 * 3], 1024);
    }

    #[test]
    fn short_final_block_matches_at_the_target_end() {
        let base = pseudo_random(1024 * 3 + 500, 12);
        let mut target = pseudo_random(2000, 13);
        target.extend_from_slice(&base[1024 * 3..]);
        let ops = round_trip(&base, &target, 1024);
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::Blocks { start: 3, count: 1 })),
            "expected the short final block to match: {ops:?}"
        );

        // A short final block must not match anywhere but the end.
        let mut interior = base[1024 * 3..].to_vec();
        interior.extend_from_slice(&pseudo_random(100, 14));
        let ops = round_trip(&base, &interior, 1024);
        assert_eq!(block_total(&ops), 0, "unexpected interior match: {ops:?}");
    }

    #[test]
    fn a_file_signature_is_the_streamed_one() {
        let directory = tempfile::tempdir().unwrap();
        // Above the parallel threshold, ending in a short block, and a
        // whole number of blocks; and one below it.
        for (length, block_size) in [
            ((PARALLEL_SIGNATURE_MINIMUM + 12_345) as usize, 1 << 16),
            (
                (PARALLEL_SIGNATURE_MINIMUM as usize).next_multiple_of(4096),
                4096,
            ),
            (100_000, 1024),
        ] {
            let path = directory.path().join("base");
            let content = pseudo_random(length, length as u64);
            std::fs::write(&path, &content).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let parallel = file_signature(&file, block_size, &|| {}).unwrap();
            let streamed = super::signature(Cursor::new(&content), block_size).unwrap();
            assert_eq!(parallel.block_size, streamed.block_size);
            assert_eq!(parallel.last_block_size, streamed.last_block_size);
            assert_eq!(parallel.hashes.len(), streamed.hashes.len());
            assert!(parallel
                .hashes
                .iter()
                .zip(&streamed.hashes)
                .all(|(a, b)| a.weak == b.weak && a.strong == b.strong));
        }
    }

    #[test]
    fn the_weak_checksum_is_the_one_reduced_at_every_step() {
        // The definition, reduced modulo 2^16 at every step as it was first
        // written; the shipped one reduces once, and must agree everywhere,
        // including a block size of 2^16 itself.
        fn reference(block: &[u8], block_size: u32) -> (u32, u32) {
            let (mut r1, mut r2) = (0u32, 0u32);
            for (index, &byte) in block.iter().enumerate() {
                r1 = (r1 + byte as u32) & WEAK_HASH_MASK;
                let coefficient = block_size.wrapping_sub(index as u32) & WEAK_HASH_MASK;
                r2 = (r2 + coefficient * byte as u32) & WEAK_HASH_MASK;
            }
            (r1, r2)
        }
        for (length, block_size, seed) in [
            (0, 1024, 1),
            (1, 1024, 2),
            (1024, 1024, 3),
            (700, 1024, 4),
            (1 << 16, 1 << 16, 5),
            (40_000, 1 << 16, 6),
        ] {
            let block = pseudo_random(length, seed);
            assert_eq!(
                weak_components(&block, block_size),
                reference(&block, block_size)
            );
        }
        let saturated = vec![0xFFu8; 1 << 16];
        assert_eq!(
            weak_components(&saturated, 1 << 16),
            reference(&saturated, 1 << 16)
        );
    }

    #[test]
    fn a_base_of_repeated_blocks_still_round_trips() {
        // Every block of the base is the same, so the block expected after a
        // match and the one the search would find are interchangeable.
        let block = pseudo_random(1024, 21);
        let base: Vec<u8> = block.iter().copied().cycle().take(64 * 1024).collect();
        let ops = round_trip(&base, &base, 1024);
        assert_eq!(block_total(&ops), 64);
        let mut changed = base.clone();
        changed[10_000] ^= 1;
        changed.splice(30_000..30_000, pseudo_random(333, 22));
        round_trip(&base, &changed, 1024);
    }

    #[test]
    fn large_pseudo_random_content_round_trips() {
        let base = pseudo_random(300_000, 15);

        // Identical content.
        let ops = round_trip(&base, &base, 1024);
        assert!(ops.len() <= 2);

        // A scattering of mutations.
        let mut mutated = base.clone();
        for offset in (0..mutated.len()).step_by(50_000) {
            mutated[offset] ^= 0xA5;
        }
        round_trip(&base, &mutated, 1024);

        // A large insertion in the middle.
        let mut inserted = base[..150_000].to_vec();
        inserted.extend_from_slice(&pseudo_random(70_000, 16));
        inserted.extend_from_slice(&base[150_000..]);
        let ops = round_trip(&base, &inserted, 1024);
        assert!(block_total(&ops) > 200, "insufficient reuse: {ops:?}");

        // A large deletion.
        let mut deleted = base[..40_000].to_vec();
        deleted.extend_from_slice(&base[220_000..]);
        round_trip(&base, &deleted, 1024);

        // Entirely unrelated content (no reuse, but every data operation
        // must still respect the size bound).
        let unrelated = pseudo_random(200_000, 17);
        round_trip(&base, &unrelated, 1024);

        // Reversed content, which defeats block alignment everywhere.
        let mut reversed = base.clone();
        reversed.reverse();
        round_trip(&base, &reversed, 1024);

        // A target that is one long run of a repeated byte, which produces
        // dense weak-checksum collisions against itself.
        let repetitive = vec![0x42u8; 200_000];
        let repeated_signature = super::signature(Cursor::new(&repetitive), 1024).unwrap();
        assert_eq!(repeated_signature.hashes.len(), 196);
        round_trip(&repetitive, &repetitive[..150_000], 1024);
        round_trip(&repetitive, &base, 1024);
    }

    #[test]
    fn patch_rejects_out_of_range_block_operations() {
        let base = pseudo_random(4096, 18);
        let signature = super::signature(Cursor::new(&base), 1024).unwrap();
        let mut cursor = Cursor::new(&base);
        let mut output = Vec::new();

        assert!(patch(
            &mut cursor,
            &signature,
            &Op::Blocks { start: 3, count: 2 },
            &mut output
        )
        .is_err());
        assert!(patch(
            &mut cursor,
            &signature,
            &Op::Blocks { start: 4, count: 1 },
            &mut output
        )
        .is_err());
        assert!(patch(
            &mut cursor,
            &signature,
            &Op::Blocks {
                start: u64::MAX,
                count: 2,
            },
            &mut output
        )
        .is_err());
        assert!(output.is_empty());

        // The full in-range run remains acceptable.
        assert!(patch(
            &mut cursor,
            &signature,
            &Op::Blocks { start: 0, count: 4 },
            &mut output
        )
        .is_ok());
        assert_eq!(output, base);
    }

    #[test]
    fn a_layout_matches_the_signature_it_stands_in_for() {
        // Every block size a signature may carry (1 KiB to 64 KiB, and 0
        // for the default), against lengths on and off block boundaries.
        for length in [0usize, 1, 1000, 1024, 1025, 4096, 4097, 65_536, 100_000] {
            for block_size in [0u32, 1024, 4096, 65_536] {
                let base: Vec<u8> = (0..length).map(|i| (i * 31 % 251) as u8).collect();
                let signed = signature(std::io::Cursor::new(&base), block_size).unwrap();
                let laid = Signature::layout(length as u64, block_size);
                assert_eq!(signed.block_size, laid.block_size, "{length} {block_size}");
                assert_eq!(
                    signed.last_block_size, laid.last_block_size,
                    "{length} {block_size}"
                );
                assert_eq!(
                    signed.hashes.len(),
                    laid.hashes.len(),
                    "{length} {block_size}"
                );
                // And a delta computed against the real signature applies
                // against the layout alone.
                let target: Vec<u8> = base.iter().rev().chain(base.iter()).copied().collect();
                let mut ops = Vec::new();
                deltify(std::io::Cursor::new(&target), &signed, &mut |op| {
                    ops.push(op);
                    Ok(())
                })
                .unwrap();
                let mut output = Vec::new();
                let mut cursor = std::io::Cursor::new(&base);
                for op in &ops {
                    patch(&mut cursor, &laid, op, &mut output).unwrap();
                }
                assert_eq!(output, target, "{length} {block_size}");
            }
        }
    }
}
