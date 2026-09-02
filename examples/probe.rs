//! A read-only look at what a session's two scans actually see.
//!
//! Diagnostic only: it opens both endpoints, scans them, and reports what
//! each root looks like — including the inputs to the emptied-root safety
//! halt, which otherwise says that *a* side was emptied without saying
//! which. It applies no transitions and writes no state.
//!
//!     cargo run --release --example probe -- <group> [--config PATH]

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let group = arguments.next().expect("usage: probe <group> [--config P]");
    let mut config: Option<PathBuf> = None;
    let mut state_root = PathBuf::from(std::env::var("HOME")?).join(".autobahn");
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config" => config = arguments.next().map(PathBuf::from),
            "--state-root" => state_root = arguments.next().map(PathBuf::from).unwrap(),
            other => anyhow::bail!("unexpected argument {other}"),
        }
    }
    let config = config.unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap())
            .join(".autobahn")
            .join("config.toml")
    });

    let text = std::fs::read_to_string(&config)?;
    let configuration: autobahn::config::Config = toml::from_str(&text)?;
    let plans = configuration.plans()?;
    let pool = autobahn::transport::mux::AgentPool::default();

    for plan in plans.iter().filter(|plan| plan.group == group) {
        println!("\n=== {} -> {}", plan.alpha_spec, plan.beta_spec());
        let (mut alpha, mut beta) = autobahn::supervisor::open_endpoints(plan, &state_root, &pool)?;

        let mut roots: Vec<Option<autobahn::tree::Node>> = Vec::new();
        for (side, endpoint) in [
            (
                "alpha",
                &mut alpha as &mut Box<dyn autobahn::endpoint::Endpoint + Send>,
            ),
            ("beta", &mut beta),
        ] {
            let snapshot = endpoint.scan()?;
            roots.push(snapshot.root.clone());
            match &snapshot.root {
                None => println!("  {side}: ROOT ABSENT"),
                Some(root) => {
                    let children = root.children();
                    println!(
                        "  {side}: {} children, {} dirs, {} files, {} links, {} bytes{}",
                        children.len(),
                        snapshot.directories,
                        snapshot.files,
                        snapshot.symlinks,
                        snapshot.total_file_size,
                        if children.is_empty() {
                            "   <-- CHILDLESS: counts as emptied"
                        } else {
                            ""
                        }
                    );
                    let names: Vec<&str> = children
                        .iter()
                        .take(6)
                        .map(|child| child.name.as_str())
                        .collect();
                    println!("        first children: {names:?}");
                    println!("        root content kind: {}", kind(root));
                }
            }
        }
        // What the emptied-subtree guard is looking at: a directory that
        // holds a substantial tree on one side and is empty or absent on
        // the other. The guard also requires the ancestor to have held
        // content there, which is not read here — so this over-reports
        // slightly, and every path it prints is worth looking at.
        println!("  directories populated on one side and empty/absent on the other:");
        let mut found = 0;
        compare(roots[0].as_ref(), roots[1].as_ref(), "", &mut found);
        if found == 0 {
            println!("        none");
        }
    }
    Ok(())
}

/// Walks the two trees together, reporting the divergences that trip the
/// emptied-subtree halt.
fn compare(
    alpha: Option<&autobahn::tree::Node>,
    beta: Option<&autobahn::tree::Node>,
    path: &str,
    found: &mut usize,
) {
    use autobahn::tree::{Content, Node};
    let children = |node: Option<&Node>| -> Vec<Node> {
        match node {
            Some(node) => node.children().to_vec(),
            None => Vec::new(),
        }
    };
    let directory =
        |node: Option<&Node>| matches!(node, Some(n) if matches!(n.content, Content::Directory(_)));
    let empty_or_absent =
        |node: Option<&Node>| !directory(node) || node.is_some_and(|n| n.children().is_empty());

    if !path.is_empty() && directory(alpha) != directory(beta)
        || (!path.is_empty() && empty_or_absent(alpha) != empty_or_absent(beta))
    {
        let populated = if empty_or_absent(alpha) { beta } else { alpha };
        let side = if empty_or_absent(alpha) {
            "beta"
        } else {
            "alpha"
        };
        let count = populated.map(entries_below).unwrap_or(0);
        if count >= 8 {
            *found += 1;
            println!(
                "        {path}   ({count} entries on {side}, gone on the other)  <-- HALTS HERE"
            );
            return;
        }
    }

    if !directory(alpha) || !directory(beta) {
        return;
    }
    // The union of both sides' names: a directory that exists only on beta
    // is exactly the case being looked for, and walking alpha's children
    // alone would never visit it.
    let (left, right) = (children(alpha), children(beta));
    let mut names: Vec<&str> = left
        .iter()
        .chain(right.iter())
        .map(|child| child.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let child_path = if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}/{name}")
        };
        compare(
            left.iter().find(|child| child.name == name),
            right.iter().find(|child| child.name == name),
            &child_path,
            found,
        );
    }
}

fn entries_below(node: &autobahn::tree::Node) -> usize {
    node.children()
        .iter()
        .map(|child| 1 + entries_below(child))
        .sum()
}

fn kind(node: &autobahn::tree::Node) -> &'static str {
    match &node.content {
        autobahn::tree::Content::Directory(_) => "directory",
        autobahn::tree::Content::File { .. } => "file",
        autobahn::tree::Content::Symlink { .. } => "symlink",
        autobahn::tree::Content::Problematic { .. } => "PROBLEMATIC (unreadable)",
        autobahn::tree::Content::Untracked => "UNTRACKED",
    }
}
