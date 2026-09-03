//! The login service: what keeps the supervisor running when no terminal
//! is.
//!
//! Autobahn has no daemon of its own and does not background itself. The
//! supervisor is a foreground process, and the platform's service manager
//! is what keeps it alive across logouts, crashes and reboots — launchd on
//! macOS, a systemd user unit on Linux. This module writes the one file
//! each of those needs and drives them with their own tools, so the
//! service is inspectable and controllable with `launchctl` or
//! `systemctl` exactly as any other.
//!
//! There is deliberately exactly one background mechanism. A `start` with
//! no service installed refuses rather than spawning something detached
//! and unregistered, which is how a supervisor ends up running with
//! nobody able to say what started it or why it vanished at reboot.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// The service's name under the platform's service manager.
const LABEL: &str = "io.autobahn.supervisor";

/// What the service manager knows about the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// No service is registered.
    NotInstalled,
    /// Registered, but not currently running.
    Stopped,
    /// Registered and running.
    Running,
}

/// Where the service's log goes: everything the supervisor would have
/// printed to a terminal.
pub fn log_path() -> Result<PathBuf> {
    Ok(crate::paths::default_state_root()?.join("service.log"))
}

/// The size at which the service log is rotated.
///
/// Small, because the log is a record of changes and changes are rare. It
/// grew to 176 MB once, and 97% of that was one statement restating a
/// list that had not moved in three days.
const MAXIMUM_LOG_SIZE: u64 = 16 << 20;

/// The previous generation of the service log.
pub fn previous_log_path() -> Result<PathBuf> {
    Ok(crate::paths::default_state_root()?.join("service.log.1"))
}

/// Keeps the service log under its cap, returning whether it rotated.
///
/// The current file is copied aside and then truncated **in place**.
/// Renaming it would not work: the service's output is an open descriptor
/// held by launchd or systemd, and it follows the inode — so the writer
/// would go on filling the file that was just moved out of the way, and
/// the new one would stay empty forever. Truncating keeps the descriptor
/// pointing at the same file, and because that descriptor is in append
/// mode the next write lands at the start.
pub fn rotate_log() -> Result<bool> {
    let path = log_path()?;
    let size = match std::fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        // No log is not a failure: `watch` in a terminal writes to the
        // terminal, and there is nothing to rotate.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("unable to measure the service log"),
    };
    if size < MAXIMUM_LOG_SIZE {
        return Ok(false);
    }
    let previous = previous_log_path()?;
    std::fs::copy(&path, &previous).with_context(|| {
        format!(
            "unable to keep the previous service log at {}",
            previous.display()
        )
    })?;
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("unable to truncate {}", path.display()))?;
    Ok(true)
}

/// Registers the service to start at login, and starts it now.
///
/// The service runs `autobahn watch`, which detects that its output is not
/// a terminal and logs cycle lines instead of drawing the live display.
pub fn install(
    config: Option<&std::path::Path>,
    state_root: Option<&std::path::Path>,
) -> Result<()> {
    let executable = std::env::current_exe().context("unable to locate this executable")?;
    let mut arguments: Vec<String> = vec!["watch".to_owned()];
    if let Some(config) = config {
        arguments.push("--config".to_owned());
        arguments.push(config.to_string_lossy().into_owned());
    }
    if let Some(state_root) = state_root {
        arguments.push("--state-root".to_owned());
        arguments.push(state_root.to_string_lossy().into_owned());
    }
    let log = log_path()?;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
    }
    platform::install(&executable, &arguments, &log)
}

/// Stops the service and unregisters it.
pub fn uninstall() -> Result<()> {
    platform::uninstall()
}

/// Starts the registered service.
pub fn start() -> Result<()> {
    if state()? == ServiceState::NotInstalled {
        bail!(
            "no service is installed; run `autobahn install` to register one, \
             or `autobahn watch` to run in this terminal"
        );
    }
    platform::start()
}

/// Stops the registered service. It stays registered, and comes back at
/// the next login; `uninstall` is how it is made to stay gone.
pub fn stop() -> Result<()> {
    if state()? == ServiceState::NotInstalled {
        bail!("no service is installed");
    }
    platform::stop()
}

/// Stops and starts the registered service — after a configuration edit,
/// or an upgrade of the binary.
pub fn restart() -> Result<()> {
    if state()? == ServiceState::NotInstalled {
        bail!(
            "no service is installed; run `autobahn install` to register one, \
             or `autobahn watch` to run in this terminal"
        );
    }
    platform::restart()
}

/// The service's current state.
pub fn state() -> Result<ServiceState> {
    platform::state()
}

/// Runs a service-manager command, turning a failure into an error that
/// carries the tool's own complaint.
fn run(mut command: Command) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("unable to run {:?}", command.get_program()))?;
    if output.status.success() {
        return Ok(());
    }
    let complaint = String::from_utf8_lossy(&output.stderr);
    let complaint = complaint.trim();
    if complaint.is_empty() {
        bail!("{:?} exited with {}", command.get_program(), output.status);
    }
    bail!("{complaint}");
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{run, ServiceState, LABEL};
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn plist_path() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    /// The launchd domain for this user's GUI session.
    fn domain() -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub fn install(executable: &Path, arguments: &[String], log: &Path) -> Result<()> {
        let plist = plist_path()?;
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
        let mut program_arguments = String::new();
        program_arguments.push_str(&format!(
            "\t\t<string>{}</string>\n",
            escape(&executable.to_string_lossy())
        ));
        for argument in arguments {
            program_arguments.push_str(&format!("\t\t<string>{}</string>\n", escape(argument)));
        }
        // RunAtLoad starts it at login; KeepAlive restarts it if it exits.
        // The PATH is set explicitly because launchd's environment carries
        // almost nothing, and the supervisor needs to find ssh.
        let content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{program_arguments}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
</dict>
</plist>
"#,
            log = escape(&log.to_string_lossy()),
        );
        std::fs::write(&plist, content)
            .with_context(|| format!("unable to write {}", plist.display()))?;
        // A previous registration (an earlier install, or a stale one) is
        // replaced rather than layered.
        let _ = Command::new("launchctl")
            .args(["bootout", &target()])
            .output();
        let mut command = Command::new("launchctl");
        command.args(["bootstrap", &domain(), &plist.to_string_lossy()]);
        run(command)
    }

    pub fn uninstall() -> Result<()> {
        let plist = plist_path()?;
        let _ = Command::new("launchctl")
            .args(["bootout", &target()])
            .output();
        match std::fs::remove_file(&plist) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("unable to remove {}", plist.display()))
            }
        }
    }

    fn target() -> String {
        format!("{}/{LABEL}", domain())
    }

    /// Whether launchd has the service loaded, running or not.
    fn loaded() -> bool {
        Command::new("launchctl")
            .args(["print", &target()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Loads the plist, which with RunAtLoad also starts it.
    ///
    /// launchd tears a booted-out service down asynchronously, and while
    /// that is in progress — longer with KeepAlive — a bootstrap of the
    /// same label fails with "Input/output error" (exit 5). A stop
    /// followed at once by a start is exactly that, so the bootstrap is
    /// retried across the teardown rather than reported on first refusal.
    fn bootstrap() -> Result<()> {
        let plist = plist_path()?;
        let mut last = None;
        for _ in 0..20 {
            let mut command = Command::new("launchctl");
            command.args(["bootstrap", &domain(), &plist.to_string_lossy()]);
            match run(command) {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        Err(last.unwrap()).context("unable to load the service into launchd")
    }

    pub fn start() -> Result<()> {
        if loaded() {
            // Loaded but possibly exited: a kickstart starts it if it is
            // not running and is harmless if it is.
            let mut command = Command::new("launchctl");
            command.args(["kickstart", &target()]);
            return run(command);
        }
        bootstrap()
    }

    pub fn stop() -> Result<()> {
        // Unloading is the only stop that KeepAlive does not immediately
        // undo. The plist stays on disk, so the next login loads it again.
        let mut command = Command::new("launchctl");
        command.args(["bootout", &target()]);
        run(command)?;
        // The unload is asynchronous; waiting for it here means a start
        // that follows at once finds the label free.
        for _ in 0..20 {
            if !loaded() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        Ok(())
    }

    pub fn restart() -> Result<()> {
        if loaded() {
            // launchd's own restart: kills the process and starts it
            // again without unloading, so there is no teardown to race.
            let mut command = Command::new("launchctl");
            command.args(["kickstart", "-k", &target()]);
            return run(command);
        }
        bootstrap()
    }

    pub fn state() -> Result<ServiceState> {
        if !plist_path()?.exists() {
            return Ok(ServiceState::NotInstalled);
        }
        let output = Command::new("launchctl")
            .args(["print", &target()])
            .output()
            .context("unable to run launchctl")?;
        if !output.status.success() {
            return Ok(ServiceState::Stopped);
        }
        let report = String::from_utf8_lossy(&output.stdout);
        // `state = running` appears in the print output for a live process.
        let running = report
            .lines()
            .any(|line| line.trim().starts_with("state = running"));
        Ok(if running {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{run, ServiceState, LABEL};
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn unit_name() -> String {
        format!("{LABEL}.service")
    }

    fn unit_path() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("systemd")
            .join("user")
            .join(unit_name()))
    }

    fn systemctl(arguments: &[&str]) -> Command {
        let mut command = Command::new("systemctl");
        command.arg("--user");
        command.args(arguments);
        command
    }

    pub fn install(executable: &Path, arguments: &[String], log: &Path) -> Result<()> {
        let unit = unit_path()?;
        if let Some(parent) = unit.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
        let mut exec = executable.to_string_lossy().into_owned();
        for argument in arguments {
            exec.push(' ');
            exec.push_str(argument);
        }
        let content = format!(
            "[Unit]\n\
             Description=autobahn file synchronization\n\
             After=network-online.target\n\
             \n\
             [Service]\n\
             ExecStart={exec}\n\
             Restart=always\n\
             RestartSec=2\n\
             StandardOutput=append:{log}\n\
             StandardError=append:{log}\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            log = log.display()
        );
        std::fs::write(&unit, content)
            .with_context(|| format!("unable to write {}", unit.display()))?;
        run(systemctl(&["daemon-reload"]))?;
        run(systemctl(&["enable", "--now", &unit_name()]))?;
        // Without lingering, a user's services stop when their last session
        // ends — the opposite of what a synchronizer on a server wants.
        let _ = Command::new("loginctl").arg("enable-linger").output();
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let unit = unit_path()?;
        let _ = systemctl(&["disable", "--now", &unit_name()]).output();
        match std::fs::remove_file(&unit) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("unable to remove {}", unit.display()))
            }
        }
        let _ = systemctl(&["daemon-reload"]).output();
        Ok(())
    }

    pub fn start() -> Result<()> {
        run(systemctl(&["start", &unit_name()]))
    }

    pub fn stop() -> Result<()> {
        run(systemctl(&["stop", &unit_name()]))
    }

    pub fn restart() -> Result<()> {
        run(systemctl(&["restart", &unit_name()]))
    }

    pub fn state() -> Result<ServiceState> {
        if !unit_path()?.exists() {
            return Ok(ServiceState::NotInstalled);
        }
        let output = systemctl(&["is-active", &unit_name()])
            .output()
            .context("unable to run systemctl")?;
        Ok(if output.status.success() {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::ServiceState;
    use anyhow::{bail, Result};
    use std::path::Path;

    pub fn install(_: &Path, _: &[String], _: &Path) -> Result<()> {
        bail!("login services are supported on macOS (launchd) and Linux (systemd) only")
    }
    pub fn uninstall() -> Result<()> {
        bail!("login services are supported on macOS (launchd) and Linux (systemd) only")
    }
    pub fn start() -> Result<()> {
        bail!("login services are supported on macOS (launchd) and Linux (systemd) only")
    }
    pub fn stop() -> Result<()> {
        bail!("login services are supported on macOS (launchd) and Linux (systemd) only")
    }
    pub fn restart() -> Result<()> {
        bail!("login services are supported on macOS (launchd) and Linux (systemd) only")
    }
    pub fn state() -> Result<ServiceState> {
        Ok(ServiceState::NotInstalled)
    }
}
