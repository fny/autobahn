#!/usr/bin/env python3
"""Turns collected JSONL into the report tables.

Statistics across repeats are medians with the spread shown, never a
single run presented as the truth. Resource figures are computed by
slicing each job's sampled series by its recorded phase windows, so a
phase's peak is the peak inside that phase. Every anomaly the data
carries — censored samples, unverified convergence, failed jobs, missing
repeats — is surfaced, not smoothed over.

Usage: aggregate.py <results-directory>
"""

import json
import os
import statistics
import sys
from collections import defaultdict


def load(directory):
    records = []
    for name in sorted(os.listdir(directory)):
        if not name.endswith(".jsonl"):
            continue
        with open(os.path.join(directory, name)) as handle:
            for line in handle:
                line = line.strip()
                if line:
                    try:
                        records.append(json.loads(line))
                    except json.JSONDecodeError:
                        records.append({"measurement": "corrupt_line", "raw": line[:200]})
    return records


def phase_window(resources, phase):
    window = resources.get("phases", {}).get(phase)
    if not window or "start" not in window or "end" not in window:
        return None
    return window["start"], window["end"]


def slice_series(series, window):
    """Rows of one host's series inside a phase window."""
    if window is None:
        return []
    start, end = window
    return [row for row in series if start <= row[0] <= end]


def windowed_resources(resources, phase):
    """Per-phase peak RSS and mean CPU (% of one core) for both hosts.
    CPU differences cumulative jiffies across the window; short-lived
    children between samples are bounded by one sample interval each."""
    window = phase_window(resources, phase)
    out = {}
    for host in ("local", "remote"):
        rows = slice_series(resources.get("series", {}).get(host, []), window)
        if len(rows) < 2:
            out[host] = None
            continue
        peak_rss = max(row[1] for row in rows)
        jiffies = rows[-1][2] - rows[0][2]
        seconds = rows[-1][0] - rows[0][0]
        cpu = round(jiffies / 100 / seconds * 100, 1) if seconds > 0 else None
        out[host] = {"peak_rss_kb": peak_rss, "cpu_percent_of_core": cpu}
    return out


def median_spread(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    return {
        "median": round(statistics.median(values), 1),
        "min": round(min(values), 1),
        "max": round(max(values), 1),
        "n": len(values),
    }


def main():
    records = load(sys.argv[1])

    problems = []
    for record in records:
        kind = record.get("measurement")
        if kind in ("tool_error", "hygiene_failure", "abort", "corrupt_line"):
            problems.append(record)
        if kind == "workload" and record.get("censored"):
            problems.append({"measurement": "censored_samples", **{
                k: record.get(k) for k in ("cell", "tool", "direction", "censored", "repeat")}})
        if kind == "reconvergence" and not all(record.get("converged", {}).values()):
            problems.append(record)
        if kind == "cold_sync" and not all(
            t.get("verified") for t in record.get("timings", {}).values()
        ):
            problems.append(record)

    # Latency per (cell, tool, direction) across repeats.
    latency = defaultdict(lambda: defaultdict(list))
    for record in records:
        if record.get("measurement") == "workload" and "p50_ms" in record:
            key = (record["cell"], record["tool"], record.get("direction", "?"))
            for metric in ("p50_ms", "p90_ms", "p99_ms", "max_ms"):
                latency[key][metric].append(record.get(metric))
            latency[key]["samples"].append(record.get("samples"))

    # Cold sync per (cell, tool, corpus).
    cold = defaultdict(list)
    for record in records:
        if record.get("measurement") == "cold_sync":
            for corpus, timing in record.get("timings", {}).items():
                if timing.get("verified"):
                    cold[(record["cell"], record["tool"], corpus)].append(
                        timing["count_matched_s"])

    # Windowed resources per (cell, tool, phase).
    resources = defaultdict(lambda: defaultdict(list))
    for record in records:
        if record.get("measurement") == "resources":
            for phase in ("cold_sync", "idle", "workload"):
                windowed = windowed_resources(record, phase)
                for host in ("local", "remote"):
                    if windowed.get(host):
                        key = (record["cell"], record["tool"], phase, host)
                        resources[key]["peak_rss_kb"].append(windowed[host]["peak_rss_kb"])
                        resources[key]["cpu"].append(windowed[host]["cpu_percent_of_core"])

    floors = [r for r in records if r.get("measurement") == "floor" and r.get("p50_ms")]

    report = {
        "jobs_started": sum(1 for r in records if r.get("measurement") == "job_start"),
        "jobs_completed": sum(1 for r in records if r.get("measurement") == "job_complete"),
        "floor_p50_ms": median_spread([f["p50_ms"] for f in floors]),
        "latency": {
            f"{cell}/{tool}/{direction}": {
                metric: median_spread(values) for metric, values in metrics.items()
            }
            for (cell, tool, direction), metrics in sorted(latency.items())
        },
        "cold_sync_s": {
            f"{cell}/{tool}/{corpus}": median_spread(values)
            for (cell, tool, corpus), values in sorted(cold.items())
        },
        "resources": {
            f"{cell}/{tool}/{phase}/{host}": {
                "peak_rss_kb": median_spread(values["peak_rss_kb"]),
                "cpu_percent_of_core": median_spread(values["cpu"]),
            }
            for (cell, tool, phase, host), values in sorted(resources.items())
        },
        "problems": problems,
    }
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
