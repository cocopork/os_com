#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

ROOT_ART=${ROOT_ART:-/home/zn/asterinas-build-env/rtio-multi-worker-20261102}
TARGET=${TARGET:-/dev/nvme1n1p1}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}

[[ ${1:-} == --authorized-target ]] || {
    echo "pass --authorized-target after confirming the target may be erased" >&2
    exit 2
}
[[ -b "$TARGET" ]]
[[ $(lsblk -dnbo SIZE "$TARGET") == 274877906944 ]]
! findmnt -rn -S "$TARGET" >/dev/null

run_point() {
    local workers=$1 workload=$2 bs=$3 jobs=$4
    local art="$ROOT_ART/workers-$workers"
    local case_name="rtio-w${workers}-${workload}-${bs}-j${jobs}"
    local log="$art/$case_name.log"
    local check="$art/$case_name-e2fsck.log"
    local per_job_mib=$((2048 / jobs))
    [[ ! -e "$log" ]] || { echo "refusing to overwrite $log" >&2; return 2; }

    cat > "$art/case.cfg" <<EOF
CASE=$case_name
MODE=rnvpce
AXIS=rtio-worker-validation
WORKLOAD=$workload
BS=$bs
JOBS=$jobs
TOTAL_MIB=2048
EXPECTED_BYTES=$((per_job_mib * jobs * 1048576))
DISK_JOURNAL=0
RUNTIME_JOURNAL=0
RUNTIME=300
EOF
    debugfs -w -R "rm /rebuild-fio.cfg" "$art/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $art/case.cfg /rebuild-fio.cfg" "$art/ext2.img" >/dev/null
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 -F -b 4096 -O '^has_journal' /dev/benchmark-target \
        >/dev/null

    set +e
    docker run --rm --network none --device /dev/kvm \
        --device "$TARGET:/dev/benchmark-target:rwm" \
        -v "$art:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
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
    local status=$?
    set -e
    [[ $status -eq 0 || $status -eq 33 ]]
    grep -q "PRELIM_COMPLETE $case_name fio_status=0 .* umount_status=0" "$log"
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target > "$check" 2>&1
}

for workers in ${WORKERS:-1 4}; do
    while IFS= read -r spec; do
        read -r workload bs jobs <<< "$spec"
        run_point "$workers" "$workload" "$bs" "$jobs"
    done <<< "${SPECS:-randwrite 4k 1
randwrite 4k 16
write 128k 1
write 128k 16}"
done
