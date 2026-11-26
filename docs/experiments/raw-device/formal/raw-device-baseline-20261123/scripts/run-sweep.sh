#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MQ_DIR=$(realpath "$SCRIPT_DIR/../rtio-virtio-multiqueue-20261102")
source "$SCRIPT_DIR/../queue-profile.sh"

ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing result directory}
ASTER_SOURCE=${ASTER_SOURCE:?set ASTER_SOURCE to selected.iso/ext2.img/exfat.img}
LINUX_SOURCE=${LINUX_SOURCE:?set LINUX_SOURCE to vmlinuz/initramfs.cpio.gz/ext2.img/exfat.img}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
QUEUE_PROFILE=device-aligned

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || {
    echo "ROOT_ART must be an existing caller-owned writable directory" >&2
    exit 2
}
for file in selected.iso ext2.img exfat.img; do
    [[ -f $ASTER_SOURCE/$file ]] || { echo "missing Asterinas input: $file" >&2; exit 2; }
done
for file in vmlinuz initramfs.cpio.gz ext2.img exfat.img; do
    [[ -f $LINUX_SOURCE/$file ]] || { echo "missing Linux input: $file" >&2; exit 2; }
done

exec 9>"$ROOT_ART/sweep.lock"
flock -n 9 || { echo "raw-device baseline sweep already running" >&2; exit 2; }

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
    local device=$1 target parent expected_size fuser_status=0
    target=$(target_for_device "$device")
    parent=$(parent_for_device "$device")
    expected_size=$(size_for_device "$device")
    [[ -b $target && -b $parent ]] || { echo "missing target: $target" >&2; return 2; }
    [[ $(lsblk -dnbo SIZE "$target") == "$expected_size" ]] || return 2
    [[ $(lsblk -dnro PKNAME "$target") == "${parent#/dev/}" ]] || return 2
    ! findmnt -rn -S "$target" >/dev/null || { echo "target mounted: $target" >&2; return 2; }
    ! findmnt -rn -S "$parent" >/dev/null || { echo "parent mounted: $parent" >&2; return 2; }
    docker run --rm --privileged --pid=host --userns=host --network none -v /:/host:ro \
        "$TOOLS_IMAGE" chroot /host /usr/bin/fuser -s "$target" "$parent" || fuser_status=$?
    [[ $fuser_status == 1 ]] || { echo "target or parent in use: $target" >&2; return 2; }
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
    local point=$1 target=$2 system=$3 device=$4
    local qemu_name="raw-baseline-qemu-$$" status virtqueues queue
    local -a boot_args=() network_args=() iothread_objects=() iothread_cpus=()
    virtqueues=$(virtqueues_for_device "$QUEUE_PROFILE" "$device")
    read -r -a iothread_cpus <<< "$(iothread_cpus_for_device "$QUEUE_PROFILE" "$device")"
    [[ ${#iothread_cpus[@]} -eq $virtqueues ]] || return 2
    for ((queue = 0; queue < virtqueues; queue++)); do
        iothread_objects+=( -object "iothread,id=iothread$queue" )
    done
    if [[ $system == asterinas ]]; then
        boot_args=( -cdrom /benchmark/selected.iso )
        network_args=( -nic none )
    else
        boot_args=(
            -kernel /benchmark/vmlinuz
            -initrd /benchmark/initramfs.cpio.gz
            -append "console=ttyS0 rdinit=/benchmark/common/bench_runner.sh fio/raw_device_sweep linux"
        )
        network_args=(
            -netdev user,id=net01
            -device virtio-net-pci,netdev=net01,disable-legacy=on,disable-modern=off
        )
    fi

    cp -- "$MQ_DIR/summarize.py" "$point/qmp-helper.py"
    docker run -d --name "$qemu_name" --pid=host --network none \
        --device /dev/kvm --device "$target:/dev/benchmark-target:rwm" \
        -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 7200 qemu-system-x86_64 \
        -enable-kvm -cpu host -machine q35 -smp 64 -m 8G -S \
        -qmp unix:/benchmark/qmp.sock,server=on,wait=off \
        -bios /root/ovmf/release/OVMF.fd -no-reboot -display none -serial stdio \
        -monitor none "${network_args[@]}" -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        "${boot_args[@]}" \
        -drive if=none,format=raw,id=bootdata,file=/benchmark/ext2.img \
        -device virtio-blk-pci,drive=bootdata,disable-legacy=on \
        -drive if=none,format=raw,id=aux,file=/benchmark/exfat.img \
        -device virtio-blk-pci,drive=aux,disable-legacy=on \
        -drive if=none,format=raw,id=target,file=/dev/benchmark-target,cache=none,aio=threads \
        -device pcie-root-port,id=target-port,chassis=1,slot=1 \
        "${iothread_objects[@]}" >"$point/qemu-container-id.txt"
    trap 'docker rm -f "$qemu_name" >/dev/null 2>&1 || true' RETURN
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-add-device /benchmark/qmp.sock "$virtqueues" \
        >"$point/qmp-device-add.json"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query /benchmark/qmp.sock \
        >"$point/qmp-iothreads.json"
    python3 - "$point/qmp-iothreads.json" "$TOOLS_IMAGE" "${iothread_cpus[*]}" "$virtqueues" <<'PY' >"$point/iothread-affinity.log"
import json
import subprocess
import sys

threads = {row["id"]: row["thread-id"] for row in json.load(open(sys.argv[1]))}
cpus = [int(cpu) for cpu in sys.argv[3].split()]
queues = int(sys.argv[4])
if len(cpus) != queues or len(threads) != queues:
    raise SystemExit("IOThread count does not match virtqueue profile")
for queue, cpu in enumerate(cpus):
    tid = threads[f"iothread{queue}"]
    command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
               sys.argv[2], "taskset", "-pc", str(cpu), str(tid)]
    print(subprocess.check_output(command, text=True).strip())
PY
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-cont /benchmark/qmp.sock \
        >"$point/qmp-cont.json"
    docker wait "$qemu_name" >"$point/qemu-exit.txt"
    docker logs "$qemu_name" >"$point/qemu.log" 2>&1
    docker inspect "$qemu_name" >"$point/qemu.inspect.json"
    status=$(<"$point/qemu-exit.txt")
    [[ $status == 0 || $status == 33 ]] || {
        echo "QEMU exited with $status" >&2
        return 2
    }
    docker rm "$qemu_name" >/dev/null
    trap - RETURN
}

run_point() {
    local case_name=$1 device=$2 system=$3 workload=$4 axis=$5 bs=$6 jobs=$7
    local per_job_mib=$8 actual_mib=$9 expected_bytes=${10} fio_rw=${11} repeats=${12} virtqueues=${13}
    local point target cfg source
    point="$ROOT_ART/$case_name"
    if [[ -f $point/result.json ]]; then
        python3 "$SCRIPT_DIR/results.py" "$point" "$case_name" >/dev/null
        echo "SKIP validated $case_name"
        return
    fi
    [[ ! -e $point ]] || { echo "refusing incomplete point: $point" >&2; return 2; }
    mkdir "$point"
    if [[ $system == asterinas ]]; then
        source=$ASTER_SOURCE
        cp --reflink=auto "$source/selected.iso" "$point/selected.iso"
    else
        source=$LINUX_SOURCE
        cp --reflink=auto "$source/vmlinuz" "$point/vmlinuz"
        cp --reflink=auto "$source/initramfs.cpio.gz" "$point/initramfs.cpio.gz"
    fi
    cp --reflink=auto "$source/ext2.img" "$point/ext2.img"
    cp --reflink=auto "$source/exfat.img" "$point/exfat.img"
    cfg="$point/case.cfg"
    printf 'CASE=%q\nDEVICE=%q\nSYSTEM=%q\nWORKLOAD=%q\nAXIS=%q\nBS=%q\nJOBS=%q\nTOTAL_MIB=2048\nPER_JOB_MIB=%q\nACTUAL_MIB=%q\nEXPECTED_BYTES=%q\nFIO_RW=%q\nREPEATS=%q\nVIRTQUEUES=%q\n' \
        "$case_name" "$device" "$system" "$workload" "$axis" "$bs" "$jobs" \
        "$per_job_mib" "$actual_mib" "$expected_bytes" "$fio_rw" "$repeats" "$virtqueues" >"$cfg"
    inject_guest "$point/ext2.img" "$cfg"
    target=$(target_for_device "$device")
    preflight_target "$device"
    echo "RUN $case_name target=$target rw=$fio_rw repeats=$repeats"
    run_guest "$point" "$target" "$system" "$device"
    python3 "$SCRIPT_DIR/results.py" "$point" "$case_name" >"$point/result.json"
    rm -f -- "$point/selected.iso" "$point/vmlinuz" "$point/initramfs.cpio.gz" \
        "$point/ext2.img" "$point/exfat.img" "$point/qmp-helper.py" "$point/qmp.sock"
    python3 - "$point/result.json" <<'PY'
import json
import statistics
import sys

rows = json.load(open(sys.argv[1]))
values = [row["fio_mib_s"] for row in rows]
print(f'{rows[0]["case"]}: mean={statistics.fmean(values):.2f} MiB/s values={values}')
PY
}

list_points() {
    if [[ -n ${CASES:-} ]]; then
        for case_name in $CASES; do python3 "$SCRIPT_DIR/matrix.py" config "$case_name"; done
    else
        python3 "$SCRIPT_DIR/matrix.py" list
    fi
}

while IFS=$'\t' read -r case_name device system workload axis bs jobs per_job_mib actual_mib expected_bytes fio_rw repeats virtqueues; do
    run_point "$case_name" "$device" "$system" "$workload" "$axis" "$bs" "$jobs" \
        "$per_job_mib" "$actual_mib" "$expected_bytes" "$fio_rw" "$repeats" "$virtqueues"
done < <(list_points)

