#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(realpath "$SCRIPT_DIR/../../../..")
MQ_HELPER=$(realpath "$SCRIPT_DIR/../rtio-virtio-multiqueue-20261102/summarize.py")
QEMU_CLEANUP_NAME=

cleanup_qemu() {
    if [[ -n $QEMU_CLEANUP_NAME ]]; then
        docker rm -f "$QEMU_CLEANUP_NAME" >/dev/null 2>&1 || true
        QEMU_CLEANUP_NAME=
    fi
}

install_qemu_cleanup() {
    QEMU_CLEANUP_NAME=$1
    trap cleanup_qemu EXIT INT TERM
}

clear_qemu_cleanup() {
    QEMU_CLEANUP_NAME=
    trap - EXIT INT TERM
}

if [[ ${MIXED_SYNC_LIBRARY_ONLY:-0} == 1 ]]; then
    return 0
fi

ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing result directory}
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to selected.iso/ext2.img/exfat.img}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
MIXED_SYNC_RUNNER_AUDIT=${MIXED_SYNC_RUNNER_AUDIT:-0}

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || {
    echo "ROOT_ART must be an existing caller-owned writable directory" >&2
    exit 2
}
for file in selected.iso ext2.img exfat.img; do
    [[ -f $SOURCE_ART/$file ]] || { echo "missing source artifact: $file" >&2; exit 2; }
done
grep -q '^pub(crate) const ENABLED: bool = false;' \
    "$REPO_ROOT/kernel/src/fs/fs_impls/ext4/pipeline_profile.rs" || {
    echo "pipeline profiling must be disabled" >&2
    exit 2
}
grep -q '^pub(crate) const LATENCY_ENABLED: bool = false;' \
    "$REPO_ROOT/kernel/src/fs/fs_impls/ext4/stats.rs" || {
    echo "latency profiling must be disabled" >&2
    exit 2
}

target_for_device() {
    case $1 in
        nvme) printf '%s\n' /dev/nvme1n1p2 ;;
        sata) printf '%s\n' /dev/sda4 ;;
        *) return 2 ;;
    esac
}

parent_for_device() {
    case $1 in
        nvme) printf '%s\n' /dev/nvme1n1 ;;
        sata) printf '%s\n' /dev/sda ;;
        *) return 2 ;;
    esac
}

size_for_device() {
    case $1 in
        nvme) printf '%s\n' 274877906944 ;;
        sata) printf '%s\n' 274877890560 ;;
        *) return 2 ;;
    esac
}

iothread_cpus_for_device() {
    case $1 in
        nvme) printf '%s\n' 56-63 ;;
        sata) printf '%s\n' 63 ;;
        *) return 2 ;;
    esac
}

list_points() {
    if [[ -n ${CASES:-} ]]; then
        local case_name
        for case_name in $CASES; do
            python3 "$SCRIPT_DIR/matrix.py" config "$case_name"
        done
    else
        python3 "$SCRIPT_DIR/matrix.py" list
    fi
}

POINT_ROWS=$(list_points)

if [[ $MIXED_SYNC_RUNNER_AUDIT == 1 ]]; then
    while IFS=$'\t' read -r case_name device mode read_percent sync_percent repeat \
        threads file_mib file_bytes dataset_mib seed warmup_seconds runtime_seconds \
        virtqueues disk_format dram_cache_mib nv_data_cache_mib cache_control; do
        printf 'AUDIT case=%s target=%s mode=%s virtqueues=%s iothread_cpus=%s profiling=off dataset_mib=%s cache_mib=%s cache_control=%s\n' \
            "$case_name" "$(target_for_device "$device")" "$mode" "$virtqueues" \
            "$(iothread_cpus_for_device "$device")" "$dataset_mib" \
            "$((dram_cache_mib + nv_data_cache_mib))" "$cache_control"
    done <<< "$POINT_ROWS"
    exit 0
fi

[[ -f $SOURCE_ART/build-metadata.json ]] || {
    echo "missing build-metadata.json; profiling state is not auditable" >&2
    exit 2
}
python3 - "$SOURCE_ART/build-metadata.json" <<'PY'
import json, sys
metadata = json.load(open(sys.argv[1]))
expected = {
    "pipeline_profile": False,
    "latency_profile": False,
    "rtio_cpu_trace": False,
    "rnvpc_data_limit_mib": 128,
    "init_args": "/ext2/mixed-sync-guest.sh",
}
for key, value in expected.items():
    if metadata.get(key) != value:
        raise SystemExit(f"invalid build metadata {key}: {metadata.get(key)!r} != {value!r}")
PY

exec 9>"$ROOT_ART/sweep.lock"
flock -n 9 || { echo "mixed-sync sweep already running" >&2; exit 2; }

preflight_target() {
    local device=$1 target parent expected_size actual_size fuser_status=0
    target=$(target_for_device "$device")
    parent=$(parent_for_device "$device")
    expected_size=$(size_for_device "$device")
    [[ -b $target && -b $parent ]] || { echo "missing target or parent: $target $parent" >&2; return 2; }
    [[ $(realpath "$target") == "$target" && $(realpath "$parent") == "$parent" ]] || return 2
    actual_size=$(lsblk -dnbo SIZE "$target")
    [[ $actual_size == "$expected_size" ]] || { echo "unexpected target size: $target $actual_size" >&2; return 2; }
    [[ $(lsblk -dnro PKNAME "$target") == "${parent#/dev/}" ]] || return 2
    [[ -z $(lsblk -dnro MOUNTPOINTS "$target") ]] || { echo "target mounted: $target" >&2; return 2; }
    [[ -z $(lsblk -dnro MOUNTPOINTS "$parent") ]] || { echo "parent mounted: $parent" >&2; return 2; }
    [[ -d /sys/class/block/${target#/dev/}/holders && -z $(ls -A "/sys/class/block/${target#/dev/}/holders") ]] || {
        echo "target has active holders: $target" >&2
        return 2
    }
    ! awk -v target="$target" '$1 == target { found = 1 } END { exit !found }' /proc/swaps || {
        echo "target is active swap: $target" >&2
        return 2
    }
    timeout --signal=TERM --kill-after=5 60 docker run --rm --privileged --pid=host \
        --userns=host --network none -v /:/host:ro \
        "$TOOLS_IMAGE" chroot /host /usr/bin/fuser -s "$target" "$parent" || fuser_status=$?
    [[ $fuser_status == 1 ]] || { echo "target or parent in use: $target" >&2; return 2; }
}

format_target() {
    local target=$1 disk_format=$2
    case $disk_format in
        ext2)
            docker run --rm --network none --device "$target:/dev/benchmark-target:rwm" \
                "$TOOLS_IMAGE" mkfs.ext2 -F -b 4096 /dev/benchmark-target
            ;;
        journal)
            docker run --rm --network none --device "$target:/dev/benchmark-target:rwm" \
                "$TOOLS_IMAGE" mkfs.ext4 -F -b 4096 /dev/benchmark-target
            ;;
        nojournal)
            docker run --rm --network none --device "$target:/dev/benchmark-target:rwm" \
                "$TOOLS_IMAGE" mkfs.ext4 -F -b 4096 -O '^has_journal' /dev/benchmark-target
            ;;
        *) return 2 ;;
    esac
}

inject_guest() {
    local image=$1 config=$2 benchmark=$3
    local guest_file
    for guest_file in mixed-sync.cfg mixed-sync-guest.sh mixed_sync_bench; do
        debugfs -w -R "rm /$guest_file" "$image" >/dev/null 2>&1 || true
    done
    debugfs -w -R "write $config /mixed-sync.cfg" "$image" >/dev/null
    debugfs -w -R "write $SCRIPT_DIR/guest.sh /mixed-sync-guest.sh" "$image" >/dev/null
    debugfs -w -R "write $benchmark /mixed_sync_bench" "$image" >/dev/null
}

run_guest() {
    local point=$1 target=$2 case_name=$3 virtqueues=$4 device=$5
    local qemu_name="mixed-sync-qemu-$$" status queue cpu
    local -a iothread_objects=()
    for ((queue = 0; queue < virtqueues; queue++)); do
        iothread_objects+=( -object "iothread,id=iothread$queue" )
    done
    cp -- "$MQ_HELPER" "$point/qmp-helper.py"
    docker run -d --name "$qemu_name" --pid=host --network none \
        --device /dev/kvm --device "$target:/dev/benchmark-target:rwm" \
        -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 10800 qemu-system-x86_64 \
        -enable-kvm -cpu host -machine q35 -smp 64 -m 8G -S \
        -qmp unix:/benchmark/qmp.sock,server=on,wait=off \
        -bios /root/ovmf/release/OVMF.fd -no-reboot -display none -serial stdio \
        -monitor none -nic none -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -cdrom /benchmark/selected.iso \
        -drive if=none,format=raw,id=bootdata,file=/benchmark/ext2.img \
        -device virtio-blk-pci,drive=bootdata,disable-legacy=on \
        -drive if=none,format=raw,id=aux,file=/benchmark/exfat.img \
        -device virtio-blk-pci,drive=aux,disable-legacy=on \
        -drive if=none,format=raw,id=target,file=/dev/benchmark-target,cache=none,aio=threads \
        -device pcie-root-port,id=target-port,chassis=1,slot=1 \
        "${iothread_objects[@]}" >"$point/qemu-container-id.txt"
    install_qemu_cleanup "$qemu_name"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-add-device /benchmark/qmp.sock "$virtqueues" \
        >"$point/qmp-device-add.json"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query /benchmark/qmp.sock \
        >"$point/qmp-iothreads.json"
    python3 - "$point/qmp-iothreads.json" "$TOOLS_IMAGE" "$device" <<'PY' >"$point/iothread-affinity.log"
import json, subprocess, sys
threads = {row["id"]: row["thread-id"] for row in json.load(open(sys.argv[1]))}
cpus = list(range(56, 64)) if sys.argv[3] == "nvme" else [63]
for queue, cpu in enumerate(cpus):
    tid = threads[f"iothread{queue}"]
    command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
               sys.argv[2], "taskset", "-pc", str(cpu), str(tid)]
    print(subprocess.check_output(command, text=True).strip())
PY
    cp -- "$SCRIPT_DIR/qmp_vcpu_affinity.py" "$point/qmp-vcpu-affinity.py"
    docker exec "$qemu_name" python3 /benchmark/qmp-vcpu-affinity.py query /benchmark/qmp.sock \
        >"$point/qmp-vcpus.json"
    python3 "$SCRIPT_DIR/qmp_vcpu_affinity.py" pin "$point/qmp-vcpus.json" \
        "$TOOLS_IMAGE" "$(iothread_cpus_for_device "$device")" \
        >"$point/vcpu-affinity.json"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-cont /benchmark/qmp.sock \
        >"$point/qmp-cont.json"
    docker wait "$qemu_name" >"$point/qemu-exit.txt"
    docker logs "$qemu_name" >"$point/qemu.log" 2>&1
    docker inspect "$qemu_name" >"$point/qemu.inspect.json"
    status=$(<"$point/qemu-exit.txt")
    [[ $status == 0 || $status == 33 ]] || {
        echo "$case_name: QEMU exited with $status" >&2
        return 2
    }
    docker rm "$qemu_name" >/dev/null
    clear_qemu_cleanup
}

make -C "$SCRIPT_DIR" clean mixed_sync_bench
BENCHMARK_BINARY="$SCRIPT_DIR/mixed_sync_bench"

run_point() {
    local case_name=$1 device=$2 mode=$3 read_percent=$4 sync_percent=$5 repeat=$6
    local threads=$7 file_mib=$8 file_bytes=$9 dataset_mib=${10} seed=${11}
    local warmup_seconds=${12} runtime_seconds=${13} virtqueues=${14} disk_format=${15}
    local dram_cache_mib=${16} nv_data_cache_mib=${17} cache_control=${18}
    local target point config check_status=0
    target=$(target_for_device "$device")
    point="$ROOT_ART/$case_name"
    if [[ -f $point/result.json ]]; then
        python3 "$SCRIPT_DIR/parse_result.py" --validate-saved "$point" "$case_name" >/dev/null
        echo "SKIP validated $case_name"
        return
    fi
    [[ ! -e $point ]] || { echo "refusing incomplete point: $point" >&2; return 2; }
    mkdir "$point"
    cp --reflink=auto "$SOURCE_ART/selected.iso" "$point/selected.iso"
    cp --reflink=auto "$SOURCE_ART/ext2.img" "$point/ext2.img"
    cp --reflink=auto "$SOURCE_ART/exfat.img" "$point/exfat.img"
    cp -- "$SOURCE_ART/build-metadata.json" "$point/build-metadata.json"
    sha256sum "$point/selected.iso" "$point/ext2.img" "$point/exfat.img" "$BENCHMARK_BINARY" \
        >"$point/artifact-sha256.txt"
    config="$point/case.cfg"
    printf 'CASE=%q\nDEVICE=%q\nMODE=%q\nREAD_PERCENT=%q\nSYNC_PERCENT=%q\nREPEAT=%q\nTHREADS=%q\nFILE_MIB=%q\nFILE_BYTES=%q\nDATASET_MIB=%q\nSEED=%q\nWARMUP_SECONDS=%q\nRUNTIME_SECONDS=%q\nDRAM_CACHE_MIB=%q\nNV_DATA_CACHE_MIB=%q\n' \
        "$case_name" "$device" "$mode" "$read_percent" "$sync_percent" "$repeat" \
        "$threads" "$file_mib" "$file_bytes" "$dataset_mib" "$seed" \
        "$warmup_seconds" "$runtime_seconds" "$dram_cache_mib" "$nv_data_cache_mib" >"$config"
    inject_guest "$point/ext2.img" "$config" "$BENCHMARK_BINARY"
    preflight_target "$device"
    format_target "$target" "$disk_format" >"$point/mkfs.log" 2>&1
    echo "RUN $case_name target=$target queues=$virtqueues"
    run_guest "$point" "$target" "$case_name" "$virtqueues" "$device"
    docker run --rm --network none --device "$target:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target >"$point/e2fsck.log" 2>&1 || check_status=$?
    printf '%s\n' "$check_status" >"$point/e2fsck.status"
    [[ $check_status == 0 ]] || { echo "$case_name: e2fsck failed" >&2; return 2; }
    python3 "$SCRIPT_DIR/parse_result.py" "$point" "$case_name" >"$point/result.json.tmp"
    mv -- "$point/result.json.tmp" "$point/result.json"
    rm -f -- "$point/selected.iso" "$point/ext2.img" "$point/exfat.img" \
        "$point/qmp-helper.py" "$point/qmp.sock"
    python3 - "$point/result.json" <<'PY'
import json, sys
row = json.load(open(sys.argv[1]))
print(f'{row["case"]}: {row["operations_per_second"]:.2f} ops/s, {row["mib_per_second"]:.2f} MiB/s')
PY
}

while IFS=$'\t' read -r case_name device mode read_percent sync_percent repeat \
    threads file_mib file_bytes dataset_mib seed warmup_seconds runtime_seconds \
    virtqueues disk_format dram_cache_mib nv_data_cache_mib cache_control; do
    run_point "$case_name" "$device" "$mode" "$read_percent" "$sync_percent" "$repeat" \
        "$threads" "$file_mib" "$file_bytes" "$dataset_mib" "$seed" "$warmup_seconds" \
        "$runtime_seconds" "$virtqueues" "$disk_format" "$dram_cache_mib" \
        "$nv_data_cache_mib" "$cache_control"
done <<< "$POINT_ROWS"
