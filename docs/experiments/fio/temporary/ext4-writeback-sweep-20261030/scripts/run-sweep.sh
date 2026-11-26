#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

ART=${ART:?set ART to an external artifact directory}
TARGET=${TARGET:-/dev/nvme1n1p1}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:20261017}

[[ ${1:-} == --authorized-target ]] || {
    echo "pass --authorized-target after confirming the test partition may be erased" >&2
    exit 2
}
[[ -b "$TARGET" ]]
[[ $(lsblk -dnbo SIZE "$TARGET") == 274877906944 ]]
if findmnt -rn -S "$TARGET" >/dev/null; then
    echo "target is mounted: $TARGET" >&2
    exit 2
fi
for file in selected.iso ext2.img exfat.img; do
    [[ -f "$ART/$file" ]] || { echo "missing $ART/$file" >&2; exit 2; }
done

run_point() {
    local axis=$1
    local value=$2
    local rw=$3
    local bs=$4
    local jobs=$5
    local case_name="ext4-wb-${axis}-${value}"
    local log="$ART/${case_name}.log"
    local check="$ART/${case_name}-e2fsck.log"
    [[ ! -e "$log" ]] || { echo "refusing to overwrite $log" >&2; return 2; }

    printf 'CASE=%s\nVARIANT=ext4\nRW=%s\nBS=%s\nJOBS=%s\nTOTAL_MB=2048\nRUNTIME=300\n' \
        "$case_name" "$rw" "$bs" "$jobs" > "$ART/case.cfg"
    debugfs -w -R "rm /rebuild-fio.cfg" "$ART/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $ART/case.cfg /rebuild-fio.cfg" "$ART/ext2.img" >/dev/null
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 -F -b 4096 /dev/benchmark-target >/dev/null

    set +e
    docker run --rm --name os-com-ext4-writeback-sweep --network none \
        --device /dev/kvm --device "$TARGET:/dev/benchmark-target:rwm" \
        -v "$ART:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 900 \
        qemu-system-x86_64 -enable-kvm -cpu host -machine q35 -smp 64 -m 8G \
        -bios /root/ovmf/release/OVMF.fd -no-reboot -display none -serial stdio \
        -monitor none -nic none -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -cdrom /benchmark/selected.iso \
        -drive if=none,format=raw,id=bootdata,file=/benchmark/ext2.img \
        -device virtio-blk-pci,drive=bootdata,disable-legacy=on \
        -drive if=none,format=raw,id=aux,file=/benchmark/exfat.img \
        -device virtio-blk-pci,drive=aux,disable-legacy=on \
        -drive if=none,format=raw,id=target,file=/dev/benchmark-target,cache=none,aio=threads \
        -device virtio-blk-pci,drive=target,serial=rebuild-nvme1n1p1,disable-legacy=on,queue-size=1024,num-queues=1,request-merging=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off \
        > "$log" 2>&1
    local qemu_status=$?
    set -e
    if [[ $qemu_status -ne 0 && $qemu_status -ne 33 ]]; then
        return "$qemu_status"
    fi
    grep -q "BUFFER_COMPLETE $case_name fio_status=0 .* umount_status=0" "$log"
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target > "$check" 2>&1
}

for jobs in 1 4 8 12 16 20 24 28 32; do
    run_point threads "$jobs" randwrite 4k "$jobs"
done
for bs_kib in 4 8 16 32 64 128 256 512 1024; do
    run_point bs "${bs_kib}k" write "${bs_kib}k" 1
done

