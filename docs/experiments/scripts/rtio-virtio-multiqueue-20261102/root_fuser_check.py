#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Checks open users from host root before exposing an NVMe partition to QEMU."""

import argparse
import json
import os
from pathlib import Path
import stat
import subprocess
import sys


def check(target, parent, target_id, parent_id, *,
          uid_map_path=Path("/proc/self/uid_map"), euid_fn=os.geteuid,
          stat_fn=os.stat, realpath_fn=os.path.realpath, run_fn=subprocess.run):
    record = {
        "schema_version": 1, "outcome": "validation_error", "error": None,
        "identity": {"effective_uid": euid_fn(), "uid_map": None, "validated": False},
        "target": {"path": target, "expected_major_minor": target_id},
        "parent": {"path": parent, "expected_major_minor": parent_id},
        "fuser": None,
    }
    try:
        if record["identity"]["effective_uid"] != 0:
            raise ValueError("host-root effective UID required")
        try:
            inside, outside, span = map(int, Path(uid_map_path).read_text().splitlines()[0].split())
        except (IndexError, ValueError) as error:
            raise ValueError("invalid uid_map first tuple") from error
        record["identity"]["uid_map"] = [inside, outside, span]
        if inside != 0 or outside != 0 or span <= 0:
            raise ValueError("uid_map must map inside UID 0 to host UID 0 with positive span")

        for name in ("target", "parent"):
            node = record[name]
            path = node["path"]
            node["canonical"] = realpath_fn(path) == path
            if not node["canonical"]:
                raise ValueError(f"noncanonical block device: {path}")
            try:
                device = stat_fn(path)
            except OSError as error:
                raise ValueError(f"block device unavailable: {path}") from error
            node["is_block_device"] = stat.S_ISBLK(device.st_mode)
            if not node["is_block_device"]:
                raise ValueError(f"not a block device: {path}")
            node["major_minor"] = f"{os.major(device.st_rdev)}:{os.minor(device.st_rdev)}"
            if node["major_minor"] != node["expected_major_minor"]:
                raise ValueError(f"block major:minor changed for {path}: {node['major_minor']}")
        record["identity"]["validated"] = True

        result = run_fn(["/usr/bin/fuser", "-s", "--", target, parent], capture_output=True)
        record["fuser"] = {
            "status": result.returncode,
            "stdout": result.stdout.decode("utf-8", errors="backslashreplace"),
            "stderr": result.stderr.decode("utf-8", errors="backslashreplace"),
        }
        if result.stderr:
            raise ValueError("fuser stderr is nonempty; process visibility is uncertain")
        if result.returncode == 0:
            record["outcome"] = "in_use"
            return 1, record
        if result.returncode != 1 or result.stdout:
            raise ValueError("unexpected fuser status or stdout")
        record["outcome"] = "unused"
        return 0, record
    except (OSError, ValueError) as error:
        record["error"] = str(error)
        return 2, record


def validate_unused(record, status, target, parent, target_id, parent_id):
    """Fail closed on incomplete evidence or disagreement with the host preflight."""
    if not isinstance(record, dict) or record.get("schema_version") != 1:
        raise ValueError("missing root fuser result schema")
    if status != 0 or record.get("outcome") != "unused" or record.get("error") is not None:
        raise ValueError(f"root fuser did not establish an unused target (status {status})")
    identity = record.get("identity")
    if not isinstance(identity, dict) or identity.get("effective_uid") != 0 or identity.get("validated") is not True:
        raise ValueError("missing validated host-root identity")
    uid_map = identity.get("uid_map")
    if (not isinstance(uid_map, list) or len(uid_map) != 3
            or any(type(value) is not int for value in uid_map)
            or uid_map[:2] != [0, 0] or uid_map[2] <= 0):
        raise ValueError("invalid host-root UID mapping")
    for name, path, expected in (("target", target, target_id), ("parent", parent, parent_id)):
        node = record.get(name)
        if (not isinstance(node, dict) or node.get("path") != path
                or node.get("canonical") is not True or node.get("is_block_device") is not True
                or node.get("major_minor") != expected or node.get("expected_major_minor") != expected):
            raise ValueError(f"invalid {name} block identity")
    if record.get("fuser") != {"status": 1, "stdout": "", "stderr": ""}:
        raise ValueError("missing clean fuser unused result")


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    checker = commands.add_parser("check")
    validator = commands.add_parser("validate-result")
    validator.add_argument("result_file", type=Path)
    validator.add_argument("stderr_file", type=Path)
    validator.add_argument("status", type=int)
    for command in (checker, validator):
        for name in ("target", "parent", "target_id", "parent_id"):
            command.add_argument(name)
    args = parser.parse_args()
    if args.command == "check":
        status, record = check(args.target, args.parent, args.target_id, args.parent_id)
        # The chroot sees a read-only host root; only the host runner saves evidence.
        print(json.dumps(record), flush=True)
        return status
    try:
        if args.stderr_file.read_bytes():
            raise ValueError("Docker/helper stderr is nonempty")
        validate_unused(json.loads(args.result_file.read_text()), args.status,
                        args.target, args.parent, args.target_id, args.parent_id)
    except (OSError, ValueError) as error:
        print(f"root fuser evidence rejected: {error}; see {args.result_file}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
