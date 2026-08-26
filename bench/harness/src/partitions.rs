//! Working-set generation (bake time) and verification (run time).
//!
//! The measuring agent's working set is a fixed [`MEASURED_SET_SIZE`]
//! files at *every* agent count: agent count must vary load and nothing
//! else. (The previous harness derived working sets from the agent count,
//! which changed edit locality alongside load and produced a result that
//! looked like mutagen getting faster under ten times the load.)
//!
//! All working sets — measured and background, side A and side B — are
//! disjoint by construction *and* asserted disjoint, at generation and
//! again on each host before a workload runs. Overlapping edits from two
//! sides are a conflict, which a safe synchronization mode refuses to
//! resolve; measuring that refusal as latency was another of the previous
//! harness's failure modes.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Rng;

pub const SCHEMA: u32 = 2;
const SEED: u64 = 0xAB_BE_2C;
const AGENT_COUNTS: &[usize] = &[1, 10, 100];
const MEASURED_SET_SIZE: usize = 40;
/// Files an agent may edit: no empties (replacing an empty file is not a
/// representative edit) and nothing large enough to turn the cadence of
/// small writes into an occasional bulk transfer.
const EDITABLE_SIZE: (u64, u64) = (256, 256 * 1024);
/// The minimum background files per agent for edits to stay spread out.
/// Ten keeps a 100-agent bidirectional cell within reach of the smallest
/// (~4k-file) corpus while still giving every agent a rotation.
const BACKGROUND_FILES_PER_AGENT: usize = 10;

#[derive(Serialize, Deserialize)]
pub struct Partitions {
    pub schema: u32,
    pub seed: u64,
    /// sides -> agent count (as a string, for JSON) -> working sets.
    pub sides: std::collections::BTreeMap<String, std::collections::BTreeMap<String, WorkingSets>>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkingSets {
    /// The measuring agent's files — identical across agent counts.
    pub measured: Vec<String>,
    /// One file list per background agent.
    pub background: Vec<Vec<String>>,
}

pub fn generate(root: &Path, output: &Path) -> Result<(), String> {
    let files = crate::walk::files(root).map_err(|error| error.to_string())?;
    let editable: Vec<&String> = files
        .iter()
        .filter(|(_, size)| (EDITABLE_SIZE.0..=EDITABLE_SIZE.1).contains(size))
        .map(|(path, _)| path)
        .collect();
    let maximum = *AGENT_COUNTS.iter().max().expect("nonempty");
    let required = 2 * (MEASURED_SET_SIZE + maximum * BACKGROUND_FILES_PER_AGENT);
    if editable.len() < required {
        return Err(format!(
            "corpus has {} editable files; {} required",
            editable.len(),
            required
        ));
    }

    let mut shuffled: Vec<String> = editable.into_iter().cloned().collect();
    let mut rng = Rng::new(SEED);
    // Fisher–Yates with the in-crate RNG: reproducible forever from the
    // recorded seed, immune to dependency upgrades.
    for i in (1..shuffled.len()).rev() {
        shuffled.swap(i, rng.index(i + 1));
    }

    let mut cursor = 0usize;
    let take = |cursor: &mut usize, count: usize| -> Vec<String> {
        let taken = shuffled[*cursor..*cursor + count].to_vec();
        *cursor += count;
        taken
    };
    let measured_a = take(&mut cursor, MEASURED_SET_SIZE);
    let measured_b = take(&mut cursor, MEASURED_SET_SIZE);
    let remainder = shuffled.len() - cursor;
    let pool_a = take(&mut cursor, remainder / 2);
    let pool_b = take(&mut cursor, remainder - remainder / 2);

    let mut sides = std::collections::BTreeMap::new();
    for (side, measured, pool) in [("a", &measured_a, &pool_a), ("b", &measured_b, &pool_b)] {
        let mut by_count = std::collections::BTreeMap::new();
        for &count in AGENT_COUNTS {
            let background: Vec<Vec<String>> = if count > 1 {
                (0..count - 1)
                    .map(|index| pool.iter().skip(index).step_by(count - 1).cloned().collect())
                    .collect()
            } else {
                Vec::new()
            };
            by_count.insert(
                count.to_string(),
                WorkingSets {
                    measured: measured.clone(),
                    background,
                },
            );
        }
        sides.insert(side.to_owned(), by_count);
    }

    let partitions = Partitions {
        schema: SCHEMA,
        seed: SEED,
        sides,
    };
    verify(&partitions)?;
    std::fs::write(
        output,
        serde_json::to_vec(&partitions).expect("serializable"),
    )
    .map_err(|error| error.to_string())?;
    println!(
        "{}: partitions for {:?} agents written and verified disjoint",
        root.display(),
        AGENT_COUNTS
    );
    Ok(())
}

pub fn verify_file(path: &Path) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|error| error.to_string())?;
    let partitions: Partitions =
        serde_json::from_slice(&data).map_err(|error| error.to_string())?;
    if partitions.schema != SCHEMA {
        return Err(format!(
            "partitions schema {} but this binary expects {SCHEMA}",
            partitions.schema
        ));
    }
    verify(&partitions)?;
    println!("verified disjoint");
    Ok(())
}

/// The disjointness invariants, checked exhaustively:
/// within one (side, count) the measured set intersects no background set
/// and background sets intersect pairwise nowhere; across sides, nothing
/// on A appears anywhere on B.
fn verify(partitions: &Partitions) -> Result<(), String> {
    let mut all: std::collections::BTreeMap<&str, HashSet<&String>> = Default::default();
    for (side, by_count) in &partitions.sides {
        // The design's central invariant: the measured working set is the
        // same at every agent count, so agent count varies load only.
        let measured_sets: Vec<&Vec<String>> =
            by_count.values().map(|sets| &sets.measured).collect();
        if measured_sets.windows(2).any(|pair| pair[0] != pair[1]) {
            return Err(format!("side {side}: measured set varies with agent count"));
        }
        let side_all = all.entry(side.as_str()).or_default();
        for (count, sets) in by_count {
            let measured: HashSet<&String> = sets.measured.iter().collect();
            let mut seen: HashSet<&String> = measured.clone();
            for (index, background) in sets.background.iter().enumerate() {
                for file in background {
                    if measured.contains(file) {
                        return Err(format!(
                            "{side}/{count}: background {index} overlaps the measured set at {file}"
                        ));
                    }
                    if !seen.insert(file) {
                        return Err(format!(
                            "{side}/{count}: {file} appears in two background sets"
                        ));
                    }
                }
            }
            side_all.extend(seen);
        }
    }
    if let (Some(a), Some(b)) = (all.get("a"), all.get("b")) {
        if let Some(overlap) = a.intersection(b).next() {
            return Err(format!("cross-side overlap at {overlap}"));
        }
    }
    Ok(())
}
