#!/bin/bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
BUILD_ROOT=${BUILD_ROOT:-/home/zn/asterinas-build-env/nvlog-comparison-20261120/build}
RESULT_ROOT=${RESULT_ROOT:-/home/zn/asterinas-build-env/nvlog-comparison-20261120/smoke}

device=${1:?usage: run-smoke.sh nvme|sata}
case "$device" in
    nvme)
        target=/dev/nvme1n1p2
        parent=/dev/nvme1n1
        expected_size=274877906944
        virtqueues=8
        ;;
    sata)
        target=/dev/sda4
        parent=/dev/sda
        expected_size=274877890560
        virtqueues=1
        ;;
    *) echo "unsupported device: $device" >&2; exit 2 ;;
esac

preflight() {
    local actual_size fuser_status=0
    [[ -b $target && -b $parent ]] || { echo "missing target or parent" >&2; return 2; }
    [[ $(realpath "$target") == "$target" && $(realpath "$parent") == "$parent" ]] || return 2
    actual_size=$(lsblk -dnbo SIZE "$target")
    [[ $actual_size == "$expected_size" ]] || { echo "unexpected target size: $actual_size" >&2; return 2; }
    [[ $(lsblk -dnro PKNAME "$target") == "${parent#/dev/}" ]] || return 2
    [[ -z $(lsblk -dnro MOUNTPOINTS "$target") ]] || { echo "target mounted" >&2; return 2; }
    [[ -z $(lsblk -dnro MOUNTPOINTS "$parent") ]] || { echo "parent mounted" >&2; return 2; }
    [[ -d /sys/class/block/${target#/dev/}/holders && -z $(ls -A "/sys/class/block/${target#/dev/}/holders") ]] || {
        echo "target has holders" >&2; return 2;
    }
    ! awk -v target="$target" '$1 == target { found = 1 } END { exit !found }' /proc/swaps || {
        echo "target is swap" >&2; return 2;
    }
    timeout --signal=TERM --kill-after=5 60 docker run --rm --privileged --pid=host \
        --userns=host --network none -v /:/host:ro "$TOOLS_IMAGE" \
        chroot /host /usr/bin/fuser -s "$target" "$parent" || fuser_status=$?
    [[ $fuser_status == 1 ]] || { echo "target or parent in use" >&2; return 2; }
}

preflight
point="$RESULT_ROOT/$device"
mkdir -p "$point"
[[ ! -e $point/result.json ]] || { echo "refusing to overwrite $point/result.json" >&2; exit 2; }
    cp --reflink=auto "$BUILD_ROOT/rootfs-v5.ext4" "$point/rootfs.ext4"
truncate -s 258M "$point/nvdimm.img"

qemu_name="nvlog-smoke-$device-$$"
qemu_status=0
timeout --signal=TERM --kill-after=30 600 docker run --rm --name "$qemu_name" \
    --network none --device /dev/kvm --device "$target:/dev/benchmark-target:rwm" \
    -v "$BUILD_ROOT:/source:ro" -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
    qemu-system-x86_64 \
        -enable-kvm -cpu host -machine q35,accel=kvm,nvdimm=on \
        -smp 64 -m 8G,slots=4,maxmem=16G \
        -kernel /source/bzImage \
        -append 'root=/dev/vda rw init=/opt/guest-init.sh console=ttyS0 panic=-1 nvlog_smoke=1' \
        -drive file=/benchmark/rootfs.ext4,if=none,format=raw,id=root \
        -device virtio-blk-pci,drive=root \
        -drive file=/dev/benchmark-target,if=none,format=raw,id=target,cache=none,aio=threads \
        -device "virtio-blk-pci,drive=target,num-queues=$virtqueues" \
        -object memory-backend-file,id=mem1,share=on,mem-path=/benchmark/nvdimm.img,size=258M \
        -device nvdimm,memdev=mem1,id=nv1,label-size=2M \
        -nographic -no-reboot >"$point/serial.log" 2>&1 || qemu_status=$?

grep -E 'NVLOG_(GUEST|SMOKE|CONFIG|RECONFIGURE)|NVPC started with' "$point/serial.log" >"$point/markers.log" || true
cat "$point/markers.log"

e2fsck_status=0
docker run --rm --network none --device "$target:/dev/target:r" "$TOOLS_IMAGE" \
    e2fsck -fn /dev/target >"$point/e2fsck.log" 2>&1 || e2fsck_status=$?

parse_valid=false
if [[ $qemu_status == 0 && $e2fsck_status == 0 ]] \
    && grep -q 'NVLOG_GUEST_PASS' "$point/serial.log" \
    && grep -q 'NVLOG_SMOKE_PASS' "$point/serial.log" \
    && grep -q 'NVPC started with 16384 pages' "$point/serial.log"; then
    parse_valid=true
fi

python3 - "$point/result.json" "$device" "$target" "$virtqueues" "$qemu_status" "$e2fsck_status" "$parse_valid" <<'PY'
import json
import sys

path, device, target, queues, qemu_status, e2fsck_status, parse_valid = sys.argv[1:]
payload = {
    "case": f"nvlog-{device}-smoke",
    "device": device,
    "target": target,
    "virtqueues": int(queues),
    "benchmark_exit": 0 if parse_valid == "true" else 1,
    "sync_exit": 0 if parse_valid == "true" else 1,
    "unmount_exit": 0 if parse_valid == "true" else 1,
    "qemu_exit": int(qemu_status),
    "e2fsck_exit": int(e2fsck_status),
    "parse_valid": parse_valid == "true",
}
with open(path, "w") as output:
    json.dump(payload, output, indent=2, sort_keys=True)
    output.write("\n")
PY

preflight
[[ $parse_valid == true ]] || { echo "smoke failed; evidence retained at $point" >&2; exit 1; }
echo "NVLog $device smoke passed: $point"
