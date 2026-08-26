//! The benchmark's measurement plane, in one binary.
//!
//! Everything that touches a measured quantity lives here, compiled, so
//! that (a) the harness's own latency contribution is small and measured
//! rather than large and guessed, (b) a hundred simulated agents are a
//! hundred threads in one small process instead of a hundred interpreters
//! competing with the tools under test, and (c) the rules for walking a
//! tree — what is excluded, what counts — exist in exactly one
//! implementation shared by every subcommand and both hosts.
//!
//! Subcommands:
//!   manifest cheap|full <root>          tree summaries for convergence
//!   partitions <root> <output.json>     bake-time working-set generation
//!   verify-partitions <partitions.json> re-assert disjointness at run time
//!   observer <port>                     destination-side verifier + floor
//!   agents <options>                    the edit workload (one measures)
//!   floor <options>                     harness self-measurement
//!   sampler <pattern> <output>          RSS/CPU series for a process tree
//!
//! Orchestration (job sequencing, AWS lifecycle, aggregation) deliberately
//! stays outside this binary: those paths tolerate script-speed languages,
//! and code that does not touch a measurement should not add risk here.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

mod agents;
mod observer;
mod partitions;
mod sampler;
mod walk;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let strings: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let result = match strings.as_slice() {
        ["manifest", "cheap", root] => walk::print_cheap(Path::new(root)),
        ["manifest", "full", root] => walk::print_full(Path::new(root)),
        ["partitions", root, output] => partitions::generate(Path::new(root), Path::new(output)),
        ["verify-partitions", file] => partitions::verify_file(Path::new(file)),
        ["observer", port] => observer::serve(port.parse().expect("port")),
        ["sampler", pattern, output] => sampler::run(pattern, Path::new(output)),
        arguments if arguments.first() == Some(&"agents") => agents::run(&arguments[1..]),
        arguments if arguments.first() == Some(&"floor") => agents::floor(&arguments[1..]),
        _ => {
            eprintln!("unknown subcommand; see source header for usage");
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("benchmark: {error}");
        std::process::exit(1);
    }
}

/// A deterministic RNG (xorshift64*), implemented here so that its output
/// can never change under a dependency upgrade: partitions generated at
/// bake time must be reproducible forever from the recorded seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    pub fn word(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A uniform index in `0..bound` (multiply-shift; bias is far below
    /// anything a workload could observe).
    pub fn index(&mut self, bound: usize) -> usize {
        ((self.word() as u128 * bound as u128) >> 64) as usize
    }

    /// Fills a buffer with high-entropy bytes. High-entropy matters: a
    /// compressible payload would let a tool's wire compression shrink the
    /// transfer and flatter it.
    pub fn fill(&mut self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(8) {
            let word = self.word().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

/// Writes a file atomically: temporary beside the target, fsync, rename.
/// This is the shape a careful editor uses, and it is what both the
/// workload and the floor write with — measured edits and floor probes
/// must pay identical write costs.
pub fn write_atomic(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = PathBuf::from(format!("{}.bench-tmp", path.display()));
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(payload)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

/// Hex, lowercase. The protocol carries digests and floor payloads as hex
/// to keep this binary dependency-light.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("odd-length hex".into());
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Reads newline-delimited JSON messages from a stream.
pub fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<serde_json::Value>> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(trimmed).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error)
        })?));
    }
}

pub fn send_message<W: Write>(writer: &mut W, value: &serde_json::Value) -> std::io::Result<()> {
    writer.write_all(serde_json::to_string(value).expect("serializable").as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Digests a file's content, streaming.
pub fn digest_file(path: &Path) -> std::io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}
