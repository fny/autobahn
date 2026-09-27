//! The blocked entries a cycle records, taken apart again.
//!
//! The supervisor writes them as `side path: message`, one string per
//! entry, because that is what a recorded status can hold. Every surface
//! that lists them — `issues`, the shop, the window — wants the three
//! parts back, and each one reading the string its own way is three
//! chances to read it differently.

/// The path inside a recorded blocked entry.
///
/// The entries are written as `side path: message` by the supervisor.
/// Splitting them back apart is what lets the listing group by cause and
/// scope by path.
pub fn path(entry: &str) -> Option<&str> {
    let rest = entry.split_once(' ')?.1;
    Some(match rest.find(": ") {
        Some(end) => &rest[..end],
        None => rest,
    })
}

/// The side, path and cause of a recorded blocked entry.
///
/// The cause is the innermost message. The wrapping context repeats the
/// file's own path, so twenty files that failed for one reason would
/// otherwise read as twenty separate reasons.
pub fn parts(entry: &str) -> (&str, &str, &str) {
    let (side, rest) = entry.split_once(' ').unwrap_or(("", entry));
    let (path, message) = match rest.find(": ") {
        Some(end) => (&rest[..end], &rest[end + 2..]),
        None => (rest, ""),
    };
    let cause = message.rsplit(": ").next().unwrap_or(message);
    (side, path, cause)
}

