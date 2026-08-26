//! The edit workload: N simulated coding agents, one of which measures.
//!
//! Working sets come from the partitions file baked into the image — the
//! measuring agent's files are identical at every agent count, so agent
//! count varies load and nothing else. Background agents are threads, not
//! processes: at 100 agents the entire workload is one small process, so
//! the harness cannot meaningfully compete with the tools under test for
//! memory, and barely for CPU.
//!
//! Each agent rewrites one of its files every 0.3–1.2 seconds with 2–64KB
//! of fresh incompressible bytes — a coding cadence, not a build. The
//! measuring agent announces each edit's exact content to the observer on
//! the receiving host, writes, and measures from the completion of its
//! local rename to the observer's verified acknowledgement — both
//! timestamps from this host's monotonic clock, so no cross-host skew can
//! enter. The interval includes the observer's detection, verification
//! read, and the acknowledgement's return trip: every sample is an upper
//! bound on the tool's own propagation, and the `floor` subcommand
//! measures how much of that upper bound is harness.
//!
//! Samples in the warmup window are recorded but excluded from
//! percentiles. An edit exceeding its deadline is censored — counted, its
//! lower bound recorded — never dropped and never fatal.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::Rng;

const EDIT_INTERVAL_MS: (u64, u64) = (300, 1200);
const EDIT_SIZE: (usize, usize) = (2048, 65536);
const WARMUP: Duration = Duration::from_secs(10);
const DEADLINE: Duration = Duration::from_secs(120);
/// The lead between announcing an edit and performing it, so the observer
/// is armed before the content can arrive. Outside the measured interval
/// (measurement starts after the local write), but bounds the cadence.
const ANNOUNCE_LEAD: Duration = Duration::from_millis(50);

struct Options {
    root: PathBuf,
    peer_root: PathBuf,
    observer: String,
    partitions: PathBuf,
    side: String,
    agents: usize,
    seconds: u64,
    label: String,
}

fn parse(arguments: &[&str]) -> Result<Options, String> {
    let mut map = std::collections::HashMap::new();
    let mut iterator = arguments.iter();
    while let Some(flag) = iterator.next() {
        let value = iterator.next().ok_or_else(|| format!("{flag} needs a value"))?;
        map.insert(flag.trim_start_matches("--").to_owned(), (*value).to_owned());
    }
    let take = |key: &str| -> Result<String, String> {
        map.get(key).cloned().ok_or_else(|| format!("--{key} is required"))
    };
    Ok(Options {
        root: PathBuf::from(take("root")?),
        peer_root: PathBuf::from(take("peer-root")?),
        observer: take("observer")?,
        partitions: PathBuf::from(take("partitions")?),
        side: take("side")?,
        agents: take("agents")?.parse().map_err(|_| "--agents".to_owned())?,
        seconds: take("seconds")?.parse().map_err(|_| "--seconds".to_owned())?,
        label: take("label")?,
    })
}

pub fn run(arguments: &[&str]) -> Result<(), String> {
    let options = parse(arguments)?;
    let data = std::fs::read(&options.partitions).map_err(|error| error.to_string())?;
    let partitions: crate::partitions::Partitions =
        serde_json::from_slice(&data).map_err(|error| error.to_string())?;
    let sets = partitions
        .sides
        .get(&options.side)
        .and_then(|by_count| by_count.get(&options.agents.to_string()))
        .ok_or_else(|| {
            format!(
                "partitions file has no sets for side {} at {} agents",
                options.side, options.agents
            )
        })?;
    if sets.background.len() != options.agents - 1 {
        return Err(format!(
            "partitions file has {} background sets for {} agents",
            sets.background.len(),
            options.agents
        ));
    }

    // Background agents: plain threads on their own cadence. They start
    // before measurement so the load exists from the first sample.
    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for (index, files) in sets.background.iter().enumerate() {
        let files: Vec<PathBuf> = files.iter().map(|f| options.root.join(f)).collect();
        let stop = Arc::clone(&stop);
        let mut rng = Rng::new(0x9000 + index as u64);
        workers.push(std::thread::spawn(move || {
            let mut payload = vec![0u8; EDIT_SIZE.1];
            while !stop.load(Ordering::Relaxed) {
                let size = EDIT_SIZE.0 + rng.index(EDIT_SIZE.1 - EDIT_SIZE.0);
                rng.fill(&mut payload[..size]);
                let file = &files[rng.index(files.len())];
                let _ = crate::write_atomic(file, &payload[..size]);
                let pause = EDIT_INTERVAL_MS.0 + rng.index((EDIT_INTERVAL_MS.1 - EDIT_INTERVAL_MS.0) as usize) as u64;
                std::thread::sleep(Duration::from_millis(pause));
            }
        }));
    }

    let report = measure(&options, &sets.measured);
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        let _ = worker.join();
    }
    println!("{}", serde_json::to_string(&report?).expect("serializable"));
    Ok(())
}

fn measure(options: &Options, measured_set: &[String]) -> Result<serde_json::Value, String> {
    let connection = TcpStream::connect(&options.observer)
        .map_err(|error| format!("observer {}: {error}", options.observer))?;
    let _ = connection.set_nodelay(true);
    connection
        .set_read_timeout(Some(DEADLINE + Duration::from_secs(30)))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(connection.try_clone().map_err(|e| e.to_string())?);
    let mut writer = connection;

    // Control-channel round trips, taken exactly like a sample.
    let mut rtts = Vec::new();
    for seq in 0..20 {
        let start = Instant::now();
        crate::send_message(&mut writer, &json!({"seq": seq, "op": "ping"}))
            .map_err(|error| error.to_string())?;
        crate::read_message(&mut reader).map_err(|error| error.to_string())?;
        rtts.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let mut rng = Rng::new(0x9999);
    let mut payload = vec![0u8; EDIT_SIZE.1];
    let started = Instant::now();
    let window = Duration::from_secs(options.seconds);
    let mut samples: Vec<f64> = Vec::new();
    let mut warmup_samples = 0usize;
    let mut censored = 0usize;
    let mut sequence = 0u64;

    while started.elapsed() < window {
        let relative = &measured_set[rng.index(measured_set.len())];
        let size = EDIT_SIZE.0 + rng.index(EDIT_SIZE.1 - EDIT_SIZE.0);
        rng.fill(&mut payload[..size]);
        let digest = blake3::hash(&payload[..size]).to_hex().to_string();
        crate::send_message(
            &mut writer,
            &json!({
                "seq": sequence,
                "path": options.peer_root.join(relative).to_string_lossy(),
                "digest": digest,
                "size": size,
                "deadline_s": DEADLINE.as_secs(),
            }),
        )
        .map_err(|error| error.to_string())?;
        std::thread::sleep(ANNOUNCE_LEAD);
        crate::write_atomic(&options.root.join(relative), &payload[..size])
            .map_err(|error| error.to_string())?;
        let start = Instant::now();
        let response = crate::read_message(&mut reader)
            .map_err(|error| error.to_string())?
            .ok_or("observer closed the connection")?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        if response["ok"].as_bool() == Some(true) {
            if start.duration_since(started) < WARMUP {
                warmup_samples += 1;
            } else {
                samples.push(elapsed);
            }
        } else {
            // Censored, not discarded: a tool that sometimes never
            // propagates must not score better for it.
            censored += 1;
        }
        sequence += 1;
        let pause = EDIT_INTERVAL_MS.0
            + rng.index((EDIT_INTERVAL_MS.1 - EDIT_INTERVAL_MS.0) as usize) as u64;
        std::thread::sleep(Duration::from_millis(pause));
    }

    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let percentile = |fraction: f64| -> Option<f64> {
        if samples.is_empty() {
            return None;
        }
        let index = ((fraction * (samples.len() - 1) as f64).round() as usize)
            .min(samples.len() - 1);
        Some((samples[index] * 10.0).round() / 10.0)
    };
    let mut sorted_rtts = rtts.clone();
    sorted_rtts.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    Ok(json!({
        "label": options.label,
        "side": options.side,
        "agents": options.agents,
        "seconds": options.seconds,
        "samples": samples.len(),
        "warmup_samples": warmup_samples,
        "censored": censored,
        "censored_over_ms": if censored > 0 { Some(DEADLINE.as_millis() as u64) } else { None },
        "control_rtt_ms_p50": (sorted_rtts[sorted_rtts.len() / 2] * 1000.0).round() / 1000.0,
        "p50_ms": percentile(0.50),
        "p90_ms": percentile(0.90),
        "p99_ms": percentile(0.99),
        "min_ms": percentile(0.0),
        "max_ms": percentile(1.0),
        "mean_ms": if samples.is_empty() { None } else {
            Some((samples.iter().sum::<f64>() / samples.len() as f64 * 10.0).round() / 10.0)
        },
        "samples_ms": samples.iter().map(|v| (v * 10.0).round() / 10.0).collect::<Vec<f64>>(),
    }))
}

/// The harness floor: announce → the observer writes the payload itself →
/// detection → verification → acknowledgement, with no synchronization
/// tool anywhere. Reported per pair so that small latencies can be read
/// net of the harness.
pub fn floor(arguments: &[&str]) -> Result<(), String> {
    let mut observer = None;
    let mut destination = None;
    let mut iterator = arguments.iter();
    while let Some(flag) = iterator.next() {
        let value = iterator.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match *flag {
            "--observer" => observer = Some((*value).to_owned()),
            "--dest-root" => destination = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    let observer = observer.ok_or("--observer is required")?;
    let destination = destination.ok_or("--dest-root is required")?;

    let connection =
        TcpStream::connect(&observer).map_err(|error| format!("observer {observer}: {error}"))?;
    let _ = connection.set_nodelay(true);
    let mut reader = BufReader::new(connection.try_clone().map_err(|e| e.to_string())?);
    let mut writer = connection;

    let mut rng = Rng::new(0xF100);
    let mut payload = vec![0u8; EDIT_SIZE.1];
    let mut samples = Vec::new();
    for seq in 0..50 {
        let size = EDIT_SIZE.0 + rng.index(EDIT_SIZE.1 - EDIT_SIZE.0);
        rng.fill(&mut payload[..size]);
        let path: PathBuf = destination.join(format!("floor-probe/file-{seq}.dat"));
        let start = Instant::now();
        crate::send_message(
            &mut writer,
            &json!({
                "seq": seq,
                "op": "floor",
                "path": path.to_string_lossy(),
                "payload_hex": crate::to_hex(&payload[..size]),
            }),
        )
        .map_err(|error| error.to_string())?;
        let response = crate::read_message(&mut reader)
            .map_err(|error| error.to_string())?
            .ok_or("observer closed the connection")?;
        if response["ok"].as_bool() == Some(true) {
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let value = |fraction: f64| -> Option<f64> {
        if samples.is_empty() {
            return None;
        }
        let index = ((fraction * (samples.len() - 1) as f64).round() as usize)
            .min(samples.len() - 1);
        Some((samples[index] * 100.0).round() / 100.0)
    };
    println!(
        "{}",
        json!({
            "measurement": "floor",
            "samples": samples.len(),
            "p50_ms": value(0.50),
            "p90_ms": value(0.90),
            "max_ms": value(1.0),
        })
    );
    Ok(())
}
