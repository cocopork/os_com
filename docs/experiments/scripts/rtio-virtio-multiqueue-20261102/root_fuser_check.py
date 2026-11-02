#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Checks open users from host root before exposing an NVMe partition to QEMU."""

import argparse
import os
from pathlib import Path
import stat
import subprocess
import sys


def check(target, parent, target_id, parent_id, stdout_file, stderr_file, *,
          uid_map_path=Path("/proc/self/uid_map"), euid_fn=os.geteuid,
          stat_fn=os.stat, realpath_fn=os.path.realpath, run_fn=subprocess.run):
    if euid_fn() != 0:
        raise ValueError("host-root effective UID required")
    try:
        inside, outside, span = map(int, Path(uid_map_path).read_text().splitlines()[0].split()[:3])
    except (IndexError, ValueError) as error:
        raise ValueError("invalid uid_map first tuple") from error
    if inside != 0 or outside != 0 or span <= 0:
        raise ValueError("uid_map must map inside UID 0 to host UID 0 with positive span")

    for path, expected in ((target, target_id), (parent, parent_id)):
        if realpath_fn(path) != path:
            raise ValueError(f"noncanonical block device: {path}")
        try:
            device = stat_fn(path)
        except OSError as error:
            raise ValueError(f"block device unavailable: {path}") from error
        if not stat.S_ISBLK(device.st_mode):
            raise ValueError(f"not a block device: {path}")
        actual = f"{os.major(device.st_rdev)}:{os.minor(device.st_rdev)}"
        if actual != expected:
            raise ValueError(f"block major:minor changed for {path}: {actual} != {expected}")

    result = run_fn(["/usr/bin/fuser", "-s", "--", target, parent], capture_output=True)
    Path(stdout_file).write_bytes(result.stdout)
    Path(stderr_file).write_bytes(result.stderr)
    if result.stderr:
        raise ValueError("fuser stderr is nonempty; process visibility is uncertain")
    print("ROOT_FUSER_IDENTITY_VALIDATED", flush=True)
    return result.returncode


def main():
    parser = argparse.ArgumentParser()
    for name in ("target", "parent", "target_id", "parent_id", "stdout_file", "stderr_file"):
        parser.add_argument(name)
    args = parser.parse_args()
    try:
        status = check(args.target, args.parent, args.target_id, args.parent_id,
                       args.stdout_file, args.stderr_file)
    except (OSError, ValueError) as error:
        print(f"root fuser check failed: {error}", file=sys.stderr)
        return 126
    return status


if __name__ == "__main__":
    sys.exit(main())
