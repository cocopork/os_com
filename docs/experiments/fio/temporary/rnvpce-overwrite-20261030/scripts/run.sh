#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ART=${ART:?set ART to the external artifact directory}
SOURCE_ART=${SOURCE_ART:-/home/zn/asterinas-build-env/preliminary-performance-sweep-20261030-current}
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

mkdir -p "$ART"
for file in selected.iso ext2.img exfat.img; do
    if [[ ! -f "$ART/$file" ]]; then
        cp --reflink=auto "$SOURCE_ART/$file" "$ART/$file"
    fi
done
sha256sum "$ART/selected.iso" > "$ART/selected.iso.sha256"

format_target() {
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 -F -b 4096 -O '^has_journal' \
        /dev/benchmark-target >/dev/null
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
    docker run --rm --name os-com-rnvpce-overwrite --network none \
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
        -device virtio-blk-pci,drive=target,serial=rnvpce-overwrite,disable-legacy=on,queue-size=1024,num-queues=1,request-merging=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off \
        > "$log" 2>&1
    local status=$?
    set -e
    [[ $status -eq 0 || $status -eq 33 ]]
}

run_case() {
    local semantics=$1 jobs=$2
    local prepare=0
    [[ "$semantics" == overwrite ]] && prepare=1
    local case_name="rnvpce-write-128k-$semantics-j$jobs"
    local expected_bytes=$(((2048 / jobs) * jobs * 1048576))
    if [[ -e "$ART/$case_name.log" || -e "$ART/$case_name-e2fsck.log" ]]; then
        if python3 "$SCRIPT_DIR/analyze.py" validate "$ART" "$case_name" >/dev/null 2>&1; then
            echo "SKIP validated $case_name"
            return
        fi
        echo "refusing to overwrite incomplete evidence for $case_name" >&2
        return 2
    fi
    local cfg="$ART/current-case.cfg"
    printf 'CASE=%q\nPREPARE=%q\nJOBS=%q\nTOTAL_MIB=2048\nEXPECTED_BYTES=%q\n' \
        "$case_name" "$prepare" "$jobs" "$expected_bytes" > "$cfg"
    inject_guest "$cfg"
    format_target
    echo "RUN $case_name"
    run_vm "$ART/$case_name.log"
    set +e
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target \
        > "$ART/$case_name-e2fsck.log" 2>&1
    local check_status=$?
    set -e
    echo "OVERWRITE_E2FSCK_STATUS $check_status" >> "$ART/$case_name-e2fsck.log"
    python3 "$SCRIPT_DIR/analyze.py" validate "$ART" "$case_name"
}

for semantics in create overwrite; do
    for jobs in 1 16; do
        run_case "$semantics" "$jobs"
    done
done
python3 "$SCRIPT_DIR/analyze.py" summarize "$ART"
