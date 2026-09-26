//! Power saving, experimental: the full walk runs less often on battery.
//!
//! Every root is walked in full every two minutes, whatever the watcher
//! says, because events can be lost without a word. On a laptop that walk
//! is autobahn's whole idle cost: measured on an M4 on battery, a
//! 160,000-file pair averaged 55 CPU ms/s and an energy impact of 76,
//! nearly all of it in the minutes with a walk (WISHLIST.md, "A full walk
//! that costs what the tree can afford"). With `power_saver_experimental`,
//! the walk runs every ten minutes while the machine is on battery, and
//! every two on AC, as before.
//!
//! What it gives up: a change the watcher never reported can take ten
//! minutes rather than two to be found. Changes the watcher reports arrive
//! as fast as ever, and a watcher that drops events says so and is walked
//! at once. Only the host this process runs on is consulted, so a remote
//! beta's agent — which never sees the configuration — walks every two
//! minutes as before.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often a root is walked in full, whatever the watcher says: the
/// ceiling on how long a missed event can persist.
pub const FULL_WALK_INTERVAL: Duration = Duration::from_secs(120);

/// The same, while saving power on battery.
pub const FULL_WALK_INTERVAL_ON_BATTERY: Duration = Duration::from_secs(600);

/// How long a reading of the power source is trusted. Unplugging is seen
/// within this long, which is short against the intervals it chooses.
const POWER_SOURCE_TTL: Duration = Duration::from_secs(60);

static ENABLED: AtomicBool = AtomicBool::new(false);

/// The last reading: when it was taken, and whether on battery.
static READING: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Turns power saving on or off, from the configuration, at start and on
/// every reload.
pub fn set_enabled(enabled: bool) {
    let was = ENABLED.swap(enabled, Ordering::SeqCst);
    if was != enabled {
        // The next interval asks afresh, and says what it finds.
        *READING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if !enabled {
            crate::note!("power saver: off; full walks every 2 minutes");
        }
    }
}

/// How long a root may go between full walks right now.
pub fn full_walk_interval() -> Duration {
    interval_for(ENABLED.load(Ordering::SeqCst) && on_battery())
}

fn interval_for(saving: bool) -> Duration {
    match saving {
        true => FULL_WALK_INTERVAL_ON_BATTERY,
        false => FULL_WALK_INTERVAL,
    }
}

/// Whether this machine is running on battery, read at most once a minute
/// and said in the log when it changes. Anything that cannot be read counts
/// as AC: the two-minute walk is the safe direction.
fn on_battery() -> bool {
    let mut reading = READING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, battery)) = *reading {
        if at.elapsed() < POWER_SOURCE_TTL {
            return battery;
        }
    }
    let battery = read_on_battery().unwrap_or(false);
    if reading.is_none_or(|(_, before)| before != battery) {
        match battery {
            true => crate::note!("power saver: on battery; full walks every 10 minutes"),
            false => crate::note!("power saver: on AC power; full walks every 2 minutes"),
        }
    }
    *reading = Some((Instant::now(), battery));
    battery
}

#[cfg(target_os = "macos")]
fn read_on_battery() -> Option<bool> {
    use std::ffi::{c_char, c_void, CStr};

    type CFTypeRef = *const c_void;
    const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFTypeRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(value: CFTypeRef);
        fn CFStringGetCString(
            string: CFTypeRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
    }

    // Safety: the snapshot is owned (a Copy function) and released once;
    // the type string follows the Get rule, is not released, and is read
    // before the snapshot that holds it goes.
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            return None;
        }
        let source = IOPSGetProvidingPowerSourceType(snapshot);
        let mut buffer = [0 as c_char; 64];
        let read = !source.is_null()
            && CFStringGetCString(source, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) != 0;
        let battery = read.then(|| CStr::from_ptr(buffer.as_ptr()).to_bytes() == b"Battery Power");
        CFRelease(snapshot);
        battery
    }
}

#[cfg(target_os = "linux")]
fn read_on_battery() -> Option<bool> {
    linux_on_battery(std::path::Path::new("/sys/class/power_supply"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_on_battery() -> Option<bool> {
    None
}

/// On battery when a battery reports it is discharging and no mains or USB
/// supply is online. A machine with no battery at all — a desktop, a
/// server, a VM — is on AC.
#[cfg(any(target_os = "linux", test))]
fn linux_on_battery(supplies: &std::path::Path) -> Option<bool> {
    let read = |path: std::path::PathBuf| {
        std::fs::read_to_string(path)
            .map(|text| text.trim().to_owned())
            .unwrap_or_default()
    };
    let mut discharging = false;
    for entry in std::fs::read_dir(supplies).ok()?.flatten() {
        let supply = entry.path();
        match read(supply.join("type")).as_str() {
            "Battery" => discharging |= read(supply.join("status")) == "Discharging",
            _ if read(supply.join("online")) == "1" => return Some(false),
            _ => {}
        }
    }
    Some(discharging)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_stretches_only_the_full_walk_interval() {
        assert_eq!(interval_for(false), Duration::from_secs(120));
        assert_eq!(interval_for(true), Duration::from_secs(600));
    }

    fn supply(root: &std::path::Path, name: &str, fields: &[(&str, &str)]) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        for (field, value) in fields {
            std::fs::write(directory.join(field), format!("{value}\n")).unwrap();
        }
    }

    #[test]
    fn linux_power_supplies_are_read_as_the_kernel_reports_them() {
        // A laptop unplugged.
        let laptop = tempfile::tempdir().unwrap();
        supply(laptop.path(), "AC", &[("type", "Mains"), ("online", "0")]);
        supply(
            laptop.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        assert_eq!(linux_on_battery(laptop.path()), Some(true));

        // Plugged in: charging, full, or not charging, the mains is online.
        supply(laptop.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        assert_eq!(linux_on_battery(laptop.path()), Some(false));

        // On a USB-C charger, with the battery still reporting discharge
        // for a moment: the online supply wins.
        let usb = tempfile::tempdir().unwrap();
        supply(
            usb.path(),
            "ucsi-source-psy-USBC000:001",
            &[("type", "USB"), ("online", "1")],
        );
        supply(
            usb.path(),
            "BAT1",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        assert_eq!(linux_on_battery(usb.path()), Some(false));

        // A server: no supplies at all, or no battery.
        let server = tempfile::tempdir().unwrap();
        assert_eq!(linux_on_battery(server.path()), Some(false));
        assert_eq!(linux_on_battery(&server.path().join("absent")), None);
    }
}
