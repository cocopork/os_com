#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Query QEMU vCPU threads and pin them away from IOThread host CPUs."""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import time


def qmp_query_cpus(socket_path: str, timeout: float = 30.0) -> list[dict]:
    deadline = time.monotonic() + timeout
    while True:
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            connection.connect(socket_path)
            break
        except OSError:
            connection.close()
            if time.monotonic() >= deadline:
                raise TimeoutError(f"QMP socket unavailable: {socket_path}")
            time.sleep(0.1)
    with connection:
        stream = connection.makefile("rwb")

        def receive() -> dict:
            while True:
                message = json.loads(stream.readline())
                if "event" not in message:
                    return message

        if "QMP" not in receive():
            raise ValueError("QMP greeting missing")
        for command in ("qmp_capabilities", "query-cpus-fast"):
            stream.write((json.dumps({"execute": command}) + "\n").encode())
            stream.flush()
            reply = receive()
            if "error" in reply:
                raise ValueError(f"QMP {command}: {reply['error']}")
        return reply["return"]


def assign_vcpu_host_cpus(records: list[dict], reserved: set[int]) -> list[dict]:
    by_cpu = {row["cpu-index"]: row["thread-id"] for row in records}
    if len(by_cpu) != len(records) or set(by_cpu) != set(range(len(records))):
        raise ValueError("QMP vCPU indexes must be unique and contiguous")
    cpu_count = max(os.cpu_count() or 0, len(records) + len(reserved))
    available = [cpu for cpu in range(cpu_count) if cpu not in reserved]
    if len(available) < len(records):
        raise ValueError("not enough host CPUs after reserving IOThread CPUs")
    return [
        {"guest_cpu": guest, "vcpu_tid": by_cpu[guest], "host_cpu": available[guest]}
        for guest in range(len(records))
    ]


def parse_cpu_set(text: str) -> set[int]:
    result = set()
    for part in text.split(","):
        bounds = part.split("-")
        if len(bounds) == 1:
            result.add(int(bounds[0]))
        elif len(bounds) == 2:
            result.update(range(int(bounds[0]), int(bounds[1]) + 1))
        else:
            raise ValueError(f"invalid CPU set: {text}")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    query = subparsers.add_parser("query")
    query.add_argument("socket")
    pin = subparsers.add_parser("pin")
    pin.add_argument("records", type=Path)
    pin.add_argument("tools_image")
    pin.add_argument("reserved")
    args = parser.parse_args()
    if args.command == "query":
        print(json.dumps(qmp_query_cpus(args.socket), indent=2))
        return
    records = json.loads(args.records.read_text())
    assignments = assign_vcpu_host_cpus(records, parse_cpu_set(args.reserved))
    for row in assignments:
        subprocess.run(
            ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
             args.tools_image, "taskset", "-pc", str(row["host_cpu"]), str(row["vcpu_tid"])],
            check=True,
            stdout=subprocess.DEVNULL,
        )
    print(json.dumps(assignments, indent=2))


if __name__ == "__main__":
    main()
