#!/bin/bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$SCRIPT_DIR/../queue-profile.sh"
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
BUILD_ROOT=${BUILD_ROOT:-/home/zn/asterinas-build-env/nvlog-comparison-20261120/build}
RESULT_ROOT=${RESULT_ROOT:-/home/zn/asterinas-build-env/nvlog-comparison-20261120/fio-v1}
QUEUE_PROFILE=${QUEUE_PROFILE:-four}

virtqueues_for_device "$QUEUE_PROFILE" nvme >/dev/null
virtqueues_for_device "$QUEUE_PROFILE" sata >/dev/null

mkdir -p "$RESULT_ROOT"
exec 9>"$RESULT_ROOT/sweep.lock"
flock -n 9 || { echo "NVLog fio sweep already running" >&2; exit 2; }

target_for() { [[ $1 == nvme ]] && echo /dev/nvme1n1p2 || echo /dev/sda4; }
parent_for() { [[ $1 == nvme ]] && echo /dev/nvme1n1 || echo /dev/sda; }
size_for() { [[ $1 == nvme ]] && echo 274877906944 || echo 274877890560; }

preflight() {
    local device=$1 target parent expected actual fuser_status=0
    target=$(target_for "$device"); parent=$(parent_for "$device"); expected=$(size_for "$device")
    [[ -b $target && -b $parent ]] || return 2
    [[ $(realpath "$target") == "$target" && $(realpath "$parent") == "$parent" ]] || return 2
    actual=$(lsblk -dnbo SIZE "$target"); [[ $actual == "$expected" ]] || return 2
    [[ $(lsblk -dnro PKNAME "$target") == "${parent#/dev/}" ]] || return 2
    [[ -z $(lsblk -dnro MOUNTPOINTS "$target") && -z $(lsblk -dnro MOUNTPOINTS "$parent") ]] || return 2
    [[ -d /sys/class/block/${target#/dev/}/holders && -z $(ls -A "/sys/class/block/${target#/dev/}/holders") ]] || return 2
    ! awk -v target="$target" '$1 == target { found=1 } END { exit !found }' /proc/swaps || return 2
    timeout --signal=TERM --kill-after=5 60 docker run --rm --privileged --pid=host \
        --userns=host --network none -v /:/host:ro "$TOOLS_IMAGE" \
        chroot /host /usr/bin/fuser -s "$target" "$parent" || fuser_status=$?
    [[ $fuser_status == 1 ]] || return 2
}

if [[ -n ${CASES:-} ]]; then
    point_rows=
    for case_name in $CASES; do
        point_rows+=$(python3 "$SCRIPT_DIR/nvlog_fio_matrix.py" config "$case_name")$'\n'
    done
elif [[ ${MATRIX:-full} == reconfigured-thread ]]; then
    point_rows=$(python3 "$SCRIPT_DIR/nvlog_fio_matrix.py" reconfigured-thread-list)
else
    point_rows=$(python3 "$SCRIPT_DIR/nvlog_fio_matrix.py" list)
fi

while IFS=$'\t' read -r case_name device workload axis bs jobs per_file_mib expected_bytes fio_rw prepare repeat runtime_seconds fsync_blocks; do
    [[ -n $case_name ]] || continue
    target=$(target_for "$device")
    virtqueues=$(virtqueues_for_device "$QUEUE_PROFILE" "$device")
    point="$RESULT_ROOT/$case_name"
    if [[ -f $point/result.json ]] && python3 "$SCRIPT_DIR/nvlog_fio_matrix.py" parse "$point" "$case_name" >/dev/null; then
        echo "SKIP validated $case_name"; continue
    fi
    [[ ! -e $point ]] || { echo "incomplete point retained: $point" >&2; exit 2; }
    preflight "$device"
    mkdir -p "$point"
    truncate -s 258M "$point/nvdimm.img"
    docker run --rm --network none --device "$target:/dev/target:rwm" "$TOOLS_IMAGE" \
        mkfs.ext4 -F -b 4096 /dev/target >"$point/mkfs.log" 2>&1

    qemu_status=0
    timeout --signal=TERM --kill-after=30 7200 docker run --rm --name "nvlog-fio-$$" \
        --network none --device /dev/kvm --device "$target:/dev/benchmark-target:rwm" \
        -v "$BUILD_ROOT:/source:ro" -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        qemu-system-x86_64 \
            -enable-kvm -cpu host -machine q35,accel=kvm,nvdimm=on \
            -smp 64 -m 8G,slots=4,maxmem=16G \
            -kernel /source/bzImage \
            -append "root=/dev/vda rw init=/opt/guest-init.sh console=ttyS0 panic=-1 nvlog_workload=fio nvpc_pages=32768 fio_case=$case_name fio_device=$device fio_workload=$workload fio_axis=$axis fio_bs=$bs fio_jobs=$jobs fio_per_file_mib=$per_file_mib fio_expected_bytes=$expected_bytes fio_prepare=$prepare fio_repeat=$repeat fio_runtime_seconds=$runtime_seconds fio_fsync_blocks=$fsync_blocks" \
            -drive file=/source/rootfs-v5.ext4,if=none,format=raw,id=root,snapshot=on \
            -device virtio-blk-pci,drive=root \
            -drive file=/dev/benchmark-target,if=none,format=raw,id=target,cache=none,aio=threads \
            -device "virtio-blk-pci,drive=target,num-queues=$virtqueues" \
            -object memory-backend-file,id=mem1,share=on,mem-path=/benchmark/nvdimm.img,size=258M \
            -device nvdimm,memdev=mem1,id=nv1,label-size=2M \
            -nographic -no-reboot >"$point/qemu.log" 2>&1 || qemu_status=$?

    e2fsck_status=0
    docker run --rm --network none --device "$target:/dev/target:r" "$TOOLS_IMAGE" \
        e2fsck -fn /dev/target >"$point/e2fsck.log" 2>&1 || e2fsck_status=$?
    printf '%s\n' "$e2fsck_status" >"$point/e2fsck.status"
    [[ $qemu_status == 0 && $e2fsck_status == 0 ]] || { echo "invalid lifecycle: $case_name" >&2; exit 2; }
    python3 "$SCRIPT_DIR/nvlog_fio_matrix.py" parse "$point" "$case_name" >"$point/result.json"
    preflight "$device"
    truncate -s 0 "$point/nvdimm.img"
    python3 - "$point/result.json" <<'PY'
import json, sys
r=json.load(open(sys.argv[1]))
print(f'PASS {r["case"]} {r["fio_mib_s"]:.2f} MiB/s')
PY
done <<< "$point_rows"
