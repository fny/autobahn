#!/usr/bin/env python3
"""AWS lifecycle for the benchmark: bake, launch, dispatch, collect, kill.

Runs on the operator's machine. Stages:

  bake      one builder instance clones Chromium, builds the subsets,
            computes manifests and partitions (once — every pair inherits
            byte-identical corpora and working sets), installs both tools
            and the harness binary, then becomes the golden AMI.
  launch    N pairs from the AMI, one security group, intra-group traffic
            open, per-pair SSH wiring (A can reach B as `dest`).
  dispatch  jobs = cells × repeats, shuffled with a recorded seed; each
            job's tool order is randomized with the same seed stream. Jobs
            are assigned round-robin to pairs and each pair executes its
            list serially. Both tools of a job always run on the same pair.
  collect   pull every pair's results.jsonl; nothing is dropped — a job
            that failed left an error record, and a pair that died leaves
            its assignment file behind as evidence.
  destroy   terminate instances, delete the security group and key pair.

Everything is tagged with the run id; `destroy` kills by tag, so an
interrupted orchestration is always cleanly collectable.

Usage:
  orchestrate.py bake --profile P --region R
  orchestrate.py run  --profile P --region R --ami AMI --budget 1000 --repeats 10
  orchestrate.py destroy --profile P --region R --run RUN_ID
  orchestrate.py aggregate results/
"""

import argparse
import base64
import concurrent.futures
import json
import os
import random
import shlex
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# The subject under test, as a static binary for the hosts. Overridable so
# that a before/after can stage two builds without editing this file.
AUTOBAHN_BINARY = os.environ.get(
    "BENCH_AUTOBAHN",
    os.path.join(HERE, os.pardir, "target/x86_64-unknown-linux-musl/release/autobahn"))
# How often a running pair's results are copied back while it works.
COLLECT_INTERVAL = 120
INSTANCE_TYPE = "c6i.4xlarge"
BUILDER_TYPE = "c6i.2xlarge"
VOLUME_GB = 200

CELLS = [
    # name, corpora, agents, bidirectional, betas, seed, mode
    #
    # `betas` is how many destinations one source feeds. Everything is a
    # pair at 1. At 10 it is a fan-out: one alpha, ten betas, which asks a
    # different question — autobahn fans a config group into one session
    # per beta, so ten betas means ten sessions scanning the same source.
    #
    # `seed` fills the destinations from the copy already in the image
    # rather than timing a first synchronization.
    #
    # Cold sync and steady-state latency are separate questions and are
    # measured separately. A latency cell wants the tools already caught
    # up, so it seeds; the first-sync cells below measure the transfer on
    # its own, where its cost buys a result instead of a delay. Mixing the
    # two made every latency job pay for a transfer it was not reporting —
    # more than half of a Chromium job, multiplied by the destination
    # count in a fan-out.
    ("chromium-1", ["chromium"], 1, False, 1, True),
    ("chromium-10", ["chromium"], 10, False, 1, True),
    ("chromium-100", ["chromium"], 100, False, 1, True),
    ("chromium-1-bidir", ["chromium"], 1, True, 1, True),
    ("chromium-10-bidir", ["chromium"], 10, True, 1, True),
    ("chromium-100-bidir", ["chromium"], 100, True, 1, True),
    # A burst: one module of the corpus copied in beside the others, the
    # time until it has landed, five times per job. What an unpacked
    # archive or a checkout feels like — and where the manifest-polling
    # measurement is too coarse (~100 ms), so the tool's own timing is
    # recorded beside it. No editors.
    ("50k-burst", ["sub50k"], 0, False, 1, True, "burst"),
    ("chromium-burst", ["chromium"], 0, False, 1, True, "burst"),
    ("50k-1", ["sub50k"], 1, False, 1, True),
    ("50k-10", ["sub50k"], 10, False, 1, True),
    ("50k-100", ["sub50k"], 100, False, 1, True),
    ("two50k-1", ["sub50k", "sub50k-b"], 1, False, 1, True),
    ("two50k-10", ["sub50k", "sub50k-b"], 10, False, 1, True),
    ("two50k-100", ["sub50k", "sub50k-b"], 100, False, 1, True),
    ("5k-1", ["sub5k"], 1, False, 1, True),
    ("5k-10", ["sub5k"], 10, False, 1, True),
    ("5k-100", ["sub5k"], 100, False, 1, True),
    # Fan-out: one alpha, ten betas, at a fixed agent count so that width
    # is the only variable.
    ("5k-10-fan", ["sub5k"], 10, False, 10, True),
    ("50k-10-fan", ["sub50k"], 10, False, 10, True),
    ("two50k-10-fan", ["sub50k", "sub50k-b"], 10, False, 10, True),
    ("chromium-10-fan", ["chromium"], 10, False, 10, True),

    # Patch mode: the measured agent rewrites a region of a large file
    # rather than replacing a small one whole. Every other cell replaces
    # its file with fresh random content, which shares no blocks with what
    # the destination holds, so the delta algorithm never has anything to
    # find and a tool that always sent full content would score the same.
    # These are the cells where transferring less is possible.
    ("50k-1-patch", ["sub50k"], 1, False, 1, True, "patch"),
    ("50k-10-patch", ["sub50k"], 10, False, 1, True, "patch"),
    ("chromium-1-patch", ["chromium"], 1, False, 1, True, "patch"),
    ("chromium-10-patch", ["chromium"], 10, False, 1, True, "patch"),
    ("chromium-10-fan-patch", ["chromium"], 10, False, 10, True, "patch"),

    # First synchronization, measured on its own. These do not seed — the
    # transfer *is* the measurement. Agents are 0: nothing edits, so the
    # number is the transfer and nothing else. Each corpus is measured
    # to one destination and to ten, which is the question a fan-out
    # actually raises for a first sync: does a source pay per destination,
    # or does it share the work?
    ("coldsync-5k", ["sub5k"], 0, False, 1, False),
    ("coldsync-50k", ["sub50k"], 0, False, 1, False),
    ("coldsync-chromium", ["chromium"], 0, False, 1, False),
    ("coldsync-5k-fan", ["sub5k"], 0, False, 10, False),
    ("coldsync-50k-fan", ["sub50k"], 0, False, 10, False),
    ("coldsync-chromium-fan", ["chromium"], 0, False, 10, False),
]


# How long an instance may live without the orchestrator finishing. Long
# enough for the slowest legitimate run, short enough that forgetting
# costs one hour rather than a weekend.
#
# Seven hours, not five: a full matrix at five repeats is 150 jobs of
# around half an hour each, and the timer firing mid-run destroys the
# whole run rather than saving anything — the machines are terminated
# with their results still on them. Override for a short run if the
# forgetting risk matters more than the completing one.
DEAD_MAN_MINUTES = int(os.environ.get("BENCH_DEAD_MAN_MINUTES", "420"))


def machines_for(cell):
    """One source, plus its destinations."""
    return 1 + cell[4]

BAKE_SCRIPT = r"""#!/bin/bash
set -euo pipefail
sudo apt-get update -qq && sudo apt-get install -y -qq git python3 rsync > /dev/null
mkdir -p ~/bench ~/corpus ~/dest
cd ~/corpus
git clone --depth 1 --single-branch https://github.com/chromium/chromium.git chromium
git -C chromium rev-parse HEAD > chromium.commit
# Symbolic links are stripped from the corpus: the tools' symlink policies
# differ enough to make convergence ambiguous, and links are noise for a
# latency benchmark. The walk excludes them for the same reason.
find chromium -type l -delete
python3 - <<'EOF'
import os, shutil
# Subsets built from whole top-level directories of chromium, disjoint
# between the two 50k subsets, targeted counts approximate by design.
# The sizes are decades of the whole tree: 5k, 50k, ~505k.
root = "chromium"
dirs = []
for d in sorted(os.listdir(root)):
    p = os.path.join(root, d)
    if d in (".git", "out") or not os.path.isdir(p):
        continue
    n = sum(len(f) for _, _, f in os.walk(p))
    if n:
        dirs.append((d, p, n))
dirs.sort(key=lambda x: -x[2])
def build(name, target, skip):
    os.makedirs(name)
    total, used = 0, []
    for d, p, n in dirs:
        if d in skip or n > target * 1.6 or total >= target:
            continue
        shutil.copytree(p, os.path.join(name, d), symlinks=True)
        total += n; used.append(d)
    return used
a = build("sub50k", 50000, set())
b = build("sub50k-b", 50000, set(a))
build("sub5k", 5000, set())
EOF
for c in chromium sub50k sub50k-b sub5k; do
  ~/bench/benchmark partitions ~/corpus/$c ~/corpus/$c.bench-partitions.json
  mkdir -p ~/corpus/$c.bench
  mv ~/corpus/$c.bench-partitions.json ~/corpus/$c.bench/partitions.json
done
# Pristine copies of exactly the files a workload may edit (the union of
# every partition), so each tool-run starts from identical source content.
python3 - <<'EOF'
import json, os, subprocess
home = os.path.expanduser("~")
for c in ("chromium", "sub50k", "sub50k-b", "sub5k"):
    with open(f"{home}/corpus/{c}.bench/partitions.json") as handle:
        partitions = json.load(handle)
    files = set()
    for by_count in partitions["sides"].values():
        for sets in by_count.values():
            files.update(sets["measured"])
            # Patch-mode cells edit these, so they must be restorable too.
            # This list and the one job.py restores from have to agree; a
            # file in the restore list and not in the pristine copy fails
            # every job before it measures anything.
            files.update(sets.get("large", []))
            for background in sets["background"]:
                files.update(background)
    listing = f"{home}/pristine-list.txt"
    with open(listing, "w") as handle:
        handle.write("\n".join(sorted(files)) + "\n")
    os.makedirs(f"{home}/corpus-pristine/{c}", exist_ok=True)
    subprocess.run(["rsync", "-a", f"--files-from={listing}",
                    f"{home}/corpus/{c}/", f"{home}/corpus-pristine/{c}/"],
                   check=True)
os.remove(listing)
EOF
"""


# Instance size per corpus, from the peak CPU each actually reached in the
# ten-repeat matrix: 4k 0.7 cores, 50k 1.7, two-50k 2.3, chromium 3.3 —
# against the 16 vCPUs every host used to get. The size leaves room for the
# tool, the load generator's threads and the observer's polling on the same
# host, and nothing more. Since the quota is counted in vCPUs, right-sizing
# is what buys parallelism.
CORPUS_SIZE = {
    "sub5k": "c6i.xlarge",       # 4 vCPU
    "sub50k": "c6i.2xlarge",     # 8 vCPU
    "sub50k-b": "c6i.2xlarge",
    "chromium": "c6i.4xlarge",   # 16 vCPU — the only corpus that needs them
}
VCPUS = {"c6i.xlarge": 4, "c6i.2xlarge": 8, "c6i.4xlarge": 16}
# Rough job cost, for longest-first packing. Only the ratios matter.
CORPUS_COST = {"sub5k": 1, "sub50k": 2, "sub50k-b": 2, "chromium": 10}


def cell_instance(cell):
    """The smallest instance every corpus in this cell can run on."""
    return max((CORPUS_SIZE[corpus] for corpus in cell[1]), key=lambda t: VCPUS[t])


def cell_cost(cell):
    """A job's rough duration, for scheduling.

    A latency cell seeds, so it pays only the fixed workload, idle and
    reconvergence time plus a verification pass. A cold-sync cell pays the
    transfer instead, which is the whole point of it, multiplied by the
    number of destinations the source must serve.
    """
    corpus = sum(CORPUS_COST[c] for c in cell[1])
    directions = 2 if cell[3] else 1
    if len(cell) > 5 and cell[5]:
        return (1.0 + 0.2 * corpus) * directions
    # Not seeded: a first-synchronization measurement. The source pushes
    # the whole tree to every destination, but the transfers overlap and
    # read the same files, so the page cache serves most of the repeats.
    # Scaling as the square root of the destination count is a guess in
    # between "free" and "ten times" — and how wrong it is happens to be
    # what these cells measure.
    return corpus * (cell[4] ** 0.5) * directions


def schedule(groups, jobs):
    """Places jobs on groups longest-first and returns the makespan.

    A group can take any job that fits: wide enough for its destinations,
    and on an instance at least as large as the job needs. Size is a floor,
    not a match — a chromium group can run a 5k job, which is what lets the
    scheduler fill idle capacity instead of stranding it.

    Longest-first is the standard heuristic and matters here because one
    chromium job outweighs ten small ones: start the big ones last and they
    become the tail nothing can hide.
    """
    loads = [0.0] * len(groups)
    placement = [[] for _ in groups]
    for job in sorted(jobs, key=lambda job: -job["cost"]):
        fits = [
            index for index, (width, instance) in enumerate(groups)
            if width >= job["machines"] and VCPUS[instance] >= VCPUS[job["instance"]]
        ]
        if not fits:
            return None, None
        chosen = min(fits, key=lambda index: (loads[index], VCPUS[groups[index][1]]))
        loads[chosen] += job["cost"]
        placement[chosen].append(job)
    return max(loads), placement


def plan_groups(selected, repeats, budget_vcpus):
    """Chooses which machine groups to build, to finish soonest.

    Jobs of different shapes cannot share groups — a fan-out job needs
    eleven machines, a chromium job needs large ones — so the budget has to
    be split between shapes. For a shape holding `cost` units of work on
    `n` groups, that shape finishes at `cost / n`. Everything finishes when
    the slowest shape does, so the split that finishes soonest is the one
    where every shape finishes together.

    Setting `cost_s / n_s = T` for every shape and spending the whole
    budget gives `T = sum(cost_s × vcpu_s) / budget` and `n_s = cost_s / T`
    directly, with no search. Integer rounding is then spent where it helps
    most.
    """
    shapes = {}
    for cell in selected:
        shape = (1 + cell[4], cell_instance(cell))
        shapes[shape] = shapes.get(shape, 0.0) + cell_cost(cell) * repeats

    vcpus = {shape: shape[0] * VCPUS[shape[1]] for shape in shapes}
    base = sum(vcpus.values())
    if base > budget_vcpus:
        raise RuntimeError(
            f"one group of each shape needs {base} vCPUs, beyond the budget of "
            f"{budget_vcpus}. Narrow the selection with --cells.")

    # The continuous optimum, then floored to whole groups.
    weighted = sum(shapes[shape] * vcpus[shape] for shape in shapes) or 1.0
    horizon = weighted / budget_vcpus
    counts = {shape: max(1, int(shapes[shape] / horizon)) for shape in shapes}

    # More groups than jobs of a shape cannot help.
    limit = {}
    for cell in selected:
        shape = (1 + cell[4], cell_instance(cell))
        limit[shape] = limit.get(shape, 0) + repeats
    for shape in counts:
        counts[shape] = min(counts[shape], limit[shape])

    spent = sum(counts[shape] * vcpus[shape] for shape in counts)
    while spent > budget_vcpus:
        # Over budget after rounding: take from whichever shape loses least.
        shape = max((s for s in counts if counts[s] > 1),
                    key=lambda s: shapes[s] / counts[s] - shapes[s] / (counts[s] - 1))
        counts[shape] -= 1
        spent -= vcpus[shape]

    # Spend what rounding left over on whichever shape is slowest.
    while True:
        affordable = [s for s in counts
                      if spent + vcpus[s] <= budget_vcpus and counts[s] < limit[s]]
        if not affordable:
            break
        shape = max(affordable, key=lambda s: shapes[s] / counts[s])
        counts[shape] += 1
        spent += vcpus[shape]

    groups = [shape for shape, count in counts.items() for _ in range(count)]
    groups.sort(key=lambda shape: (-VCPUS[shape[1]], -shape[0]))
    return groups


def run(command, check=True, capture=True):
    return subprocess.run(
        command, shell=True, check=check,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT, text=True,
    )


def aws(profile, region, arguments):
    result = run(f"aws --profile {profile} --region {region} {arguments}")
    return result.stdout.strip()


# ── bake ─────────────────────────────────────────────────────────────

def bake(options):
    """Launches a builder, provisions it, and snapshots the AMI. The
    harness binary and both tools are copied from the local machine so the
    image runs exactly the bits that were reviewed."""
    run_id = f"bench-{int(time.time())}"
    print(f"run id: {run_id}")
    key, group = provision_network(options, run_id)
    instance = launch(options, run_id, BUILDER_TYPE, group, key, count=1)[0]
    # wait_for_address yields (public, private); the builder is reached on
    # its public address.
    address, _ = wait_for_address(options, [instance])[instance]
    wait_for_ssh(address, key)

    ssh = f"ssh -o StrictHostKeyChecking=accept-new -i {key_path(key)} ubuntu@{address}"
    run(f"{ssh} 'mkdir -p ~/bench'")
    for source, target in [
        (f"{HERE}/harness/target/x86_64-unknown-linux-musl/release/benchmark", "~/bench/benchmark"),
        (f"{HERE}/job.py", "~/bench/job.py"),
        (AUTOBAHN_BINARY, "~/autobahn"),
        (os.path.expanduser("~/Workspace/mutagen-bench/bin-stock/mutagen"), "~/mutagen"),
        # Mutagen requires its agent bundle beside the executable; without
        # it every SSH session creation fails outright.
        (os.path.expanduser("~/Workspace/mutagen-bench/bin-stock/mutagen-agents.tar.gz"),
         "~/mutagen-agents.tar.gz"),
        (f"{HERE}/toysync.py", "~/bench/toysync.py"),
    ]:
        run(f"scp -o StrictHostKeyChecking=accept-new -i {key_path(key)} {source} ubuntu@{address}:{target}")
    run(f"{ssh} 'chmod +x ~/bench/benchmark ~/autobahn ~/mutagen; "
        f"mkdir -p ~/agents && cp ~/autobahn ~/agents/autobahn-linux-x86_64'")

    script = base64.b64encode(BAKE_SCRIPT.encode()).decode()
    print("baking (chromium clone + subsets + partitions; ~15 minutes)...")
    run(f"{ssh} 'echo {script} | base64 -d > ~/bake.sh && chmod +x ~/bake.sh && ~/bake.sh'",
        capture=False)

    # The harness and the subject binary leave the image before it is
    # snapshotted. They change far more often than the corpus does, and
    # baking them means re-cloning a half-million-file repository every
    # time one of them moves — which also changes the corpus underneath a
    # comparison that was supposed to be about the binary. They are pushed
    # to every member at run time instead, beside job.py.
    #
    # The partitions written above stay baked, and the harness that wrote
    # them came from the same build tree the run will push from, so the
    # two still agree. Only mutagen stays in the image: it is a stock
    # release that does not move between runs.
    run(f"{ssh} 'rm -f ~/bench/benchmark ~/autobahn ~/agents/autobahn-linux-x86_64'")

    print("creating image...")
    ami = aws(options.profile, options.region,
              f"ec2 create-image --instance-id {instance} --name {run_id}-golden "
              f"--query ImageId --output text")
    aws(options.profile, options.region, f"ec2 wait image-available --image-ids {ami}")
    aws(options.profile, options.region, f"ec2 terminate-instances --instance-ids {instance}")
    print(f"AMI ready: {ami}")
    print(f"dispatch with: run --run {run_id} --ami {ami}   "
          f"(the run's key and security group are found by name)")
    print(f"destroy with:  destroy --run {run_id}")
    print(json.dumps({"run": run_id, "ami": ami, "key": key, "group": group}))


# ── launch and dispatch ──────────────────────────────────────────────

# Rebuilt on every machine before any job runs, rather than trusted from
# the image. The pristine copy and the list job.py restores from are two
# separate renderings of the same set, and when they disagree every job
# fails at restore before measuring anything — which is exactly what a
# patch-mode cell's large files did, because they were added to one list
# and not the other. Rebuilding here makes the image's vintage irrelevant
# and costs a few seconds of rsync against files that are already there.
PRISTINE_REPAIR = r"""
python3 - <<'REPAIR_EOF'
import json, os, shutil
home = os.path.expanduser("~")
for c in ("chromium", "sub50k", "sub50k-b", "sub5k"):
    path = f"{home}/corpus/{c}.bench/partitions.json"
    if not os.path.exists(path):
        continue
    with open(path) as handle:
        partitions = json.load(handle)
    files = set()
    for by_count in partitions["sides"].values():
        for sets in by_count.values():
            files.update(sets["measured"])
            files.update(sets.get("large", []))
            for background in sets["background"]:
                files.update(background)
    # Check first, copy only what is absent. rsync would compare every one
    # of a corpus's several hundred thousand files on every machine, which
    # cost this run nearly two hours of setup; a stat apiece costs seconds
    # and the copy list is normally empty or a few hundred long.
    root = f"{home}/corpus-pristine/{c}"
    missing = [f for f in files if not os.path.exists(f"{root}/{f}")]
    for relative in missing:
        source = f"{home}/corpus/{c}/{relative}"
        target = f"{root}/{relative}"
        os.makedirs(os.path.dirname(target), exist_ok=True)
        shutil.copy2(source, target)
    still = [f for f in files if not os.path.exists(f"{root}/{f}")]
    if still:
        raise SystemExit(f"{c}: {len(still)} restorable files absent from the "
                         f"pristine copy, e.g. {still[0]}")
    if missing:
        print(f"{c}: restored {len(missing)} missing file(s) to the pristine copy")
print("pristine verified")
REPAIR_EOF
"""


def provision_network(options, run_id):
    key = f"{run_id}-key"
    material = aws(options.profile, options.region,
                   f"ec2 create-key-pair --key-name {key} --query KeyMaterial --output text")
    with open(key_path(key), "w") as handle:
        handle.write(material + "\n")
    os.chmod(key_path(key), 0o600)
    vpc = aws(options.profile, options.region,
              "ec2 describe-vpcs --filters Name=is-default,Values=true "
              "--query 'Vpcs[0].VpcId' --output text")
    group = aws(options.profile, options.region,
                f"ec2 create-security-group --group-name {run_id}-sg "
                f"--description 'temporary benchmark {run_id}' --vpc-id {vpc} "
                f"--query GroupId --output text")
    my_ip = run("curl -s --max-time 10 https://checkip.amazonaws.com").stdout.strip()
    aws(options.profile, options.region,
        f"ec2 authorize-security-group-ingress --group-id {group} "
        f"--protocol tcp --port 22 --cidr {my_ip}/32")
    aws(options.profile, options.region,
        f"ec2 authorize-security-group-ingress --group-id {group} "
        f"--protocol -1 --source-group {group}")
    return key, group


def key_path(key):
    return os.path.expanduser(f"~/.ssh/{key}.pem")


def default_subnets(options):
    """The default subnet in each availability zone, as {zone: subnet}.

    One zone rarely has capacity for a hundred identical instances, so a
    run spreads across zones. Groups are still placed *within* one zone —
    a pair split across zones would measure cross-zone latency, which is
    not what any cell is asking about.
    """
    output = aws(options.profile, options.region,
                 "ec2 describe-subnets --filters Name=default-for-az,Values=true "
                 "--query 'Subnets[].[AvailabilityZone,SubnetId]' --output json")
    return {zone: subnet for zone, subnet in json.loads(output)}


def launch(options, run_id, instance_type, group, key, count, ami=None, subnet=None):
    image = ami or aws(options.profile, options.region,
                       "ssm get-parameter --name /aws/service/canonical/ubuntu/server/24.04/"
                       "stable/current/amd64/hvm/ebs-gp3/ami-id "
                       "--query Parameter.Value --output text")
    placement = f"--subnet-id {subnet} " if subnet else ""
    identifiers = aws(
        options.profile, options.region,
        f"ec2 run-instances --image-id {image} --instance-type {instance_type} "
        f"--count {count} --key-name {key} --security-group-ids {group} {placement}"
        f"--instance-initiated-shutdown-behavior terminate "
        f"--block-device-mappings '[{{\"DeviceName\":\"/dev/sda1\",\"Ebs\":"
        f"{{\"VolumeSize\":{VOLUME_GB},\"VolumeType\":\"gp3\",\"Iops\":6000,"
        f"\"Throughput\":500,\"DeleteOnTermination\":true}}}}]' "
        f"--tag-specifications 'ResourceType=instance,"
        f"Tags=[{{Key=Name,Value={run_id}}},{{Key=bench-run,Value={run_id}}}]' "
        f"--query 'Instances[].InstanceId' --output text",
    ).split()
    aws(options.profile, options.region,
        f"ec2 wait instance-running --instance-ids {' '.join(identifiers)}")
    return identifiers


def wait_for_address(options, instances):
    output = aws(options.profile, options.region,
                 f"ec2 describe-instances --instance-ids {' '.join(instances)} "
                 "--query 'Reservations[].Instances[].[InstanceId,PublicIpAddress,PrivateIpAddress]' "
                 "--output json")
    return {row[0]: (row[1], row[2]) for row in json.loads(output)}


def _ssh_ready(address, key, attempts=48):
    """Whether an instance answers SSH, without raising. Used to decide
    which instances to replace rather than which run to abandon."""
    public = address[0] if isinstance(address, tuple) else address
    for _ in range(attempts):
        if run(f"ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 "
               f"-i {key_path(key)} ubuntu@{public} true", check=False).returncode == 0:
            return True
        time.sleep(5)
    return False


def wait_for_ssh(address, key):
    public = address[0] if isinstance(address, tuple) else address
    for _ in range(60):
        if run(f"ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 "
               f"-i {key_path(key)} ubuntu@{public} true", check=False).returncode == 0:
            return
        time.sleep(5)
    raise RuntimeError(f"ssh to {public} never came up")


def dispatch(options):
    """Runs the matrix and tears the machines down, whatever happens.

    The orchestrator used to print the destroy command and return, which
    is fine until a run fails in its first ten minutes and nobody notices
    for three and a half hours. Instances are cheap per minute and
    expensive per afternoon, so teardown is not left to the operator.

    `--keep` opts out, for when a run is being debugged and the machines
    are the evidence.
    """
    try:
        return _dispatch_body(options)
    finally:
        if getattr(options, "keep", False):
            print("--keep: machines left running; destroy them yourself with "
                  f"`orchestrate.py destroy --run {getattr(options, 'run', '<run>')}`")
        else:
            print("tearing down...")
            try:
                destroy(argparse.Namespace(
                    profile=options.profile, region=options.region,
                    run=options.run, keep_ami=True))
            except Exception as error:                      # noqa: BLE001
                # Never let a teardown failure mask the run's own error,
                # but say so loudly: this is the expensive kind of quiet.
                print(f"TEARDOWN FAILED ({error}); destroy by hand: "
                      f"orchestrate.py destroy --profile {options.profile} "
                      f"--region {options.region} --run {options.run}")


def _dispatch_body(options):
    """Launches pairs, wires them, assigns jobs, runs them, collects."""
    run_id = options.run or f"bench-{int(time.time())}"
    # Resolved back onto the options so the teardown wrapper can find the
    # machines even when the id was generated here.
    options.run = run_id
    seed = options.seed
    rng = random.Random(seed)

    if options.group:
        key, group = f"{run_id}-key", options.group
    else:
        # The bake's network is named deterministically; an existing one
        # for this run id is reused rather than re-created (which would
        # fail on the duplicate key pair).
        existing = aws(options.profile, options.region,
                       f"ec2 describe-security-groups --filters "
                       f"Name=group-name,Values={run_id}-sg "
                       "--query 'SecurityGroups[0].GroupId' --output text")
        if existing and existing != "None" and os.path.exists(key_path(f"{run_id}-key")):
            key, group = f"{run_id}-key", existing
        else:
            key, group = provision_network(
                argparse.Namespace(profile=options.profile, region=options.region), run_id)

    # Which cells are in play decides how many machines of which size are
    # needed, so the selection is resolved before anything is launched.
    selected = CELLS
    if getattr(options, "cells", None):
        wanted = set(options.cells.split(","))
        selected = [c for c in CELLS if c[0] in wanted]
        missing = wanted - {c[0] for c in selected}
        if missing:
            raise RuntimeError(f"unknown cells: {sorted(missing)}")
        print(f"cell filter: {[c[0] for c in selected]}")

    planned = plan_groups(selected, options.repeats, options.budget)
    from collections import Counter
    print(f"{len(planned)} group(s), {sum(w for w, _ in planned)} machines, "
          f"{sum(w * VCPUS[t] for w, t in planned)} of {options.budget} vCPU")
    for (width, instance), count in sorted(Counter(planned).items()):
        print(f"    {count} × {width} machines of {instance}")

    # Groups are spread across availability zones, because one zone will
    # not supply a hundred identical instances — but each group is launched
    # *within* one zone, since a pair split across zones would measure
    # cross-zone latency rather than the tool.
    zones = default_subnets(options)
    if not zones:
        raise RuntimeError("no default subnets found; cannot place instances")
    ordered_zones = sorted(zones)
    placements = {}   # (zone, instance_type) -> machines needed
    group_zone = []
    for index, (width, instance_type) in enumerate(planned):
        zone = ordered_zones[index % len(ordered_zones)]
        group_zone.append(zone)
        placements[(zone, instance_type)] = placements.get((zone, instance_type), 0) + width

    pools, instances = {}, []
    for (zone, instance_type), needed in sorted(placements.items()):
        remaining, attempts = needed, []
        # A zone that cannot supply its share hands the rest to another, so
        # a single tight zone does not fail the run. Groups already placed
        # in that zone keep their placement; only the overflow moves.
        for candidate in [zone] + [z for z in ordered_zones if z != zone]:
            if remaining == 0:
                break
            try:
                launched = launch(options, run_id, instance_type, group, key,
                                  count=remaining, ami=options.ami,
                                  subnet=zones[candidate])
            except subprocess.CalledProcessError:
                attempts.append(candidate)
                continue
            pools.setdefault((zone, instance_type), []).extend(launched)
            instances.extend(launched)
            remaining = 0
        if remaining:
            raise RuntimeError(
                f"no zone could supply {needed} × {instance_type} "
                f"(tried {', '.join(attempts)})")
    addresses = wait_for_address(options, instances)
    # Wait in parallel, and replace what never answers.
    #
    # Waiting serially is slow, but the real problem is that one bad
    # instance used to abort the whole run. At this scale that is not an
    # unlucky edge case: even a half-percent chance of an instance failing
    # to boot gives better than even odds of at least one across a hundred
    # and seventy-nine, so the common case was a dead run.
    for attempt in range(3):
        failed = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=32) as waiters:
            futures = {
                waiters.submit(_ssh_ready, addresses[instance], key): instance
                for instance in instances
            }
            for future in concurrent.futures.as_completed(futures):
                if not future.result():
                    failed.append(futures[future])
        if not failed:
            break
        print(f"{len(failed)} instance(s) never came up; replacing them "
              f"(attempt {attempt + 1} of 3)")
        aws(options.profile, options.region,
            f"ec2 terminate-instances --instance-ids {' '.join(failed)}")
        for dead in failed:
            pool_key = next(k for k, members in pools.items() if dead in members)  # noqa: B023
            zone, instance_type = pool_key
            replacement = launch(options, run_id, instance_type, group, key,
                                 count=1, ami=options.ami, subnet=zones[zone])[0]
            pools[pool_key][pools[pool_key].index(dead)] = replacement
            instances[instances.index(dead)] = replacement
        addresses = wait_for_address(options, instances)
    else:
        raise RuntimeError("instances kept failing to come up after three rounds")

    # Groups are positional and variable width: one source followed by its
    # destinations. A pairwise group is width 2; a fan-out group is width
    # 11. Wiring is the same either way — the source learns every
    # destination as `dest1`..`destN`, with `dest` aliased to the first so
    # nothing that assumes a single destination has to change.
    groups, taken = [], {key_: 0 for key_ in pools}
    for index, (width, instance_type) in enumerate(planned):
        pool_key = (group_zone[index], instance_type)
        start = taken[pool_key]
        groups.append(pools[pool_key][start:start + width])
        taken[pool_key] = start + width
    group_types = [instance_type for _, instance_type in planned]

    for index, members in enumerate(groups):
        source = members[0]
        followers = members[1:]
        a_public, a_private = addresses[source]
        ssh_a = f"ssh -i {key_path(key)} ubuntu@{a_public}"
        run(f"{ssh_a} 'ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519 -q || true'")
        public_key = run(f"{ssh_a} 'cat ~/.ssh/id_ed25519.pub'").stdout.strip()

        aliases, privates, config = [], [], []
        for position, follower in enumerate(followers, start=1):
            b_public, b_private = addresses[follower]
            run(f"ssh -i {key_path(key)} ubuntu@{b_public} "
                f"'echo {json.dumps(public_key)} >> ~/.ssh/authorized_keys'")
            alias = f"dest{position}"
            aliases.append(alias)
            privates.append(b_private)
            config.append(f"Host {alias}\n  HostName {b_private}\n  User ubuntu\n"
                          f"  StrictHostKeyChecking accept-new\n")
            if position == 1:
                # `dest` is the first destination, so every single-
                # destination path keeps working untouched.
                config.append(f"Host dest\n  HostName {b_private}\n  User ubuntu\n"
                              f"  StrictHostKeyChecking accept-new\n")
        # One quoting boundary, not several: the remote command is built as
        # plain text and quoted once. Embedding pre-quoted fragments inside
        # an outer quote closes it at the first fragment, and the shell then
        # reads the rest of the SSH config as commands.
        wiring = (
            f"printf %s {shlex.quote(''.join(config))} >> ~/.ssh/config; "
            f"printf %s {shlex.quote(chr(10).join(privates) + chr(10))} > ~/bench/peer-ip; "
            f"printf %s {shlex.quote(a_private + chr(10))} > ~/bench/self-ip; "
            f"printf %s {shlex.quote(','.join(aliases))} > ~/bench/destinations"
        )
        result = run(f"ssh -n -i {key_path(key)} ubuntu@{a_public} {shlex.quote(wiring)}",
                     check=False)
        if result.returncode != 0:
            raise RuntimeError(f"group {index}: wiring failed: {result.stdout[-400:]}")
        for alias in aliases:
            reachable = run(f"{ssh_a} 'ssh -o ConnectTimeout=5 {alias} true && echo OK'")
            if "OK" not in reachable.stdout:
                raise RuntimeError(f"group {index}: source cannot reach {alias}")

        # Observers on every destination (for a-to-b) and on the source
        # (for b-to-a), started and then *proven* listening before any job
        # is dispatched.
        # Every machine in the group, before anything measures.
        #
        # The driver is pushed rather than taken from the image. It is
        # harness logic and changes far more often than the corpus it runs
        # against, and a rebake to correct a Python file would re-clone a
        # half-million-file repository. The harness binary stays baked,
        # because the partitions it wrote are baked with it and the two
        # have to agree.
        # The driver, the harness and the subject binary, all pushed
        # rather than baked (see bake()). Every member needs the harness:
        # destinations run the observer from it. The source needs
        # autobahn; the destinations get their agent streamed over SSH by
        # the controller, but the copy under ~/agents is what a controller
        # uses when it has to install one, so every member carries it and
        # no member's role has to be guessed.
        #
        # Pushed to a group's members concurrently. Serially, seven
        # megabytes to each of a hundred and fifty machines is an hour of
        # the whole fleet sitting idle at full price before the first
        # measurement — which is most of what the run costs to begin with.
        def stage(host):
            for source, target in [
                (f"{HERE}/job.py", "~/bench/job.py"),
                (f"{HERE}/harness/target/x86_64-unknown-linux-musl/release/benchmark",
                 "~/bench/benchmark"),
                (AUTOBAHN_BINARY, "~/autobahn"),
            ]:
                run(f"scp -o StrictHostKeyChecking=accept-new -i {key_path(key)} "
                    f"{source} ubuntu@{host}:{target}")
            run(f"ssh -o StrictHostKeyChecking=accept-new -i {key_path(key)} ubuntu@{host} "
                f"'chmod +x ~/bench/benchmark ~/autobahn && mkdir -p ~/agents && "
                f"cp ~/autobahn ~/agents/autobahn-linux-x86_64'")

        hosts = [addresses[member][0] for member in members]
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(hosts)) as pool:
            for outcome in concurrent.futures.as_completed(
                    [pool.submit(stage, host) for host in hosts]):
                outcome.result()

        repairs = []
        for member in members:
            host = addresses[member][0]
            # Terminate regardless of what happens to the orchestrator.
            # Auto-destroy below covers the tidy paths; this covers the
            # untidy ones — a killed orchestrator, a lost laptop, a crash —
            # which is the case that actually ran up a bill. Instances are
            # launched to terminate on shutdown, so this ends them rather
            # than stopping them.
            subprocess.run(
                ["ssh", "-n", "-o", "StrictHostKeyChecking=accept-new",
                 "-i", key_path(key), f"ubuntu@{host}",
                 f"sudo shutdown -h +{DEAD_MAN_MINUTES} 'benchmark dead-man timer'"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            repairs.append((host, subprocess.Popen(
                ["ssh", "-n", "-o", "StrictHostKeyChecking=accept-new",
                 "-i", key_path(key), f"ubuntu@{host}", PRISTINE_REPAIR],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)))
        for host, process in repairs:
            output = process.communicate()[0]
            if process.returncode != 0 or "pristine verified" not in output:
                raise RuntimeError(
                    f"group {index}: pristine rebuild failed on {host}: "
                    f"{output[-400:]}")

        listeners = [(addresses[f][0], "9911 9912") for f in followers]
        listeners.append((a_public, "10011 10012"))
        for host, ports in listeners:
            run(f"ssh -i {key_path(key)} ubuntu@{host} "
                f"'for p in {ports}; do setsid nohup ~/bench/benchmark observer $p "
                f"--listen 0.0.0.0 --root ~/dest > ~/observer-$p.log 2>&1 < /dev/null & done'")
        for host, ports in listeners:
            ready = run(
                f"ssh -i {key_path(key)} ubuntu@{host} "
                f"'for i in $(seq 1 20); do "
                f"ok=1; for p in {ports}; do ss -ltn | grep -q :$p || ok=0; done; "
                f"[ $ok = 1 ] && echo READY && exit; sleep 1; done; echo NOT-READY'")
            if "READY" not in ready.stdout or "NOT-READY" in ready.stdout:
                raise RuntimeError(f"observers on {host} never came up: {ready.stdout}")

    # Jobs: cells × repeats, shuffled; tool order randomized per job.
    jobs = []
    for repeat in range(options.repeats):
        for cell in selected:
            name, corpora, agents, bidirectional, betas, seed = cell[:6]
            mode = cell[6] if len(cell) > 6 else "replace"
            tools = ["autobahn", "mutagen"]
            rng.shuffle(tools)
            jobs.append({
                "run": run_id, "repeat": repeat, "job": f"{name}-r{repeat}",
                "cell": {"name": name, "corpora": corpora, "agents": agents,
                         "bidirectional": bidirectional, "betas": betas,
                         "mode": mode,
                         "pre_seeded": seed},
                "tools": tools,
            })
    rng.shuffle(jobs)

    # A job can only run on a group with enough destinations, so assignment
    # is a fit rather than a rotation. A job needs a group wide enough for
    # its destinations AND on an instance at least as large as its corpus
    # was sized for — a Chromium job on a machine sized for 5k would not
    # measure the tool. Longest job first, because one Chromium job
    # outweighs ten small ones and starting it last makes it the tail
    # nothing can hide.
    #
    # This must agree with schedule(), which is what the planner used to
    # decide how many groups of each shape to build; if the two disagree,
    # the plan's makespan is a fiction.
    assignments = {index: [] for index in range(len(groups))}
    capacity = [len(members) - 1 for members in groups]
    load = [0.0] * len(groups)
    by_name = {cell[0]: cell for cell in selected}
    for job in sorted(jobs, key=lambda job: -cell_cost(by_name[job["cell"]["name"]])):
        cell = by_name[job["cell"]["name"]]
        needed, wanted = cell[4], VCPUS[cell_instance(cell)]
        candidates = [index for index in range(len(groups))
                      if capacity[index] >= needed
                      and VCPUS[group_types[index]] >= wanted]
        if not candidates:
            raise RuntimeError(
                f"cell {cell[0]} needs {needed} destination(s) on "
                f"{cell_instance(cell)} or larger, and no group provides that")
        chosen = min(candidates,
                     key=lambda index: (load[index], capacity[index],
                                        VCPUS[group_types[index]], index))
        load[chosen] += cell_cost(cell)
        job["pair"] = f"pair-{chosen}"
        assignments[chosen].append(job)

    # The complete plan is persisted before anything runs — locally and on
    # every pair — so a pair that dies leaves evidence of what it owed, and
    # the aggregator can compare delivered results against this manifest
    # instead of trusting whatever happened to come back.
    os.makedirs(f"results-{run_id}", exist_ok=True)
    a0_public, _ = addresses[groups[0][0]]
    chromium_commit = run(
        f"ssh -i {key_path(key)} ubuntu@{a0_public} 'cat ~/corpus/chromium.commit'",
        check=False).stdout.strip() or None
    plan = {"run": run_id, "seed": seed, "ami": options.ami,
            "chromium_commit": chromium_commit,
            "groups": [[width, instance] for width, instance in planned],
            "repeats": options.repeats,
            "cells": [c[0] for c in selected], "jobs": jobs}
    with open(f"results-{run_id}/plan.json", "w") as handle:
        json.dump(plan, handle, indent=2)

    print(f"{len(jobs)} jobs over {len(groups)} group(s) (seed {seed})")
    processes = []
    for pair_index, members in enumerate(groups):
        a_public, _ = addresses[members[0]]
        with open(f"results-{run_id}/assignment-pair-{pair_index}.json", "w") as handle:
            json.dump(assignments[pair_index], handle)
        run(f"scp -i {key_path(key)} results-{run_id}/assignment-pair-{pair_index}.json "
            f"ubuntu@{a_public}:~/assignment.json")
        script_lines = [
            "set -u",
            "rm -f ~/results.jsonl ~/driver.log",
            # The aliases the orchestrator wired, so job.py addresses every
            # destination rather than assuming one.
            'export BENCH_DESTINATIONS="$(cat ~/bench/destinations)"',
        ]
        for job in assignments[pair_index]:
            spec = json.dumps(json.dumps(job))  # shell-quoted JSON
            script_lines.append(
                f"python3 ~/bench/job.py --spec {spec} --output ~/results.jsonl"
                f" >> ~/driver.log 2>&1"
                f" || echo {json.dumps(job['job'])} >> ~/results.err"
            )
        script = "; ".join(script_lines)
        processes.append((pair_index, a_public, subprocess.Popen(
            ["ssh", "-n", "-i", key_path(key), f"ubuntu@{a_public}", script],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)))

    def collect_pair(pair_index, a_public, logs=False):
        # A connect timeout, because this runs on a loop for hours: one
        # host wedged in a half-open connection would otherwise stall the
        # collection of every other pair behind it.
        for artifact in ("results.jsonl", "results.err", "driver.log"):
            run(f"scp -o ConnectTimeout=15 -i {key_path(key)} ubuntu@{a_public}:~/{artifact} "
                f"results-{run_id}/pair-{pair_index}-{artifact}", check=False)
        if logs:
            # Per-job tool logs come back too, at the end. An intermittent
            # failure that leaves no log is far more expensive to chase
            # than the transfer of a few megabytes of text.
            run(f"scp -r -i {key_path(key)} ubuntu@{a_public}:~/logs "
                f"results-{run_id}/pair-{pair_index}-logs", check=False)

    def samples_so_far():
        total = 0
        for name in os.listdir(f"results-{run_id}"):
            if name.endswith("-results.jsonl"):
                with open(f"results-{run_id}/{name}") as handle:
                    total += sum(1 for _ in handle)
        return total

    # Results are copied back while the run proceeds, not only when it
    # ends. `results.jsonl` is append-only on the host, so a copy taken
    # mid-job is a prefix of the final one and the next copy supersedes
    # it. A run that dies — the orchestrator killed, a host lost, the
    # dead-man timer firing at five hours — then still leaves every
    # completed job here instead of nothing at all. A full matrix is
    # several hours and several hundred dollars; losing it to the last
    # ten minutes is not a risk worth carrying for one scp per job.
    pending = list(processes)
    started = time.time()
    while pending:
        time.sleep(COLLECT_INTERVAL)
        for entry in list(pending):
            pair_index, a_public, process = entry
            finished = process.poll() is not None
            collect_pair(pair_index, a_public, logs=finished)
            if finished:
                pending.remove(entry)
                print(f"  pair {pair_index} finished "
                      f"({len(processes) - len(pending)}/{len(processes)} pairs done)")
        print(f"  [{int(time.time() - started) // 60}m] "
              f"{samples_so_far()} samples, {len(pending)} pair(s) running",
              flush=True)
    print(f"collected into results-{run_id}/; aggregate with: "
          f"orchestrate.py aggregate results-{run_id}/; destroy with: "
          f"orchestrate.py destroy --profile {options.profile} "
          f"--region {options.region} --run {run_id}")


def destroy(options):
    instances = aws(options.profile, options.region,
                    f"ec2 describe-instances --filters Name=tag:bench-run,Values={options.run} "
                    "Name=instance-state-name,Values=pending,running,stopping,stopped "
                    "--query 'Reservations[].Instances[].InstanceId' --output text").split()
    if instances:
        aws(options.profile, options.region,
            f"ec2 terminate-instances --instance-ids {' '.join(instances)}")
        aws(options.profile, options.region,
            f"ec2 wait instance-terminated --instance-ids {' '.join(instances)}")
    for group in aws(options.profile, options.region,
                     f"ec2 describe-security-groups --filters Name=group-name,Values={options.run}-sg "
                     "--query 'SecurityGroups[].GroupId' --output text").split():
        aws(options.profile, options.region, f"ec2 delete-security-group --group-id {group}")
    run(f"aws --profile {options.profile} --region {options.region} "
        f"ec2 delete-key-pair --key-name {options.run}-key", check=False)
    if os.path.exists(key_path(f"{options.run}-key")):
        os.remove(key_path(f"{options.run}-key"))
    # The golden AMI and its snapshots are billed storage; a destroyed run
    # leaves nothing behind unless --keep-ami was given.
    if not getattr(options, "keep_ami", False):
        for image in aws(options.profile, options.region,
                         f"ec2 describe-images --owners self "
                         f"--filters Name=name,Values={options.run}-golden "
                         "--query 'Images[].ImageId' --output text").split():
            snapshots = aws(options.profile, options.region,
                            f"ec2 describe-images --image-ids {image} "
                            "--query 'Images[].BlockDeviceMappings[].Ebs.SnapshotId' "
                            "--output text").split()
            aws(options.profile, options.region, f"ec2 deregister-image --image-id {image}")
            for snapshot in snapshots:
                aws(options.profile, options.region,
                    f"ec2 delete-snapshot --snapshot-id {snapshot}")
    print("destroyed")


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="stage", required=True)
    for stage in ("bake", "run", "destroy"):
        s = sub.add_parser(stage)
        s.add_argument("--profile", required=True)
        s.add_argument("--region", required=True)
        if stage == "run":
            s.add_argument("--ami", required=True)
            s.add_argument("--budget", type=int, default=1000,
                           help="vCPUs to spend; the planner picks how many "
                                "machines of which size, to finish soonest")
            s.add_argument("--repeats", type=int, default=3)
            s.add_argument("--seed", type=int, default=1)
            s.add_argument("--run", default=None)
            s.add_argument("--keep", action="store_true",
                           help="leave the machines running when the run ends")
            s.add_argument("--group", default=None)
            s.add_argument("--cells", default=None,
                           help="comma-separated cell names; default is the whole matrix")
        if stage == "destroy":
            s.add_argument("--run", required=True)
            s.add_argument("--keep-ami", action="store_true")
    aggregate_parser = sub.add_parser("aggregate")
    aggregate_parser.add_argument("directory")
    options = parser.parse_args()
    if options.stage == "bake":
        bake(options)
    elif options.stage == "run":
        dispatch(options)
    elif options.stage == "destroy":
        destroy(options)
    else:
        subprocess.run([sys.executable, f"{HERE}/aggregate.py", options.directory], check=True)


if __name__ == "__main__":
    main()
