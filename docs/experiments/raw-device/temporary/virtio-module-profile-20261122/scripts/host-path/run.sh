#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing writable result directory}
SOURCE=${SOURCE:?set SOURCE to the three experiment ISOs plus ext2.img and exfat.img}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
QEMU_PROFILE_BINARY=${QEMU_PROFILE_BINARY:-/home/zn/asterinas-build-env/qemu-10.2.1-profile-bin/qemu-system-x86_64}
TARGET=${TARGET:-/dev/nvme1n1p2}
PARENT=${PARENT:-/dev/nvme1n1}
IOTHREAD_CPUS=(56 57 58 59 60 61 62 63)

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || exit 2
for file in 8q-module-profile.iso ext2.img exfat.img; do
    [[ -f $SOURCE/$file ]] || { echo "missing source: $file" >&2; exit 2; }
done
[[ -b $TARGET && -b $PARENT ]] || exit 2
[[ -x $QEMU_PROFILE_BINARY ]] || { echo "missing QEMU profile binary" >&2; exit 2; }
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
if [row["repeat"] for row in rows] != [1]:
    raise SystemExit("incomplete repetitions")
if any(row["io_bytes"] != expected for row in rows):
    raise SystemExit("short I/O")
PY
}

run_point() {
    local case_name=$1 mode=$2 jobs=$3 bs=$4 per_job_mib=$5 actual_mib=$6
    local expected_bytes=$7 repeats=$8 virtqueues=$9 iothreads=${10}
    local point="$ROOT_ART/$case_name" qemu_name="virtio-direct-${mode}-${jobs}-$$" status
    local bpf_name="virtio-host-bpf-${jobs}-$$"
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
    cp "$SCRIPT_DIR/host-path.bt" "$point/host-path.bt"
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
        -v "$QEMU_PROFILE_BINARY:/usr/local/qemu/bin/qemu-system-x86_64:ro" \
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
    trap 'docker rm -f "$bpf_name" "$qemu_name" >/dev/null 2>&1 || true' RETURN
    docker exec "$qemu_name" python3 /benchmark/qmp_helper.py add \
        /benchmark/qmp.sock "$virtqueues" "$iothreads" >"$point/qmp-add.json"
    docker exec "$qemu_name" python3 /benchmark/qmp_helper.py query-iothreads \
        /benchmark/qmp.sock >"$point/qmp-iothreads.json"
    docker exec "$qemu_name" python3 /benchmark/qmp_helper.py query-cpus-fast \
        /benchmark/qmp.sock >"$point/qmp-cpus.json"
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
    local host_pid target_fd target_dev
    local -a iothread_tids vcpu_tids
    local container_pid
    container_pid=$(docker inspect -f '{{.State.Pid}}' "$qemu_name")
    host_pid=$(pgrep -P "$container_pid")
    [[ $host_pid =~ ^[0-9]+$ ]] || { echo "QEMU pid not found" >&2; return 2; }
    sed -i "s|QEMU_BINARY|$QEMU_PROFILE_BINARY|g" \
        "$point/host-path.bt"
    target_fd=$(docker run --rm --privileged --pid=host --network none -v /:/host:ro \
        "$TOOLS_IMAGE" chroot /host /bin/sh -c \
        'for fd in /proc/'"$host_pid"'/fd/*; do [ "$(readlink "$fd")" = /dev/benchmark-target ] && basename "$fd"; done; true')
    [[ $target_fd =~ ^[0-9]+$ ]] || { echo "target fd not found" >&2; return 2; }
    target_dev=$(( (259 << 20) | 2 ))
    mapfile -t iothread_tids < <(python3 - "$point/qmp-iothreads.json" <<'PY'
import json, sys
rows = sorted(json.load(open(sys.argv[1])), key=lambda row: row["id"])
for row in rows:
    print(row["thread-id"])
PY
    )
    [[ ${#iothread_tids[@]} == 8 ]] || { echo "IOThread tids not found" >&2; return 2; }
    mapfile -t vcpu_tids < <(python3 - "$point/qmp-cpus.json" <<'PY'
import json, sys
rows = sorted(json.load(open(sys.argv[1])), key=lambda row: row["cpu-index"])
for row in rows:
    print(row["thread-id"])
PY
    )
    [[ ${#vcpu_tids[@]} == 64 ]] || { echo "vCPU tids not found" >&2; return 2; }
    docker run -d --name "$bpf_name" --privileged --pid=host --network none \
        -v /:/host:ro "$TOOLS_IMAGE" chroot /host /usr/bin/bpftrace \
        "$point/host-path.bt" "$host_pid" "$target_fd" "$target_dev" \
        "${iothread_tids[@]}" "${vcpu_tids[0]}" "${vcpu_tids[63]}" \
        >"$point/bpf-container-id.txt"
    for _ in $(seq 1 100); do
        if docker logs "$bpf_name" 2>&1 | rg -q 'Attaching [0-9]+ probes'; then
            break
        fi
        sleep 0.05
    done
    docker logs "$bpf_name" 2>&1 | rg -q 'Attaching [0-9]+ probes' || {
        docker logs "$bpf_name" >&2
        return 2
    }
    [[ $(docker inspect -f '{{.State.Running}}' "$bpf_name") == true ]] || {
        docker logs "$bpf_name" >&2
        return 2
    }
    docker exec "$qemu_name" python3 /benchmark/qmp_helper.py cont /benchmark/qmp.sock \
        >"$point/qmp-cont.json"
    docker wait "$qemu_name" >"$point/qemu-exit.txt"
    docker stop --signal=INT --time=10 "$bpf_name" >/dev/null
    docker logs "$bpf_name" >"$point/host-path-bpf.log" 2>&1
    docker logs "$qemu_name" >"$point/qemu.log" 2>&1
    status=$(<"$point/qemu-exit.txt")
    [[ $status == 33 ]] || { echo "QEMU exit $status" >&2; return 2; }
    python3 "$SCRIPT_DIR/results.py" "$point/qemu.log" "$case_name" "$expected_bytes" \
        >"$point/result.json"
    if rg -q '\[VIRTIO_PROFILE\]' "$point/qemu.log"; then
        python3 "$SCRIPT_DIR/results.py" profile "$point/qemu.log" "$case_name" \
            >"$point/profile.json"
    fi
    if rg -q '\[(RAW_BLOCK_PROFILE|BLOCK_MODULE_PROFILE)\]' "$point/qemu.log"; then
        python3 "$SCRIPT_DIR/results.py" module "$point/qemu.log" "$case_name" \
            >"$point/module.json"
    fi
    docker rm "$bpf_name" "$qemu_name" >/dev/null
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
    if [[ -n ${ONLY_JOBS:-} && $jobs != "$ONLY_JOBS" ]]; then
        continue
    fi
    run_point "$case_name" "$mode" "$jobs" "$bs" "$per_job_mib" "$actual_mib" \
        "$expected_bytes" "$repeats" "$virtqueues" "$iothreads"
done < <(python3 "$SCRIPT_DIR/matrix.py" list)
