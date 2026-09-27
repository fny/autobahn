//! Notifications the system posts, rather than the deprecated path.
//!
//! `notify-rust` on macOS reaches for `NSUserNotification`, which Apple
//! deprecated in 10.14 and which shows a plain banner that stacks: ten
//! transitions are ten notifications nobody reads. The framework that
//! replaced it — `UserNotifications` — groups them under the app,
//! replaces one by identifier instead of adding another, and is what
//! every other notification on the machine goes through.
//!
//! It has one condition: the process must be a bundled application with
//! an identifier. Run from a terminal there is no bundle, and asking
//! the notification centre for its instance from such a process throws.
//! So this checks first, says no, and leaves the caller to fall back to
//! what it did before.

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::atomic::{AtomicU8, Ordering};

    use block2::RcBlock;
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNUserNotificationCenter,
    };

    /// Whether the system has been asked for permission yet: asking on
    /// every notification would be rude, and the answer is remembered by
    /// the system anyway.
    static ASKED: AtomicU8 = AtomicU8::new(0);

    /// Whether this process is a bundled application, which is what the
    /// notification centre requires of whoever posts.
    fn available() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    /// Asks once, in the background. A refusal is not an error worth
    /// reporting: the person said no, and the menu bar still shows
    /// everything the notification would have.
    fn ask() {
        if ASKED.swap(1, Ordering::SeqCst) == 1 {
            return;
        }
        let handler = RcBlock::new(|_granted: objc2::runtime::Bool, _error: *mut NSError| {});
        let centre = UNUserNotificationCenter::currentNotificationCenter();
        centre.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &handler,
        );
    }

    /// Posts one. `key` is what makes a later notification replace this
    /// one rather than pile on top of it: the same alerting set keeps
    /// the same identifier.
    pub fn post(key: &str, title: &str, body: &str) -> bool {
        if !available() {
            return false;
        }
        ask();
        {
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(title));
            content.setBody(&NSString::from_str(body));
            content.setThreadIdentifier(&NSString::from_str("autobahn"));
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                &NSString::from_str(key),
                &content,
                None,
            );
            let centre = UNUserNotificationCenter::currentNotificationCenter();
            centre.addNotificationRequest_withCompletionHandler(&request, None);
        }
        true
    }
}

#[cfg(target_os = "macos")]
pub(crate) use mac::post;

#[cfg(not(target_os = "macos"))]
pub(crate) fn post(_key: &str, _title: &str, _body: &str) -> bool {
    false
}
