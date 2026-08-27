#!/usr/bin/env python3
"""Runs one job — one cell, both tools, on this host pair — and emits JSONL.

Host A (this host) is the source and driver; host B (SSH alias `dest`,
private IP in ~/bench/peer-ip) is the destination and runs the observers.
Every record carries run/pair/job/cell/repeat identity, a schema version,
and tool/binary provenance; every phase records wall-clock boundaries plus
a measured A↔B clock offset, so resource series can be sliced honestly.

Tool order comes from the caller (randomized per job by the orchestrator).
State is destroyed and *verified* destroyed between tools — including both
tools' installed remote agents, so every run pays first-contact
installation identically.

Process cleanup matches by exact executable paths and comm names, never by
substring of a command line: this driver's own argv contains the words
"mutagen" and "autobahn" (they are in the job spec), and a substring pkill
would kill the driver itself — the previous harness did exactly that.

For local testing (`smoke.sh`), BENCH_LOCAL=1 makes `peer` run locally and
the spec may name the `toysync` tool.

Usage:
  job.py --spec '<json>' --output results.jsonl
"""

import argparse
import json
import os
import shlex
import subprocess
import time

SCHEMA = 3
HOME = os.environ.get("BENCH_HOME", os.path.expanduser("~"))
LOCAL = os.environ.get("BENCH_LOCAL") == "1"
BENCH = f"{HOME}/bench"
CORPUS = f"{HOME}/corpus"
DEST = f"{HOME}/dest"
BINARY = f"{BENCH}/benchmark"
OBSERVER_BASE_PORT = int(os.environ.get("BENCH_OBSERVER_PORT", "9911"))
REVERSE_OBSERVER_BASE_PORT = OBSERVER_BASE_PORT + 100
COLD_SYNC_TIMEOUT_SECONDS = 3600
POLL_SECONDS = 5
IDLE_WINDOW_SECONDS = 60
WORKLOAD_SECONDS = int(os.environ.get("BENCH_WORKLOAD_SECONDS", "150"))

TOOLS = {
    # pattern: sampler seed pattern (command-line substring unique to the
    #          tool's processes and impossible in this driver's argv,
    #          because it includes the executable's path);
    # comm:    exact process names for cleanup, local and remote.
    "autobahn": {
        "local_pattern": f"{HOME}/autobahn up",
        "remote_pattern": ".autobahn/bin/autobahn-",
        "local_comms": ["autobahn"],
        "remote_comms": ["autobahn-.*"],
    },
    "mutagen": {
        "local_pattern": f"{HOME}/mutagen daemon run",
        "remote_pattern": ".mutagen/agents/",
        "local_comms": ["mutagen"],
        "remote_comms": ["mutagen-agent"],
    },
    "toysync": {
        "local_pattern": f"{BENCH}/toysync.py",
        "remote_pattern": f"{BENCH}/toysync.py",
        "local_comms": [],  # cleaned by pattern below, which is safe: the
        "remote_comms": [],  # driver's argv never contains toysync.py.
    },
}


class Emitter:
    def __init__(self, path, identity):
        self.handle = open(path, "a", buffering=1)
        self.identity = identity

    def emit(self, record):
        self.handle.write(json.dumps({"schema": SCHEMA, **self.identity, **record}) + "\n")


def run(command, check=False, timeout=None):
    """Runs a shell command locally. Anything measurement-adjacent goes
    through argv lists instead (see run_argv); shell=True here is for
    plumbing only and never receives interpolated labels or specs."""
    return subprocess.run(
        command, shell=True, check=check, timeout=timeout,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )


def run_argv(argv, check=False, timeout=None):
    return subprocess.run(
        argv, check=check, timeout=timeout,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )


def peer(command, timeout=None):
    """Runs a shell command on host B. Single-quoted so this host's shell
    cannot expand anything, and -n so ssh never consumes stdin."""
    if LOCAL:
        return run(command, timeout=timeout)
    return run(f"ssh -n dest {shlex.quote(command)}", timeout=timeout)


def peer_ip():
    if LOCAL:
        return "127.0.0.1"
    with open(f"{BENCH}/peer-ip") as handle:
        return handle.read().strip()


def self_ip():
    if LOCAL:
        return "127.0.0.1"
    with open(f"{BENCH}/self-ip") as handle:
        return handle.read().strip()


def clock_offset():
    """Estimates B's wall clock minus A's, bracketing B's reading between
    two local readings. Accuracy is ± half the probe's round trip, which
    is recorded alongside so the aggregator can judge it."""
    before = time.time()
    remote = float(peer("date +%s.%N").stdout.strip().splitlines()[-1])
    after = time.time()
    return {"offset_s": remote - (before + after) / 2, "uncertainty_s": (after - before) / 2}


# ── state hygiene ────────────────────────────────────────────────────

def kill_tools():
    """Kills both tools everywhere, by exact comm or full executable path
    — never by a substring that could appear in this driver's argv.

    In LOCAL mode only toysync is touched: local mode runs on a developer
    machine that may host *real* synchronization processes, and killing a
    production agent from a smoke test is precisely the kind of collateral
    this harness exists to rule out."""
    # The bracket makes the pattern not match its own shell wrapper.
    run(f"pkill -f 'python3 {BENCH}/[t]oysync.py'; true")
    # A driver that died mid-workload can leave orphaned workload agents
    # editing the corpus; they would contaminate the next tool's restore,
    # floor, and cold sync. The "agents" word never matches an observer.
    run(f"pkill -f '{BINARY} [a]gents'; true")
    peer(f"pkill -f '{BINARY} [a]gents'; true")
    if LOCAL:
        time.sleep(1)
        return
    run("pkill -x autobahn; pkill -x mutagen; true")
    peer("pkill '^autobahn-'; pkill -x mutagen-agent; true")
    time.sleep(2)


def destroy_tool_state(emitter):
    """Removes all tool state on both hosts and verifies cleanliness.
    Both tools lose their installed remote agents too, so every cold sync
    pays first-contact installation — the same cost for both."""
    kill_tools()
    result = run(f"rm -rf {HOME}/.autobahn {HOME}/.mutagen {HOME}/ab.toml")
    if result.returncode != 0:
        raise RuntimeError(f"local state removal failed: {result.stdout[-300:]}")
    result = peer(f"rm -rf {HOME}/.autobahn {HOME}/.mutagen")
    if result.returncode != 0:
        raise RuntimeError(f"remote state removal failed: {result.stdout[-300:]}")
    if LOCAL:
        leftovers = run(f"pgrep -f 'python3 {BENCH}/[t]oysync.py'; true").stdout.strip()
        remote_leftovers = ""
    else:
        leftovers = run(
            "pgrep -x autobahn; pgrep -x mutagen; true"
        ).stdout.strip()
        remote_leftovers = peer(
            "pgrep '^autobahn-'; pgrep -x mutagen-agent; true"
        ).stdout.strip()
    if leftovers or remote_leftovers:
        emitter.emit({"measurement": "hygiene_failure",
                      "local": leftovers, "remote": remote_leftovers})
        raise RuntimeError("tool processes survived cleanup")


def restore_sources(corpora):
    """Restores every file a workload may have edited from the pristine
    copy baked into the image, so each tool (and each job on this pair)
    starts from identical source content. The set of restorable files is
    exactly the union of all partitions — nothing else is ever edited."""
    for corpus in corpora:
        pristine = f"{HOME}/corpus-pristine/{corpus}"
        if not os.path.isdir(pristine):
            if LOCAL:
                continue  # the smoke test owns its own corpus lifecycle
            raise RuntimeError(f"pristine copy missing: {pristine}")
        with open(f"{CORPUS}/{corpus}.bench/partitions.json") as handle:
            partitions = json.load(handle)
        files = set()
        for by_count in partitions["sides"].values():
            for sets in by_count.values():
                files.update(sets["measured"])
                for background in sets["background"]:
                    files.update(background)
        listing = f"{HOME}/restore-list.txt"
        with open(listing, "w") as handle:
            handle.write("\n".join(sorted(files)) + "\n")
        result = run(f"rsync -a --files-from={listing} {pristine}/ {CORPUS}/{corpus}/")
        if result.returncode != 0:
            raise RuntimeError(f"source restore failed: {result.stdout[-300:]}")


def clear_destinations(corpora):
    for corpus in corpora:
        result = peer(f"rm -rf {DEST}/{corpus} && mkdir -p {DEST}/{corpus}")
        if result.returncode != 0:
            raise RuntimeError(f"destination reset failed: {result.stdout[-300:]}")


# ── convergence ──────────────────────────────────────────────────────

def summary(kind, root, remote):
    command = f"{BINARY} manifest {kind} {root}"
    result = peer(command, timeout=1800) if remote else run(command, timeout=1800)
    if result.returncode != 0:
        return f"<error: {result.stdout.strip()[-200:]}>"
    return result.stdout.strip().splitlines()[-1]


def clean(summary_text):
    """A summary is usable for verification only when its walk saw no
    errors: two walks failing identically must not certify two trees
    equal."""
    return "<error" not in summary_text and summary_text.endswith("errors=0")


def await_cold_sync(corpora, emitter, tool):
    """Cheap match first, then the full digest summary as the arbiter.
    Both timestamps are reported; the *verified* one is the headline."""
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
                if source_full == destination_full and clean(source_full):
                    timings[corpus] = {
                        "count_matched_s": round(count_matched, 1),
                        "digest_verified_s": round(verified, 1),
                        "verified": True,
                    }
                    remaining.discard(corpus)
    for corpus in remaining:
        timings[corpus] = {"verified": False, "timeout_s": COLD_SYNC_TIMEOUT_SECONDS}
    return timings


def await_quiescence(corpora):
    """Settled means the source and destination *content* agree and stay
    agreed across a gap — full summaries, not counts. Idle CPU sampled
    without this is 'shortly after convergence', which the previous run
    discovered is not the same thing."""
    for _ in range(30):
        pairs = {}
        stable = True
        for corpus in corpora:
            source = summary("full", f"{CORPUS}/{corpus}", remote=False)
            destination = summary("full", f"{DEST}/{corpus}", remote=True)
            if source != destination or not clean(source):
                stable = False
                break
            pairs[corpus] = source
        if not stable:
            time.sleep(POLL_SECONDS)
            continue
        time.sleep(10)
        for corpus in corpora:
            if (summary("full", f"{CORPUS}/{corpus}", remote=False) != pairs[corpus]
                    or summary("full", f"{DEST}/{corpus}", remote=True) != pairs[corpus]):
                stable = False
                break
        if stable:
            return True
    return False


def await_reconvergence(corpora, timeout_seconds=600):
    started = time.monotonic()
    results = {}
    for corpus in corpora:
        converged = False
        while time.monotonic() - started < timeout_seconds:
            source = summary("full", f"{CORPUS}/{corpus}", remote=False)
            destination = summary("full", f"{DEST}/{corpus}", remote=True)
            if source == destination and clean(source):
                converged = True
                break
            time.sleep(POLL_SECONDS)
        results[corpus] = converged
    return results


# ── samplers ─────────────────────────────────────────────────────────

def stop_samplers():
    # The bracket keeps the pattern from matching its own shell wrapper.
    run(f"pkill -f '{BINARY} [s]ampler' 2>/dev/null; true")
    peer(f"pkill -f '{BINARY} [s]ampler' 2>/dev/null; true")


def start_samplers(tool):
    patterns = TOOLS[tool]
    stop_samplers()
    run(f"setsid nohup {BINARY} sampler {shlex.quote(patterns['local_pattern'])} "
        f"{HOME}/rss-local.log >/dev/null 2>&1 < /dev/null &")
    peer(f"setsid nohup {BINARY} sampler {shlex.quote(patterns['remote_pattern'])} "
         f"{HOME}/rss-remote.log >/dev/null 2>&1 < /dev/null &")


def collect_series():
    local = run(f"cat {HOME}/rss-local.log 2>/dev/null").stdout
    remote = peer(f"cat {HOME}/rss-remote.log 2>/dev/null").stdout

    def parse(text):
        rows = []
        for line in text.strip().splitlines():
            parts = line.split()
            if len(parts) == 4:
                try:
                    rows.append([float(parts[0]), int(parts[1]), int(parts[2]), int(parts[3])])
                except ValueError:
                    continue
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
    elif tool == "mutagen":
        run(f"{HOME}/mutagen daemon start", check=True)
        for corpus in corpora:
            run(f"{HOME}/mutagen sync create --name={corpus} --sync-mode=two-way-safe "
                f"--ignore=/.git --ignore=/out --watch-polling-interval=5 "
                f"{CORPUS}/{corpus} dest:{DEST}/{corpus}", check=True)
    elif tool == "toysync":
        for corpus in corpora:
            run(f"setsid nohup python3 {BENCH}/toysync.py {CORPUS}/{corpus} "
                f"{DEST}/{corpus} > /dev/null 2>&1 < /dev/null &")
    else:
        raise RuntimeError(f"unknown tool {tool}")


# ── measurements ─────────────────────────────────────────────────────

def measure_floor(emitter, nonce):
    result = run_argv([
        BINARY, "floor",
        "--observer", f"{peer_ip()}:{OBSERVER_BASE_PORT}",
        "--dest-root", DEST,
        "--nonce", str(nonce),
    ], timeout=300)
    peer(f"rm -rf {DEST}/floor-probe")
    parsed = None
    for line in result.stdout.strip().splitlines():
        try:
            parsed = json.loads(line)
        except json.JSONDecodeError:
            continue
    emitter.emit(parsed or {"measurement": "floor", "error": result.stdout[-500:]})


def verify_partitions(corpora):
    """Runs on both hosts *before* the workload phase window opens, so
    this setup walk never dilutes the workload's resource attribution."""
    for corpus in corpora:
        partitions = f"{CORPUS}/{corpus}.bench/partitions.json"
        run_argv([BINARY, "verify-partitions", partitions], check=True)
        verify = peer(f"{BINARY} verify-partitions {partitions}")
        if verify.returncode != 0:
            raise RuntimeError(f"peer partition verification failed: {verify.stdout[-300:]}")


def run_workload(cell, emitter, tool, nonce):
    corpora = cell["corpora"]
    # Launch all workload processes (argv lists — no shell parses a label),
    # wait for them all, close the phase, and only then collect remote
    # outputs, so result collection never dilutes the workload window.
    processes = []
    for index, corpus in enumerate(corpora):
        argv = [
            BINARY, "agents",
            "--root", f"{CORPUS}/{corpus}",
            "--peer-root", f"{DEST}/{corpus}",
            "--observer", f"{peer_ip()}:{OBSERVER_BASE_PORT + index}",
            "--partitions", f"{CORPUS}/{corpus}.bench/partitions.json",
            "--side", "a",
            "--agents", str(cell["agents"]),
            "--seconds", str(WORKLOAD_SECONDS),
            "--label", f"{corpus} a-to-b",
            "--nonce", str(nonce * 1000 + index),
        ]
        processes.append((
            f"{corpus}:a-to-b", None, nonce * 1000 + index,
            subprocess.Popen(argv, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, text=True),
        ))
        if cell.get("bidirectional"):
            remote_result = f"{HOME}/bench-b-{index}.json"
            # A failed SSH launch must never let a previous run's file be
            # read as this run's result.
            peer(f"rm -f {remote_result} {remote_result}.err")
            remote_command = " ".join(shlex.quote(part) for part in [
                BINARY, "agents",
                "--root", f"{DEST}/{corpus}",
                "--peer-root", f"{CORPUS}/{corpus}",
                "--observer", f"{self_ip()}:{REVERSE_OBSERVER_BASE_PORT + index}",
                "--partitions", f"{CORPUS}/{corpus}.bench/partitions.json",
                "--side", "b",
                "--agents", str(cell["agents"]),
                "--seconds", str(WORKLOAD_SECONDS),
                "--label", f"{corpus} b-to-a",
                "--nonce", str(nonce * 1000 + 500 + index),
            ]) + f" > {remote_result} 2> {remote_result}.err"
            if LOCAL:
                process = subprocess.Popen(remote_command, shell=True,
                                           stdout=subprocess.DEVNULL,
                                           stderr=subprocess.DEVNULL)
            else:
                process = subprocess.Popen(
                    ["ssh", "-n", "dest", remote_command],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            processes.append((f"{corpus}:b-to-a", remote_result,
                              nonce * 1000 + 500 + index, process))

    outputs = []
    for direction, remote_result, expected_nonce, process in processes:
        if remote_result is None:
            outputs.append({"direction": direction, "local": True,
                            "nonce": expected_nonce,
                            "output": process.communicate()[0],
                            "returncode": process.returncode})
        else:
            process.wait()
            outputs.append({"direction": direction, "local": False,
                            "nonce": expected_nonce,
                            "remote_result": remote_result,
                            "returncode": process.returncode})

    return outputs


def collect_workload(outputs, emitter, tool):
    """Parses each direction's report. A report is accepted only when the
    launcher exited zero AND the parsed JSON carries this run's exact
    nonce — so a stale result file, or a launch that failed before its
    redirection ran, can never masquerade as a measurement."""
    clean = True
    reports = []
    for entry in outputs:
        if entry["local"]:
            output = entry["output"]
        else:
            output = peer(
                f"cat {entry['remote_result']} {entry['remote_result']}.err 2>/dev/null"
            ).stdout
        parsed = None
        for line in (output or "").strip().splitlines():
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
        record = {"measurement": "workload", "tool": tool,
                  "direction": entry["direction"]}
        if parsed and parsed.get("nonce") != entry["nonce"]:
            record["error"] = (f"nonce mismatch: expected {entry['nonce']}, "
                               f"report carries {parsed.get('nonce')} — stale result")
            clean = False
        elif parsed and entry["returncode"] != 0:
            record["error"] = f"launcher exited {entry['returncode']} despite parseable output"
            clean = False
        elif parsed:
            record.update(parsed)
            record["local"] = entry["local"]
            reports.append(record)
        else:
            record["error"] = (output or "").strip()[-1000:]
            clean = False
        emitter.emit(record)
    return clean, reports


# ── one tool, one cell ───────────────────────────────────────────────

def run_tool(tool, cell, emitter, nonce):
    corpora = cell["corpora"]
    phases = {}

    def phase(name):
        phases[name] = {"start": time.time()}

    def phase_end(name):
        phases[name]["end"] = time.time()

    destroy_tool_state(emitter)
    restore_sources(corpora)
    clear_destinations(corpora)
    start_samplers(tool)
    # Whatever happens below — a cold-sync abort, an exception on its
    # way to run_job's handler — the samplers never outlive this tool.
    try:
        offset = clock_offset()
        time.sleep(1)

        status = "ok"
        phase("cold_sync")
        start_tool(tool, corpora)
        timings = await_cold_sync(corpora, emitter, tool)
        phase_end("cold_sync")
        emitter.emit({"measurement": "cold_sync", "tool": tool, "timings": timings})
        if not all(t.get("verified") for t in timings.values()):
            emitter.emit({"measurement": "abort", "tool": tool,
                          "reason": "cold sync unverified"})
            destroy_tool_state(emitter)
            return "cold_sync_failed"

        settled = await_quiescence(corpora)
        phase("idle")
        time.sleep(IDLE_WINDOW_SECONDS)
        phase_end("idle")
        emitter.emit({"measurement": "idle_window", "tool": tool, "settled": settled})
        if not settled:
            status = "unsettled_idle"

        verify_partitions(corpora)
        phase("workload")
        outputs = run_workload(cell, emitter, tool, nonce)
        phase_end("workload")
        clean, reports = collect_workload(outputs, emitter, tool)
        if not clean:
            status = "workload_error"
        # Background editing stops when the offered-load window ends, so the
        # window the agents report — not launch-to-drain, whose tail is a
        # tool-dependent drain of up to the full deadline — is the honest
        # resource window. Only local reports are used: remote epochs live on
        # the other host's clock.
        local_windows = [r for r in reports
                         if r.get("local") and r.get("window_start_epoch")]
        if local_windows:
            phases["workload"] = {
                "start": min(r["window_start_epoch"] for r in local_windows),
                "end": max(r["window_end_epoch"] for r in local_windows),
            }

        phase("reconvergence")
        converged = await_reconvergence(corpora)
        phase_end("reconvergence")
        emitter.emit({"measurement": "reconvergence", "tool": tool, "converged": converged})
        if not all(converged.values()):
            status = "diverged"

        emitter.emit({
            "measurement": "resources", "tool": tool,
            "phases": phases, "clock_offset": offset, "series": collect_series(),
        })
        destroy_tool_state(emitter)
        return status
    finally:
        stop_samplers()


def digest_of(path):
    import hashlib
    try:
        with open(path, "rb") as handle:
            return hashlib.sha256(handle.read()).hexdigest()
    except OSError:
        return None


def stable_nonce(text):
    """A nonce derived stably from the run identity — Python's hash() is
    salted per process and would make recorded nonces unreproducible."""
    import hashlib
    return int.from_bytes(hashlib.sha256(text.encode()).digest()[:4], "big")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--spec", required=True)
    parser.add_argument("--output", required=True)
    arguments = parser.parse_args()
    spec = json.loads(arguments.spec)

    identity = {key: spec[key] for key in ("run", "pair", "job", "repeat")}
    identity["cell"] = spec["cell"]["name"]
    emitter = Emitter(arguments.output, identity)

    versions = {}
    for tool in spec["tools"]:
        if tool == "toysync":
            versions[tool] = "toysync"
        else:
            versions[tool] = run(f"{HOME}/{tool} --version 2>/dev/null || {HOME}/{tool} version") \
                .stdout.strip().splitlines()[-1]
    emitter.emit({
        "measurement": "job_start", "spec": spec,
        "tool_versions": versions,
        "binaries_sha256": {
            "benchmark": digest_of(BINARY),
            "autobahn": digest_of(f"{HOME}/autobahn"),
            "mutagen": digest_of(f"{HOME}/mutagen"),
        },
        "chromium_commit": (open(f"{CORPUS}/chromium.commit").read().strip()
                            if os.path.exists(f"{CORPUS}/chromium.commit") else None),
    })

    # The nonce makes every payload stream unique to (run, job): a payload
    # from any earlier run can never satisfy this run's verification. It
    # is derived stably so the recorded value reproduces the streams.
    base_nonce = stable_nonce(f"{spec['run']}/{spec['job']}")

    measure_floor(emitter, base_nonce)
    statuses = {}
    for tool_index, tool in enumerate(spec["tools"]):
        try:
            statuses[tool] = run_tool(tool, spec["cell"], emitter,
                                      base_nonce + tool_index + 1)
        except Exception as error:  # noqa: BLE001 — recorded, not raised past
            emitter.emit({"measurement": "tool_error", "tool": tool,
                          "error": repr(error)})
            statuses[tool] = "error"
            try:
                destroy_tool_state(emitter)
            except Exception:
                pass
    emitter.emit({"measurement": "job_complete", "statuses": statuses})


if __name__ == "__main__":
    main()
