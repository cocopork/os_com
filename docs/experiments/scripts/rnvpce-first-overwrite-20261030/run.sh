#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ART=${ART:?set ART to the external artifact directory}
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

format_target() {
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 -F -b 4096 -O '^has_journal' /dev/benchmark-target >/dev/null
}

inject_guest() {
    local cfg=$1
    debugfs -w -R "rm /rebuild-fio.cfg" "$ART/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $cfg /rebuild-fio.cfg" "$ART/ext2.img" >/dev/null
    debugfs -w -R "rm /rebuild-fio.sh" "$ART/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$ART/ext2.img" >/dev/null
}

run_vm() {
    local log=$1
    set +e
    docker run --rm --name os-com-rnvpce-first-overwrite --network none \
        --device /dev/kvm --device "$TARGET:/dev/benchmark-target:rwm" \
        -v "$ART:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 1800 \
        qemu-system-x86_64 -enable-kvm -cpu host -machine q35 -smp 64 -m 8G \
        -bios /root/ovmf/release/OVMF.fd -no-reboot -display none -serial stdio \
        -monitor none -nic none -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -cdrom /benchmark/selected.iso \
        -drive if=none,format=raw,id=bootdata,file=/benchmark/ext2.img \
        -device virtio-blk-pci,drive=bootdata,disable-legacy=on \
        -drive if=none,format=raw,id=aux,file=/benchmark/exfat.img \
        -device virtio-blk-pci,drive=aux,disable-legacy=on \
        -drive if=none,format=raw,id=target,file=/dev/benchmark-target,cache=none,aio=threads \
        -device virtio-blk-pci,drive=target,serial=rnvpce-perf,disable-legacy=on,queue-size=1024,num-queues=1,request-merging=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off \
        > "$log" 2>&1
    local status=$?
    set -e
    [[ $status -eq 0 || $status -eq 33 ]]
}

run_case() {
    local rw=$1 bs=$2 semantics=$3 jobs=$4
    local prepare=0
    [[ "$semantics" == overwrite ]] && prepare=1
    local case_name="rnvpce-${rw}-${bs}-${semantics}-j${jobs}"
    local expected_bytes=$(((2048 / jobs) * jobs * 1048576))
    if [[ -e "$ART/$case_name.log" ]]; then
        python3 "$SCRIPT_DIR/analyze.py" validate "$ART" "$case_name"
        echo "SKIP validated $case_name"
        return
    fi
    local cfg="$ART/current-case.cfg"
    printf 'CASE=%q\nPREPARE=%q\nRW=%q\nBS=%q\nJOBS=%q\nTOTAL_MIB=2048\nEXPECTED_BYTES=%q\n' \
        "$case_name" "$prepare" "$rw" "$bs" "$jobs" "$expected_bytes" > "$cfg"
    inject_guest "$cfg"
    format_target
    echo "RUN $case_name"
    run_vm "$ART/$case_name.log"
    set +e
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target > "$ART/$case_name-e2fsck.log" 2>&1
    local check_status=$?
    set -e
    echo "PERF_E2FSCK_STATUS $check_status" >> "$ART/$case_name-e2fsck.log"
    python3 "$SCRIPT_DIR/analyze.py" validate "$ART" "$case_name"
}

for workload in 'randwrite 4k' 'write 128k'; do
    read -r rw bs <<< "$workload"
    for semantics in first overwrite; do
        for jobs in 1 16; do
            run_case "$rw" "$bs" "$semantics" "$jobs"
        done
    done
done
python3 "$SCRIPT_DIR/analyze.py" summarize "$ART"

