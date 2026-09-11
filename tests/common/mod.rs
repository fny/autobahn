//! Support shared by the integration suites.

use std::sync::Once;

static HOME: Once = Once::new();

/// Points `HOME` at a private directory for the rest of this test process.
///
/// Everything autobahn keeps outside a session's `--state-root` — the
/// endpoint-pair locks, and the staging an agent holds for a session
/// driven from elsewhere — lives under `$HOME/.autobahn`, deliberately,
/// so that two processes disagreeing about their state root still meet
/// at the lock. In a test that means the real `~/.autobahn`: the suites
/// left seven thousand staging directories and two thousand locks there.
///
/// Called first by every harness and every direct agent spawn, so it runs
/// before anything in the process reads `HOME`. The directory is leaked
/// on purpose: subprocesses spawned late in the run still need it.
pub fn isolate_home() {
    HOME.call_once(|| {
        let home = tempfile::tempdir()
            .expect("a private home for the test process")
            .into_path();
        std::env::set_var("HOME", &home);
    });
}
