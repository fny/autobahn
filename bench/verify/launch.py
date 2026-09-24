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
import shlex
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
    # Every command goes through orchestrate.run as an argument list; the
    # remote side's shell strings quote every value they carry.
    run, ssh, scp = orchestrate.run, orchestrate.ssh, orchestrate.scp
    q = shlex.quote
    for index, (a, b) in enumerate(pairs):
        a_public, a_private = addresses[a]
        b_public, b_private = addresses[b]
        orchestrate.authorize_source_key(key, a_public, [b_public])
        config = (f"Host dest\n  HostName {b_private}\n  User ubuntu\n"
                  f"  StrictHostKeyChecking accept-new\n")
        run(ssh(key, a_public, f"printf %s {q(config)} >> ~/.ssh/config; "
                               f"ssh -o ConnectTimeout=5 dest true"))
        # The peer and self addresses, as dispatch writes them. Verification
        # scripts read these to place observers; without them the address
        # resolves to an empty string and every sample is censored.
        run(ssh(key, a_public, f"mkdir -p ~/bench && echo {q(b_private)} > ~/bench/peer-ip "
                               f"&& echo {q(a_private)} > ~/bench/self-ip"))
        run(ssh(key, b_public, f"mkdir -p ~/bench && echo {q(a_private)} > ~/bench/peer-ip "
                               f"&& echo {q(b_private)} > ~/bench/self-ip"))

        # Push the current tools over the baked ones, on both hosts.
        local = os.path.expanduser("~/Workspace/autobahn/target/x86_64-unknown-linux-musl/release/autobahn")
        for public in (a_public, b_public):
            run(scp(key, local, f"ubuntu@{public}:~/autobahn"))
            run(ssh(key, public, "chmod +x ~/autobahn && mkdir -p ~/agents && "
                                 "cp ~/autobahn ~/agents/autobahn-linux-x86_64 "
                                 "&& rm -rf ~/.autobahn ~/.autobahn-dev"))
        for name, script in sorted(_scripts().items()):
            _install_script(key, a_public, name, script)
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
    run, ssh, scp = orchestrate.run, orchestrate.ssh, orchestrate.scp
    q = shlex.quote

    orchestrate.authorize_source_key(
        key, source_public, [addresses[d][0] for d in destinations])

    config, hosts = [], []
    for index, destination in enumerate(destinations, start=1):
        public, private = addresses[destination]
        config.append(f"Host dest{index}\n  HostName {private}\n  User ubuntu\n"
                      f"  StrictHostKeyChecking accept-new\n")
        hosts.append({"name": f"dest{index}", "public": public, "private": private})

    run(ssh(key, source_public, f"printf %s {q(''.join(config))} >> ~/.ssh/config"))
    privates = "\n".join(host["private"] for host in hosts)
    run(ssh(key, source_public,
            f"mkdir -p ~/bench && printf %s {q(privates + chr(10))} "
            f"> ~/bench/peer-ip && echo {q(source_private)} > ~/bench/self-ip"))
    for index in range(1, len(destinations) + 1):
        run(ssh(key, source_public, f"ssh -o ConnectTimeout=10 dest{index} true"))

    # The image carries baked binaries; push the current ones over them.
    local = os.path.expanduser(
        "~/Workspace/autobahn/target/x86_64-unknown-linux-musl/release/autobahn")
    for public in [source_public] + [host["public"] for host in hosts]:
        run(scp(key, local, f"ubuntu@{public}:~/autobahn"))
        run(ssh(key, public, "chmod +x ~/autobahn && "
                             "mkdir -p ~/agents && cp ~/autobahn ~/agents/autobahn-linux-x86_64 "
                             "&& rm -rf ~/.autobahn ~/.autobahn-dev"))

    for name, script in sorted(_scripts(("coldfan.sh",)).items()):
        _install_script(key, source_public, name, script)

    manifest = {"run": run_id, "key": identity, "group": group, "ami": options.ami,
                "source": source_public, "source_private": source_private,
                "destinations": hosts}
    with open(f"{HERE}/hosts-{run_id}.json", "w") as handle:
        json.dump(manifest, handle, indent=2)
    print(json.dumps(manifest, indent=2))
    print(f"\nssh -i {identity} ubuntu@{source_public}")
    print(f"destroy with: python3 {ORCH} destroy --profile {options.profile} "
          f"--region {options.region} --run {run_id} --keep-ami")


def _install_script(key, host, name, script):
    """Copies a verification script to `host` as ~/name, over stdin."""
    target = f"~/{shlex.quote(name)}"
    orchestrate.run(orchestrate.ssh(key, host, f"cat > {target} && chmod +x {target}"),
                    input=script + "\n")


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
