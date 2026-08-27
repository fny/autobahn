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
  orchestrate.py run  --profile P --region R --ami AMI --pairs 15 --repeats 3
  orchestrate.py destroy --profile P --region R --run RUN_ID
  orchestrate.py aggregate results/
"""

import argparse
import base64
import json
import os
import random
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
INSTANCE_TYPE = "c6i.4xlarge"
BUILDER_TYPE = "c6i.2xlarge"
VOLUME_GB = 200

CELLS = [
    # name, corpora, agents, bidirectional
    ("chromium-1", ["chromium"], 1, False),
    ("chromium-10", ["chromium"], 10, False),
    ("chromium-100", ["chromium"], 100, False),
    ("chromium-1-bidir", ["chromium"], 1, True),
    ("chromium-10-bidir", ["chromium"], 10, True),
    ("chromium-100-bidir", ["chromium"], 100, True),
    ("40k-1", ["sub40k-a"], 1, False),
    ("40k-10", ["sub40k-a"], 10, False),
    ("40k-100", ["sub40k-a"], 100, False),
    ("two40k-1", ["sub40k-a", "sub40k-b"], 1, False),
    ("two40k-10", ["sub40k-a", "sub40k-b"], 10, False),
    ("two40k-100", ["sub40k-a", "sub40k-b"], 100, False),
    ("4k-1", ["sub4k"], 1, False),
    ("4k-10", ["sub4k"], 10, False),
    ("4k-100", ["sub4k"], 100, False),
]

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
# between the two 40k subsets, targeted counts approximate by design.
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
a = build("sub40k-a", 40000, set())
b = build("sub40k-b", 40000, set(a))
build("sub4k", 4000, set())
EOF
for c in chromium sub40k-a sub40k-b sub4k; do
  ~/bench/benchmark partitions ~/corpus/$c ~/corpus/$c.bench-partitions.json
  mkdir -p ~/corpus/$c.bench
  mv ~/corpus/$c.bench-partitions.json ~/corpus/$c.bench/partitions.json
done
# Pristine copies of exactly the files a workload may edit (the union of
# every partition), so each tool-run starts from identical source content.
python3 - <<'EOF'
import json, os, subprocess
home = os.path.expanduser("~")
for c in ("chromium", "sub40k-a", "sub40k-b", "sub4k"):
    with open(f"{home}/corpus/{c}.bench/partitions.json") as handle:
        partitions = json.load(handle)
    files = set()
    for by_count in partitions["sides"].values():
        for sets in by_count.values():
            files.update(sets["measured"])
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
        (os.path.expanduser("~/Workspace/autobahn/target/x86_64-unknown-linux-musl/release/autobahn"), "~/autobahn"),
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


def launch(options, run_id, instance_type, group, key, count, ami=None):
    image = ami or aws(options.profile, options.region,
                       "ssm get-parameter --name /aws/service/canonical/ubuntu/server/24.04/"
                       "stable/current/amd64/hvm/ebs-gp3/ami-id "
                       "--query Parameter.Value --output text")
    identifiers = aws(
        options.profile, options.region,
        f"ec2 run-instances --image-id {image} --instance-type {instance_type} "
        f"--count {count} --key-name {key} --security-group-ids {group} "
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


def wait_for_ssh(address, key):
    public = address[0] if isinstance(address, tuple) else address
    for _ in range(60):
        if run(f"ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 "
               f"-i {key_path(key)} ubuntu@{public} true", check=False).returncode == 0:
            return
        time.sleep(5)
    raise RuntimeError(f"ssh to {public} never came up")


def dispatch(options):
    """Launches pairs, wires them, assigns jobs, runs them, collects."""
    run_id = options.run or f"bench-{int(time.time())}"
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

    instances = launch(options, run_id, INSTANCE_TYPE, group, key,
                       count=options.pairs * 2, ami=options.ami)
    addresses = wait_for_address(options, instances)
    for instance in instances:
        wait_for_ssh(addresses[instance], key)

    # Pairing is positional; each A learns its B's private IP.
    pairs = [(instances[2 * i], instances[2 * i + 1]) for i in range(options.pairs)]
    for index, (a, b) in enumerate(pairs):
        a_public, _ = addresses[a]
        b_public, b_private = addresses[b]
        _, a_private = addresses[a]
        ssh_a = f"ssh -i {key_path(key)} ubuntu@{a_public}"
        run(f"{ssh_a} 'ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519 -q || true; "
            f"cat ~/.ssh/id_ed25519.pub'")
        public_key = run(f"{ssh_a} 'cat ~/.ssh/id_ed25519.pub'").stdout.strip()
        run(f"ssh -i {key_path(key)} ubuntu@{b_public} "
            f"'echo {json.dumps(public_key)} >> ~/.ssh/authorized_keys'")
        run(f"{ssh_a} 'printf \"Host dest\\n  HostName {b_private}\\n  User ubuntu\\n"
            f"  StrictHostKeyChecking accept-new\\n\" >> ~/.ssh/config; "
            f"echo {b_private} > ~/bench/peer-ip; echo {a_private} > ~/bench/self-ip; "
            f"ssh -o ConnectTimeout=5 dest true'")
        # Observers on both hosts (B for a-to-b, A for b-to-a), started
        # and then *proven* listening before any job is dispatched.
        run(f"ssh -i {key_path(key)} ubuntu@{b_public} "
            f"'for p in 9911 9912; do setsid nohup ~/bench/benchmark observer $p "
            f"> ~/observer-$p.log 2>&1 < /dev/null & done'")
        run(f"{ssh_a} 'for p in 10011 10012; do setsid nohup ~/bench/benchmark observer $p "
            f"> ~/observer-$p.log 2>&1 < /dev/null & done'")
        for host, ports in ((b_public, "9911 9912"), (a_public, "10011 10012")):
            ready = run(
                f"ssh -i {key_path(key)} ubuntu@{host} "
                f"'for i in $(seq 1 20); do "
                f"ok=1; for p in {ports}; do ss -ltn | grep -q :$p || ok=0; done; "
                f"[ $ok = 1 ] && echo READY && exit; sleep 1; done; echo NOT-READY'")
            if "READY" not in ready.stdout or "NOT-READY" in ready.stdout:
                raise RuntimeError(f"observers on {host} never came up: {ready.stdout}")

    # Jobs: cells × repeats, shuffled; tool order randomized per job.
    selected = CELLS
    if getattr(options, "cells", None):
        wanted = set(options.cells.split(","))
        selected = [c for c in CELLS if c[0] in wanted]
        missing = wanted - {c[0] for c in selected}
        if missing:
            raise RuntimeError(f"unknown cells: {sorted(missing)}")
        print(f"cell filter: {[c[0] for c in selected]}")
    jobs = []
    for repeat in range(options.repeats):
        for name, corpora, agents, bidirectional in selected:
            tools = ["autobahn", "mutagen"]
            rng.shuffle(tools)
            jobs.append({
                "run": run_id, "repeat": repeat, "job": f"{name}-r{repeat}",
                "cell": {"name": name, "corpora": corpora, "agents": agents,
                         "bidirectional": bidirectional},
                "tools": tools,
            })
    rng.shuffle(jobs)

    assignments = {index: [] for index in range(len(pairs))}
    for index, job in enumerate(jobs):
        pair_index = index % len(pairs)
        job["pair"] = f"pair-{pair_index}"
        assignments[pair_index].append(job)

    # The complete plan is persisted before anything runs — locally and on
    # every pair — so a pair that dies leaves evidence of what it owed, and
    # the aggregator can compare delivered results against this manifest
    # instead of trusting whatever happened to come back.
    os.makedirs(f"results-{run_id}", exist_ok=True)
    a0_public, _ = addresses[pairs[0][0]]
    chromium_commit = run(
        f"ssh -i {key_path(key)} ubuntu@{a0_public} 'cat ~/corpus/chromium.commit'",
        check=False).stdout.strip() or None
    plan = {"run": run_id, "seed": seed, "ami": options.ami,
            "chromium_commit": chromium_commit,
            "pairs": options.pairs, "repeats": options.repeats,
            "cells": [c[0] for c in selected], "jobs": jobs}
    with open(f"results-{run_id}/plan.json", "w") as handle:
        json.dump(plan, handle, indent=2)

    print(f"{len(jobs)} jobs over {len(pairs)} pairs (seed {seed})")
    processes = []
    for pair_index, (a, _) in enumerate(pairs):
        a_public, _ = addresses[a]
        with open(f"results-{run_id}/assignment-pair-{pair_index}.json", "w") as handle:
            json.dump(assignments[pair_index], handle)
        run(f"scp -i {key_path(key)} results-{run_id}/assignment-pair-{pair_index}.json "
            f"ubuntu@{a_public}:~/assignment.json")
        script_lines = ["set -u", "rm -f ~/results.jsonl ~/driver.log"]
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

    for pair_index, a_public, process in processes:
        process.wait()
        for artifact in ("results.jsonl", "results.err", "driver.log"):
            run(f"scp -i {key_path(key)} ubuntu@{a_public}:~/{artifact} "
                f"results-{run_id}/pair-{pair_index}-{artifact}", check=False)
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
            s.add_argument("--pairs", type=int, default=15)
            s.add_argument("--repeats", type=int, default=3)
            s.add_argument("--seed", type=int, default=1)
            s.add_argument("--run", default=None)
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
