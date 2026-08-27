//! The destination-side verifier.
//!
//! One connection per measuring agent; newline-delimited JSON. Every
//! verification request runs on its own worker thread and replies are
//! multiplexed back by sequence number, so the writer can keep multiple
//! measured edits in flight — a slow verification never blocks the next
//! one, which is what keeps the workload open-loop.
//!
//! Requests:
//!   {"seq", "path", "digest", "size", "deadline_s"}   verify: acknowledge
//!         when the path holds exactly the announced bytes.
//!   {"seq", "op": "ping"}                             round-trip probe.
//!   {"seq", "op": "floor_arm", "path", "payload_hex"} stage a floor probe:
//!         remember the payload and start a verify worker for it.
//!   {"seq", "op": "floor_write"}                      write the staged
//!         payload (buffered atomic rename, as a tool's destination write
//!         is); the already-armed verify worker acknowledges when it sees
//!         the content. The writer times floor_write → acknowledgement,
//!         which mirrors the workload's measured interval (detection +
//!         verification + ack return) plus one inbound trip and the write
//!         itself — a stated, bounded overestimate.
//!
//! Responses: {"seq", "ok"} plus "reason" on failure. Replies are not
//! ordered; the writer matches by seq.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

/// The detection poll: fine-grained at first, backing off once a wait
/// is clearly not about to resolve. The backoff bounds verifier
/// contention — with many slow verifications in flight, fine polling
/// would put tens of thousands of metadata reads per second on the
/// destination host, a load correlated with exactly the tool being
/// measured. The added detection error after backoff is at most half of
/// POLL_SLOW — well under a millisecond against the multi-second
/// latencies that reach that state. The floor reports realized totals.
const POLL_FAST: Duration = Duration::from_micros(500);
const POLL_SLOW: Duration = Duration::from_millis(1);
const POLL_BACKOFF_AFTER: Duration = Duration::from_secs(1);

pub fn serve(port: u16) -> Result<(), String> {
    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|error| format!("bind: {error}"))?;
    println!("observer listening on {port}");
    for connection in listener.incoming() {
        let Ok(connection) = connection else { continue };
        let _ = connection.set_nodelay(true);
        std::thread::spawn(move || {
            let _ = handle(connection);
        });
    }
    Ok(())
}

/// A staged floor probe: where to write, and what.
type StagedProbes = HashMap<u64, (PathBuf, Vec<u8>)>;

fn handle(connection: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(connection.try_clone()?);
    let writer = Arc::new(Mutex::new(connection));
    // Floor probes staged by floor_arm, waiting for their floor_write.
    let staged: Arc<Mutex<StagedProbes>> = Default::default();
    // Set when the connection ends, so verify workers stop polling
    // instead of surviving into the next phase or the next tool.
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Any exit — clean EOF or a read error — must mark the connection
    // closed, or verify workers would outlive an abnormal disconnect
    // and poll into the next phase.
    while let Ok(Some(request)) = crate::read_message(&mut reader) {
        let seq = request["seq"].as_u64().unwrap_or(0);
        match request["op"].as_str() {
            Some("ping") => {
                reply(&writer, seq, true, None);
            }
            Some("floor_arm") => {
                let path = PathBuf::from(request["path"].as_str().unwrap_or_default());
                match crate::from_hex(request["payload_hex"].as_str().unwrap_or_default()) {
                    Ok(payload) => {
                        let digest = blake3::hash(&payload).to_hex().to_string();
                        let size = payload.len() as u64;
                        staged
                            .lock()
                            .expect("staged lock")
                            .insert(seq, (path.clone(), payload));
                        // The verify worker is armed *now*, before the
                        // write exists — exactly as a workload verify is
                        // armed before the tool can have propagated.
                        let writer = Arc::clone(&writer);
                        let closed = Arc::clone(&closed);
                        std::thread::spawn(move || {
                            let ok = await_content(
                                &path,
                                &digest,
                                size,
                                Duration::from_secs(30),
                                &closed,
                            );
                            reply(&writer, seq, ok, (!ok).then_some("deadline"));
                        });
                    }
                    Err(error) => reply(&writer, seq, false, Some(&error)),
                }
            }
            Some("floor_write") => {
                // No reply of its own: the armed verify worker's reply is
                // the acknowledgement the writer is timing.
                if let Some((path, payload)) = staged.lock().expect("staged lock").remove(&seq) {
                    let _ = write_buffered(&path, &payload);
                }
            }
            _ => {
                let path = PathBuf::from(request["path"].as_str().unwrap_or_default());
                let digest = request["digest"].as_str().unwrap_or_default().to_owned();
                let size = request["size"].as_u64().unwrap_or(0);
                let deadline =
                    Duration::from_secs_f64(request["deadline_s"].as_f64().unwrap_or(180.0));
                let writer = Arc::clone(&writer);
                let closed = Arc::clone(&closed);
                std::thread::spawn(move || {
                    let ok = await_content(&path, &digest, size, deadline, &closed);
                    reply(&writer, seq, ok, (!ok).then_some("deadline"));
                });
            }
        }
    }
    closed.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

fn reply(writer: &Arc<Mutex<TcpStream>>, seq: u64, ok: bool, reason: Option<&str>) {
    let mut message = json!({"seq": seq, "ok": ok});
    if let Some(reason) = reason {
        message["reason"] = json!(reason);
    }
    let mut guard = writer.lock().expect("writer lock");
    let _ = crate::send_message(&mut *guard, &message);
}

/// The floor's stand-in for "the tool wrote the destination file": atomic
/// rename without fsync, as tools' destination writes are.
fn write_buffered(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = PathBuf::from(format!("{}.floor-tmp", path.display()));
    std::fs::write(&temporary, payload)?;
    std::fs::rename(&temporary, path)
}

/// Polls until the path holds exactly the expected content. The size gate
/// keeps the loop from hashing files mid-write; the digest is the verdict —
/// a size match alone never acknowledges, so a tool that writes in place
/// (or stages then renames) cannot produce a false early acknowledgement:
/// wrong or partial bytes hash wrong, and the loop simply polls again.
fn await_content(
    path: &Path,
    expected_digest: &str,
    expected_size: u64,
    deadline: Duration,
    closed: &std::sync::atomic::AtomicBool,
) -> bool {
    let start = Instant::now();
    let end = start + deadline;
    while Instant::now() < end && !closed.load(std::sync::atomic::Ordering::Relaxed) {
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.len() == expected_size {
                if let Ok(digest) = crate::digest_file(path) {
                    if digest.to_hex().to_string() == expected_digest {
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(if start.elapsed() < POLL_BACKOFF_AFTER {
            POLL_FAST
        } else {
            POLL_SLOW
        });
    }
    false
}
