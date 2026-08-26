//! One resource series for one process pattern.
//!
//! Emits `epoch_seconds rss_kb cpu_jiffies process_count` once per second,
//! summed over every process whose command line contains the pattern (the
//! sampler itself and its ancestors excluded). Attribution to phases
//! happens downstream: the job records phase boundaries as timestamps and
//! the aggregator slices this series by them — a phase's peak is the peak
//! within that phase's window, never a process-lifetime figure wearing a
//! phase's caption, which is the mistake this design replaces.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

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
    let page_kb = 4096 / 1024; // Linux x86-64/arm64 base page size.

    let mut file = std::fs::File::create(output).map_err(|error| error.to_string())?;
    loop {
        let mut total_rss = 0u64;
        let mut total_jiffies = 0u64;
        let mut count = 0u32;
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
                    continue;
                };
                if pid == own || pid == parent {
                    continue;
                }
                let Ok(command) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                    continue;
                };
                let command = String::from_utf8_lossy(&command).replace('\0', " ");
                if !command.contains(pattern) {
                    continue;
                }
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    continue;
                };
                // Fields come after the parenthesized command, which can
                // itself contain spaces and parentheses.
                let Some(after) = stat.rsplit_once(')').map(|(_, after)| after) else {
                    continue;
                };
                let fields: Vec<&str> = after.split_whitespace().collect();
                // After the split: index 11 = utime, 12 = stime, 21 = rss
                // (in pages), matching stat fields 14, 15, and 24.
                if fields.len() > 21 {
                    let utime: u64 = fields[11].parse().unwrap_or(0);
                    let stime: u64 = fields[12].parse().unwrap_or(0);
                    let rss_pages: u64 = fields[21].parse().unwrap_or(0);
                    total_rss += rss_pages * page_kb as u64;
                    total_jiffies += utime + stime;
                    count += 1;
                }
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        writeln!(file, "{now:.3} {total_rss} {total_jiffies} {count}")
            .map_err(|error| error.to_string())?;
        let _ = file.flush();
        std::thread::sleep(Duration::from_secs(1));
    }
}
