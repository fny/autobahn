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

use std::path::{Path, PathBuf};
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

/// What a login service is registered to run: the file the service
/// manager executes, its arguments, and the `AUTOBAHN_HOME` written into
/// its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// The executable the service manager starts.
    pub executable: PathBuf,
    /// Its arguments, starting with `watch`.
    pub arguments: Vec<String>,
    /// The state root baked into the service's environment, if any.
    pub home: Option<PathBuf>,
}

impl Registration {
    /// The value following `flag` in the arguments.
    fn option(&self, flag: &str) -> Option<&str> {
        let at = self
            .arguments
            .iter()
            .position(|argument| argument == flag)?;
        self.arguments.get(at + 1).map(String::as_str)
    }

    /// The configuration the service was told to read with `--config`.
    /// `None` when it reads the default one, and when the path is
    /// relative, as units written before paths were made absolute may
    /// hold: that path is resolved in the service's working directory,
    /// which this process cannot stand in for.
    pub fn config(&self) -> Option<PathBuf> {
        self.option("--config")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }

    /// The state root the service keeps its state (and its control
    /// socket) under: its `--state-root`, else its `AUTOBAHN_HOME`.
    /// `None` when it uses the default.
    pub fn state_root(&self) -> Option<PathBuf> {
        match self.option("--state-root") {
            Some(root) => Some(PathBuf::from(root)).filter(|root| root.is_absolute()),
            None => self.home.clone(),
        }
    }

    /// Refuses anything a service definition cannot carry intact.
    ///
    /// A newline in a unit file starts a directive of its own, and in a
    /// plist it is a path nobody meant; neither is ever a real path. On
    /// Linux, systemd itself refuses an executable path holding a quote
    /// or a backslash, so that is refused here, where it can be said why.
    fn check(&self, log: &Path) -> Result<()> {
        let mut texts = vec![
            (
                "the executable",
                self.executable.to_string_lossy().into_owned(),
            ),
            ("the log", log.to_string_lossy().into_owned()),
        ];
        for argument in &self.arguments {
            texts.push(("an argument", argument.clone()));
        }
        if let Some(home) = &self.home {
            texts.push((
                crate::paths::HOME_VARIABLE,
                home.to_string_lossy().into_owned(),
            ));
        }
        for (what, text) in texts {
            if text.chars().any(|character| character.is_control()) {
                bail!(
                    "{what} ({}) holds a newline or another control character, which a \
                     service definition cannot carry; refusing to write one",
                    text.escape_debug()
                );
            }
        }
        if cfg!(target_os = "linux") {
            let executable = self.executable.to_string_lossy();
            if executable.contains(['"', '\\', '\'']) {
                bail!(
                    "systemd refuses to run an executable whose path holds a quote or a \
                     backslash ({executable}); move autobahn elsewhere and install again"
                );
            }
        }
        Ok(())
    }
}

/// Registers the service to start at login, and starts it now.
///
/// The service runs `autobahn watch`, which detects that its output is not
/// a terminal and logs cycle lines instead of drawing the live display.
///
/// `--config` and `--state-root` are written absolute: a login service
/// starts in a working directory nobody chose, so a relative path typed
/// here would later name something else.
pub fn install(config: Option<&Path>, state_root: Option<&Path>) -> Result<()> {
    let executable = std::env::current_exe().context("unable to locate this executable")?;
    // A login service inherits none of the shell's environment, so an
    // `AUTOBAHN_HOME` in effect at install time is written into the unit:
    // otherwise the service would keep its state in `~/.autobahn` while
    // every command typed in that shell read another directory, and the
    // two would never see each other's sessions.
    let home = crate::paths::home_override()?;
    let registration = registration_for(executable, config, state_root, home)?;
    let log = log_path()?;
    registration.check(&log)?;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
    }
    platform::install(&registration, &log)
}

/// The registration `install` writes.
fn registration_for(
    executable: PathBuf,
    config: Option<&Path>,
    state_root: Option<&Path>,
    home: Option<PathBuf>,
) -> Result<Registration> {
    let mut arguments: Vec<String> = vec!["watch".to_owned()];
    if let Some(config) = config {
        arguments.push("--config".to_owned());
        arguments.push(absolute(config)?.to_string_lossy().into_owned());
    }
    if let Some(state_root) = state_root {
        arguments.push("--state-root".to_owned());
        arguments.push(absolute(state_root)?.to_string_lossy().into_owned());
    }
    Ok(Registration {
        executable,
        arguments,
        home,
    })
}

/// A path made absolute against the current directory, without resolving
/// symbolic links: the path as typed, only no longer relative.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("unable to make {} absolute", path.display()))
}

/// What the installed service is registered to run, read back from its
/// unit or plist. `None` when no service is installed.
pub fn registration() -> Result<Option<Registration>> {
    platform::registration()
}

/// The configuration the installed service was given with `--config`:
/// what `start` and `restart` must check instead of the default, since it
/// is what the service will load.
pub fn installed_config() -> Result<Option<PathBuf>> {
    Ok(registration()?.and_then(|registration| registration.config()))
}

/// Points the installed service at another executable, keeping its
/// arguments and environment, and has the service manager reload it. The
/// service is not restarted here; the caller does that.
pub fn retarget(executable: &Path) -> Result<()> {
    let Some(mut registration) = registration()? else {
        bail!("no service is installed");
    };
    registration.executable = executable.to_path_buf();
    // The log stays where the service's own state root puts it, which
    // is the default one when its unit names none, whatever this
    // process's environment says.
    let log = match &registration.home {
        Some(home) => home.join("service.log"),
        None => PathBuf::from(std::env::var("HOME").context("HOME is not set")?)
            .join(".autobahn")
            .join("service.log"),
    };
    registration.check(&log)?;
    platform::rewrite(&registration, &log)
}

/// One argument in systemd's quoted form, for `ExecStart=`.
///
/// Double quotes, with `\` and `"` escaped; `%` doubled, because systemd
/// expands specifiers; and, in an argument but not the executable, `$`
/// doubled, because systemd substitutes environment variables into
/// arguments and not into the path it executes.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn systemd_quote(text: &str, executable: bool) -> String {
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' if !executable => quoted.push_str("$$"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Splits an `ExecStart=` value back into its words: the inverse of
/// [`systemd_quote`], and a reading of the unquoted form earlier versions
/// wrote.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn systemd_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut characters = line.chars().peekable();
    loop {
        while characters
            .next_if(|character| character.is_whitespace())
            .is_some()
        {}
        let Some(&first) = characters.peek() else {
            break;
        };
        let mut word = String::new();
        if first == '"' {
            characters.next();
            while let Some(character) = characters.next() {
                match character {
                    '"' => break,
                    '\\' => {
                        if let Some(escaped) = characters.next() {
                            word.push(escaped);
                        }
                    }
                    other => word.push(other),
                }
            }
        } else {
            while let Some(character) = characters.next_if(|character| !character.is_whitespace()) {
                word.push(character);
            }
        }
        words.push(word);
    }
    let unescape = |word: &str, executable: bool| {
        let word = word.replace("%%", "%");
        match executable {
            true => word,
            false => word.replace("$$", "$"),
        }
    };
    words
        .iter()
        .enumerate()
        .map(|(index, word)| unescape(word, index == 0))
        .collect()
}

/// The systemd user unit for a registration.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn render_unit(registration: &Registration, log: &Path) -> String {
    let mut exec = systemd_quote(&registration.executable.to_string_lossy(), true);
    for argument in &registration.arguments {
        exec.push(' ');
        exec.push_str(&systemd_quote(argument, false));
    }
    // `Environment=` unquotes and unescapes like `ExecStart=`, and
    // expands specifiers, but substitutes no variables.
    let environment = match &registration.home {
        Some(home) => format!(
            "Environment=\"{}={}\"\n",
            crate::paths::HOME_VARIABLE,
            home.to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
        ),
        None => String::new(),
    };
    format!(
        "[Unit]\n\
         Description=autobahn file synchronization\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         {environment}\
         Restart=always\n\
         RestartSec=2\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        log = log.to_string_lossy().replace('%', "%%")
    )
}

/// Reads a registration back out of a unit [`render_unit`] wrote.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_unit(content: &str) -> Option<Registration> {
    let mut registration = None;
    let mut home = None;
    for line in content.lines() {
        if let Some(exec) = line.strip_prefix("ExecStart=") {
            let mut words = systemd_words(exec).into_iter();
            registration = Some((PathBuf::from(words.next()?), words.collect::<Vec<_>>()));
        } else if let Some(environment) = line.strip_prefix("Environment=") {
            // The first word, whose `%%` the split has already undone.
            let Some(word) = systemd_words(environment).into_iter().next() else {
                continue;
            };
            let prefix = format!("{}=", crate::paths::HOME_VARIABLE);
            if let Some(value) = word.strip_prefix(&prefix) {
                home = Some(PathBuf::from(value));
            }
        }
    }
    let (executable, arguments) = registration?;
    Some(Registration {
        executable,
        arguments,
        home,
    })
}

/// Escapes text for an XML element's content.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The inverse of [`xml_escape`], plus the other predefined entities.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The launchd plist for a registration.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn render_plist(registration: &Registration, log: &Path) -> String {
    let mut program_arguments = String::new();
    program_arguments.push_str(&format!(
        "\t\t<string>{}</string>\n",
        xml_escape(&registration.executable.to_string_lossy())
    ));
    for argument in &registration.arguments {
        program_arguments.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(argument)));
    }
    let mut environment = String::new();
    if let Some(home) = &registration.home {
        environment.push_str(&format!(
            "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
            crate::paths::HOME_VARIABLE,
            xml_escape(&home.to_string_lossy())
        ));
    }
    // RunAtLoad starts it at login; KeepAlive restarts it if it exits.
    // The PATH is set explicitly because launchd's environment carries
    // almost nothing, and the supervisor needs to find ssh.
    format!(
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
{environment}	</dict>
</dict>
</plist>
"#,
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// Reads a registration back out of a plist [`render_plist`] wrote. Only
/// that shape: this is not a general plist reader.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn parse_plist(content: &str) -> Option<Registration> {
    let strings = |section: &str| -> Vec<String> {
        section
            .split("<string>")
            .skip(1)
            .filter_map(|piece| piece.split_once("</string>"))
            .map(|(text, _)| xml_unescape(text))
            .collect()
    };
    let after = content.split_once("<key>ProgramArguments</key>")?.1;
    let array = after.split_once("</array>")?.0;
    let mut words = strings(array).into_iter();
    let executable = PathBuf::from(words.next()?);
    let arguments = words.collect();
    let key = format!("<key>{}</key>", crate::paths::HOME_VARIABLE);
    let home = content
        .split_once(key.as_str())
        .and_then(|(_, rest)| strings(rest).into_iter().next())
        .map(PathBuf::from);
    Some(Registration {
        executable,
        arguments,
        home,
    })
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

/// Stops and starts the registered service — after an upgrade of the
/// binary, or a configuration edit when `reload = false`.
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
    use super::{parse_plist, render_plist, run, Registration, ServiceState, LABEL};
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

    /// Writes the plist.
    fn write(registration: &Registration, log: &Path) -> Result<PathBuf> {
        let plist = plist_path()?;
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
        std::fs::write(&plist, render_plist(registration, log))
            .with_context(|| format!("unable to write {}", plist.display()))?;
        Ok(plist)
    }

    pub fn install(registration: &Registration, log: &Path) -> Result<()> {
        let plist = write(registration, log)?;
        // A previous registration (an earlier install, or a stale one) is
        // replaced rather than layered.
        let _ = Command::new("launchctl")
            .args(["bootout", &target()])
            .output();
        let mut command = Command::new("launchctl");
        command.args(["bootstrap", &domain(), &plist.to_string_lossy()]);
        run(command)
    }

    /// Rewrites the plist and reloads it. launchd keeps the definition it
    /// loaded, so a changed plist means nothing until it is booted out and
    /// bootstrapped again.
    pub fn rewrite(registration: &Registration, log: &Path) -> Result<()> {
        write(registration, log)?;
        if loaded() {
            stop()?;
        }
        bootstrap()
    }

    pub fn registration() -> Result<Option<Registration>> {
        let plist = plist_path()?;
        match std::fs::read_to_string(&plist) {
            Ok(content) => Ok(parse_plist(&content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("unable to read {}", plist.display())),
        }
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
    use super::{parse_unit, render_unit, run, Registration, ServiceState, LABEL};
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

    /// Writes the unit and has systemd read it.
    fn write(registration: &Registration, log: &Path) -> Result<()> {
        let unit = unit_path()?;
        if let Some(parent) = unit.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
        std::fs::write(&unit, render_unit(registration, log))
            .with_context(|| format!("unable to write {}", unit.display()))?;
        run(systemctl(&["daemon-reload"]))
    }

    pub fn install(registration: &Registration, log: &Path) -> Result<()> {
        write(registration, log)?;
        run(systemctl(&["enable", "--now", &unit_name()]))?;
        // Without lingering, a user's services stop when their last session
        // ends — the opposite of what a synchronizer on a server wants.
        let _ = Command::new("loginctl").arg("enable-linger").output();
        Ok(())
    }

    pub fn rewrite(registration: &Registration, log: &Path) -> Result<()> {
        write(registration, log)
    }

    pub fn registration() -> Result<Option<Registration>> {
        let unit = unit_path()?;
        match std::fs::read_to_string(&unit) {
            Ok(content) => Ok(parse_unit(&content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("unable to read {}", unit.display())),
        }
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
    use super::{Registration, ServiceState};
    use anyhow::{bail, Result};
    use std::path::Path;

    pub fn install(_: &Registration, _: &Path) -> Result<()> {
        bail!("login services are supported on macOS (launchd) and Linux (systemd) only")
    }
    pub fn rewrite(_: &Registration, _: &Path) -> Result<()> {
        bail!("login services are supported on macOS (launchd) and Linux (systemd) only")
    }
    pub fn registration() -> Result<Option<Registration>> {
        Ok(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every character that broke the old unit: a space splits an
    /// argument, a quote ends one, `%` is a specifier and `$` a variable.
    const AWKWARD: &str = "my \"odd\" 100% $HOME";

    fn awkward(root: &Path) -> Registration {
        Registration {
            executable: root.join("bin dir 100% $x").join("autobahn"),
            arguments: vec![
                "watch".to_owned(),
                "--config".to_owned(),
                root.join(AWKWARD)
                    .join("config.toml")
                    .to_string_lossy()
                    .into_owned(),
                "--state-root".to_owned(),
                root.join("state \\ root").to_string_lossy().into_owned(),
            ],
            home: Some(root.join(AWKWARD)),
        }
    }

    #[test]
    fn a_unit_reads_back_as_the_registration_it_was_written_from() {
        let registration = awkward(Path::new("/tmp/a"));
        let unit = render_unit(&registration, Path::new("/tmp/a/lo g%s/service.log"));
        assert_eq!(parse_unit(&unit), Some(registration), "{unit}");
        // Doubled characters stay doubled, in the environment as anywhere.
        let registration = awkward(Path::new("/tmp/a%%b$$c"));
        let unit = render_unit(&registration, Path::new("/tmp/service.log"));
        assert_eq!(parse_unit(&unit), Some(registration), "{unit}");
        // An environment line of another shape does not hide the rest.
        let unit = unit.replace("[Service]\n", "[Service]\nEnvironment=\n");
        assert!(parse_unit(&unit).is_some(), "{unit}");
    }

    /// A relative path in a unit written before paths were made absolute
    /// is not resolved here, in another working directory.
    #[test]
    fn a_legacy_relative_config_is_not_guessed_at() {
        let registration = Registration {
            executable: PathBuf::from("/usr/bin/autobahn"),
            arguments: vec![
                "watch".into(),
                "--config".into(),
                "work.toml".into(),
                "--state-root".into(),
                "state".into(),
            ],
            home: None,
        };
        assert_eq!(registration.config(), None);
        assert_eq!(registration.state_root(), None);
    }

    /// The unit as systemd itself reads it, where systemd is at hand.
    #[test]
    fn a_unit_for_awkward_paths_passes_systemd_s_own_check() {
        let Ok(probe) = Command::new("systemd-analyze").arg("--version").output() else {
            return;
        };
        if !probe.status.success() {
            return;
        }
        let root = tempfile::tempdir().expect("a temporary directory");
        let registration = awkward(root.path());
        let executable = &registration.executable;
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(executable, b"#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = root.path().join("lo g%s").join("service.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let unit = root.path().join(format!("{LABEL}.service"));
        std::fs::write(&unit, render_unit(&registration, &log)).unwrap();
        let output = Command::new("systemd-analyze")
            .args(["--user", "verify"])
            .arg(&unit)
            .output()
            .expect("runs systemd-analyze");
        let complaint = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && !complaint.contains(&unit.to_string_lossy().to_string()),
            "{complaint}\n{}",
            std::fs::read_to_string(&unit).unwrap()
        );
    }

    #[test]
    fn systemd_quoting_escapes_what_systemd_would_expand() {
        assert_eq!(systemd_quote("a b", false), "\"a b\"");
        assert_eq!(systemd_quote("100%", false), "\"100%%\"");
        assert_eq!(systemd_quote("$HOME", false), "\"$$HOME\"");
        assert_eq!(systemd_quote("$HOME", true), "\"$HOME\"");
        assert_eq!(systemd_quote("say \"hi\"", false), "\"say \\\"hi\\\"\"");
        assert_eq!(systemd_quote("a\\b", false), "\"a\\\\b\"");
        // What earlier versions wrote still reads.
        assert_eq!(
            systemd_words("/usr/bin/autobahn watch --config /etc/a.toml"),
            vec!["/usr/bin/autobahn", "watch", "--config", "/etc/a.toml"]
        );
    }

    #[test]
    fn a_relative_config_is_stored_absolute() {
        let registration = registration_for(
            PathBuf::from("/usr/bin/autobahn"),
            Some(Path::new("configs/work.toml")),
            Some(Path::new("state")),
            None,
        )
        .expect("a registration");
        let here = std::env::current_dir().unwrap();
        assert_eq!(registration.config(), Some(here.join("configs/work.toml")));
        assert_eq!(registration.state_root(), Some(here.join("state")));
    }

    #[test]
    fn a_newline_in_a_path_is_refused() {
        let log = Path::new("/tmp/service.log");
        let mut registration = awkward(Path::new("/tmp"));
        registration.check(log).expect("awkward but carriable");
        registration.arguments[2] = "/tmp/a\nExecStartPre=/bin/evil".to_owned();
        let error = registration.check(log).expect_err("a newline is refused");
        assert!(format!("{error}").contains("newline"), "{error}");

        let mut registration = awkward(Path::new("/tmp"));
        registration.home = Some(PathBuf::from("/tmp/home\n[Service]"));
        registration.check(log).expect_err("in AUTOBAHN_HOME too");
    }

    /// The plist for the same awkward paths is XML a parser accepts, and
    /// reads back as the registration.
    #[test]
    fn a_plist_for_awkward_paths_is_well_formed_and_reads_back() {
        let mut registration = awkward(Path::new("/tmp/a"));
        registration.arguments[2] = "/tmp/a <&> b/config.toml".to_owned();
        let plist = render_plist(&registration, Path::new("/tmp/a & b/service.log"));
        assert_eq!(parse_plist(&plist), Some(registration.clone()), "{plist}");

        let Ok(mut child) = Command::new("python3")
            .args([
                "-c",
                "import sys, xml.dom.minidom as m; d = m.parseString(sys.stdin.buffer.read()); \
                 print('\\n'.join(s.firstChild.data if s.firstChild else '' \
                 for s in d.getElementsByTagName('array')[0].getElementsByTagName('string')))",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(plist.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut expected = vec![registration.executable.to_string_lossy().into_owned()];
        expected.extend(registration.arguments.iter().cloned());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim_end(),
            expected.join("\n")
        );
    }
}
