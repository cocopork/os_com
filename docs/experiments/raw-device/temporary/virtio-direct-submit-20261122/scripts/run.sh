#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing writable result directory}
SOURCE=${SOURCE:?set SOURCE to the three experiment ISOs plus ext2.img and exfat.img}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
TARGET=${TARGET:-/dev/nvme1n1p2}
PARENT=${PARENT:-/dev/nvme1n1}
IOTHREAD_CPUS=(56 57 58 59 60 61 62 63)

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || exit 2
for file in 8q-8w-staged.iso 8q-direct.iso ext2.img exfat.img; do
    [[ -f $SOURCE/$file ]] || { echo "missing source: $file" >&2; exit 2; }
done
[[ -b $TARGET && -b $PARENT ]] || exit 2
[[ $(lsblk -dnbo SIZE "$TARGET") == 274877906944 ]] || exit 2
[[ $(lsblk -dnro PKNAME "$TARGET") == "${PARENT#/dev/}" ]] || exit 2
! findmnt -rn -S "$TARGET" >/dev/null || { echo "target is mounted" >&2; exit 2; }
! findmnt -rn -S "$PARENT" >/dev/null || { echo "parent is mounted" >&2; exit 2; }
fuser_status=0
docker run --rm --privileged --pid=host --userns=host --network none -v /:/host:ro \
    "$TOOLS_IMAGE" chroot /host /usr/bin/fuser -s "$TARGET" "$PARENT" || fuser_status=$?
[[ $fuser_status == 1 ]] || { echo "target is in use" >&2; exit 2; }

exec 9>"$ROOT_ART/sweep.lock"
flock -n 9 || { echo "experiment already running" >&2; exit 2; }

inject_guest() {
    local image=$1 config=$2
    debugfs -w -R "rm /virtio-direct.cfg" "$image" >/dev/null 2>&1 || true
    debugfs -w -R "rm /rebuild-fio.sh" "$image" >/dev/null 2>&1 || true
    debugfs -w -R "write $config /virtio-direct.cfg" "$image" >/dev/null
    debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$image" >/dev/null
}

validate_result() {
    local result=$1 expected_bytes=$2
    python3 - "$result" "$expected_bytes" <<'PY'
import json, sys
rows = json.load(open(sys.argv[1]))
expected = int(sys.argv[2])
if [row["repeat"] for row in rows] != [1, 2, 3]:
    raise SystemExit("incomplete repetitions")
if any(row["io_bytes"] != expected for row in rows):
    raise SystemExit("short I/O")
PY
}

run_point() {
    local case_name=$1 mode=$2 jobs=$3 bs=$4 per_job_mib=$5 actual_mib=$6
    local expected_bytes=$7 repeats=$8 virtqueues=$9 iothreads=${10}
    local point="$ROOT_ART/$case_name" qemu_name="virtio-direct-${mode}-${jobs}-$$" status
    if [[ -f $point/result.json ]]; then
        validate_result "$point/result.json" "$expected_bytes"
        echo "SKIP validated $case_name"
        return
    fi
    [[ ! -e $point ]] || { echo "refusing existing point: $point" >&2; return 2; }
    mkdir "$point"
    cp --reflink=auto "$SOURCE/$mode.iso" "$point/selected.iso"
    cp --reflink=auto "$SOURCE/ext2.img" "$point/ext2.img"
    cp --reflink=auto "$SOURCE/exfat.img" "$point/exfat.img"
    cp "$SCRIPT_DIR/qmp_helper.py" "$point/qmp_helper.py"
    cat >"$point/case.cfg" <<EOF
CASE=$case_name
MODE=$mode
JOBS=$jobs
PER_JOB_MIB=$per_job_mib
ACTUAL_MIB=$actual_mib
EXPECTED_BYTES=$expected_bytes
VIRTQUEUES=$virtqueues
IOTHREADS=$iothreads
EOF
    inject_guest "$point/ext2.img" "$point/case.cfg"

    local -a iothread_objects=()
    for ((index = 0; index < iothreads; index++)); do
        iothread_objects+=( -object "iothread,id=iothread$index" )
    done
    docker run -d --name "$qemu_name" --pid=host --network none \
        --device /dev/kvm --device "$TARGET:/dev/benchmark-target:rwm" \
        -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        timeout --signal=TERM --kill-after=30 1800 qemu-system-x86_64 \
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
        "${iothread_objects[@]}" >"$point/container-id.txt"
    trap 'docker rm -f "$qemu_name" >/dev/null 2>&1 || true' RETURN
    docker exec "$qemu_name" python3 /benchmark/qmp_helper.py add \
        /benchmark/qmp.sock "$virtqueues" "$iothreads" >"$point/qmp-add.json"
    docker exec "$qemu_name" python3 /benchmark/qmp_helper.py query-iothreads \
        /benchmark/qmp.sock >"$point/qmp-iothreads.json"
    python3 - "$point/qmp-iothreads.json" "$TOOLS_IMAGE" "${IOTHREAD_CPUS[*]}" <<'PY' \
        >"$point/iothread-affinity.log"
import json, subprocess, sys
threads = {row["id"]: row["thread-id"] for row in json.load(open(sys.argv[1]))}
cpus = [int(cpu) for cpu in sys.argv[3].split()]
if len(threads) != 8:
    raise SystemExit("expected eight QEMU IOThreads")
for index, cpu in enumerate(cpus):
    command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
               sys.argv[2], "taskset", "-pc", str(cpu), str(threads[f"iothread{index}"])]
    print(subprocess.check_output(command, text=True).strip())
PY
    docker exec "$qemu_name" python3 /benchmark/qmp_helper.py cont /benchmark/qmp.sock \
        >"$point/qmp-cont.json"
    docker wait "$qemu_name" >"$point/qemu-exit.txt"
    docker logs "$qemu_name" >"$point/qemu.log" 2>&1
    status=$(<"$point/qemu-exit.txt")
    [[ $status == 33 ]] || { echo "QEMU exit $status" >&2; return 2; }
    python3 "$SCRIPT_DIR/results.py" "$point/qemu.log" "$case_name" "$expected_bytes" \
        >"$point/result.json"
    docker rm "$qemu_name" >/dev/null
    trap - RETURN
    rm -f "$point/selected.iso" "$point/ext2.img" "$point/exfat.img" "$point/qmp.sock"
    python3 - "$point/result.json" <<'PY'
import json, statistics, sys
rows = json.load(open(sys.argv[1]))
values = [row["fio_mib_s"] for row in rows]
print(f'{rows[0]["case"]}: mean={statistics.fmean(values):.2f} MiB/s values={values}')
PY
}

while IFS=$'\t' read -r case_name mode jobs bs per_job_mib actual_mib expected_bytes repeats virtqueues iothreads; do
    run_point "$case_name" "$mode" "$jobs" "$bs" "$per_job_mib" "$actual_mib" \
        "$expected_bytes" "$repeats" "$virtqueues" "$iothreads"
done < <(python3 "$SCRIPT_DIR/matrix.py" list)
