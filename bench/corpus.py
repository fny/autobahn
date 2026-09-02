#!/usr/bin/env python3
"""Generates a synthetic corpus for local benchmarking.

The AWS benchmark measures against real trees (Chromium, and subsets of
it); this makes the small synthetic ones used for local A/B runs, so they
never need to be kept on disk between sessions. Two shapes:

  code   40 modules x 100 packages x 10 files, .rs, 256 B to 6 KiB of
         random bytes — 40,000 files, about 210 MB. The shape of a large
         Rust workspace, minus the compressibility.
  flat   8,000 directories x 26 small files, 18 to 83 bytes — 208,000
         files, about 20 MB. Many entries, almost no content: the shape
         that stresses scanning and snapshots rather than transfer.

Both take a --scale multiplier. Content is random rather than
compressible so that transfer measurements are honest about bytes.

    bench/corpus.py code /tmp/corpus
    bench/corpus.py flat /tmp/corpus --scale 2
"""

import argparse
import os
import random
import sys


def code(root, scale, rng):
    modules, packages, files = 40 * scale, 100, 10
    for m in range(modules):
        for p in range(packages):
            directory = os.path.join(root, f"module{m:02d}", f"pkg{p:02d}")
            os.makedirs(directory, exist_ok=True)
            for f in range(files):
                size = rng.randint(256, 6144)
                with open(os.path.join(directory, f"file{f}.rs"), "wb") as out:
                    out.write(rng.randbytes(size))
    return modules * packages * files


def flat(root, scale, rng):
    directories, files = 8000 * scale, 26
    for d in range(directories):
        directory = os.path.join(root, f"dir{d:05d}")
        os.makedirs(directory, exist_ok=True)
        for f in range(files):
            size = rng.randint(18, 83)
            with open(os.path.join(directory, f"file{f:03d}.txt"), "wb") as out:
                out.write(rng.randbytes(size))
    return directories * files


SHAPES = {"code": code, "flat": flat}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("shape", choices=SHAPES)
    parser.add_argument("root")
    parser.add_argument("--scale", type=int, default=1)
    parser.add_argument("--seed", type=int, default=7, help="for a reproducible corpus")
    args = parser.parse_args()
    if os.path.exists(args.root) and os.listdir(args.root):
        sys.exit(f"{args.root} exists and is not empty; refusing to write into it")
    count = SHAPES[args.shape](args.root, args.scale, random.Random(args.seed))
    print(f"{count} files in {args.root}")


if __name__ == "__main__":
    main()
