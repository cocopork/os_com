#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
GUEST="$SCRIPT_DIR/guest.sh"
ANALYZE="$SCRIPT_DIR/analyze.py"
ART=${ART:?set ART to the external artifact directory}
TARGET=${TARGET:-/dev/nvme1n1p1}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:20261017}

[[ ${1:-} == --authorized-target ]] || {
    echo "pass --authorized-target after confirming the test partition may be erased" >&2
    exit 2
}
[[ -b "$TARGET" ]] || { echo "not a block device: $TARGET" >&2; exit 2; }
[[ $(lsblk -dnbo SIZE "$TARGET") == 274877906944 ]] || {
    echo "unexpected target size: $TARGET" >&2
    exit 2
}
if findmnt -rn -S "$TARGET" >/dev/null; then
    echo "target is mounted: $TARGET" >&2
    exit 2
fi
for file in selected.iso ext2.img exfat.img; do
    [[ -f "$ART/$file" ]] || { echo "missing $ART/$file" >&2; exit 2; }
done

format_target() {
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 -F -b 4096 -O '^has_journal' \
        /dev/benchmark-target >/dev/null
}

run_point() {
    local round=$1 jobs=$2
    local case_name="nvpage-pool-r${round}-write-128k-overwrite-j${jobs}"
    local log="$ART/$case_name.log"
    local check="$ART/$case_name-e2fsck.log"
    local cfg="$ART/current-case.cfg"
    local expected_bytes=$(((2048 / jobs) * jobs * 1048576))
    if [[ -e "$log" || -e "$check" ]]; then
        if python3 "$ANALYZE" validate "$ART" "$case_name" >/dev/null 2>&1; then
            echo "SKIP validated $case_name"
            return
        fi
        echo "refusing to overwrite incomplete $case_name evidence" >&2
        return 2
    fi

    printf 'CASE=%q\nPREPARE=1\nJOBS=%q\nTOTAL_MIB=2048\nEXPECTED_BYTES=%q\n' \
        "$case_name" "$jobs" "$expected_bytes" > "$cfg"
    debugfs -w -R "rm /rebuild-fio.cfg" "$ART/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $cfg /rebuild-fio.cfg" "$ART/ext2.img" >/dev/null
    debugfs -w -R "rm /rebuild-fio.sh" "$ART/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $GUEST /rebuild-fio.sh" "$ART/ext2.img" >/dev/null
    format_target

    echo "RUN $case_name"
    set +e
    docker run --rm --name os-com-scalable-nvpage-pool --network none \
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
        -device virtio-blk-pci,drive=target,serial=scalable-nvpage-pool,disable-legacy=on,queue-size=1024,num-queues=1,request-merging=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off \
        > "$log" 2>&1
    local qemu_status=$?
    set -e
    [[ $qemu_status -eq 0 || $qemu_status -eq 33 ]]

    set +e
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target > "$check" 2>&1
    local check_status=$?
    set -e
    echo "OVERWRITE_E2FSCK_STATUS $check_status" >> "$check"
    python3 "$ANALYZE" validate "$ART" "$case_name"
}

for round in 1 2 3; do
    if (( round % 2 == 1 )); then
        run_point "$round" 1
        run_point "$round" 16
    else
        run_point "$round" 16
        run_point "$round" 1
    fi
done
python3 "$ANALYZE" summarize "$ART"
