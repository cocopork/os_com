#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MQ_DIR=$(realpath "$SCRIPT_DIR/../rtio-virtio-multiqueue-20261102")
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing result directory}
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to selected.iso/ext2.img/exfat.img}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || {
    echo "ROOT_ART must be an existing caller-owned writable directory" >&2
    exit 2
}
for file in selected.iso ext2.img exfat.img; do
    [[ -f $SOURCE_ART/$file ]] || { echo "missing source artifact: $file" >&2; exit 2; }
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
    local point=$1 target=$2 case_name=$3
    local qemu_name="full-e2e-qemu-$$"
    local status
    cp -- "$MQ_DIR/summarize.py" "$point/qmp-helper.py"
    docker run -d --name "$qemu_name" --pid=host --network none \
        --device /dev/kvm --device "$target:/dev/benchmark-target:rwm" \
        -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 7200 qemu-system-x86_64 \
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
        -object iothread,id=iothread0 -object iothread,id=iothread1 \
        -object iothread,id=iothread2 -object iothread,id=iothread3 \
        >"$point/qemu-container-id.txt"
    trap 'docker rm -f "$qemu_name" >/dev/null 2>&1 || true' RETURN
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-add-device /benchmark/qmp.sock 4 \
        >"$point/qmp-device-add.json"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query /benchmark/qmp.sock \
        >"$point/qmp-iothreads.json"
    python3 - "$point/qmp-iothreads.json" "$TOOLS_IMAGE" <<'PY' >"$point/iothread-affinity.log"
import json, subprocess, sys
threads = {row["id"]: row["thread-id"] for row in json.load(open(sys.argv[1]))}
for queue, cpu in enumerate((60, 61, 62, 63)):
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
        echo "$case_name: QEMU exited with $status" >&2
        return 2
    }
    docker rm "$qemu_name" >/dev/null
    trap - RETURN
}

run_point() {
    local case_name=$1 device=$2 mode=$3 workload=$4 axis=$5 bs=$6 jobs=$7
    local per_file_mib=$8 expected_bytes=$9 disk_format=${10} fio_rw=${11}
    local prepare=${12} repeat=${13}
    local target point cfg check_status=0
    target=$(target_for_device "$device")
    point="$ROOT_ART/$case_name"
    if [[ -f $point/result.json ]]; then
        python3 "$SCRIPT_DIR/sweep.py" parse "$point" "$case_name" >/dev/null
        echo "SKIP validated $case_name"
        return
    fi
    [[ ! -e $point ]] || { echo "refusing incomplete point: $point" >&2; return 2; }
    mkdir "$point"
    cp --reflink=auto "$SOURCE_ART/selected.iso" "$point/selected.iso"
    cp --reflink=auto "$SOURCE_ART/ext2.img" "$point/ext2.img"
    cp --reflink=auto "$SOURCE_ART/exfat.img" "$point/exfat.img"
    cfg="$point/case.cfg"
    printf 'CASE=%q\nDEVICE=%q\nMODE=%q\nWORKLOAD=%q\nAXIS=%q\nBS=%q\nJOBS=%q\nPER_FILE_MIB=%q\nEXPECTED_BYTES=%q\nPREPARE=%q\nREPEAT=%q\n' \
        "$case_name" "$device" "$mode" "$workload" "$axis" "$bs" "$jobs" \
        "$per_file_mib" "$expected_bytes" "$prepare" "$repeat" >"$cfg"
    inject_guest "$point/ext2.img" "$cfg"
    preflight_target "$device"
    format_target "$target" "$disk_format" >"$point/mkfs.log" 2>&1
    echo "RUN $case_name target=$target rw=$fio_rw"
    run_guest "$point" "$target" "$case_name"
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
    else
        python3 "$SCRIPT_DIR/sweep.py" list
    fi
}

while IFS=$'\t' read -r case_name device mode workload axis bs jobs per_file_mib expected_bytes disk_format fio_rw prepare repeat; do
    run_point "$case_name" "$device" "$mode" "$workload" "$axis" "$bs" "$jobs" \
        "$per_file_mib" "$expected_bytes" "$disk_format" "$fio_rw" "$prepare" "$repeat"
done < <(list_points)

python3 "$SCRIPT_DIR/summarize.py" "$ROOT_ART"
