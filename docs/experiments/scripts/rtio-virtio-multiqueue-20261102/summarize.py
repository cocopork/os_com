#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Validation and evidence helpers for the host NVMe multiqueue experiment."""

import argparse
import json
from pathlib import Path
import re
import socket
import stat
import subprocess
import sys
import time

GIB_256 = 256 * 1024**3
DISK_NAME = re.compile(r"nvme[0-9]+n[0-9]+")


def qemu_preflight(qemu_bin):
    version = subprocess.check_output([qemu_bin, "-version"], text=True)
    help_text = subprocess.check_output([qemu_bin, "-device", "virtio-blk-pci,help"], text=True)
    return validate_qemu(version, help_text)


def validate_qemu(version, help_text):
    match = re.search(r"QEMU emulator version (\d+)\.(\d+)(?:\.(\d+))?", version)
    if not match or tuple(map(int, match.group(1, 2))) < (9, 0):
        raise ValueError("multi-hctx requires QEMU 9.0 or newer")
    if "iothread-vq-mapping=" not in help_text:
        raise ValueError("QEMU lacks iothread-vq-mapping")
    return {"version": version.strip(), "device_help": help_text}


def target_preflight(target, lsblk_bin="lsblk", findmnt_bin="findmnt"):
    path = Path(target)
    if not stat.S_ISBLK(path.stat().st_mode):
        raise ValueError(f"not a block device: {target}")
    output = subprocess.check_output(
        [lsblk_bin, "-J", "-b", "-n", "-o", "NAME,TYPE,SIZE,PKNAME", target], text=True
    )
    devices = json.loads(output)["blockdevices"]
    if len(devices) != 1:
        raise ValueError("target must resolve to exactly one partition")
    device = devices[0]
    parent = device.get("pkname")
    if device.get("type") != "part" or not parent or not DISK_NAME.fullmatch(parent):
        raise ValueError("target must be an NVMe partition")
    if int(device.get("size", 0)) != GIB_256:
        raise ValueError("target must be exactly 256 GiB")
    for source in (target, f"/dev/{parent}"):
        mount_check = subprocess.run([findmnt_bin, "-rn", "-S", source], capture_output=True)
        if mount_check.returncode == 0:
            raise ValueError(f"mounted source: {source}")
        if mount_check.returncode != 1:
            raise ValueError(f"findmnt failed for {source}: {mount_check.stderr.decode().strip()}")
    return {"target": str(path.resolve()), "parent": parent, "size_bytes": GIB_256}


def expand_cpu_list(value):
    cpus = set()
    for part in value.strip().split(","):
        if not part:
            continue
        bounds = part.split("-")
        if len(bounds) == 1:
            cpus.add(int(bounds[0]))
        elif len(bounds) == 2:
            cpus.update(range(int(bounds[0]), int(bounds[1]) + 1))
        else:
            raise ValueError(f"invalid CPU list: {value}")
    return cpus


def select_hctx_cpus(mq_root, queues, allowed_cpus):
    root = Path(mq_root)
    choices = []
    for path in sorted(root.glob("*/cpu_list"), key=lambda path: int(path.parent.name)):
        cpus = sorted(expand_cpu_list(path.read_text()) & allowed_cpus)
        if cpus:
            choices.append((int(path.parent.name), cpus, str(path)))

    def choose(index, selected, used):
        if len(selected) == queues:
            return selected
        if index == len(choices):
            return None
        hctx, cpus, source = choices[index]
        for cpu in cpus:
            if cpu not in used:
                result = choose(index + 1, selected + [{"hctx": hctx, "cpu": cpu, "source": source}], used | {cpu})
                if result is not None:
                    return result
        return choose(index + 1, selected, used)

    result = choose(0, [], set())
    if result is None:
        raise ValueError(f"fewer than {queues} distinct hctx/allowed CPU pairs")
    return result


def qemu_mapping(queues):
    if queues < 1:
        raise ValueError("queue count must be positive")
    return [{"iothread": f"iothread{queue}", "vqs": [queue]} for queue in range(queues)]


def qmp_device_args(queues):
    return {"driver": "virtio-blk-pci", "id": "target-virtio-blk", "bus": "target-port", "drive": "target",
            "serial": "rtio-mq-target", "disable-legacy": "on", "queue-size": 1024,
            "num-queues": queues, "iothread-vq-mapping": qemu_mapping(queues),
            "request-merging": False, "discard": False, "write-zeroes": False,
            "event_idx": False, "indirect_desc": False}


def qmp_command(socket_path, commands, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        try:
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.connect(socket_path)
            break
        except OSError:
            connection.close()
            if time.monotonic() >= deadline:
                raise TimeoutError(f"QMP socket unavailable: {socket_path}")
            time.sleep(0.1)
    with connection:
        stream = connection.makefile("rwb")

        def receive():
            while True:
                message = json.loads(stream.readline())
                if "event" not in message:
                    return message

        greeting = receive()
        if "QMP" not in greeting:
            raise ValueError("QMP greeting missing")
        replies = []
        for command in [{"execute": "qmp_capabilities"}, *commands]:
            stream.write((json.dumps(command) + "\n").encode())
            stream.flush()
            reply = receive()
            if "error" in reply:
                raise ValueError(f"QMP {command['execute']}: {reply['error']}")
            replies.append(reply["return"])
        return replies[1:]


def mapping_record(placements, iothreads):
    by_id = {row["id"]: row["thread-id"] for row in iothreads}
    if len(by_id) != len(iothreads):
        raise ValueError("duplicate QMP IOThread IDs")
    result = []
    for queue, placement in enumerate(placements):
        iothread_id = f"iothread{queue}"
        if iothread_id not in by_id:
            raise ValueError(f"QMP did not report {iothread_id}")
        result.append({"guest_queue": queue, "iothread_id": iothread_id,
                       "iothread_tid": by_id[iothread_id], "host_cpu": placement["cpu"],
                       "nvme_hctx": placement["hctx"], "sysfs_cpu_list": placement["source"]})
    return result


def summarize_counts(counts):
    total = sum(counts.values())
    peak = max(counts.values(), default=0)
    active = sum(count > 0 for count in counts.values())
    return {"counts": {str(hctx): count for hctx, count in sorted(counts.items())},
            "total": total, "active_hctx": active, "largest_fraction": peak / total if total else None,
            "accepted": active >= 4 and total > 0 and peak * 10 <= total * 7}


def parse_trace(raw):
    counts = {}
    for hctx, count in re.findall(r"@hctx\[(\d+)\]:\s*(\d+)", raw):
        counts[int(hctx)] = counts.get(int(hctx), 0) + int(count)
    if not counts:
        raise ValueError("no NVMe hctx submission counts in trace")
    return summarize_counts(counts)


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("qemu-preflight").add_argument("qemu_bin")
    qemu_files = sub.add_parser("qemu-preflight-files")
    qemu_files.add_argument("version_file")
    qemu_files.add_argument("help_file")
    sub.add_parser("target-preflight").add_argument("target")
    select = sub.add_parser("select-hctx-cpus")
    select.add_argument("mq_root")
    select.add_argument("queues", type=int)
    select.add_argument("allowed_cpus")
    sub.add_parser("qemu-mapping").add_argument("queues", type=int)
    query = sub.add_parser("qmp-query")
    query.add_argument("socket")
    add_device = sub.add_parser("qmp-add-device")
    add_device.add_argument("socket")
    add_device.add_argument("queues", type=int)
    sub.add_parser("qmp-cont").add_argument("socket")
    record = sub.add_parser("mapping-record")
    record.add_argument("placements")
    record.add_argument("iothreads")
    sub.add_parser("trace-counts").add_argument("raw_trace")
    args = parser.parse_args()
    if args.command == "qemu-preflight":
        result = qemu_preflight(args.qemu_bin)
    elif args.command == "qemu-preflight-files":
        result = validate_qemu(Path(args.version_file).read_text(), Path(args.help_file).read_text())
    elif args.command == "target-preflight":
        result = target_preflight(args.target)
    elif args.command == "select-hctx-cpus":
        result = select_hctx_cpus(args.mq_root, args.queues, expand_cpu_list(args.allowed_cpus))
    elif args.command == "qemu-mapping":
        result = qemu_mapping(args.queues)
    elif args.command == "qmp-query":
        result = qmp_command(args.socket, [{"execute": "query-iothreads"}])[0]
    elif args.command == "qmp-add-device":
        result = qmp_command(args.socket, [{"execute": "device_add", "arguments": qmp_device_args(args.queues)}])[0]
    elif args.command == "qmp-cont":
        result = qmp_command(args.socket, [{"execute": "cont"}])[0]
    elif args.command == "mapping-record":
        result = mapping_record(json.loads(Path(args.placements).read_text()), json.loads(Path(args.iothreads).read_text()))
    else:
        result = parse_trace(Path(args.raw_trace).read_text())
    json.dump(result, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError, TimeoutError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(2)
