//! The application's own presence: the icon in the dock, and whether
//! there is one at all.
//!
//! Two questions that turn out to be one. A window and a menu bar item
//! are separate things a person may want separately — a menu bar with
//! no window, a window with no menu bar, or both — and on macOS asking
//! for a menu bar alone means asking not to be in the dock, which is
//! also asking to give up the badge. So they live together here.
//!
//! Everything below is macOS. On Linux the dock is whatever the desktop
//! provides and there is no portable way to write on it, so the stubs
//! do nothing and say so by doing nothing.

/// How much of itself the application shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    /// A window and an item in the menu bar. What it has always done.
    #[default]
    Both,
    /// A window, and nothing in the menu bar.
    Window,
    /// An item in the menu bar, and no window until it is asked for —
    /// and on macOS, no icon in the dock either.
    Menubar,
}

impl Presence {
    pub fn parse(word: &str) -> Option<Presence> {
        match word.trim() {
            "both" => Some(Presence::Both),
            "window" => Some(Presence::Window),
            "menubar" => Some(Presence::Menubar),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Presence::Both => "both",
            Presence::Window => "window",
            Presence::Menubar => "menubar",
        }
    }

    pub fn opens_a_window(self) -> bool {
        !matches!(self, Presence::Menubar)
    }

    pub fn takes_the_menu_bar(self) -> bool {
        !matches!(self, Presence::Window)
    }
}

/// Where the desk keeps what is its own.
///
/// Not in `config.toml`: that file is the fleet's, it is read by the
/// supervisor on every machine, and whether this window draws an icon
/// is nobody's business but this machine's.
pub fn preferences(state_root: &std::path::Path) -> std::path::PathBuf {
    state_root.join("desk.toml")
}

/// What the file says, or the default when it says nothing.
pub fn read(state_root: &std::path::Path) -> Presence {
    let Ok(text) = std::fs::read_to_string(preferences(state_root)) else {
        return Presence::default();
    };
    text.parse::<toml::Table>()
        .ok()
        .and_then(|table| {
            table
                .get("presence")
                .and_then(|value| value.as_str())
                .and_then(Presence::parse)
        })
        .unwrap_or_default()
}

/// Writes it back, and says why if it could not.
pub fn write(state_root: &std::path::Path, presence: Presence) -> Option<String> {
    let path = preferences(state_root);
    let text = format!(
        "# How much of itself Autobahn Desk shows: both, window, menubar.\n\
         presence = \"{}\"\n",
        presence.word()
    );
    std::fs::write(&path, text)
        .err()
        .map(|error| format!("{}: {error}", path.display()))
}

/// Takes the application out of the dock, or puts it back.
///
/// A menu bar application with an icon in the dock is two ways to reach
/// one window, and the dock one cannot be dismissed. `Accessory` is the
/// policy for a program that lives in the menu bar; `Regular` is the
/// one for a program that lives in a window.
#[cfg(target_os = "macos")]
pub fn in_the_dock(wanted: bool) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(mtm);
    let policy = match wanted {
        true => NSApplicationActivationPolicy::Regular,
        false => NSApplicationActivationPolicy::Accessory,
    };
    application.setActivationPolicy(policy);
}

#[cfg(not(target_os = "macos"))]
pub fn in_the_dock(_wanted: bool) {}

/// Writes a count on the dock icon, or clears it.
///
/// The number is what needs a person — the same count the menu bar
/// item colours itself by — because a badge that counted sessions
/// would read as trouble on a fleet that is perfectly well.
#[cfg(target_os = "macos")]
pub fn badge(waiting: usize) {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    unsafe {
        match waiting {
            0 => tile.setBadgeLabel(None),
            count => tile.setBadgeLabel(Some(&NSString::from_str(&count.to_string()))),
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn badge(_waiting: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The word in the file and the word in the code are the same word,
    /// in both directions: a preference that did not survive a restart
    /// would be worse than no preference at all.
    #[test]
    fn a_presence_survives_being_written_down() {
        for presence in [Presence::Both, Presence::Window, Presence::Menubar] {
            assert_eq!(Presence::parse(presence.word()), Some(presence));
        }
        assert_eq!(Presence::parse("something else"), None);
        assert_eq!(Presence::default(), Presence::Both);
    }

    #[test]
    fn a_file_that_says_nothing_says_both() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        assert_eq!(read(directory.path()), Presence::Both);

        assert_eq!(write(directory.path(), Presence::Menubar), None);
        assert_eq!(read(directory.path()), Presence::Menubar);

        // A file somebody edited into nonsense is not a crash; it is a
        // file that has not chosen, which is what the default is for.
        std::fs::write(preferences(directory.path()), "presence = \"sideways\"\n").unwrap();
        assert_eq!(read(directory.path()), Presence::Both);
        std::fs::write(preferences(directory.path()), "not toml [").unwrap();
        assert_eq!(read(directory.path()), Presence::Both);
    }

    /// Which of the two things each mode asks for.
    #[test]
    fn each_mode_asks_for_what_it_is_named_after() {
        assert!(Presence::Both.opens_a_window() && Presence::Both.takes_the_menu_bar());
        assert!(Presence::Window.opens_a_window() && !Presence::Window.takes_the_menu_bar());
        assert!(!Presence::Menubar.opens_a_window() && Presence::Menubar.takes_the_menu_bar());
    }
}
