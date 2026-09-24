//! The edit workload: N simulated coding agents, one of which measures.
//!
//! **Open-loop.** The measuring agent issues edits on its cadence whether
//! or not earlier edits have been acknowledged: a reader thread matches
//! acknowledgements to in-flight edits by sequence number. A slow tool
//! therefore faces the *same* offered load as a fast one — a closed loop
//! would let latency throttle the load and flatter exactly the tool being
//! measured. Files with an edit still in flight are skipped for new edits,
//! so each in-flight verification watches a stable target.
//!
//! **Non-replayable payloads.** The payload RNG is seeded from a
//! caller-supplied nonce (unique per run/job/tool, recorded in results),
//! so no earlier run can have placed a payload this run is about to
//! announce — without this, a deterministic seed plus an unrestored source
//! tree lets the observer acknowledge content that predates the measured
//! write.
//!
//! **Censoring.** An edit unacknowledged within the deadline *after its
//! own T0* is censored: counted, bounded below by the deadline, included
//! in percentile positions as "over deadline" — never silently dropped. A
//! tool that sometimes fails to propagate must not score better for it.
//!
//! Background agents are threads with their own disjoint working sets and
//! report achieved edit counts, so undelivered load is visible.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::Rng;

const EDIT_INTERVAL_MS: (u64, u64) = (300, 1200);
const EDIT_SIZE: (usize, usize) = (2048, 65536);
const WARMUP: Duration = Duration::from_secs(10);
const DEADLINE: Duration = Duration::from_secs(120);
/// The lead between announcing an edit and performing it, so the verify
/// worker is armed before the content can possibly arrive.
const ANNOUNCE_LEAD: Duration = Duration::from_millis(50);
/// In-flight measured edits are bounded; with a 40-file measured set and
/// a ~0.75s cadence, 32 in flight means a tool ~24s behind — beyond that
/// new edits skip ticks (recorded) rather than queue without bound.
const MAX_IN_FLIGHT: usize = 32;
/// The region rewritten by a patch-mode edit. Small against the files it
/// lands in, so that what the tool transfers is a fair question rather
/// than a foregone one.
const PATCH_SIZE: (usize, usize) = (1024, 16 * 1024);
/// How long a destination has to answer the opening ping. A destination
/// that accepts a connection and then says nothing must fail the run, not
/// stall it indefinitely.
const PING_TIMEOUT: Duration = Duration::from_secs(30);

struct Options {
    root: PathBuf,
    peer_root: PathBuf,
    observer: String,
    partitions: PathBuf,
    side: String,
    agents: usize,
    seconds: u64,
    label: String,
    nonce: u64,
    /// `replace` rewrites a small file with fresh content; `patch` rewrites
    /// a region of a large one, leaving the rest intact.
    mode: Mode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Replace,
    Patch,
}

fn parse(arguments: &[&str]) -> Result<Options, String> {
    let mut map = std::collections::HashMap::new();
    let mut iterator = arguments.iter();
    while let Some(flag) = iterator.next() {
        let value = iterator
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        map.insert(flag.trim_start_matches("--").to_owned(), (*value).to_owned());
    }
    let take = |key: &str| -> Result<String, String> {
        map.get(key)
            .cloned()
            .ok_or_else(|| format!("--{key} is required"))
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
        nonce: take("nonce")?.parse().map_err(|_| "--nonce".to_owned())?,
        mode: match map.get("mode").map(String::as_str).unwrap_or("replace") {
            "replace" => Mode::Replace,
            "patch" => Mode::Patch,
            other => return Err(format!("unknown --mode {other}")),
        },
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
    // In patch mode the measuring agent works the large files; the
    // background agents keep to their own small ones, so load is generated
    // the same way in both modes and only the measured edit differs.
    let measured_set: Vec<String> = match options.mode {
        Mode::Replace => sets.measured.clone(),
        Mode::Patch => {
            if sets.large.is_empty() {
                return Err(format!(
                    "patch mode needs large files, and side {} has none — \
                     the corpus does not hold any of the required size",
                    options.side
                ));
            }
            sets.large.clone()
        }
    };
    if sets.background.len() != options.agents - 1 {
        return Err(format!(
            "partitions file has {} background sets for {} agents",
            sets.background.len(),
            options.agents
        ));
    }

    // Background agents: threads with achieved-load accounting. A write
    // failure is counted, not swallowed — an agent that stopped editing
    // would silently reduce the offered load.
    let stop = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let edits = Arc::new(AtomicU64::new(0));
    let write_errors = Arc::new(AtomicU64::new(0));
    // Epoch milliseconds of the last *completed* background write. A
    // worker can be inside write_atomic when the stop flag flips, so the
    // window end reported below is extended to cover that final rename —
    // otherwise the tool work it triggers would fall just outside the
    // advertised resource window.
    let last_write_end = Arc::new(AtomicU64::new(0));
    let mut workers = Vec::new();
    for (index, files) in sets.background.iter().enumerate() {
        let files: Vec<PathBuf> = files.iter().map(|f| options.root.join(f)).collect();
        let stop = Arc::clone(&stop);
        let go = Arc::clone(&go);
        let edits = Arc::clone(&edits);
        let write_errors = Arc::clone(&write_errors);
        let last_write_end = Arc::clone(&last_write_end);
        let mut rng = Rng::new(options.nonce ^ (0x9000 + index as u64));
        workers.push(std::thread::spawn(move || {
            // Editing starts only when the measuring agent opens its
            // window — otherwise background load precedes
            // window_start_epoch (observer connection and control pings
            // happen first, and a stall there would extend the gap
            // unboundedly), and the resource window would not cover the
            // whole offered load.
            while !go.load(Ordering::Relaxed) {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut payload = vec![0u8; EDIT_SIZE.1];
            while !stop.load(Ordering::Relaxed) {
                let size = EDIT_SIZE.0 + rng.index(EDIT_SIZE.1 - EDIT_SIZE.0);
                rng.fill(&mut payload[..size]);
                let file = &files[rng.index(files.len())];
                match crate::write_atomic(file, &payload[..size]) {
                    Ok(()) => edits.fetch_add(1, Ordering::Relaxed),
                    Err(_) => write_errors.fetch_add(1, Ordering::Relaxed),
                };
                last_write_end.fetch_max((epoch_seconds() * 1000.0) as u64, Ordering::Relaxed);
                let pause = EDIT_INTERVAL_MS.0
                    + rng.index((EDIT_INTERVAL_MS.1 - EDIT_INTERVAL_MS.0) as usize) as u64;
                std::thread::sleep(Duration::from_millis(pause));
            }
        }));
    }

    let report = measure(&options, &measured_set, &go, &stop);
    stop.store(true, Ordering::Relaxed);
    let mut background_panics = 0u64;
    for worker in workers {
        if worker.join().is_err() {
            background_panics += 1;
        }
    }
    let mut report = report?;
    // Workers are joined by now, so this covers every completed write.
    let background_end = last_write_end.load(Ordering::Relaxed) as f64 / 1000.0;
    if let Some(end) = report["window_end_epoch"].as_f64() {
        if background_end > end {
            report["window_end_epoch"] = json!(background_end);
        }
    }
    report["background_edits"] = json!(edits.load(Ordering::Relaxed));
    report["background_write_errors"] = json!(write_errors.load(Ordering::Relaxed));
    // A panicked worker means the offered load was not what this report
    // claims; the aggregator treats a nonzero count as a problem.
    report["background_panics"] = json!(background_panics);
    println!("{}", serde_json::to_string(&report).expect("serializable"));
    Ok(())
}

/// One in-flight measured edit, across every destination it must reach.
///
/// A fan-out edit is not finished when *a* destination has it — it is
/// finished when they all do. So the entry survives until the last
/// acknowledgement, and the sample is measured to that one. The first
/// acknowledgement is kept too, because the gap between first and last is
/// the interesting thing about a fan-out: it says whether destinations
/// converge together or straggle.
struct InFlight {
    file_index: usize,
    started: Instant,
    /// Whether T0 fell inside the warmup window.
    warmup: bool,
    /// Acknowledgements still outstanding, one per destination.
    remaining: usize,
    /// When the first destination confirmed the content.
    first_ack: Option<Instant>,
}

fn measure(
    options: &Options,
    measured_set: &[String],
    go_background: &AtomicBool,
    stop_background: &AtomicBool,
) -> Result<serde_json::Value, String> {
    // One connection per destination. A pairwise cell passes one address;
    // a fan-out cell passes one per beta.
    let addresses: Vec<&str> = options
        .observer
        .split(',')
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .collect();
    if addresses.is_empty() {
        return Err("--observer needs at least one address".into());
    }
    let destinations = addresses.len();

    let mut writers = Vec::with_capacity(destinations);
    let mut readers = Vec::with_capacity(destinations);
    let mut shutdown_handles = Vec::with_capacity(destinations);
    for address in &addresses {
        let connection = TcpStream::connect(address)
            .map_err(|error| format!("observer {address}: {error}"))?;
        let _ = connection.set_nodelay(true);
        let reader_stream = connection.try_clone().map_err(|e| e.to_string())?;
        // The reader wakes periodically to check for shutdown: a blocked
        // read would otherwise pin the thread forever, because dropping
        // the writer clone does not close a socket the reader still holds.
        reader_stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|error| error.to_string())?;
        shutdown_handles.push(connection.try_clone().map_err(|e| e.to_string())?);
        readers.push(BufReader::new(reader_stream));
        writers.push(connection);
    }

    // Control-channel round trips, taken exactly like a sample would be.
    // The slowest destination is the one that bounds a fan-out edit, so the
    // reported round trip is the worst of them.
    let mut rtts = Vec::new();
    for seq in 0..20u64 {
        let start = Instant::now();
        for writer in writers.iter_mut() {
            crate::send_message(writer, &json!({"seq": seq, "op": "ping"}))
                .map_err(|error| error.to_string())?;
        }
        for (index, reader) in readers.iter_mut().enumerate() {
            // Bounded: a destination that accepts the connection but never
            // answers must fail the run rather than hang it. With ten
            // destinations that is no longer a remote possibility, and a
            // silent hang would look like a slow benchmark rather than a
            // broken one.
            let give_up = Instant::now() + PING_TIMEOUT;
            loop {
                if Instant::now() > give_up {
                    return Err(format!(
                        "observer {} never answered a ping within {:?}",
                        addresses[index], PING_TIMEOUT
                    ));
                }
                match crate::read_message(reader) {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        return Err(format!("observer {} closed during ping", addresses[index]))
                    }
                    Err(error) if is_timeout(&error) => continue,
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        rtts.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    // The reader thread matches acknowledgements to in-flight edits and
    // records latencies; the main thread issues edits on its cadence.
    let in_flight: Arc<Mutex<HashMap<u64, InFlight>>> = Default::default();
    let busy: Arc<Mutex<std::collections::HashSet<usize>>> = Default::default();
    // (ms to the last destination, ms between first and last, warmup)
    let outcomes: Arc<Mutex<Vec<(f64, f64, bool)>>> = Default::default();
    let censored = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    // One reader per destination. They share the in-flight table, so an
    // edit is retired by whichever thread delivers its final
    // acknowledgement — no thread owns an edit, and none has to wait on
    // another.
    let mut reader_threads = Vec::with_capacity(destinations);
    for mut reader in readers {
        let reader_in_flight = Arc::clone(&in_flight);
        let reader_busy = Arc::clone(&busy);
        let reader_outcomes = Arc::clone(&outcomes);
        let reader_censored = Arc::clone(&censored);
        let reader_done = Arc::clone(&done);
        reader_threads.push(std::thread::spawn(move || {
            while !reader_done.load(Ordering::Relaxed) {
                let message = match crate::read_message(&mut reader) {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(error) if is_timeout(&error) => continue,
                    Err(_) => break,
                };
                let seq = message["seq"].as_u64().unwrap_or(u64::MAX);
                let accepted = message["ok"].as_bool() == Some(true);
                let mut table = reader_in_flight.lock().expect("in-flight lock");
                let Some(entry) = table.get_mut(&seq) else {
                    continue;
                };
                let now = Instant::now();
                entry.first_ack.get_or_insert(now);
                // A refusal from any destination fails the whole edit: the
                // content did not reach everywhere it was meant to.
                if !accepted {
                    let entry = table.remove(&seq).expect("just looked it up");
                    drop(table);
                    reader_busy.lock().expect("busy lock").remove(&entry.file_index);
                    if !entry.warmup {
                        reader_censored.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                entry.remaining -= 1;
                if entry.remaining > 0 {
                    continue; // other destinations still owe an answer
                }
                let entry = table.remove(&seq).expect("just looked it up");
                drop(table);
                reader_busy.lock().expect("busy lock").remove(&entry.file_index);
                let elapsed = entry.started.elapsed();
                let spread = entry
                    .first_ack
                    .map(|first| now.duration_since(first).as_secs_f64() * 1000.0)
                    .unwrap_or(0.0);
                // The deadline is judged here too: an acknowledgement that
                // arrives after the deadline is censored, not a sample —
                // otherwise a late ack racing the periodic sweep would be
                // reported as a finite latency.
                if elapsed <= DEADLINE {
                    reader_outcomes.lock().expect("outcomes lock").push((
                        elapsed.as_secs_f64() * 1000.0,
                        spread,
                        entry.warmup,
                    ));
                } else if !entry.warmup {
                    reader_censored.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    let mut rng = Rng::new(options.nonce ^ 0x9999);
    let mut payload = vec![0u8; EDIT_SIZE.1];
    let started = Instant::now();
    let window_start_epoch = epoch_seconds();
    go_background.store(true, Ordering::Relaxed);
    let window = Duration::from_secs(options.seconds);
    let mut skipped_ticks = 0usize;
    let mut sequence = 0u64;

    while started.elapsed() < window {
        // Sweep in-flight edits past their deadline into the censored
        // count, freeing their files.
        {
            let mut in_flight = in_flight.lock().expect("in-flight lock");
            let mut busy_set = busy.lock().expect("busy lock");
            let expired: Vec<u64> = in_flight
                .iter()
                .filter(|(_, entry)| entry.started.elapsed() > DEADLINE)
                .map(|(&seq, _)| seq)
                .collect();
            for seq in expired {
                if let Some(entry) = in_flight.remove(&seq) {
                    busy_set.remove(&entry.file_index);
                    if !entry.warmup {
                        censored.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // Pick a file with no verification in flight.
        let choice = {
            let busy_set = busy.lock().expect("busy lock");
            let in_flight_count = in_flight.lock().expect("in-flight lock").len();
            if in_flight_count >= MAX_IN_FLIGHT {
                None
            } else {
                (0..8)
                    .map(|_| rng.index(measured_set.len()))
                    .find(|index| !busy_set.contains(index))
            }
        };
        match choice {
            None => skipped_ticks += 1,
            Some(file_index) => {
                let relative = &measured_set[file_index];
                // Replace mode writes fresh content over a small file, so
                // the destination's base shares nothing with it and the
                // whole file transfers. Patch mode rewrites a region of a
                // large file and leaves the rest, which is the only shape
                // of edit a delta algorithm can exploit — and the shape a
                // person editing a file actually produces.
                let content: Vec<u8> = match options.mode {
                    Mode::Replace => {
                        let size = EDIT_SIZE.0 + rng.index(EDIT_SIZE.1 - EDIT_SIZE.0);
                        rng.fill(&mut payload[..size]);
                        payload[..size].to_vec()
                    }
                    Mode::Patch => {
                        let path = options.root.join(relative);
                        let mut existing =
                            std::fs::read(&path).map_err(|error| {
                                format!("unable to read {} for patching: {error}", path.display())
                            })?;
                        if existing.len() <= PATCH_SIZE.1 {
                            return Err(format!(
                                "{} is {} bytes, too small to patch",
                                path.display(),
                                existing.len()
                            ));
                        }
                        let length = PATCH_SIZE.0 + rng.index(PATCH_SIZE.1 - PATCH_SIZE.0);
                        let offset = rng.index(existing.len() - length);
                        rng.fill(&mut payload[..length]);
                        existing[offset..offset + length]
                            .copy_from_slice(&payload[..length]);
                        existing
                    }
                };
                let size = content.len();
                let digest = blake3::hash(&content).to_hex().to_string();
                // Registration precedes the announcement, so no reply can
                // ever find its sequence unknown. The provisional start is
                // refined to the true T0 the moment the local write
                // completes; a reply consuming the entry in that gap (a
                // tool faster than a local fsync, effectively impossible)
                // would measure from the announcement — an overestimate,
                // which is the conservative direction.
                let warmup = started.elapsed() < WARMUP;
                busy.lock().expect("busy lock").insert(file_index);
                in_flight.lock().expect("in-flight lock").insert(
                    sequence,
                    InFlight {
                        file_index,
                        started: Instant::now(),
                        warmup,
                        remaining: destinations,
                        first_ack: None,
                    },
                );
                let announcement = json!({
                    "seq": sequence,
                    "path": options.peer_root.join(relative).to_string_lossy(),
                    "digest": digest,
                    "size": size,
                    // The observer's own deadline is generous; the
                    // writer's own clock decides censoring.
                    "deadline_s": DEADLINE.as_secs() + 60,
                });
                for writer in writers.iter_mut() {
                    crate::send_message(writer, &announcement)
                        .map_err(|error| error.to_string())?;
                }
                std::thread::sleep(ANNOUNCE_LEAD);
                crate::write_atomic(&options.root.join(relative), &content)
                    .map_err(|error| error.to_string())?;
                // T0: the local write is complete and the content is the
                // tool's to propagate.
                if let Some(entry) = in_flight
                    .lock()
                    .expect("in-flight lock")
                    .get_mut(&sequence)
                {
                    entry.started = Instant::now();
                    // Warmup is a property of the true T0, not of the
                    // tick that scheduled the edit.
                    entry.warmup = started.elapsed() < WARMUP;
                }
                sequence += 1;
            }
        }
        let pause = EDIT_INTERVAL_MS.0
            + rng.index((EDIT_INTERVAL_MS.1 - EDIT_INTERVAL_MS.0) as usize) as u64;
        std::thread::sleep(Duration::from_millis(pause));
    }

    // The offered load ends here: background agents stop *now*, so the
    // drain that follows adds no editing activity and the resource
    // window can end with the load window instead of a tool-dependent
    // drain of up to the full deadline.
    stop_background.store(true, Ordering::Relaxed);
    let window_end_epoch = epoch_seconds();

    // Drain: give stragglers up to the deadline, then classify.
    let drain_deadline = Instant::now() + DEADLINE;
    while Instant::now() < drain_deadline {
        if in_flight.lock().expect("in-flight lock").is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    {
        let mut in_flight = in_flight.lock().expect("in-flight lock");
        for (_, entry) in in_flight.drain() {
            if !entry.warmup {
                censored.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    done.store(true, Ordering::Relaxed);
    for handle in &shutdown_handles {
        let _ = handle.shutdown(std::net::Shutdown::Both);
    }
    drop(writers);
    for thread in reader_threads {
        let _ = thread.join();
    }

    let censored = censored.load(Ordering::Relaxed) as usize;
    let recorded = outcomes.lock().expect("outcomes lock").clone();
    let mut spreads: Vec<f64> = recorded
        .iter()
        .filter(|(_, _, warmup)| !warmup)
        .map(|(_, spread, _)| *spread)
        .collect();
    spreads.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let mut samples: Vec<f64> = recorded
        .iter()
        .filter(|(_, _, warmup)| !warmup)
        .map(|(ms, _, _)| *ms)
        .collect();
    let warmup_samples = recorded.len() - samples.len();
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));

    // Percentiles over *attempts*: censored edits occupy the top
    // positions at ">= deadline". A percentile landing in that region is
    // reported as null with the flag set, never as a finite number.
    let attempts = samples.len() + censored;
    let percentile = |fraction: f64| -> serde_json::Value {
        if attempts == 0 {
            return serde_json::Value::Null;
        }
        let position = (fraction * (attempts - 1) as f64 * 10.0).round() / 10.0;
        let index = position.round() as usize;
        if index < samples.len() {
            json!((samples[index] * 10.0).round() / 10.0)
        } else {
            serde_json::Value::Null // in the censored region
        }
    };
    let mut sorted_rtts = rtts.clone();
    sorted_rtts.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));

    Ok(json!({
        "label": options.label,
        "side": options.side,
        "agents": options.agents,
        "seconds": options.seconds,
        "nonce": options.nonce,
        "destinations": destinations,
        // How far apart the first and last destination confirmed the same
        // edit. Zero for a single destination; for a fan-out it is the
        // straggle, which is what a fan-out is really being asked about.
        "spread_p50_ms": spreads.get(spreads.len() / 2).map(|v| (v * 10.0).round() / 10.0),
        "spread_max_ms": spreads.last().map(|v| (v * 10.0).round() / 10.0),
        "window_start_epoch": window_start_epoch,
        "window_end_epoch": window_end_epoch,
        "samples": samples.len(),
        "warmup_samples": warmup_samples,
        "censored": censored,
        "censored_over_ms": if censored > 0 { Some(DEADLINE.as_millis() as u64) } else { None },
        "skipped_ticks": skipped_ticks,
        "attempts": attempts,
        "control_rtt_ms_p50": (sorted_rtts[sorted_rtts.len() / 2] * 1000.0).round() / 1000.0,
        "p50_ms": percentile(0.50),
        "p90_ms": percentile(0.90),
        "p99_ms": percentile(0.99),
        "min_ms": samples.first().map(|v| (v * 10.0).round() / 10.0),
        "max_ms": samples.last().map(|v| (v * 10.0).round() / 10.0),
        "mean_ms": if samples.is_empty() { None } else {
            Some((samples.iter().sum::<f64>() / samples.len() as f64 * 10.0).round() / 10.0)
        },
        "samples_ms": samples.iter().map(|v| (v * 10.0).round() / 10.0).collect::<Vec<f64>>(),
    }))
}

/// Distinguishes a read-timeout wakeup from a real failure; both
/// WouldBlock and TimedOut appear depending on platform.
fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// The harness floor. Mirrors the workload's measured interval: a verify
/// worker is armed first (`floor_arm`, untimed), then after the announce
/// lead the writer times `floor_write` → acknowledgement. Relative to a
/// workload sample this adds one inbound network trip and the observer's
/// own buffered write — a bounded overestimate, in the conservative
/// direction (the floor can only be reported too high, never too low).
pub fn floor(arguments: &[&str]) -> Result<(), String> {
    let mut observer = None;
    let mut nonce = 0u64;
    let mut iterator = arguments.iter();
    while let Some(flag) = iterator.next() {
        let value = iterator
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        match *flag {
            "--observer" => observer = Some((*value).to_owned()),
            "--nonce" => nonce = value.parse().map_err(|_| "--nonce".to_owned())?,
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    let observer = observer.ok_or("--observer is required")?;

    let connection =
        TcpStream::connect(&observer).map_err(|error| format!("observer {observer}: {error}"))?;
    let _ = connection.set_nodelay(true);
    let mut reader = BufReader::new(connection.try_clone().map_err(|e| e.to_string())?);
    let mut writer = connection;

    let mut rng = Rng::new(nonce ^ 0xF100);
    let mut payload = vec![0u8; EDIT_SIZE.1];
    let mut samples = Vec::new();
    let mut failures = 0u32;
    for seq in 0..50u64 {
        let size = EDIT_SIZE.0 + rng.index(EDIT_SIZE.1 - EDIT_SIZE.0);
        rng.fill(&mut payload[..size]);
        // Relative: the observer writes it beneath its own --root.
        crate::send_message(
            &mut writer,
            &json!({
                "seq": seq,
                "op": "floor_arm",
                "path": format!("floor-probe/file-{seq}.dat"),
                "payload_hex": crate::to_hex(&payload[..size]),
            }),
        )
        .map_err(|error| error.to_string())?;
        std::thread::sleep(ANNOUNCE_LEAD);
        let start = Instant::now();
        crate::send_message(&mut writer, &json!({"seq": seq, "op": "floor_write"}))
            .map_err(|error| error.to_string())?;
        let response = crate::read_message(&mut reader)
            .map_err(|error| error.to_string())?
            .ok_or("observer closed the connection")?;
        if response["ok"].as_bool() == Some(true) {
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        } else {
            failures += 1;
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    // Percentiles over *attempts*: failed probes occupy the top
    // positions, exactly as censored workload edits do, so a percentile
    // landing among failures reports null rather than a flattering
    // finite number computed from the successes alone.
    let attempts = samples.len() + failures as usize;
    let value = |fraction: f64| -> Option<f64> {
        if attempts == 0 {
            return None;
        }
        let index = (fraction * (attempts - 1) as f64).round() as usize;
        if index < samples.len() {
            Some((samples[index] * 100.0).round() / 100.0)
        } else {
            None
        }
    };
    println!(
        "{}",
        json!({
            "measurement": "floor",
            "samples": samples.len(),
            "failures": failures,
            "attempts": attempts,
            "p50_ms": value(0.50),
            "p90_ms": value(0.90),
            "max_ms": if failures > 0 {
                None // the true maximum is censored above the deadline
            } else {
                samples.last().map(|v| (v * 100.0).round() / 100.0)
            },
        })
    );
    Ok(())
}

/// Wall-clock seconds, for phase windows correlated with the sampler.
fn epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
