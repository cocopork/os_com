#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Validation and evidence helpers for the host NVMe multiqueue experiment."""

import argparse
import json
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import sys
import time

GIB_256 = 256 * 1024**3


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


def block_identity(path):
    device = os.stat(path)
    if not stat.S_ISBLK(device.st_mode):
        raise ValueError(f"not a block device: {path}")
    return f"{os.major(device.st_rdev)}:{os.minor(device.st_rdev)}"


def target_preflight(target, lsblk_bin="lsblk", findmnt_bin="findmnt", *,
                     sys_dev_root=Path("/sys/dev/block"), swaps_path=Path("/proc/swaps"),
                     fuser_bin="fuser", identity_fn=block_identity, canonical_fn=os.path.realpath):
    allowed = {"/dev/nvme1n1p1", "/dev/nvme1n1p2"}
    if target not in allowed or canonical_fn(target) != target:
        raise ValueError("target must be canonical /dev/nvme1n1p1 or /dev/nvme1n1p2")
    parent_path = "/dev/nvme1n1"
    target_id = identity_fn(target)
    parent_id = identity_fn(parent_path)

    def lsblk_device(path):
        output = subprocess.check_output(
            [lsblk_bin, "-J", "-b", "-n", "-o", "NAME,TYPE,SIZE,PKNAME,MAJ:MIN,MOUNTPOINTS", path], text=True
        )
        devices = json.loads(output)["blockdevices"]
        if len(devices) != 1:
            raise ValueError(f"lsblk identity ambiguous: {path}")
        return devices[0]

    target_data = lsblk_device(target)
    if target_data.get("type") != "part" or target_data.get("pkname") != "nvme1n1":
        raise ValueError("target parent must be nvme1n1")
    if target_data.get("name") != Path(target).name or target_data.get("maj:min") != target_id:
        raise ValueError("target lsblk major:minor does not match host device")
    if int(target_data.get("size", 0)) != GIB_256:
        raise ValueError("target must be exactly 256 GiB")
    parent_data = lsblk_device(parent_path)
    if parent_data.get("type") != "disk" or parent_data.get("name") != "nvme1n1" or parent_data.get("maj:min") != parent_id:
        raise ValueError("parent lsblk major:minor does not match host device")
    if target_data.get("children"):
        raise ValueError("target has active block descendant")
    for device_id in (target_id, parent_id):
        holders_dir = Path(sys_dev_root) / device_id / "holders"
        if not holders_dir.is_dir():
            raise ValueError(f"missing holder identity: {holders_dir}")
        if any(holders_dir.iterdir()):
            raise ValueError(f"active holder on {device_id}")
    for device_data in (target_data, parent_data):
        if any(mount is not None for mount in device_data.get("mountpoints", [])):
            raise ValueError("target or parent is mounted")

    mount_output = subprocess.check_output([findmnt_bin, "-J", "-o", "SOURCE,MAJ:MIN,TARGET"], text=True)
    def mounted(filesystems):
        for filesystem in filesystems:
            if filesystem.get("maj:min") in (target_id, parent_id):
                return True
            if mounted(filesystem.get("children", [])):
                return True
        return False
    if mounted(json.loads(mount_output).get("filesystems", [])):
        raise ValueError("mounted target or parent major:minor")

    for line in Path(swaps_path).read_text().splitlines()[1:]:
        swap_path = line.split()[0]
        try:
            swap_id = identity_fn(swap_path)
        except ValueError:
            swap_stat = os.stat(swap_path)
            swap_id = f"{os.major(swap_stat.st_dev)}:{os.minor(swap_stat.st_dev)}"
        if swap_id in (target_id, parent_id):
            raise ValueError("target or parent is active swap")
    for path in (target, parent_path):
        used = subprocess.run([fuser_bin, "-s", path], capture_output=True)
        if used.returncode == 0:
            raise ValueError(f"target or parent is in use: {path}")
        if used.returncode != 1 or used.stdout or used.stderr:
            raise ValueError(f"fuser failed: {path}")
    return {"target": target, "parent": "nvme1n1", "size_bytes": GIB_256,
            "target_major_minor": target_id, "parent_major_minor": parent_id}


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
    begin = raw.find("TRACE_FINAL_BEGIN\n")
    end = raw.find("TRACE_FINAL_END\n")
    if begin < 0 or end <= begin or "READY parent=nvme1n1\n" not in raw[:begin]:
        raise ValueError("trace lacks ready marker or complete final map")
    counts = {}
    for hctx, count in re.findall(r"@hctx\[(\d+)\]:\s*(\d+)", raw[begin:end]):
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
