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
//!         remember the payload and start a verify worker for it. The
//!         path is relative, and lands under the observer's `--root`.
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
//!
//! The protocol is unauthenticated, and `floor_write` writes a file, so
//! the observer listens on loopback unless told otherwise (`--listen`;
//! the fleet passes 0.0.0.0 and relies on its security group), writes
//! only beneath `--root` and never through a symlink, and bounds what one
//! client can make it hold: request size, staged probes, connections.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

/// The detection poll: fine-grained at first, backing off once a wait
/// is clearly not about to resolve. The backoff bounds verifier
/// contention — with many slow verifications in flight, fine polling
/// would put tens of thousands of metadata reads per second on the
/// destination host, a load correlated with exactly the tool being
/// measured. After backoff the expected added detection delay is half
/// of POLL_SLOW and the worst case is one full interval — ~1ms against
/// the multi-second latencies that reach that state. The floor reports
/// realized totals.
const POLL_FAST: Duration = Duration::from_micros(500);
const POLL_SLOW: Duration = Duration::from_millis(1);
const POLL_BACKOFF_AFTER: Duration = Duration::from_secs(1);

/// The largest floor payload accepted. Workload edits are at most 64 KiB.
const MAX_PAYLOAD: usize = 1 << 20;
/// The longest request line: a maximal payload, hex-encoded, and room.
const MAX_REQUEST: u64 = 2 * MAX_PAYLOAD as u64 + 4096;
/// Floor probes one connection may have armed and not yet written.
const MAX_STAGED: usize = 64;
/// Simultaneous connections. One per measuring agent, per observer.
const MAX_CONNECTIONS: usize = 512;

/// `observer <port> --root <dir> [--listen <addr>]`.
pub fn run(arguments: &[&str]) -> Result<(), String> {
    let (port, rest) = arguments.split_first().ok_or("observer needs a port")?;
    let port: u16 = port.parse().map_err(|_| format!("bad port {port}"))?;
    let mut listen = "127.0.0.1".to_owned();
    let mut root = None;
    let mut iterator = rest.iter();
    while let Some(flag) = iterator.next() {
        let value = iterator
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        match *flag {
            "--listen" => listen = (*value).to_owned(),
            "--root" => root = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    let root = root.ok_or("--root is required")?;
    let listener = TcpListener::bind((listen.as_str(), port))
        .map_err(|error| format!("bind {listen}:{port}: {error}"))?;
    println!("observer listening on {listen}:{port}");
    serve(listener, Arc::new(root))
}

fn serve(listener: TcpListener, root: Arc<PathBuf>) -> Result<(), String> {
    let open = Arc::new(AtomicUsize::new(0));
    for connection in listener.incoming() {
        let Ok(connection) = connection else { continue };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            continue; // dropped, which closes it
        }
        let _ = connection.set_nodelay(true);
        let root = Arc::clone(&root);
        let open = Arc::clone(&open);
        std::thread::spawn(move || {
            let _ = handle(connection, &root);
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// Where a floor probe named `relative` lands: beneath `root`, or nowhere.
/// The name must be relative and plain — no root, no prefix, no `..`, no
/// `.` — and no directory on the way down may be a symlink, so nothing a
/// tool or a client left inside the root can redirect the write out of it.
/// The final component is safe by construction: the write renames over
/// it, which replaces a symlink rather than following it.
fn floor_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative);
    let mut path = root.to_path_buf();
    let mut components = relative.components().peekable();
    if components.peek().is_none() {
        return Err("empty floor path".into());
    }
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(format!(
                "floor path {} is not a plain relative path",
                relative.display()
            ));
        };
        path.push(name);
        if components.peek().is_some() {
            if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                if !metadata.is_dir() {
                    return Err(format!(
                        "floor path {} passes through a non-directory",
                        relative.display()
                    ));
                }
            }
        }
    }
    Ok(path)
}

/// A staged floor probe: where to write, and what.
type StagedProbes = HashMap<u64, (PathBuf, Vec<u8>)>;

fn handle(connection: TcpStream, root: &Path) -> std::io::Result<()> {
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
    while let Ok(Some(request)) = read_bounded(&mut reader) {
        let seq = request["seq"].as_u64().unwrap_or(0);
        match request["op"].as_str() {
            Some("ping") => {
                reply(&writer, seq, true, None);
            }
            Some("floor_arm") => {
                let staged_count = staged.lock().expect("staged lock").len();
                let armed = floor_path(root, request["path"].as_str().unwrap_or_default())
                    .and_then(|path| {
                        if staged_count >= MAX_STAGED {
                            return Err("too many floor probes staged".into());
                        }
                        let payload =
                            crate::from_hex(request["payload_hex"].as_str().unwrap_or_default())?;
                        if payload.len() > MAX_PAYLOAD {
                            return Err("floor payload too large".into());
                        }
                        Ok((path, payload))
                    });
                match armed {
                    Ok((path, payload)) => {
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
                    let _ = write_buffered(root, &path, &payload);
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

/// Reads one request, refusing a line longer than `MAX_REQUEST` so that
/// one client cannot make the observer buffer without limit.
fn read_bounded<R: std::io::BufRead>(reader: &mut R) -> std::io::Result<Option<serde_json::Value>> {
    use std::io::Read;
    let mut limited = reader.take(MAX_REQUEST);
    let message = crate::read_message(&mut limited)?;
    if limited.limit() == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request too long",
        ));
    }
    Ok(message)
}

/// The floor's stand-in for "the tool wrote the destination file": atomic
/// rename without fsync, as tools' destination writes are. `path` came
/// from `floor_path`; its directories are made one at a time and checked
/// again here, since the tree may have changed since the probe was armed,
/// and the temporary is created fresh, so a symlink already standing in
/// its place fails the write instead of redirecting it.
fn write_buffered(root: &Path, path: &Path, payload: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let refuse = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, why.to_owned());
    let relative = path
        .strip_prefix(root)
        .map_err(|_| refuse("outside root"))?;
    let mut directory = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            directory.push(component);
            match std::fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Err(refuse("floor path passes through a non-directory")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&directory) {
                        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                            return Err(error)
                        }
                        _ => {}
                    }
                    if !std::fs::symlink_metadata(&directory)?.is_dir() {
                        return Err(refuse("floor path passes through a non-directory"));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    let temporary = PathBuf::from(format!("{}.floor-tmp", path.display()));
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(payload)?;
    drop(file);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::net::{Ipv4Addr, UdpSocket};

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("benchmark-observer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch");
        path
    }

    /// An observer on loopback, rooted at `root`; returns its address.
    fn start(root: &Path) -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let address = listener.local_addr().expect("address");
        let root = Arc::new(root.to_path_buf());
        std::thread::spawn(move || serve(listener, root));
        address
    }

    /// Arms and writes one floor probe; returns the observer's reply.
    fn probe(address: std::net::SocketAddr, path: &str) -> serde_json::Value {
        let connection = TcpStream::connect(address).expect("connect");
        let mut reader = BufReader::new(connection.try_clone().expect("clone"));
        let mut writer = connection;
        let arm = json!({"seq": 1, "op": "floor_arm", "path": path, "payload_hex": "6869"});
        crate::send_message(&mut writer, &arm).expect("arm");
        crate::send_message(&mut writer, &json!({"seq": 1, "op": "floor_write"})).expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("reply");
        serde_json::from_str(&line).expect("json")
    }

    #[test]
    fn a_floor_probe_lands_beneath_the_root() {
        let root = scratch("lands");
        let reply = probe(start(&root), "floor-probe/file-1.dat");
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(
            std::fs::read(root.join("floor-probe/file-1.dat")).unwrap(),
            b"hi"
        );
    }

    #[test]
    fn a_floor_probe_outside_the_root_is_refused() {
        let root = scratch("outside").join("root");
        std::fs::create_dir_all(&root).unwrap();
        let address = start(&root);
        let outside = root.parent().unwrap().join("x");
        for path in [
            outside.to_str().unwrap(),
            "/etc/x",
            "../x",
            "a/../../x",
            "./x",
            "",
        ] {
            let reply = probe(address, path);
            assert_eq!(reply["ok"], false, "{path}: {reply}");
        }
        assert!(!outside.exists());
        assert!(!root.parent().unwrap().join("x").exists());
    }

    #[test]
    fn a_floor_probe_through_a_symlink_in_the_root_is_refused() {
        let base = scratch("symlink");
        let root = base.join("root");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("link")).unwrap();
        let reply = probe(start(&root), "link/x");
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(!elsewhere.join("x").exists());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn a_directory_swapped_for_a_symlink_after_arming_is_not_followed() {
        let base = scratch("swap");
        let root = base.join("root");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let path = floor_path(&root, "d/x").expect("plain path");
        std::os::unix::fs::symlink(&elsewhere, root.join("d")).unwrap();
        assert!(write_buffered(&root, &path, b"hi").is_err());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn an_oversized_request_ends_the_connection() {
        let root = scratch("oversized");
        let connection = TcpStream::connect(start(&root)).expect("connect");
        let mut reader = BufReader::new(connection.try_clone().unwrap());
        let mut writer = connection;
        let chunk = vec![b'a'; 1 << 16];
        let mut sent = 0u64;
        while sent <= MAX_REQUEST {
            if writer.write_all(&chunk).is_err() {
                break;
            }
            sent += chunk.len() as u64;
        }
        let mut line = String::new();
        // The observer hangs up rather than buffering on: EOF or reset.
        assert!(matches!(reader.read_line(&mut line), Ok(0) | Err(_)));
    }

    #[test]
    fn by_default_the_observer_is_not_reachable_off_loopback() {
        // This host's own non-loopback address, found without sending
        // anything: connecting a UDP socket only chooses a route.
        let Some(address) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|socket| {
                socket
                    .connect((Ipv4Addr::new(192, 0, 2, 1), 9))
                    .map(|_| socket)
            })
            .and_then(|socket| socket.local_addr())
            .ok()
            .map(|address| address.ip())
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        else {
            eprintln!("no non-loopback address on this host; nothing to check");
            return;
        };
        let port = {
            let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            probe.local_addr().unwrap().port()
        };
        let root = scratch("loopback");
        let port_text = port.to_string();
        let root_text = root.to_string_lossy().into_owned();
        std::thread::spawn(move || run(&[&port_text, "--root", &root_text]));
        let reached_loopback = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok()
        });
        assert!(reached_loopback, "the observer never started");
        assert!(
            TcpStream::connect_timeout(&(address, port).into(), Duration::from_secs(1)).is_err()
        );
    }

    #[test]
    fn the_observer_needs_a_root() {
        assert!(run(&["0"]).is_err());
    }
}
