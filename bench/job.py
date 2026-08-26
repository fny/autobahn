#!/usr/bin/env python3
"""Runs one job — one cell, both tools, on this host pair — and emits JSONL.

Host A (this host) is the source and driver; host B (reachable as the SSH
alias `dest`, private IP in ~/bench/peer-ip) is the destination and runs
the observers. Every record carries the run/pair/job/cell/repeat identity
and a schema version; every phase records its wall-clock boundaries so the
resource series can be sliced honestly afterwards.

The tool order comes from the caller (the orchestrator randomizes it per
job). State is destroyed and *verified* destroyed between tools.

Usage:
  job.py --spec '<json>' --output results.jsonl

Spec:
  {"run": ..., "pair": ..., "job": ..., "repeat": ...,
   "cell": {"name": ..., "corpora": ["chromium"], "agents": 10,
            "bidirectional": false},
   "tools": ["mutagen", "autobahn"]}
"""

import argparse
import hashlib
import json
import os
import shlex
import subprocess
import time

SCHEMA = 2
HOME = os.path.expanduser("~")
BENCH = f"{HOME}/bench"
CORPUS = f"{HOME}/corpus"
DEST = f"{HOME}/dest"
OBSERVER_BASE_PORT = 9911
COLD_SYNC_TIMEOUT_SECONDS = 3600
POLL_SECONDS = 5
QUIESCENCE_CHECKS = 2
QUIESCENCE_GAP_SECONDS = 10
IDLE_WINDOW_SECONDS = 60
WORKLOAD_SECONDS = 150

TOOL_PATTERNS = {
    # (local sampler pattern, remote sampler pattern, local kill, remote kill)
    "autobahn": ("autobahn up", "autobahn-", "[a]utobahn up", "[a]utobahn-"),
    "mutagen": ("mutagen daemon run", "mutagen-agent", "[m]utagen", "[m]utagen-agent"),
}


class Emitter:
    def __init__(self, path, identity):
        self.handle = open(path, "a", buffering=1)
        self.identity = identity

    def emit(self, record):
        self.handle.write(json.dumps({"schema": SCHEMA, **self.identity, **record}) + "\n")


def run(command, check=False, timeout=None):
    return subprocess.run(
        command, shell=True, check=check, timeout=timeout,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )


def peer(command, timeout=None):
    # Single-quoted so this host's shell cannot expand anything meant for
    # the peer, and -n so ssh never consumes our stdin.
    return run(f"ssh -n dest {shlex.quote(command)}", timeout=timeout)


def peer_ip():
    with open(f"{BENCH}/peer-ip") as handle:
        return handle.read().strip()


def self_ip():
    with open(f"{BENCH}/self-ip") as handle:
        return handle.read().strip()


# ── state hygiene ────────────────────────────────────────────────────

def destroy_tool_state(emitter):
    """Kills both tools and removes all their state, then verifies both
    hosts are actually clean. A dirty starting condition is a failed job,
    not a warning."""
    run(f"pkill -f '[a]utobahn up' 2>/dev/null; pkill -f '[m]utagen' 2>/dev/null; true")
    peer("pkill -f '[a]utobahn-' 2>/dev/null; pkill -f '[m]utagen-agent' 2>/dev/null; true")
    time.sleep(2)
    run(f"rm -rf {HOME}/.autobahn/sessions {HOME}/.autobahn/status {HOME}/.mutagen {HOME}/ab.toml")
    peer(f"rm -rf {HOME}/.autobahn/staging {HOME}/.mutagen")
    leftovers = run(
        "pgrep -af '[a]utobahn up|[m]utagen' | grep -v pgrep; true"
    ).stdout.strip()
    remote_leftovers = peer(
        "pgrep -af '[a]utobahn-|[m]utagen-agent' | grep -v pgrep; true"
    ).stdout.strip()
    if leftovers or remote_leftovers:
        emitter.emit({
            "measurement": "hygiene_failure",
            "local": leftovers, "remote": remote_leftovers,
        })
        raise RuntimeError("tool processes survived cleanup")


def clear_destinations(corpora):
    for corpus in corpora:
        peer(f"rm -rf {DEST}/{corpus} && mkdir -p {DEST}/{corpus}")


# ── convergence ──────────────────────────────────────────────────────

def summary(kind, root, remote):
    command = f"{BENCH}/benchmark manifest {kind} {root}"
    result = peer(command, timeout=1800) if remote else run(command, timeout=1800)
    return result.stdout.strip().splitlines()[-1]


def await_cold_sync(corpora, emitter, tool):
    """Waits for every corpus to converge: cheap match first, then a full
    digest verification as the arbiter. Returns per-corpus timings."""
    expectations = {c: summary("cheap", f"{CORPUS}/{c}", remote=False) for c in corpora}
    started = time.monotonic()
    timings = {}
    remaining = set(corpora)
    while remaining and time.monotonic() - started < COLD_SYNC_TIMEOUT_SECONDS:
        time.sleep(POLL_SECONDS)
        for corpus in sorted(remaining):
            observed = summary("cheap", f"{DEST}/{corpus}", remote=True)
            if observed == expectations[corpus]:
                count_matched = time.monotonic() - started
                source_full = summary("full", f"{CORPUS}/{corpus}", remote=False)
                destination_full = summary("full", f"{DEST}/{corpus}", remote=True)
                verified = time.monotonic() - started
                if source_full == destination_full:
                    timings[corpus] = {
                        "count_matched_s": round(count_matched, 1),
                        "digest_verified_s": round(verified, 1),
                        "verified": True,
                    }
                    remaining.discard(corpus)
                # A cheap match with a digest mismatch means the tree is
                # still moving (or wrong); keep polling either way.
    for corpus in remaining:
        timings[corpus] = {"verified": False, "timeout_s": COLD_SYNC_TIMEOUT_SECONDS}
    return timings


def await_quiescence(corpora):
    """True once every destination is stable across consecutive checks."""
    for _ in range(60):
        before = {c: summary("cheap", f"{DEST}/{c}", remote=True) for c in corpora}
        stable = True
        for _ in range(QUIESCENCE_CHECKS):
            time.sleep(QUIESCENCE_GAP_SECONDS)
            after = {c: summary("cheap", f"{DEST}/{c}", remote=True) for c in corpora}
            if after != before:
                stable = False
                break
        if stable:
            return True
    return False


def await_reconvergence(corpora, timeout_seconds=600):
    """After a workload, waits until both sides hold identical content
    again — the end-to-end no-divergence check. Both sides changed during
    a bidirectional workload, so the arbiter compares current full
    summaries of A and B to each other."""
    started = time.monotonic()
    results = {}
    for corpus in corpora:
        converged = False
        while time.monotonic() - started < timeout_seconds:
            source = summary("full", f"{CORPUS}/{corpus}", remote=False)
            destination = summary("full", f"{DEST}/{corpus}", remote=True)
            if source == destination:
                converged = True
                break
            time.sleep(POLL_SECONDS)
        results[corpus] = converged
    return results


# ── samplers ─────────────────────────────────────────────────────────

def start_samplers(tool):
    local_pattern, remote_pattern, _, _ = TOOL_PATTERNS[tool]
    run("pkill -f '[b]enchmark sampler' 2>/dev/null; true")
    peer("pkill -f '[b]enchmark sampler' 2>/dev/null; true")
    run(f"setsid nohup {BENCH}/benchmark sampler {shlex.quote(local_pattern)} "
        f"{HOME}/rss-local.log >/dev/null 2>&1 < /dev/null &")
    peer(f"setsid nohup {BENCH}/benchmark sampler {shlex.quote(remote_pattern)} "
         f"{HOME}/rss-remote.log >/dev/null 2>&1 < /dev/null &")


def collect_series():
    local = run(f"cat {HOME}/rss-local.log 2>/dev/null").stdout
    remote = peer(f"cat {HOME}/rss-remote.log 2>/dev/null").stdout
    def parse(text):
        rows = []
        for line in text.strip().splitlines():
            parts = line.split()
            if len(parts) == 4:
                rows.append([float(parts[0]), int(parts[1]), int(parts[2]), int(parts[3])])
        return rows
    return {"local": parse(local), "remote": parse(remote)}


# ── tools ────────────────────────────────────────────────────────────

def start_tool(tool, corpora):
    if tool == "autobahn":
        lines = []
        for corpus in corpora:
            lines += [
                f"[groups.{corpus}]",
                f'alpha = "{CORPUS}/{corpus}"',
                'mode = "two-way-safe"',
                "interval = 5",
                'ignores = ["/.git", "/out"]',
                f'betas = ["dest:{DEST}/{corpus}"]',
                "",
            ]
        with open(f"{HOME}/ab.toml", "w") as handle:
            handle.write("\n".join(lines))
        run(f"setsid nohup {HOME}/autobahn up --config {HOME}/ab.toml "
            f"> {HOME}/ab.log 2>&1 < /dev/null &")
    else:
        run(f"{HOME}/mutagen daemon start", check=True)
        for corpus in corpora:
            run(f"{HOME}/mutagen sync create --name={corpus} --sync-mode=two-way-safe "
                f"--ignore=/.git --ignore=/out --watch-polling-interval=5 "
                f"{CORPUS}/{corpus} dest:{DEST}/{corpus}", check=True)


# ── the floor ────────────────────────────────────────────────────────

def measure_floor(emitter):
    """The harness's own end-to-end latency with no sync tool anywhere,
    measured by the same binary that measures the tools."""
    result = run(
        f"{BENCH}/benchmark floor --observer {peer_ip()}:{OBSERVER_BASE_PORT} "
        f"--dest-root {DEST}", timeout=300,
    )
    peer(f"rm -rf {DEST}/floor-probe")
    parsed = None
    for line in result.stdout.strip().splitlines():
        try:
            parsed = json.loads(line)
        except json.JSONDecodeError:
            continue
    emitter.emit(parsed or {"measurement": "floor", "error": result.stdout[-500:]})


# ── workload ─────────────────────────────────────────────────────────

def run_workload(cell, emitter, tool):
    corpora = cell["corpora"]
    # Paranoia that costs nothing: both hosts re-assert partition
    # disjointness against the same baked file before any edit happens.
    for corpus in corpora:
        partitions = f"{CORPUS}/{corpus}.bench/partitions.json"
        run(f"{BENCH}/benchmark verify-partitions {partitions}", check=True)
        peer(f"{BENCH}/benchmark verify-partitions {partitions}")
    processes = []
    for index, corpus in enumerate(corpora):
        arguments = (
            f"--root {CORPUS}/{corpus} --peer-root {DEST}/{corpus} "
            f"--observer {peer_ip()}:{OBSERVER_BASE_PORT + index} "
            f"--partitions {CORPUS}/{corpus}.bench/partitions.json "
            f"--side a --agents {cell['agents']} --seconds {WORKLOAD_SECONDS} "
            f"--label {corpus}:A->B"
        )
        processes.append((
            f"{corpus}:A->B", None,
            subprocess.Popen(
                f"{BENCH}/benchmark agents {arguments}", shell=True,
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True),
        ))
        if cell.get("bidirectional"):
            remote_result = f"{HOME}/bda-{index}.json"
            remote_arguments = (
                f"--root {DEST}/{corpus} --peer-root {CORPUS}/{corpus} "
                f"--observer {self_ip()}:{OBSERVER_BASE_PORT + 100 + index} "
                f"--partitions {CORPUS}/{corpus}.bench/partitions.json "
                f"--side b --agents {cell['agents']} --seconds {WORKLOAD_SECONDS} "
                f"--label {corpus}:B->A"
            )
            processes.append((
                f"{corpus}:B->A", remote_result,
                subprocess.Popen(
                    ["ssh", "-n", "dest",
                     f"{BENCH}/benchmark agents {remote_arguments} "
                     f"> {remote_result} 2> {remote_result}.err"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
            ))

    for direction, remote_result, process in processes:
        if remote_result is None:
            output = process.communicate()[0]
        else:
            process.wait()
            output = peer(f"cat {remote_result} {remote_result}.err 2>/dev/null").stdout
        parsed = None
        for line in output.strip().splitlines():
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
        record = {"measurement": "workload", "tool": tool, "direction": direction}
        if parsed:
            record.update(parsed)
        else:
            record["error"] = output.strip()[-1000:]
        emitter.emit(record)


# ── one tool, one cell ───────────────────────────────────────────────

def run_tool(tool, cell, emitter):
    corpora = cell["corpora"]
    phases = {}

    def phase(name):
        phases[name] = phases.get(name, {})
        phases[name]["start"] = time.time()

    def phase_end(name):
        phases[name]["end"] = time.time()

    destroy_tool_state(emitter)
    clear_destinations(corpora)
    start_samplers(tool)
    time.sleep(1)

    phase("cold_sync")
    start_tool(tool, corpora)
    timings = await_cold_sync(corpora, emitter, tool)
    phase_end("cold_sync")
    emitter.emit({"measurement": "cold_sync", "tool": tool, "timings": timings})
    if not all(t.get("verified") for t in timings.values()):
        emitter.emit({"measurement": "abort", "tool": tool, "reason": "cold sync unverified"})
        destroy_tool_state(emitter)
        return

    phase("quiescence")
    settled = await_quiescence(corpora)
    phase_end("quiescence")

    phase("idle")
    time.sleep(IDLE_WINDOW_SECONDS)
    phase_end("idle")
    emitter.emit({"measurement": "idle_window", "tool": tool, "settled": settled})

    phase("workload")
    run_workload(cell, emitter, tool)
    phase_end("workload")

    phase("reconvergence")
    converged = await_reconvergence(corpora)
    phase_end("reconvergence")
    emitter.emit({
        "measurement": "reconvergence", "tool": tool, "converged": converged,
    })

    series = collect_series()
    emitter.emit({
        "measurement": "resources", "tool": tool,
        "phases": phases, "series": series,
    })
    destroy_tool_state(emitter)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--spec", required=True)
    parser.add_argument("--output", required=True)
    arguments = parser.parse_args()
    spec = json.loads(arguments.spec)

    identity = {key: spec[key] for key in ("run", "pair", "job", "repeat")}
    identity["cell"] = spec["cell"]["name"]
    emitter = Emitter(arguments.output, identity)
    emitter.emit({
        "measurement": "job_start",
        "spec": spec,
        "autobahn_version": run(f"{HOME}/autobahn --version").stdout.strip(),
        "mutagen_version": run(f"{HOME}/mutagen version").stdout.strip(),
    })

    measure_floor(emitter)
    for tool in spec["tools"]:
        try:
            run_tool(tool, spec["cell"], emitter)
        except Exception as error:  # noqa: BLE001 — a failed tool must be recorded, not raised past
            emitter.emit({
                "measurement": "tool_error", "tool": tool, "error": repr(error),
            })
            try:
                destroy_tool_state(emitter)
            except Exception:
                pass
    emitter.emit({"measurement": "job_complete"})


if __name__ == "__main__":
    main()
