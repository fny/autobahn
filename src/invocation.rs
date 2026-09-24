//! Command lines autobahn builds to run itself.
//!
//! The shop and the menu bar app act by running the CLI, so they cannot
//! resolve differently than a person at a terminal could. The paths they
//! pass are names on disk, and the other side chooses those: a file named
//! `--all` must arrive as a file, not as a flag. So every flag comes
//! first, then `--`, then every value a tree could have chosen.
//!
//! The options that belong to every subcommand — `--config` and
//! `--state-root` — go straight after the subcommand's name, at index 1,
//! never at the end: after the `--` they would be read as paths.

/// `resolve`, keeping `keep`'s version of `paths` in `group`, without
/// asking.
///
/// The group goes after the `--` too, as the first positional: it comes
/// from the configuration rather than from a tree, but nothing stops a
/// group being called `-y`, and after the separator it cannot matter.
/// `--keep=` joins the flag and its value into one argument for the same
/// reason.
pub fn resolve_command(group: &str, keep: &str, paths: &[String]) -> Vec<String> {
    let mut command = vec![
        "resolve".to_owned(),
        format!("--keep={keep}"),
        "--yes".to_owned(),
        "--".to_owned(),
        group.to_owned(),
    ];
    command.extend(paths.iter().cloned());
    command
}

/// `diff` of `path` in `group` against the destination `host`.
pub fn diff_command(group: &str, path: &str, host: &str) -> Vec<String> {
    vec![
        "diff".to_owned(),
        format!("--host={host}"),
        "--".to_owned(),
        group.to_owned(),
        path.to_owned(),
    ]
}

/// `command` with `options` put straight after the subcommand's name,
/// where they are read as options whatever follows.
pub fn with_options<S: Clone>(command: &[S], options: &[S]) -> Vec<S> {
    let split = command.len().min(1);
    let mut out = command[..split].to_vec();
    out.extend_from_slice(options);
    out.extend_from_slice(&command[split..]);
    out
}
