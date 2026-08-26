//! One resource series for one tool's process *tree*.
//!
//! Seed processes are found by command-line pattern; every descendant of a
//! seed (via /proc PPid chains) is included regardless of its own command
//! line — a tool's SSH transport children and helpers belong to the tool
//! even though their argv never mentions it.
//!
//! CPU is reported as a monotone cumulative total: per-PID jiffies are
//! tracked between ticks, and a process that exits contributes its last
//! observed count to a persistent base, so the series never decreases and
//! windowed differences remain valid across process churn. The residual
//! error is what a process burned between its last sample and its exit —
//! bounded by one tick per exit and noted in the report.
//!
//! Output, once per second:
//!   `epoch_seconds rss_kb cumulative_cpu_jiffies process_count`

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

struct ProcessRecord {
    parent: u32,
    command: String,
    rss_kb: u64,
    jiffies: u64,
}

fn scan_processes() -> HashMap<u32, ProcessRecord> {
    let mut table = HashMap::new();
    let page_kb = 4096 / 1024;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in entries.filter_map(Result::ok) {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(command_raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let command = String::from_utf8_lossy(&command_raw).replace('\0', " ");
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Fields come after the parenthesized comm, which may itself
        // contain spaces or parentheses; split at the last ')'.
        let Some(after) = stat.rsplit_once(')').map(|(_, after)| after) else {
            continue;
        };
        let fields: Vec<&str> = after.split_whitespace().collect();
        // After the split: index 1 = ppid (stat field 4), 11 = utime (14),
        // 12 = stime (15), 21 = rss pages (24).
        if fields.len() > 21 {
            table.insert(
                pid,
                ProcessRecord {
                    parent: fields[1].parse().unwrap_or(0),
                    command,
                    rss_kb: fields[21].parse::<u64>().unwrap_or(0) * page_kb,
                    jiffies: fields[11].parse::<u64>().unwrap_or(0)
                        + fields[12].parse::<u64>().unwrap_or(0),
                },
            );
        }
    }
    table
}

/// Seeds plus every transitive descendant of a seed.
fn tool_tree(table: &HashMap<u32, ProcessRecord>, pattern: &str, excluded: &HashSet<u32>) -> HashSet<u32> {
    let mut members: HashSet<u32> = table
        .iter()
        .filter(|(pid, record)| record.command.contains(pattern) && !excluded.contains(pid))
        .map(|(&pid, _)| pid)
        .collect();
    loop {
        let additions: Vec<u32> = table
            .iter()
            .filter(|(pid, record)| {
                !members.contains(pid) && members.contains(&record.parent)
            })
            .map(|(&pid, _)| pid)
            .collect();
        if additions.is_empty() {
            break;
        }
        members.extend(additions);
    }
    members
}

pub fn run(pattern: &str, output: &Path) -> Result<(), String> {
    let own = std::process::id();
    let parent = std::fs::read_to_string(format!("/proc/{own}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("PPid:"))
                .and_then(|line| line.split_whitespace().nth(1)?.parse::<u32>().ok())
        })
        .unwrap_or(0);
    let excluded: HashSet<u32> = [own, parent].into_iter().collect();

    let mut file = std::fs::File::create(output).map_err(|error| error.to_string())?;
    // Jiffies of members seen last tick, and the accumulated total of
    // members that have exited.
    let mut last_seen: HashMap<u32, u64> = HashMap::new();
    let mut exited_base = 0u64;
    loop {
        let table = scan_processes();
        let members = tool_tree(&table, pattern, &excluded);

        // Anything tracked last tick that is gone (or no longer a member)
        // banks its final observed jiffies.
        let vanished: Vec<u32> = last_seen
            .keys()
            .filter(|pid| !members.contains(pid))
            .copied()
            .collect();
        for pid in vanished {
            exited_base += last_seen.remove(&pid).unwrap_or(0);
        }

        let mut live_jiffies = 0u64;
        let mut total_rss = 0u64;
        for &pid in &members {
            if let Some(record) = table.get(&pid) {
                live_jiffies += record.jiffies;
                total_rss += record.rss_kb;
                last_seen.insert(pid, record.jiffies);
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        writeln!(
            file,
            "{now:.3} {total_rss} {} {}",
            exited_base + live_jiffies,
            members.len()
        )
        .map_err(|error| error.to_string())?;
        let _ = file.flush();
        std::thread::sleep(Duration::from_secs(1));
    }
}
