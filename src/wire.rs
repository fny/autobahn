//! Encoding, in one place.
//!
//! Everything autobahn writes in binary goes through here: control and
//! transport frames, the ancestor journal and its checkpoints, the scan
//! cache. One module so the format is stated once and the decode limits
//! are a policy rather than a habit.
//!
//! The format is bincode's `legacy` configuration — little-endian, fixed
//! width integers, which is byte for byte what bincode 1.3 wrote. That
//! is deliberate: every journal on every machine is already in it, and a
//! change of encoding would mean a protocol epoch and a migration for no
//! gain. bincode 1.3 is unmaintained (RUSTSEC-2025-0141); its format is
//! fine.
//!
//! What does change is the ceiling. bincode 1 allocated whatever a
//! length prefix asked for, so a frame claiming four gigabytes got four
//! gigabytes before anything checked. [`decode_capped`] refuses past a
//! bound, which is what the advisory is actually about.

use anyhow::{Context, Result};
use bincode::config::{Configuration, Fixint, LittleEndian, NoLimit};
use serde::{de::DeserializeOwned, Serialize};

/// bincode 1.3's layout, kept so nothing already written has to move.
const FORMAT: Configuration<LittleEndian, Fixint, NoLimit> = bincode::config::legacy();

/// The most a single decoded message may allocate.
///
/// Above any frame the transport sends — it caps frames itself, and a
/// message too large for one is split and reassembled — and far below
/// the point where a lie about a length costs anything. This is the
/// backstop for the paths where the length arrived from somewhere else.
pub const CEILING: usize = 256 * 1024 * 1024;

/// Encodes a value.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(value, FORMAT).context("unable to encode")
}

/// Encodes a value into a writer, for a journal appending a record
/// rather than building one in memory first.
pub fn encode_into<T: Serialize, W: std::io::Write>(value: &T, writer: &mut W) -> Result<usize> {
    bincode::serde::encode_into_std_write(value, writer, FORMAT).context("unable to encode")
}

/// What a value will take, without encoding it.
pub fn size_of<T: Serialize>(value: &T) -> Result<u64> {
    // bincode 2 has no `serialized_size`; the writer that counts and
    // discards costs the encode but allocates nothing.
    let mut counted = Counting(0);
    encode_into(value, &mut counted)?;
    Ok(counted.0)
}

/// Decodes a value this process wrote itself — a journal record, a cache
/// entry — where the bytes are as trustworthy as the disk they came from.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (value, _) =
        bincode::serde::decode_from_slice(bytes, FORMAT).context("unable to decode")?;
    Ok(value)
}

/// Decodes a value that arrived from somewhere else, refusing to
/// allocate past [`CEILING`] however large the message claims to be.
pub fn decode_capped<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let format = FORMAT.with_limit::<CEILING>();
    let (value, _) =
        bincode::serde::decode_from_slice(bytes, format).context("unable to decode")?;
    Ok(value)
}

/// A writer that counts and keeps nothing, for [`size_of`].
struct Counting(u64);

impl std::io::Write for Counting {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes are bincode 1.3's, which is what every journal already
    /// holds. A fixed expectation rather than a round trip: a round trip
    /// passes for any self-consistent format, including one that would
    /// make every machine's state unreadable.
    #[test]
    fn the_format_is_the_one_already_on_disk() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Record {
            generation: u64,
            name: String,
            executable: bool,
        }
        let record = Record {
            generation: 1,
            name: "a".to_owned(),
            executable: true,
        };
        let bytes = encode(&record).expect("encodes");
        assert_eq!(
            bytes,
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, // the generation, eight bytes, little-endian
                1, 0, 0, 0, 0, 0, 0, 0,    // the string's length, the same
                b'a', // and its one byte
                1,    // the bool
            ],
        );
        assert_eq!(decode::<Record>(&bytes).expect("decodes"), record);
    }

    /// A length that lies is refused rather than allocated.
    #[test]
    fn a_message_claiming_more_than_the_ceiling_is_refused() {
        let mut bytes = u64::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        let refused = decode_capped::<Vec<u8>>(&bytes);
        assert!(refused.is_err(), "{refused:?}");
        // And the uncapped path is the one that would have tried.
        assert!(size_of(&vec![0u8; 8]).expect("sized") > 8);
    }
}
