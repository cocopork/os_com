#!/usr/bin/env python3
"""Adds a virtio-blk device with grouped virtqueue-to-IOThread mapping."""

import argparse
import json
import socket
import time


def iothread_vq_mapping(queues: int, iothreads: int) -> list[dict]:
    if queues < 1 or iothreads < 1 or queues % iothreads:
        raise ValueError("queues must be a positive multiple of iothreads")
    return [
        {"iothread": f"iothread{worker}",
         "vqs": list(range(worker, queues, iothreads))}
        for worker in range(iothreads)
    ]


def qmp_device_arguments(queues: int, iothreads: int) -> dict:
    return {
        "driver": "virtio-blk-pci", "id": "target-virtio-blk",
        "bus": "target-port", "drive": "target", "serial": "virtio-direct-target",
        "disable-legacy": "on", "queue-size": 1024, "num-queues": queues,
        "iothread-vq-mapping": iothread_vq_mapping(queues, iothreads),
        "request-merging": False, "discard": False, "write-zeroes": False,
        "event_idx": False, "indirect_desc": False,
    }


def qmp_command(socket_path: str, command: dict, timeout: int = 30):
    deadline = time.monotonic() + timeout
    while True:
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            connection.connect(socket_path)
            break
        except OSError:
            connection.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.1)
    with connection:
        stream = connection.makefile("rwb")

        def receive():
            while True:
                message = json.loads(stream.readline())
                if "event" not in message:
                    return message

        receive()
        stream.write(json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
        stream.flush()
        receive()
        stream.write(json.dumps(command).encode() + b"\n")
        stream.flush()
        result = receive()
        if "error" in result:
            raise RuntimeError(result["error"])
        return result.get("return")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    add = subcommands.add_parser("add")
    add.add_argument("socket")
    add.add_argument("queues", type=int)
    add.add_argument("iothreads", type=int)
    query = subcommands.add_parser("query-iothreads")
    query.add_argument("socket")
    subcommands.add_parser("cont").add_argument("socket")
    args = parser.parse_args()
    if args.command == "add":
        arguments = qmp_device_arguments(args.queues, args.iothreads)
        result = qmp_command(args.socket, {"execute": "device_add", "arguments": arguments})
    elif args.command == "query-iothreads":
        result = qmp_command(args.socket, {"execute": "query-iothreads"})
    else:
        result = qmp_command(args.socket, {"execute": "cont"})
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
