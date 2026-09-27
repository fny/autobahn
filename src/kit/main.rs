//! `autobahn-desk-kit`: the window again, drawn with GPUI Kit.
//!
//! A second binary rather than a second command, because the kit brings
//! its own GPUI and two of them must not meet in one process. Everything
//! below the window — the status document, the control socket, the
//! configuration's shape, the words — is the same code the first window
//! uses.

fn main() -> anyhow::Result<()> {
    let mut config: Option<std::path::PathBuf> = None;
    let mut state_root: Option<std::path::PathBuf> = None;
    let mut shoot: Option<std::path::PathBuf> = None;
    let mut pane: Option<String> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config" => config = arguments.next().map(Into::into),
            "--state-root" => state_root = arguments.next().map(Into::into),
            "--shoot" => shoot = arguments.next().map(Into::into),
            "--pane" => pane = arguments.next(),
            "--help" | "-h" => {
                println!("autobahn-desk-kit [--config <file>] [--state-root <directory>]");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let state_root = match state_root {
        Some(root) => root,
        None => autobahn::paths::default_state_root()?,
    };
    match shoot {
        Some(directory) => autobahn::kit::shoot(config, state_root, directory, pane),
        None => autobahn::kit::run(config, state_root),
    }
}
