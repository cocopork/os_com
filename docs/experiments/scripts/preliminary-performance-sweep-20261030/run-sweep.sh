#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ART=${ART:?set ART to the external artifact directory}
SOURCE_ART=${SOURCE_ART:-/home/zn/asterinas-build-env/scalable-rnvpce-allocation-20261030-final3}
TARGET=${TARGET:-/dev/nvme1n1p1}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:20261017}
MODE=${2:---full}

[[ ${1:-} == --authorized-target ]] || {
    echo "pass --authorized-target after confirming the test partition may be erased" >&2
    exit 2
}
[[ "$MODE" == --full || "$MODE" == --smoke || "$MODE" == --case=* ]] || {
    echo "usage: $0 --authorized-target [--smoke|--full|--case=CASE]" >&2
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
    local mode=$1
    local mkfs_args=(-F -b 4096)
    if [[ "$mode" != ext4 ]]; then
        mkfs_args+=(-O '^has_journal')
    fi
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 "${mkfs_args[@]}" /dev/benchmark-target >/dev/null
    local features
    features=$(docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" dumpe2fs -h /dev/benchmark-target 2>/dev/null | \
        sed -n 's/^Filesystem features:[[:space:]]*//p')
    if [[ "$mode" == ext4 ]]; then
        [[ " $features " == *" has_journal "* ]]
    else
        [[ " $features " != *" has_journal "* ]]
    fi
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
    docker run --rm --name os-com-preliminary-sweep --network none \
        --device /dev/kvm --device "$TARGET:/dev/benchmark-target:rwm" \
        -v "$ART:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 7800 \
        qemu-system-x86_64 -enable-kvm -cpu host -machine q35 -smp 64 -m 8G \
        -bios /root/ovmf/release/OVMF.fd -no-reboot -display none -serial stdio \
        -monitor none -nic none -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -cdrom /benchmark/selected.iso \
        -drive if=none,format=raw,id=bootdata,file=/benchmark/ext2.img \
        -device virtio-blk-pci,drive=bootdata,disable-legacy=on \
        -drive if=none,format=raw,id=aux,file=/benchmark/exfat.img \
        -device virtio-blk-pci,drive=aux,disable-legacy=on \
        -drive if=none,format=raw,id=target,file=/dev/benchmark-target,cache=none,aio=threads \
        -device virtio-blk-pci,drive=target,serial=preliminary-nvme,disable-legacy=on,queue-size=1024,num-queues=1,request-merging=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off \
        > "$log" 2>&1
    local status=$?
    set -e
    [[ $status -eq 0 || $status -eq 33 ]]
}

run_point() {
    local case_name=$1 mode=$2 axis=$3 workload=$4 bs=$5 jobs=$6
    local total_mib=$7 expected_bytes=$8 disk_journal=$9 runtime_journal=${10}
    local log="$ART/$case_name.log"
    local check="$ART/$case_name-e2fsck.log"
    if [[ -e "$log" || -e "$check" ]]; then
        if python3 "$SCRIPT_DIR/sweep.py" validate "$ART" "$case_name" >/dev/null 2>&1; then
            echo "SKIP validated $case_name"
            return
        fi
        echo "refusing to overwrite incomplete evidence for $case_name" >&2
        return 2
    fi
    local cfg="$ART/current-case.cfg"
    printf 'CASE=%q\nMODE=%q\nAXIS=%q\nWORKLOAD=%q\nBS=%q\nJOBS=%q\nTOTAL_MIB=%q\nEXPECTED_BYTES=%q\nDISK_JOURNAL=%q\nRUNTIME_JOURNAL=%q\nRUNTIME=7200\n' \
        "$case_name" "$mode" "$axis" "$workload" "$bs" "$jobs" "$total_mib" \
        "$expected_bytes" "$disk_journal" "$runtime_journal" > "$cfg"
    inject_guest "$cfg"
    format_target "$mode"
    echo "RUN $case_name"
    run_vm "$log"
    set +e
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target > "$check" 2>&1
    local check_status=$?
    set -e
    echo "PRELIM_E2FSCK_STATUS $check_status" >> "$check"
    python3 "$SCRIPT_DIR/sweep.py" validate "$ART" "$case_name"
}

run_smoke() {
    local mode=$1
    local case_name="smoke-$mode-write-4k"
    local expected_bytes=$((64 * 1048576))
    local journal=0
    [[ "$mode" == ext4 ]] && journal=1
    rm -f "$ART/$case_name.log" "$ART/$case_name-e2fsck.log"
    local cfg="$ART/current-case.cfg"
    printf 'CASE=%q\nMODE=%q\nAXIS=smoke\nWORKLOAD=write\nBS=4k\nJOBS=1\nTOTAL_MIB=64\nEXPECTED_BYTES=%q\nDISK_JOURNAL=%q\nRUNTIME_JOURNAL=%q\nRUNTIME=300\n' \
        "$case_name" "$mode" "$expected_bytes" "$journal" "$journal" > "$cfg"
    inject_guest "$cfg"
    format_target "$mode"
    echo "SMOKE $mode"
    run_vm "$ART/$case_name.log"
    grep -q "PRELIM_CONFIG case=$case_name mode=$mode" "$ART/$case_name.log"
    grep -q "PRELIM_COMPLETE $case_name fio_status=0 .* umount_status=0" "$ART/$case_name.log"
    set +e
    docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target > "$ART/$case_name-e2fsck.log" 2>&1
    local status=$?
    set -e
    echo "PRELIM_E2FSCK_STATUS $status" >> "$ART/$case_name-e2fsck.log"
    [[ $status -eq 0 ]]
}

if [[ "$MODE" == --smoke ]]; then
    for variant in ext4 rnvpc rnvpce; do run_smoke "$variant"; done
elif [[ "$MODE" == --case=* ]]; then
    python3 "$SCRIPT_DIR/sweep.py" config "${MODE#--case=}" | \
        while IFS=$'\t' read -r case_name variant axis workload bs jobs total bytes disk runtime; do
            run_point "$case_name" "$variant" "$axis" "$workload" "$bs" "$jobs" "$total" "$bytes" "$disk" "$runtime"
        done
else
    while IFS=$'\t' read -r case_name variant axis workload bs jobs total bytes disk runtime; do
        run_point "$case_name" "$variant" "$axis" "$workload" "$bs" "$jobs" "$total" "$bytes" "$disk" "$runtime"
    done < <(python3 "$SCRIPT_DIR/sweep.py" list)
    python3 "$SCRIPT_DIR/sweep.py" summarize "$ART"
fi
