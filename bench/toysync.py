#!/usr/bin/env python3
"""A deliberately dumb synchronization tool, for testing the harness.

Copies source to destination in a loop, propagating changed files by
mtime+size comparison every 200ms. It exists so the smoke test can validate
the *measurement* plumbing — observer, agents, floor, manifests, sampler,
censoring — against a subject whose behavior is trivial and known, with no
EC2 and neither real tool involved. Its expected p50 under the harness is
roughly half its loop interval plus the harness floor; the smoke test
asserts that band.

Usage: toysync.py <source> <destination>
"""

import os
import shutil
import sys
import time

INTERVAL_SECONDS = 0.2
MARKERS = (".bench-tmp", ".floor-tmp")


def synchronize(source, destination):
    for directory, subdirectories, names in os.walk(source):
        relative = os.path.relpath(directory, source)
        target_dir = os.path.join(destination, relative) if relative != "." else destination
        os.makedirs(target_dir, exist_ok=True)
        for name in names:
            if any(marker in name for marker in MARKERS):
                continue
            source_path = os.path.join(directory, name)
            target_path = os.path.join(target_dir, name)
            try:
                s = os.stat(source_path)
            except OSError:
                continue
            try:
                t = os.stat(target_path)
                if t.st_size == s.st_size and t.st_mtime_ns == s.st_mtime_ns:
                    continue
            except OSError:
                pass
            try:
                shutil.copy2(source_path, target_path)
            except OSError:
                pass


def main():
    source, destination = sys.argv[1], sys.argv[2]
    while True:
        synchronize(source, destination)
        time.sleep(INTERVAL_SECONDS)


if __name__ == "__main__":
    main()
