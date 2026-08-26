#!/usr/bin/env python3
"""Turns collected JSONL into report tables, honestly.

The rules, each answering a way a summary can lie:

- Delivered results are compared against the *plan* (plan.json in the
  results directory): a job that never reported is listed as missing, not
  silently absent from averages.
- A tool-run whose tree diverged (failed reconvergence) or errored is
  *tainted*: its latency is excluded from headline statistics and listed
  under problems, never quietly blended in.
- Latency percentiles are computed two ways: pooled over every raw sample
  across repeats (the primary), and median-of-per-run-percentiles (shown
  for comparison). Censored attempts are carried into pooled percentiles
  as "over deadline" positions.
- Idle CPU comes only from runs that verified quiescence first.
- Resource windows use the sampled series sliced by recorded phase
  boundaries, with the baseline taken from the last sample *before* the
  window (so startup work inside the window is counted), and remote
  timestamps corrected by the measured clock offset.
- The harness floor is reported beside every latency table.

Usage: aggregate.py <results-directory>
"""

import json
import os
import statistics
import sys
from collections import defaultdict


def load(directory):
    records, plan = [], None
    for name in sorted(os.listdir(directory)):
        path = os.path.join(directory, name)
        if name == "plan.json":
            plan = json.load(open(path))
        if not name.endswith(".jsonl"):
            continue
        with open(path) as handle:
            for line in handle:
                line = line.strip()
                if line:
                    try:
                        records.append(json.loads(line))
                    except json.JSONDecodeError:
                        records.append({"measurement": "corrupt_line",
                                        "file": name, "raw": line[:200]})
    return records, plan


def run_key(record):
    return (record.get("job"), record.get("tool"))


def find_tainted(records):
    """Tool-runs whose latency must not enter headline statistics, and
    why."""
    tainted = {}
    for record in records:
        key = run_key(record)
        kind = record.get("measurement")
        if kind == "tool_error":
            tainted[key] = "tool_error"
        elif kind == "reconvergence" and not all(record.get("converged", {}).values()):
            tainted[key] = "diverged"
        elif kind == "cold_sync" and not all(
            t.get("verified") for t in record.get("timings", {}).values()
        ):
            tainted[key] = "cold_sync_unverified"
        elif kind == "job_complete":
            # The driver's verdict covers the whole tool-run: a workload
            # error in either direction taints every latency sample that
            # tool produced in this job.
            for tool, status in record.get("statuses", {}).items():
                if status in ("workload_error", "diverged", "error"):
                    tainted.setdefault((record.get("job"), tool), status)
    return tainted


def percentile_from_pool(samples, censored, fraction, deadline_ms):
    """A percentile over attempts, censored attempts occupying the top
    positions. Landing in the censored region reports the deadline as a
    lower bound rather than a number."""
    attempts = len(samples) + censored
    if attempts == 0:
        return None
    ordered = sorted(samples)
    index = round(fraction * (attempts - 1))
    if index < len(ordered):
        return round(ordered[index], 1)
    return f">{deadline_ms}"


def median_spread(values):
    values = [v for v in values if isinstance(v, (int, float))]
    if not values:
        return None
    return {"median": round(statistics.median(values), 1),
            "min": round(min(values), 1), "max": round(max(values), 1),
            "n": len(values)}


def windowed(resources, phase, host):
    """Peak RSS and mean CPU inside one phase window for one host, with
    the CPU baseline taken from the last sample at or before the window's
    start and remote timestamps shifted by the measured clock offset."""
    window = resources.get("phases", {}).get(phase)
    if not window or "start" not in window or "end" not in window:
        return None
    start, end = window["start"], window["end"]
    series = resources.get("series", {}).get(host, [])
    if host == "remote":
        offset = resources.get("clock_offset", {}).get("offset_s", 0.0)
        series = [[row[0] - offset, row[1], row[2], row[3]] for row in series]
    inside = [row for row in series if start <= row[0] <= end]
    if len(inside) < 2:
        return None
    baseline = None
    for row in series:
        if row[0] <= start:
            baseline = row
        else:
            break
    baseline = baseline or inside[0]
    peak_rss = max(row[1] for row in inside)
    jiffies = inside[-1][2] - baseline[2]
    seconds = inside[-1][0] - baseline[0]
    cpu = round(jiffies / 100 / seconds * 100, 1) if seconds > 0 else None
    return {"peak_rss_kb": peak_rss, "cpu_percent_of_core": cpu}


def main():
    records, plan = load(sys.argv[1])
    tainted = find_tainted(records)

    problems = []
    for record in records:
        kind = record.get("measurement")
        if kind in ("tool_error", "hygiene_failure", "abort", "corrupt_line"):
            problems.append(record)
        elif kind == "workload" and (
            "error" in record or record.get("censored")
            or record.get("background_write_errors")
            or record.get("background_panics")
            or record.get("skipped_ticks")
        ):
            problems.append({k: record.get(k) for k in
                             ("measurement", "cell", "tool", "direction",
                              "repeat", "job", "censored", "error",
                              "background_write_errors", "background_panics",
                              "skipped_ticks")})
        elif kind == "floor" and ("error" in record or record.get("failures")):
            problems.append(record)
        elif kind == "reconvergence" and not all(record.get("converged", {}).values()):
            problems.append(record)
        elif kind == "idle_window" and not record.get("settled"):
            problems.append({k: record.get(k) for k in
                             ("measurement", "cell", "tool", "job", "settled")})

    # Delivered vs planned.
    started = {r.get("job") for r in records if r.get("measurement") == "job_start"}
    completed = {r.get("job") for r in records if r.get("measurement") == "job_complete"}
    missing, unfinished = [], sorted(started - completed)
    if plan:
        planned = {job["job"] for job in plan.get("jobs", [])}
        missing = sorted(planned - started)

    # Latency: pooled raw samples per (cell, tool, direction), tainted
    # runs excluded and counted.
    pools = defaultdict(lambda: {"samples": [], "censored": 0,
                                 "per_run_p50": [], "runs": 0, "tainted": 0,
                                 "deadline_ms": 120000})
    for record in records:
        if record.get("measurement") != "workload":
            continue
        key = (record.get("cell"), record.get("tool"), record.get("direction"))
        pool = pools[key]
        if run_key(record) in tainted:
            pool["tainted"] += 1
            continue
        if "error" in record:
            continue  # already under problems
        pool["runs"] += 1
        pool["samples"].extend(record.get("samples_ms", []))
        pool["censored"] += record.get("censored", 0)
        if record.get("censored_over_ms"):
            pool["deadline_ms"] = record["censored_over_ms"]
        if isinstance(record.get("p50_ms"), (int, float)):
            pool["per_run_p50"].append(record["p50_ms"])

    latency = {}
    for (cell, tool, direction), pool in sorted(pools.items()):
        latency[f"{cell}/{tool}/{direction}"] = {
            "runs": pool["runs"],
            "tainted_runs_excluded": pool["tainted"],
            "pooled_samples": len(pool["samples"]),
            "censored": pool["censored"],
            "p50_ms": percentile_from_pool(pool["samples"], pool["censored"], 0.50, pool["deadline_ms"]),
            "p90_ms": percentile_from_pool(pool["samples"], pool["censored"], 0.90, pool["deadline_ms"]),
            "p99_ms": percentile_from_pool(pool["samples"], pool["censored"], 0.99, pool["deadline_ms"]),
            "per_run_p50_spread": median_spread(pool["per_run_p50"]),
        }

    # Cold sync: the digest-verified time is the headline; the cheap match
    # is context. Tainted runs are excluded automatically because
    # unverified timings never carry digest_verified_s.
    cold = defaultdict(lambda: {"verified": [], "count_matched": []})
    for record in records:
        if record.get("measurement") != "cold_sync":
            continue
        for corpus, timing in record.get("timings", {}).items():
            if timing.get("verified"):
                key = (record["cell"], record["tool"], corpus)
                cold[key]["verified"].append(timing["digest_verified_s"])
                cold[key]["count_matched"].append(timing["count_matched_s"])

    # Resources per (cell, tool, phase, host), idle only from settled runs.
    settled_runs = {run_key(r) for r in records
                    if r.get("measurement") == "idle_window" and r.get("settled")}
    resources = defaultdict(lambda: defaultdict(list))
    for record in records:
        if record.get("measurement") != "resources":
            continue
        for phase in ("cold_sync", "idle", "workload"):
            if phase == "idle" and run_key(record) not in settled_runs:
                continue
            for host in ("local", "remote"):
                sliced = windowed(record, phase, host)
                if sliced:
                    key = (record["cell"], record["tool"], phase, host)
                    resources[key]["peak_rss_kb"].append(sliced["peak_rss_kb"])
                    resources[key]["cpu"].append(sliced["cpu_percent_of_core"])

    floors = [r for r in records
              if r.get("measurement") == "floor" and isinstance(r.get("p50_ms"), (int, float))]

    report = {
        "plan": {"jobs": len(plan["jobs"]), "seed": plan.get("seed")} if plan else None,
        "jobs": {"started": len(started), "completed": len(completed),
                 "missing_from_plan": missing, "started_but_unfinished": unfinished},
        "floor_p50_ms": median_spread([f["p50_ms"] for f in floors]),
        "latency": latency,
        "cold_sync_s": {
            f"{cell}/{tool}/{corpus}": {
                "digest_verified": median_spread(values["verified"]),
                "count_matched": median_spread(values["count_matched"]),
            }
            for (cell, tool, corpus), values in sorted(cold.items())
        },
        "resources": {
            f"{cell}/{tool}/{phase}/{host}": {
                "peak_rss_kb": median_spread(values["peak_rss_kb"]),
                "cpu_percent_of_core": median_spread(values["cpu"]),
            }
            for (cell, tool, phase, host), values in sorted(resources.items())
        },
        "tainted_runs": {f"{job}/{tool}": reason
                         for (job, tool), reason in sorted(tainted.items()) if job},
        "problems": problems,
    }
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
