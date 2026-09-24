//! What the supervisor writes to its log, and how much of it.
//!
//! The log is the only account of a session that outlives the cycle it
//! describes. A transient error — staging failing to produce content, a
//! connection dropping mid-frame — clears itself before anyone reads the
//! status page, so the log is where the evidence has to be. Two things
//! were missing for that: a time on every line, and a level that can be
//! turned up when something is being chased.
//!
//! The level is read once and held in an atomic, because the decision is
//! taken on every logged line from every worker thread and must not cost
//! a lock.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// How much the supervisor says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Errors only. What a supervisor with a working fleet should produce.
    Quiet = 0,
    /// Errors, plus one line per cycle that changed something. The default,
    /// and what the service has always written.
    Normal = 1,
    /// Everything above, plus the detail needed to explain a cycle after
    /// it has gone: timings, what was asked for, what arrived, and what
    /// the supervisor decided to do next.
    Debug = 2,
}

impl Level {
    /// Parses a level name, for the environment variable and the config.
    pub fn parse(name: &str) -> Option<Level> {
        match name.trim().to_ascii_lowercase().as_str() {
            "quiet" | "error" | "errors" => Some(Level::Quiet),
            "normal" | "info" => Some(Level::Normal),
            "debug" | "verbose" | "trace" => Some(Level::Debug),
            _ => None,
        }
    }

    /// The name this level is written as.
    pub fn name(self) -> &'static str {
        match self {
            Level::Quiet => "quiet",
            Level::Normal => "normal",
            Level::Debug => "debug",
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Normal as u8);

/// Sets the level. `AUTOBAHN_LOG` wins over the configured value, so a
/// level can be turned up for one run without editing anything — which is
/// the state someone is in when they are chasing something.
pub fn set_level(configured: Option<Level>) {
    let level = std::env::var("AUTOBAHN_LOG")
        .ok()
        .and_then(|name| Level::parse(&name))
        .or(configured)
        .unwrap_or(Level::Normal);
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The level in force.
pub fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Quiet,
        2 => Level::Debug,
        _ => Level::Normal,
    }
}

/// Whether a line at this level would be written. Checked before the
/// arguments are formatted, so debug lines cost nothing when off.
pub fn enabled(wanted: Level) -> bool {
    level() >= wanted
}

/// The local time, as every log line is stamped.
///
/// Local rather than UTC because the reader is a person looking for what
/// happened when they saw a notification, and seconds rather than
/// milliseconds because cycles are measured in seconds.
pub fn timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as libc::time_t)
        .unwrap_or(0);
    // SAFETY: `localtime_r` writes into a `tm` this call owns, and
    // `strftime` writes at most `buffer.len()` bytes including the
    // terminator. Neither keeps a reference past the call.
    unsafe {
        let mut parts: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&seconds, &mut parts).is_null() {
            return String::new();
        }
        let mut buffer = [0u8; 32];
        let written = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%Y-%m-%d %H:%M:%S".as_ptr(),
            &parts,
        );
        String::from_utf8_lossy(&buffer[..written]).into_owned()
    }
}

/// Set once a log line could not be written, and never cleared: the
/// supervisor says so where `status` can show it, since the log itself is
/// the one place that cannot.
static FAILED: AtomicBool = AtomicBool::new(false);

/// Whether a log line has failed to be written since this process started
/// — standard output closed under `watch | head`, or the disk under the
/// service log full.
pub fn failed() -> bool {
    FAILED.load(Ordering::Relaxed)
}

/// Where a line goes.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum Stream {
    Out,
    Err,
}

/// Writes one stamped line, as the macros below do. A line that cannot be
/// written costs that line and sets [`failed`] — never the thread that
/// wrote it, which is a session's worker.
#[doc(hidden)]
pub fn emit(stream: Stream, marker: &str, arguments: std::fmt::Arguments) {
    let line = log_line(&timestamp(), marker, &arguments.to_string());
    let written = match stream {
        Stream::Out => write_line(&mut std::io::stdout().lock(), &line),
        Stream::Err => write_line(&mut std::io::stderr().lock(), &line),
    };
    if !written {
        FAILED.store(true, Ordering::Relaxed);
    }
}

/// One log line: the stamp, the marker, and the message with its control
/// characters escaped. A message carries names and errors from elsewhere —
/// a file name may hold a newline or an escape sequence — and one event is
/// always one line, which nothing in it can split, forge or repaint.
fn log_line(stamp: &str, marker: &str, message: &str) -> String {
    format!("{stamp} {marker}{}\n", crate::text::display_safe(message))
}

/// Writes a line and flushes it, saying whether both worked.
fn write_line(out: &mut dyn Write, line: &str) -> bool {
    out.write_all(line.as_bytes()).is_ok() && out.flush().is_ok()
}

/// Writes one line at `Normal`, stamped with the time.
#[macro_export]
macro_rules! note {
    ($($argument:tt)*) => {
        {
            if $crate::logging::enabled($crate::logging::Level::Normal) {
                $crate::logging::emit(
                    $crate::logging::Stream::Out,
                    "",
                    format_args!($($argument)*),
                );
            }
        }
    };
}

/// Writes one line at `Debug`, stamped with the time and marked, so the
/// extra detail is filterable back out of a log that has both.
#[macro_export]
macro_rules! debug {
    ($($argument:tt)*) => {
        {
            if $crate::logging::enabled($crate::logging::Level::Debug) {
                $crate::logging::emit(
                    $crate::logging::Stream::Out,
                    "debug: ",
                    format_args!($($argument)*),
                );
            }
        }
    };
}

/// Writes one line to standard error, stamped. Errors are never silenced:
/// `Quiet` is the floor, not a way to lose them.
#[macro_export]
macro_rules! complain {
    ($($argument:tt)*) => {
        {
            $crate::logging::emit(
                $crate::logging::Stream::Err,
                "",
                format_args!($($argument)*),
            )
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_round_trip() {
        for level in [Level::Quiet, Level::Normal, Level::Debug] {
            assert_eq!(Level::parse(level.name()), Some(level));
        }
    }

    #[test]
    fn the_usual_spellings_are_accepted() {
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse(" verbose "), Some(Level::Debug));
        assert_eq!(Level::parse("info"), Some(Level::Normal));
        assert_eq!(Level::parse("errors"), Some(Level::Quiet));
        assert_eq!(Level::parse("loud"), None);
    }

    /// Ordering is what `enabled` is built on: a higher level includes
    /// everything a lower one would have said.
    #[test]
    fn a_higher_level_includes_the_lower_ones() {
        assert!(Level::Debug > Level::Normal);
        assert!(Level::Normal > Level::Quiet);
    }

    /// A writer standing in for a full disk, or a closed pipe.
    struct Failing;

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no space left on device"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("no space left on device"))
        }
    }

    #[test]
    fn a_line_that_cannot_be_written_is_lost_without_a_panic() {
        assert!(!write_line(&mut Failing, "lost\n"));
        let mut kept = Vec::new();
        assert!(write_line(&mut kept, "kept\n"));
        assert_eq!(kept, b"kept\n");
    }

    #[test]
    fn one_event_is_one_line_whatever_its_names_hold() {
        let name = "report\n2026-09-24 07:00:00 [work@host] synchronized\x1b]52;c;cHduZWQ=\x07.txt";
        let line = log_line(
            "2026-09-24 07:00:01",
            "",
            &format!("[work@host] conflict: {name}"),
        );
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        assert!(line.ends_with('\n'));
        assert!(!line.contains('\x1b') && !line.contains('\x07'), "{line:?}");
        assert!(
            line.contains("report\\n2026"),
            "the newline reads as an escape: {line:?}"
        );
        assert_eq!(
            log_line("2026-09-24 07:00:01", "debug: ", "plain"),
            "2026-09-24 07:00:01 debug: plain\n"
        );
    }

    #[test]
    fn a_timestamp_is_a_readable_local_time() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 19, "YYYY-MM-DD HH:MM:SS");
        assert!(stamp.starts_with("20"));
    }
}
