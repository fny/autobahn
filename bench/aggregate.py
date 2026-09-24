#!/usr/bin/env python3
"""Turns collected JSONL into report tables, honestly.

The rules, each answering a way a summary can lie:

- Delivered results are compared against the *plan* (plan.json in the
  results directory): a job that never reported is listed as missing, not
  silently absent from averages.
- A tool-run whose tree diverged (failed reconvergence) or errored, or
  whose job ran a different cell than it claims, is *tainted*: one rule
  (`excluded`) keeps it out of every headline figure — latency, cold sync,
  resources, bursts — and each table lists what it left out and why,
  never quietly blending it in.
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


def destination_count(record, plan):
    """How many destinations a job actually ran with: recorded in its
    job_start, or, for runs from before the record existed, the width of
    the group the plan put it on — the driver then used every destination
    its group offered, whatever the cell asked for."""
    if "destinations" in record:
        return len(record["destinations"])
    pair = record.get("pair") or record.get("spec", {}).get("pair")
    groups = (plan or {}).get("groups")
    if not groups or not isinstance(pair, str) or not pair.startswith("pair-"):
        return None
    try:
        width = groups[int(pair[len("pair-"):])][0]
    except (ValueError, IndexError, TypeError):
        return None
    return width - 1  # one source, the rest destinations


def find_tainted(records, plan=None):
    """Tool-runs whose records must not enter headline statistics, and
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
        elif kind == "workload" and (
            record.get("background_write_errors") or record.get("background_panics")
        ):
            # The offered load was not what the report claims; every
            # latency sample this tool-run produced is suspect.
            tainted.setdefault(key, "background_load_failure")
        elif kind == "job_start":
            # A job that ran with more destinations than its cell asked
            # for was a different cell — a fan-out under a pairwise name.
            # This is what turned six jobs of bench-1789947877 into 10x
            # anomalies before the record existed to say so; for that run
            # the count comes from the plan's groups.
            wanted = record.get("spec", {}).get("cell", {}).get("betas")
            got = destination_count(record, plan)
            if wanted is not None and got is not None and got != wanted:
                for tool in record.get("spec", {}).get("tools", []):
                    tainted[(record.get("job"), tool)] = (
                        f"destination_width_mismatch:{got}_of_{wanted}")
        elif kind == "job_complete":
            # The driver's verdict covers the whole tool-run: a workload
            # error in either direction taints every latency sample that
            # tool produced in this job, and a tool that never settled
            # before the workload was measured with a pre-existing
            # backlog.
            for tool, status in record.get("statuses", {}).items():
                if status in ("workload_error", "diverged", "error",
                              "unsettled_idle"):
                    tainted.setdefault((record.get("job"), tool), status)
    return tainted


def excluded(record, tainted):
    """Why this tool-run's record stays out of every headline figure, or
    None. The single exclusion rule: every aggregation asks it, so no table
    can admit a run another table dropped."""
    return tainted.get(run_key(record))


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


def window_rows(series, start, end):
    """The rows inside [start, end], and the CPU baseline: the last row at
    or before the start (so startup work inside the window is counted), or
    the first row inside."""
    inside = [row for row in series if start <= row[0] <= end]
    baseline = None
    for row in series:
        if row[0] <= start:
            baseline = row
        else:
            break
    return inside, (baseline or (inside[0] if inside else None))


def windowed(resources, phase, host):
    """Peak RSS and mean CPU inside one phase window for one host, with
    the CPU baseline taken from the last sample at or before the window's
    start and remote timestamps shifted by the measured clock offset.

    Several destinations' CPU is the sum of each one's own change over the
    window, from its own samples: cumulative counters from different hosts
    are never differenced across a merged series."""
    window = resources.get("phases", {}).get(phase)
    if not window or "start" not in window or "end" not in window:
        return None
    start, end = window["start"], window["end"]
    offset = resources.get("clock_offset", {}).get("offset_s", 0.0) if host == "remote" else 0.0

    def shifted(series):
        return [[row[0] - offset, row[1], row[2], row[3]] for row in series]

    series = shifted(resources.get("series", {}).get(host, []))
    inside, baseline = window_rows(series, start, end)
    if len(inside) < 2:
        return None
    peak_rss = max(row[1] for row in inside)
    per_host = resources.get("series", {}).get("remote_by_host") if host == "remote" else None
    if per_host:
        rate = 0.0
        for own in per_host:
            own_inside, own_baseline = window_rows(shifted(own), start, end)
            if len(own_inside) < 2:
                continue
            seconds = own_inside[-1][0] - own_baseline[0]
            if seconds > 0:
                rate += (own_inside[-1][2] - own_baseline[2]) / 100 / seconds
        return {"peak_rss_kb": peak_rss, "cpu_percent_of_core": round(rate * 100, 1)}
    jiffies = inside[-1][2] - baseline[2]
    seconds = inside[-1][0] - baseline[0]
    cpu = round(jiffies / 100 / seconds * 100, 1) if seconds > 0 else None
    return {"peak_rss_kb": peak_rss, "cpu_percent_of_core": cpu}


def main():
    records, plan = load(sys.argv[1])
    tainted = find_tainted(records, plan)

    problems = []
    for record in records:
        kind = record.get("measurement")
        if kind in ("tool_error", "hygiene_failure", "abort", "corrupt_line"):
            problems.append(record)
        elif kind == "job_start" and destination_count(record, plan) not in (
            None, record.get("spec", {}).get("cell", {}).get("betas")
        ):
            problems.append({"measurement": "destination_width_mismatch",
                             "cell": record.get("cell"), "job": record.get("job"),
                             "destinations": record.get("destinations",
                                                        destination_count(record, plan)),
                             "betas": record.get("spec", {}).get("cell", {}).get("betas")})
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
        elif kind == "resources" and not any(
            row[3] for host in ("local", "remote")
            for row in record.get("series", {}).get(host, [])
        ):
            # Every sample saw zero processes: the sampler's patterns
            # never matched this tool. Resource numbers would read as a
            # confident zero, which is worse than no number at all.
            problems.append({"measurement": "resource_sampling_never_matched",
                             "cell": record.get("cell"), "tool": record.get("tool"),
                             "job": record.get("job")})
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
                                 "skipped_ticks": 0,
                                 "per_run_p50": [], "runs": 0, "tainted": 0,
                                 "excluded": [], "deadline_ms": 120000})
    for record in records:
        if record.get("measurement") != "workload":
            continue
        key = (record.get("cell"), record.get("tool"), record.get("direction"))
        pool = pools[key]
        reason = excluded(record, tainted)
        if reason:
            pool["tainted"] += 1
            pool["excluded"].append({"job": record.get("job"), "reason": reason})
            continue
        if "error" in record:
            continue  # already under problems
        pool["runs"] += 1
        pool["samples"].extend(record.get("samples_ms", []))
        pool["censored"] += record.get("censored", 0)
        pool["skipped_ticks"] += record.get("skipped_ticks", 0)
        if record.get("censored_over_ms"):
            pool["deadline_ms"] = record["censored_over_ms"]
        if isinstance(record.get("p50_ms"), (int, float)):
            pool["per_run_p50"].append(record["p50_ms"])

    latency = {}
    for (cell, tool, direction), pool in sorted(pools.items()):
        latency[f"{cell}/{tool}/{direction}"] = {
            "runs": pool["runs"],
            "tainted_runs_excluded": pool["tainted"],
            "excluded": pool["excluded"],
            "pooled_samples": len(pool["samples"]),
            "censored": pool["censored"],
            # Ticks the measuring agent skipped — at full in-flight
            # capacity, or because its eight random probes all hit files
            # with a verification already in flight. An offered-load
            # shortfall that qualifies this row; the censored count is
            # the companion evidence when the cause is tool slowness.
            "skipped_ticks": pool["skipped_ticks"],
            "p50_ms": percentile_from_pool(pool["samples"], pool["censored"], 0.50, pool["deadline_ms"]),
            "p90_ms": percentile_from_pool(pool["samples"], pool["censored"], 0.90, pool["deadline_ms"]),
            "p99_ms": percentile_from_pool(pool["samples"], pool["censored"], 0.99, pool["deadline_ms"]),
            "per_run_p50_spread": median_spread(pool["per_run_p50"]),
        }

    # Cold sync: the digest-verified time is the headline; the cheap match
    # is context. A run the exclusion rule drops is left out even when its
    # own timing verified: a job that ran with the wrong number of
    # destinations synchronized a different cell, and its time belongs to
    # no row.
    cold = defaultdict(lambda: {"verified": [], "count_matched": [], "excluded": []})
    for record in records:
        if record.get("measurement") != "cold_sync":
            continue
        reason = excluded(record, tainted)
        for corpus, timing in record.get("timings", {}).items():
            if reason:
                key = (record["cell"], record["tool"], corpus)
                cold[key]["excluded"].append({"job": record.get("job"), "reason": reason})
                continue
            # A pre-seeded cell starts converged, so there is no cold sync
            # to time: it reports `verified` with no duration at all. The
            # guard below used to read `verified` as proof that a duration
            # was there, which was true until seeding existed and then
            # crashed the whole report on the first latency cell.
            if timing.get("pre_seeded") or "digest_verified_s" not in timing:
                continue
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
        if excluded(record, tainted):
            continue  # a run that misbehaved yields no headline resources
        if not any(row[3] for host in ("local", "remote")
                   for row in record.get("series", {}).get(host, [])):
            continue  # the sampler never matched this tool; zero is not a measurement
        for phase in ("cold_sync", "idle", "workload"):
            if phase == "idle" and run_key(record) not in settled_runs:
                continue
            for host in ("local", "remote"):
                sliced = windowed(record, phase, host)
                if sliced:
                    key = (record["cell"], record["tool"], phase, host)
                    resources[key]["peak_rss_kb"].append(sliced["peak_rss_kb"])
                    resources[key]["cpu"].append(sliced["cpu_percent_of_core"])

    # Bursts: the wall time until a module copied in has landed, per run
    # the median of its bursts, and autobahn's own cycle seconds beside it.
    bursts = defaultdict(lambda: {"wall": [], "cycle": [], "files": 0})
    for record in records:
        if record.get("measurement") != "burst" or excluded(record, tainted):
            continue
        walls = [w for w in record.get("walls_s", []) if isinstance(w, (int, float))]
        key = (record["cell"], record["tool"])
        if walls:
            bursts[key]["wall"].append(statistics.median(walls))
        cycles = record.get("cycle_seconds") or []
        if cycles:
            bursts[key]["cycle"].append(statistics.median(cycles))
        bursts[key]["files"] = record.get("files_per_burst", 0)

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
                "excluded": values["excluded"],
            }
            for (cell, tool, corpus), values in sorted(cold.items())
        },
        "burst_s": {
            f"{cell}/{tool}": {
                "files": values["files"],
                "wall": median_spread(values["wall"]),
                "cycle_seconds": median_spread(values["cycle"]) if values["cycle"] else None,
            }
            for (cell, tool), values in sorted(bursts.items())
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
