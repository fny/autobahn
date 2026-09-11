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

use std::sync::atomic::{AtomicU8, Ordering};

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

/// Writes one line at `Normal`, stamped with the time.
#[macro_export]
macro_rules! note {
    ($($argument:tt)*) => {
        {
            if $crate::logging::enabled($crate::logging::Level::Normal) {
                println!("{} {}", $crate::logging::timestamp(), format_args!($($argument)*));
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
                println!("{} debug: {}", $crate::logging::timestamp(), format_args!($($argument)*));
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
            eprintln!("{} {}", $crate::logging::timestamp(), format_args!($($argument)*))
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

    #[test]
    fn a_timestamp_is_a_readable_local_time() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 19, "YYYY-MM-DD HH:MM:SS");
        assert!(stamp.starts_with("20"));
    }
}
