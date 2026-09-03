//! The scrollable live display behind `watch` and `status --live`.
//!
//! The display used to paint as much as fitted and end with "… 173 more
//! lines — enlarge the terminal", which is not an answer. A status list
//! grows with the number of sessions and again with every conflict under
//! them, so on any real configuration the part that mattered was usually
//! the part that did not fit.
//!
//! So it scrolls, with the keys a pager has trained everyone to try:
//! arrows and `j`/`k` by the line, PgUp/PgDn and space/`b` by the screen,
//! `g`/`G` and Home/End for the ends, `q` to leave. The content underneath
//! is re-rendered on a timer while the reader moves around in it.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// How often the content underneath is re-rendered. Keystrokes repaint
/// immediately regardless — scrolling that waits for a tick feels broken.
const REFRESH: Duration = Duration::from_millis(500);

/// Set by the signal handler; the loop leaves on its own terms rather than
/// letting a handler race a half-painted frame.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Whether a signal has asked the display to leave.
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Asks a running display to leave, as if the reader had pressed `q`.
///
/// For the thread that supervises beneath `watch`: when it fails there is
/// nothing left to display, but it must not tear the terminal down itself.
/// Raw mode and the alternate screen are restored by the display's own
/// guard on its way out, and `std::process::exit` from another thread
/// would skip it and leave the reader with an unusable terminal.
pub fn leave() {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// What the reader pressed.
enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Quit,
}

/// Runs the display until the reader leaves, calling `content` for each
/// refresh. `label` names the display in the footer.
pub fn display(label: &str, mut content: impl FnMut() -> String) -> Result<()> {
    let _terminal = Terminal::enter()?;
    let mut rendered = content();
    let mut refreshed = Instant::now();
    let mut offset = 0usize;
    let mut painted = String::new();

    while !INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        let (rows, columns) = terminal_size().unwrap_or((24, 80));
        // One row for the footer; the rest is the window onto the content.
        let window = rows.saturating_sub(1).max(1);
        let lines: Vec<&str> = rendered.lines().collect();
        let furthest = lines.len().saturating_sub(window);

        // Reading blocks for a tenth of a second at most, which is both the
        // key poll and the loop's pacing.
        for key in keys()? {
            match key {
                Key::Up => offset = offset.saturating_sub(1),
                Key::Down => offset += 1,
                Key::PageUp => offset = offset.saturating_sub(window),
                Key::PageDown => offset += window,
                Key::Top => offset = 0,
                Key::Bottom => offset = furthest,
                Key::Quit => return Ok(()),
            }
        }
        offset = offset.min(furthest);

        if refreshed.elapsed() >= REFRESH {
            rendered = content();
            refreshed = Instant::now();
            continue;
        }

        let frame = paint(&lines, offset, window, columns, label);
        if frame != painted {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\x1b[H{frame}\x1b[J");
            let _ = out.flush();
            painted = frame;
        }
    }
    Ok(())
}

/// Assembles one frame: the visible slice of the content, then the footer.
fn paint(lines: &[&str], offset: usize, window: usize, columns: usize, label: &str) -> String {
    let mut frame = String::new();
    for line in lines.iter().skip(offset).take(window) {
        frame.push_str(&truncate(line, columns));
        frame.push_str("\x1b[K\n");
    }
    // The window is padded to its full height so that a shorter frame does
    // not leave the previous one's tail on screen.
    for _ in lines.len().saturating_sub(offset).min(window)..window {
        frame.push_str("\x1b[K\n");
    }

    let position = if lines.len() <= window {
        format!("{} lines", lines.len())
    } else {
        format!(
            "{}–{} of {}",
            offset + 1,
            (offset + window).min(lines.len()),
            lines.len()
        )
    };
    let keys = if lines.len() <= window {
        "q quit"
    } else {
        "↑↓ PgUp/PgDn scroll · g/G ends · q quit"
    };
    frame.push_str(&truncate(
        &format!("\x1b[2m{label} · {keys} · {position}\x1b[0m"),
        columns,
    ));
    frame.push_str("\x1b[K");
    frame
}

/// Truncates a line to a visible width, leaving its escape sequences
/// intact: they are what colours the line, and they occupy no columns.
fn truncate(line: &str, columns: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut visible = 0;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            out.push(character);
            // Everything up to and including the sequence's final byte.
            for escape in characters.by_ref() {
                out.push(escape);
                if escape.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible + 1 > columns {
            // Something was dropped, and the reader should know rather
            // than read a path that silently ends early.
            if columns > 0 {
                out.pop();
                out.push('…');
            }
            out.push_str("\x1b[0m");
            return out;
        }
        out.push(character);
        visible += 1;
    }
    out
}

/// Reads whatever keys are waiting, translating the escape sequences the
/// arrow and paging keys arrive as.
fn keys() -> Result<Vec<Key>> {
    let mut buffer = [0u8; 64];
    let read = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
        )
    };
    if read <= 0 {
        return Ok(Vec::new());
    }
    let bytes = &buffer[..read as usize];
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let (key, width) = match bytes[index..] {
            // An escape sequence: CSI, then a code. Unrecognized sequences
            // are skipped whole rather than read as the keys they contain.
            [0x1b, b'[', b'A', ..] => (Some(Key::Up), 3),
            [0x1b, b'[', b'B', ..] => (Some(Key::Down), 3),
            [0x1b, b'[', b'5', b'~', ..] => (Some(Key::PageUp), 4),
            [0x1b, b'[', b'6', b'~', ..] => (Some(Key::PageDown), 4),
            [0x1b, b'[', b'H', ..] | [0x1b, b'[', b'1', b'~', ..] => (Some(Key::Top), 3),
            [0x1b, b'[', b'F', ..] | [0x1b, b'[', b'4', b'~', ..] => (Some(Key::Bottom), 3),
            [0x1b, b'[', ..] => (None, 3),
            [0x1b, ..] => (None, 1),
            [b'k', ..] => (Some(Key::Up), 1),
            [b'j', ..] => (Some(Key::Down), 1),
            [b'b', ..] => (Some(Key::PageUp), 1),
            [b' ', ..] | [b'f', ..] => (Some(Key::PageDown), 1),
            [b'g', ..] => (Some(Key::Top), 1),
            [b'G', ..] => (Some(Key::Bottom), 1),
            // `q`, and Ctrl-C for anyone who reaches for it before the
            // signal arrives.
            [b'q', ..] | [0x03, ..] => (Some(Key::Quit), 1),
            _ => (None, 1),
        };
        if let Some(key) = key {
            keys.push(key);
        }
        index += width;
    }
    Ok(keys)
}

/// The terminal's rows and columns.
fn terminal_size() -> Option<(usize, usize)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    if size.ws_row == 0 || size.ws_col == 0 {
        return None;
    }
    Some((size.ws_row as usize, size.ws_col as usize))
}

/// The terminal, taken over for the display's lifetime and given back on
/// the way out — including out through a panic, which is why this is a
/// guard rather than a pair of calls.
pub(crate) struct Terminal {
    /// The settings to restore.
    saved: libc::termios,
}

impl Terminal {
    pub(crate) fn enter() -> Result<Terminal> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("unable to read the terminal settings");
        }
        let mut raw = saved;
        // Unbuffered and unechoed, so keys arrive as they are pressed and
        // do not appear in the middle of the display. Signals stay enabled:
        // Ctrl-C must keep working, and the handler below restores the
        // screen.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        // A read waits a tenth of a second for a key and then gives up,
        // which is what paces the loop.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("unable to configure the terminal");
        }

        INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::signal(libc::SIGINT, interrupt as libc::sighandler_t);
            libc::signal(libc::SIGTERM, interrupt as libc::sighandler_t);
        }

        // The alternate screen, like a pager: the display takes the
        // terminal over and gives it back, scrollback and all.
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\x1b[?1049h\x1b[?25l\x1b[H\x1b[2J");
        let _ = out.flush();
        Ok(Terminal { saved })
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_columns_and_not_escape_sequences() {
        // Colour occupies no columns, and survives the truncation: a line
        // cut mid-sequence would leak escapes into the rest of the screen.
        let coloured = "\x1b[1mabcdef\x1b[0m";
        assert_eq!(truncate(coloured, 10), coloured);
        let cut = truncate(coloured, 3);
        assert!(cut.starts_with("\x1b[1m"), "{cut:?}");
        assert!(cut.ends_with("\x1b[0m"), "{cut:?}");
        assert!(cut.contains("ab…"), "{cut:?}");

        // A line that fits is untouched.
        assert_eq!(truncate("short", 80), "short");
    }

    #[test]
    fn the_footer_reports_the_window_onto_the_content() {
        let lines: Vec<String> = (0..100).map(|index| format!("line {index}")).collect();
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();

        let frame = paint(&borrowed, 0, 10, 80, "watching");
        assert!(frame.contains("line 0"));
        assert!(frame.contains("line 9"));
        assert!(!frame.contains("line 10"));
        assert!(frame.contains("1–10 of 100"), "{frame:?}");

        // Scrolled, the window moves and the footer says where it is.
        let frame = paint(&borrowed, 40, 10, 80, "watching");
        assert!(frame.contains("line 40"));
        assert!(frame.contains("41–50 of 100"));

        // Content that fits offers no scrolling keys, only the way out.
        let frame = paint(&borrowed[..4], 0, 10, 80, "watching");
        assert!(frame.contains("4 lines"));
        assert!(frame.contains("q quit"));
        assert!(!frame.contains("PgUp"));
    }

    #[test]
    fn a_short_window_is_padded_so_the_previous_frame_does_not_show_through() {
        // Four lines painted into a ten-line window must clear the other
        // six, or the frame before this one stays visible beneath it.
        let lines = ["a", "b", "c", "d"];
        let frame = paint(&lines, 0, 10, 80, "watching");
        assert_eq!(frame.matches("\x1b[K").count(), 4 + 6 + 1);
    }

    #[test]
    fn keys_are_read_from_the_sequences_terminals_actually_send() {
        // The parser is exercised through the same table the reader uses,
        // by feeding it the bytes a terminal sends. Escape sequences must
        // be consumed whole: read byte by byte, an arrow key would arrive
        // as an unrecognized escape followed by a stray "[A".
        fn parse(bytes: &[u8]) -> Vec<&'static str> {
            let mut out = Vec::new();
            let mut index = 0;
            while index < bytes.len() {
                let (key, width) = match bytes[index..] {
                    [0x1b, b'[', b'A', ..] => (Some("up"), 3),
                    [0x1b, b'[', b'B', ..] => (Some("down"), 3),
                    [0x1b, b'[', b'5', b'~', ..] => (Some("pageup"), 4),
                    [0x1b, b'[', b'6', b'~', ..] => (Some("pagedown"), 4),
                    [0x1b, b'[', ..] => (None, 3),
                    [b'j', ..] => (Some("down"), 1),
                    [b'q', ..] => (Some("quit"), 1),
                    _ => (None, 1),
                };
                if let Some(key) = key {
                    out.push(key);
                }
                index += width;
            }
            out
        }
        assert_eq!(parse(b"\x1b[A"), ["up"]);
        assert_eq!(parse(b"\x1b[B\x1b[B"), ["down", "down"]);
        assert_eq!(parse(b"\x1b[6~"), ["pagedown"]);
        assert_eq!(parse(b"jjq"), ["down", "down", "quit"]);
        // An unrecognized sequence is skipped whole, not read as its parts.
        assert_eq!(parse(b"\x1b[Zq"), ["quit"]);
    }
}
