#!/bin/bash
set -uo pipefail

target=/dev/vdb
mountpoint=/mnt/test
status=0

echo "NVLOG_SMOKE_BEGIN target=$target"
lsblk -b -o NAME,SIZE,TYPE,FSTYPE || status=1

if [[ $status -eq 0 ]]; then
    mkfs.ext4 -F "$target" || status=1
fi
if [[ $status -eq 0 ]]; then
    mount -t ext4 "$target" "$mountpoint" || status=1
fi
if [[ $status -eq 0 ]]; then
    nvpcctl open "$mountpoint" strict || status=1
fi
if [[ $status -eq 0 ]]; then
    dd if=/dev/zero of="$mountpoint/smoke.bin" bs=1M count=64 conv=fsync status=none || status=1
fi
if [[ $status -eq 0 ]]; then
    python3 - <<'PY' || status=1
import os
import random

path = "/mnt/test/smoke.bin"
fd = os.open(path, os.O_RDWR)
try:
    rng = random.Random(20261120)
    payload = bytes(4096)
    for _ in range(1024):
        page = rng.randrange((64 * 1024 * 1024) // 4096)
        os.pwrite(fd, payload, page * 4096)
        os.fsync(fd)
finally:
    os.close(fd)
PY
fi

echo "NVLOG_SMOKE_USAGE_BEGIN"
nvpcctl usage || status=1
echo "NVLOG_SMOKE_USAGE_END"
grep -E '^(MemTotal|MemFree|MemAvailable|Cached|Dirty|Writeback):' /proc/meminfo || true

sync || status=1
nvpcctl close "$mountpoint" || status=1
umount "$mountpoint" || status=1

if [[ $status -eq 0 ]]; then
    echo "NVLOG_SMOKE_PASS"
else
    echo "NVLOG_SMOKE_FAIL status=$status"
fi
exit "$status"
