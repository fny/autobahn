//! The destination-side verifier.
//!
//! One thread per connection, newline-delimited JSON. For each announced
//! edit the observer polls the named path until it holds exactly the
//! announced bytes, then acknowledges. The writer holds both clocks; the
//! observer's whole job is to answer truthfully and fast — its poll
//! interval and hash cost are inside every measured latency, which is why
//! they are small here and why the `floor` operation exists to measure
//! them.
//!
//! Requests:
//!   {"seq", "path", "digest", "size", "deadline_s"}   verify propagation
//!   {"seq", "op": "ping"}                             round-trip probe
//!   {"seq", "op": "floor", "path", "payload_hex"}     the observer writes
//!         the payload itself (same atomic-write path as the workload),
//!         then detects and verifies it exactly as it would a sync tool's
//!         output — the harness measured with no tool in the loop.
//!
//! Responses: {"seq", "ok"} plus "reason" on failure.

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

/// The detection poll. Small enough that its expected contribution
/// (half the interval) is a fraction of a millisecond; the floor phase
/// reports the realized total rather than this theoretical one.
const POLL: Duration = Duration::from_micros(500);

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

fn handle(connection: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(connection.try_clone()?);
    let mut writer = connection;
    while let Some(request) = crate::read_message(&mut reader)? {
        let seq = request["seq"].clone();
        let response = match request["op"].as_str() {
            Some("ping") => json!({"seq": seq, "ok": true}),
            Some("floor") => {
                let path = request["path"].as_str().unwrap_or_default().to_owned();
                match crate::from_hex(request["payload_hex"].as_str().unwrap_or_default()) {
                    Ok(payload) => {
                        let digest = blake3::hash(&payload).to_hex().to_string();
                        let size = payload.len() as u64;
                        match write_buffered(Path::new(&path), &payload) {
                            Ok(()) => {
                                let ok = await_content(
                                    Path::new(&path),
                                    &digest,
                                    size,
                                    Duration::from_secs(30),
                                );
                                json!({"seq": seq, "ok": ok})
                            }
                            Err(error) =>

                                json!({"seq": seq, "ok": false, "reason": error.to_string()}),
                        }
                    }
                    Err(error) => json!({"seq": seq, "ok": false, "reason": error}),
                }
            }
            _ => {
                let path = request["path"].as_str().unwrap_or_default().to_owned();
                let digest = request["digest"].as_str().unwrap_or_default().to_owned();
                let size = request["size"].as_u64().unwrap_or(0);
                let deadline =
                    Duration::from_secs_f64(request["deadline_s"].as_f64().unwrap_or(120.0));
                let ok = await_content(Path::new(&path), &digest, size, deadline);
                if ok {
                    json!({"seq": seq, "ok": true})
                } else {
                    json!({"seq": seq, "ok": false, "reason": "deadline"})
                }
            }
        };
        crate::send_message(&mut writer, &response)?;
    }
    Ok(())
}

/// The floor's stand-in for "the tool wrote the destination file":
/// atomic rename, but *without* fsync — synchronization tools buffer their
/// destination writes, and a floor that pays a durability cost the tools
/// don't would overstate the harness's share of every measurement.
fn write_buffered(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = std::path::PathBuf::from(format!("{}.floor-tmp", path.display()));
    std::fs::write(&temporary, payload)?;
    std::fs::rename(&temporary, path)
}

/// Polls until the path holds exactly the expected content. The size gate
/// keeps the poll loop from hashing partially written files; the digest
/// is the actual verdict — a size match alone never acknowledges.
fn await_content(path: &Path, expected_digest: &str, expected_size: u64, deadline: Duration) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.len() == expected_size {
                if let Ok(digest) = crate::digest_file(path) {
                    if digest.to_hex().to_string() == expected_digest {
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(POLL);
    }
    false
}
