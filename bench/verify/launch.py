#!/usr/bin/env python3
"""Launches one host pair from the benchmark image for fault-injection and
differential testing, then pushes the current binaries over the baked ones.

The image carries the corpus, which is the expensive part and never
changes. The tools change constantly, so they are pushed at launch rather
than baked — a rebake to swap a four-megabyte binary would re-clone a
half-million-file repository.

Usage: launch.py --profile P --region R [--ami AMI] [--pairs N]
"""

import argparse
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ORCH = os.path.join(os.path.dirname(HERE), "orchestrate.py")
sys.path.insert(0, os.path.dirname(HERE))
import orchestrate  # noqa: E402  — reuses its AWS helpers


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", required=True)
    parser.add_argument("--region", required=True)
    parser.add_argument("--ami", required=True)
    parser.add_argument("--pairs", type=int, default=1)
    parser.add_argument("--fan", type=int, default=0,
                        help="one source feeding N destinations, wired as "
                             "dest1..destN (instead of --pairs)")
    parser.add_argument("--type", default="c6i.2xlarge")
    parser.add_argument("--run", default=None)
    options = parser.parse_args()

    run_id = options.run or f"verify-{int(time.time())}"
    print(f"run id: {run_id}")
    key, group = orchestrate.provision_network(options, run_id)
    orchestrate.INSTANCE_TYPE = options.type
    count = 1 + options.fan if options.fan else options.pairs * 2
    instances = orchestrate.launch(options, run_id, options.type, group, key,
                                   count=count, ami=options.ami)
    addresses = orchestrate.wait_for_address(options, instances)
    for instance in instances:
        orchestrate.wait_for_ssh(addresses[instance], key)

    if options.fan:
        return _wire_fan(options, run_id, key, group, instances, addresses)

    pairs = [(instances[2 * i], instances[2 * i + 1]) for i in range(options.pairs)]
    hosts = []
    for index, (a, b) in enumerate(pairs):
        a_public, a_private = addresses[a]
        b_public, b_private = addresses[b]
        ssh_a = f"ssh -o StrictHostKeyChecking=accept-new -i {orchestrate.key_path(key)} ubuntu@{a_public}"
        ssh_b = f"ssh -o StrictHostKeyChecking=accept-new -i {orchestrate.key_path(key)} ubuntu@{b_public}"
        orchestrate.run(f"{ssh_a} 'ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519 -q || true'")
        public_key = orchestrate.run(f"{ssh_a} 'cat ~/.ssh/id_ed25519.pub'").stdout.strip()
        orchestrate.run(f"{ssh_b} 'echo {json.dumps(public_key)} >> ~/.ssh/authorized_keys'")
        orchestrate.run(
            f"{ssh_a} 'printf \"Host dest\\n  HostName {b_private}\\n  User ubuntu\\n"
            f"  StrictHostKeyChecking accept-new\\n\" >> ~/.ssh/config; ssh -o ConnectTimeout=5 dest true'")
        # The peer and self addresses, as dispatch writes them. Verification
        # scripts read these to place observers; without them the address
        # resolves to an empty string and every sample is censored.
        orchestrate.run(
            f"{ssh_a} 'mkdir -p ~/bench && echo {b_private} > ~/bench/peer-ip "
            f"&& echo {a_private} > ~/bench/self-ip'")
        orchestrate.run(
            f"{ssh_b} 'mkdir -p ~/bench && echo {a_private} > ~/bench/peer-ip "
            f"&& echo {b_private} > ~/bench/self-ip'")

        # Push the current tools over the baked ones, on both hosts.
        local = os.path.expanduser("~/Workspace/autobahn/target/x86_64-unknown-linux-musl/release/autobahn")
        for ssh, public in ((ssh_a, a_public), (ssh_b, b_public)):
            orchestrate.run(f"scp -o StrictHostKeyChecking=accept-new -i {orchestrate.key_path(key)} "
                            f"{local} ubuntu@{public}:~/autobahn")
            orchestrate.run(f"ssh -i {orchestrate.key_path(key)} ubuntu@{public} "
                            f"'chmod +x ~/autobahn && mkdir -p ~/agents && cp ~/autobahn ~/agents/autobahn-linux-x86_64 "
                            f"&& rm -rf ~/.autobahn ~/.autobahn-dev'")
        for name, script in sorted(_scripts().items()):
            for public in (a_public,):
                orchestrate.run(f"ssh -i {orchestrate.key_path(key)} ubuntu@{public} "
                                f"'cat > ~/{name} && chmod +x ~/{name}' <<'SCRIPT_EOF'\n{script}\nSCRIPT_EOF")
        hosts.append({"pair": index, "a": a_public, "b": b_public,
                      "a_private": a_private, "b_private": b_private})

    manifest = {"run": run_id, "key": orchestrate.key_path(key), "group": group,
                "ami": options.ami, "hosts": hosts}
    with open(f"{HERE}/hosts-{run_id}.json", "w") as handle:
        json.dump(manifest, handle, indent=2)
    print(json.dumps(manifest, indent=2))
    print(f"\ndestroy with: python3 {ORCH} destroy --profile {options.profile} "
          f"--region {options.region} --run {run_id} --keep-ami")


def _wire_fan(options, run_id, key, group, instances, addresses):
    """One source, N destinations reachable as dest1..destN.

    The pair wiring above gives each source exactly one destination named
    `dest`, which is all a differential or soak test needs. Measuring fan-out
    needs the destinations numbered and all reachable from the same source.
    """
    source, destinations = instances[0], instances[1:]
    source_public, source_private = addresses[source]
    identity = orchestrate.key_path(key)
    ssh_source = (f"ssh -o StrictHostKeyChecking=accept-new -i {identity} "
                  f"ubuntu@{source_public}")

    orchestrate.run(f"{ssh_source} 'ssh-keygen -t ed25519 -N \"\" "
                    f"-f ~/.ssh/id_ed25519 -q || true'")
    public_key = orchestrate.run(f"{ssh_source} 'cat ~/.ssh/id_ed25519.pub'").stdout.strip()

    config, hosts = [], []
    for index, destination in enumerate(destinations, start=1):
        public, private = addresses[destination]
        orchestrate.run(f"ssh -o StrictHostKeyChecking=accept-new -i {identity} "
                        f"ubuntu@{public} 'echo {json.dumps(public_key)} "
                        f">> ~/.ssh/authorized_keys'")
        config.append(f"Host dest{index}\\n  HostName {private}\\n  User ubuntu\\n"
                      f"  StrictHostKeyChecking accept-new\\n")
        hosts.append({"name": f"dest{index}", "public": public, "private": private})

    orchestrate.run(f"{ssh_source} 'printf \"{''.join(config)}\" >> ~/.ssh/config'")
    privates = "\n".join(host["private"] for host in hosts)
    orchestrate.run(
        f"{ssh_source} 'mkdir -p ~/bench && printf {json.dumps(privates + chr(10))} "
        f"> ~/bench/peer-ip && echo {source_private} > ~/bench/self-ip'")
    for index in range(1, len(destinations) + 1):
        orchestrate.run(f"{ssh_source} 'ssh -o ConnectTimeout=10 dest{index} true'")

    # The image carries baked binaries; push the current ones over them.
    local = os.path.expanduser(
        "~/Workspace/autobahn/target/x86_64-unknown-linux-musl/release/autobahn")
    for public in [source_public] + [host["public"] for host in hosts]:
        orchestrate.run(f"scp -o StrictHostKeyChecking=accept-new -i {identity} "
                        f"{local} ubuntu@{public}:~/autobahn")
        orchestrate.run(f"ssh -i {identity} ubuntu@{public} 'chmod +x ~/autobahn && "
                        f"mkdir -p ~/agents && cp ~/autobahn ~/agents/autobahn-linux-x86_64 "
                        f"&& rm -rf ~/.autobahn ~/.autobahn-dev'")

    for name, script in sorted(_scripts(("coldfan.sh",)).items()):
        orchestrate.run(f"ssh -i {identity} ubuntu@{source_public} "
                        f"'cat > ~/{name} && chmod +x ~/{name}' <<'SCRIPT_EOF'\n{script}\nSCRIPT_EOF")

    manifest = {"run": run_id, "key": identity, "group": group, "ami": options.ami,
                "source": source_public, "source_private": source_private,
                "destinations": hosts}
    with open(f"{HERE}/hosts-{run_id}.json", "w") as handle:
        json.dump(manifest, handle, indent=2)
    print(json.dumps(manifest, indent=2))
    print(f"\nssh -i {identity} ubuntu@{source_public}")
    print(f"destroy with: python3 {ORCH} destroy --profile {options.profile} "
          f"--region {options.region} --run {run_id} --keep-ami")


def _scripts(names=("soak.sh", "differential.sh")):
    """Verification scripts copied to each driver host."""
    scripts = {}
    for name in names:
        path = os.path.join(HERE, name)
        if os.path.exists(path):
            scripts[name] = open(path).read()
    return scripts


if __name__ == "__main__":
    main()
