#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MQ_DIR=$(realpath "$SCRIPT_DIR/../../../../validation/temporary/rtio-virtio-multiqueue-20261102/scripts")
source "$SCRIPT_DIR/../queue-profile.sh"
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing result directory}
SOURCE_ART=${SOURCE_ART:-}
SOURCE_ART_NVME=${SOURCE_ART_NVME:-$SOURCE_ART}
SOURCE_ART_SATA=${SOURCE_ART_SATA:-$SOURCE_ART}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
QUEUE_PROFILE=${QUEUE_PROFILE:-four}
PAGECACHE_CAPACITY_MB=${PAGECACHE_CAPACITY_MB:-128}
DIRECT_IO=${DIRECT_IO:-0}
ISOLATE_IOTHREAD_CPUS=${ISOLATE_IOTHREAD_CPUS:-0}
QEMU_SMP=${QEMU_SMP:-64}
PIN_VCPUS=${PIN_VCPUS:-0}

[[ $DIRECT_IO == 0 || $DIRECT_IO == 1 ]] || {
    echo "DIRECT_IO must be 0 or 1" >&2
    exit 2
}

[[ -f $MQ_DIR/summarize.py ]] || { echo "missing QMP helper: $MQ_DIR/summarize.py" >&2; exit 2; }

virtqueues_for_device "$QUEUE_PROFILE" nvme >/dev/null
virtqueues_for_device "$QUEUE_PROFILE" sata >/dev/null

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || {
    echo "ROOT_ART must be an existing caller-owned writable directory" >&2
    exit 2
}
source_for_device() {
    case $1 in
        nvme) printf '%s\n' "$SOURCE_ART_NVME" ;;
        sata) printf '%s\n' "$SOURCE_ART_SATA" ;;
        *) return 2 ;;
    esac
}

for device in nvme sata; do
    source=$(source_for_device "$device")
    [[ -n $source ]] || { echo "missing source directory for $device" >&2; exit 2; }
    for file in selected.iso ext2.img exfat.img; do
        [[ -f $source/$file ]] || {
            echo "missing $device source artifact: $source/$file" >&2
            exit 2
        }
    done
done

exec 9>"$ROOT_ART/sweep.lock"
flock -n 9 || { echo "full end-to-end sweep already running" >&2; exit 2; }

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

preflight_target() {
    local device=$1 target parent size expected_size fuser_status=0
    target=$(target_for_device "$device")
    parent=$(parent_for_device "$device")
    expected_size=$(size_for_device "$device")
    [[ -b $target && -b $parent ]] || { echo "missing target or parent: $target $parent" >&2; return 2; }
    [[ $(realpath "$target") == "$target" && $(realpath "$parent") == "$parent" ]] || return 2
    size=$(lsblk -dnbo SIZE "$target")
    [[ $size == "$expected_size" ]] || { echo "unexpected target size: $target $size" >&2; return 2; }
    [[ $(lsblk -dnro PKNAME "$target") == "${parent#/dev/}" ]] || return 2
    ! findmnt -rn -S "$target" >/dev/null || { echo "target mounted: $target" >&2; return 2; }
    ! findmnt -rn -S "$parent" >/dev/null || { echo "parent mounted: $parent" >&2; return 2; }
    docker run --rm --privileged --pid=host --userns=host --network none -v /:/host:ro \
        "$TOOLS_IMAGE" chroot /host /usr/bin/fuser -s "$target" "$parent" || fuser_status=$?
    [[ $fuser_status == 1 ]] || { echo "target or parent in use: $target" >&2; return 2; }
}

format_target() {
    local target=$1 disk_format=$2
    local args=(-F -b 4096)
    [[ $disk_format == journal ]] || args+=(-O '^has_journal')
    docker run --rm --network none --device "$target:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 "${args[@]}" /dev/benchmark-target
}

inject_guest() {
    local image=$1 cfg=$2
    for guest_file in rebuild-fio.cfg rebuild-fio.sh; do
        debugfs -w -R "rm /$guest_file" "$image" >/dev/null 2>&1 || true
    done
    debugfs -w -R "write $cfg /rebuild-fio.cfg" "$image" >/dev/null
    debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$image" >/dev/null
}

run_guest() {
    local point=$1 target=$2 case_name=$3 device=$4
    local qemu_name="full-e2e-qemu-$$"
    local status queue virtqueues
    local -a iothread_objects=() iothread_cpus=()
    virtqueues=$(virtqueues_for_device "$QUEUE_PROFILE" "$device")
    read -r -a iothread_cpus <<< "$(iothread_cpus_for_device "$QUEUE_PROFILE" "$device")"
    [[ ${#iothread_cpus[@]} -eq $virtqueues ]] || return 2
    for ((queue = 0; queue < virtqueues; queue++)); do
        iothread_objects+=( -object "iothread,id=iothread$queue" )
    done
    cp -- "$MQ_DIR/summarize.py" "$point/qmp-helper.py"
    docker run -d --name "$qemu_name" --pid=host --network none \
        --device /dev/kvm --device "$target:/dev/benchmark-target:rwm" \
        -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 7200 qemu-system-x86_64 \
        -enable-kvm -cpu host -machine q35 -smp "$QEMU_SMP" -m 8G -mem-prealloc -S \
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
        "${iothread_objects[@]}" \
        >"$point/qemu-container-id.txt"
    trap 'docker rm -f "$qemu_name" >/dev/null 2>&1 || true' RETURN
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-add-device /benchmark/qmp.sock "$virtqueues" \
        >"$point/qmp-device-add.json"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query /benchmark/qmp.sock \
        >"$point/qmp-iothreads.json"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query-cpus /benchmark/qmp.sock \
        >"$point/qmp-cpus.json"
    python3 - "$point/qmp-iothreads.json" "$point/qmp-cpus.json" "$TOOLS_IMAGE" "${iothread_cpus[*]}" "$virtqueues" "$ISOLATE_IOTHREAD_CPUS" "$PIN_VCPUS" <<'PY' >"$point/iothread-affinity.log"
import json, subprocess, sys
threads = {row["id"]: row["thread-id"] for row in json.load(open(sys.argv[1]))}
vcpus = sorted(json.load(open(sys.argv[2])), key=lambda row: row["cpu-index"])
tools_image = sys.argv[3]
cpus = [int(cpu) for cpu in sys.argv[4].split()]
queues = int(sys.argv[5])
isolate = sys.argv[6] == "1"
pin_vcpus = sys.argv[7] == "1"
if len(cpus) != queues or len(threads) != queues:
    raise SystemExit("IOThread count does not match virtqueue profile")
if isolate:
    first_tid = next(iter(threads.values()))
    status = open(f"/proc/{first_tid}/status").read().splitlines()
    tgid = next(line.split()[1] for line in status if line.startswith("Tgid:"))
    command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
               tools_image, "taskset", "-apc", "0-55,64-119", tgid]
    print(subprocess.check_output(command, text=True).strip())
if pin_vcpus:
    host_cpus = list(range(56)) + list(range(64, 120))
    if len(vcpus) > len(host_cpus):
        raise SystemExit("not enough non-IOThread host CPUs for vCPU pinning")
    for row, host_cpu in zip(vcpus, host_cpus):
        command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
                   tools_image, "taskset", "-pc", str(host_cpu), str(row["thread-id"])]
        print(subprocess.check_output(command, text=True).strip())
for queue, cpu in enumerate(cpus):
    tid = threads[f"iothread{queue}"]
    command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
               tools_image, "taskset", "-pc", str(cpu), str(tid)]
    print(subprocess.check_output(command, text=True).strip())
PY
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
    trap - RETURN
}

run_point() {
    local case_name=$1 device=$2 mode=$3 workload=$4 axis=$5 bs=$6 jobs=$7
    local per_file_mib=$8 expected_bytes=$9 disk_format=${10} fio_rw=${11}
    local prepare=${12} repeat=${13} runtime_seconds=${14} fsync_blocks=${15}
    local target point cfg source check_status=0
    target=$(target_for_device "$device")
    source=$(source_for_device "$device")
    point="$ROOT_ART/$case_name"
    if [[ -f $point/result.json ]]; then
        python3 "$SCRIPT_DIR/sweep.py" parse "$point" "$case_name" >/dev/null
        echo "SKIP validated $case_name"
        return
    fi
    [[ ! -e $point ]] || { echo "refusing incomplete point: $point" >&2; return 2; }
    mkdir "$point"
    cp --reflink=auto "$source/selected.iso" "$point/selected.iso"
    cp --reflink=auto "$source/ext2.img" "$point/ext2.img"
    cp --reflink=auto "$source/exfat.img" "$point/exfat.img"
    cfg="$point/case.cfg"
    printf 'CASE=%q\nDEVICE=%q\nMODE=%q\nWORKLOAD=%q\nAXIS=%q\nBS=%q\nJOBS=%q\nPER_FILE_MIB=%q\nEXPECTED_BYTES=%q\nPREPARE=%q\nREPEAT=%q\nRUNTIME_SECONDS=%q\nFSYNC_BLOCKS=%q\nPAGECACHE_CAPACITY_MB=%q\nDIRECT_IO=%q\n' \
        "$case_name" "$device" "$mode" "$workload" "$axis" "$bs" "$jobs" \
        "$per_file_mib" "$expected_bytes" "$prepare" "$repeat" "$runtime_seconds" \
        "$fsync_blocks" "$PAGECACHE_CAPACITY_MB" "$DIRECT_IO" >"$cfg"
    inject_guest "$point/ext2.img" "$cfg"
    preflight_target "$device"
    format_target "$target" "$disk_format" >"$point/mkfs.log" 2>&1
    echo "RUN $case_name target=$target rw=$fio_rw"
    run_guest "$point" "$target" "$case_name" "$device"
    docker run --rm --network none --device "$target:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target >"$point/e2fsck.log" 2>&1 || check_status=$?
    printf '%s\n' "$check_status" >"$point/e2fsck.status"
    [[ $check_status == 0 ]] || { echo "$case_name: e2fsck failed" >&2; return 2; }
    docker run --rm --network none --device "$target:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" debugfs -R 'ls -l /data' /dev/benchmark-target >"$point/layout.txt" 2>&1 || true
    python3 "$SCRIPT_DIR/sweep.py" parse "$point" "$case_name" >"$point/result.json"
    rm -f -- "$point/selected.iso" "$point/ext2.img" "$point/exfat.img" "$point/qmp-helper.py" "$point/qmp.sock"
    python3 - "$point/result.json" <<'PY'
import json, sys
row = json.load(open(sys.argv[1]))
print(f'{row["case"]}: {row["fio_mib_s"]:.2f} MiB/s mean={row["mean_latency_us"]:.2f} us')
PY
}

list_points() {
    if [[ -n ${CASES:-} ]]; then
        for case_name in $CASES; do python3 "$SCRIPT_DIR/sweep.py" config "$case_name"; done
    elif [[ ${MATRIX:-full} == reconfigured-thread ]]; then
        python3 "$SCRIPT_DIR/sweep.py" reconfigured-thread-list
    else
        python3 "$SCRIPT_DIR/sweep.py" list
    fi
}

while IFS=$'\t' read -r case_name device mode workload axis bs jobs per_file_mib expected_bytes disk_format fio_rw prepare repeat runtime_seconds fsync_blocks; do
    run_point "$case_name" "$device" "$mode" "$workload" "$axis" "$bs" "$jobs" \
        "$per_file_mib" "$expected_bytes" "$disk_format" "$fio_rw" "$prepare" "$repeat" \
        "$runtime_seconds" "$fsync_blocks"
done < <(list_points)

python3 "$SCRIPT_DIR/summarize.py" "$ROOT_ART"
