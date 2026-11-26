#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MQ_HELPER=$(realpath "$SCRIPT_DIR/../rtio-virtio-multiqueue-20261102/summarize.py")
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing writable result directory}
SOURCE_NVME=${SOURCE_NVME:?set SOURCE_NVME to the four-worker selected.iso directory}
SOURCE_SATA=${SOURCE_SATA:?set SOURCE_SATA to the one-worker selected.iso directory}
FILEBENCH=${FILEBENCH:-$SCRIPT_DIR/../../../os_com_implementation/test/filebench-test/bin/filebench}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || { echo "invalid ROOT_ART" >&2; exit 2; }
[[ -x $FILEBENCH ]] || { echo "missing Filebench binary: $FILEBENCH" >&2; exit 2; }
exec 9>"$ROOT_ART/matrix.lock"
flock -n 9 || { echo "Filebench matrix is already running" >&2; exit 2; }

target_for() { [[ $1 == nvme ]] && echo /dev/nvme1n1p2 || echo /dev/sda4; }
parent_for() { [[ $1 == nvme ]] && echo /dev/nvme1n1 || echo /dev/sda; }
size_for() { [[ $1 == nvme ]] && echo 274877906944 || echo 274877890560; }
queues_for() { [[ $1 == nvme ]] && echo 4 || echo 1; }
source_for() { [[ $1 == nvme ]] && echo "$SOURCE_NVME" || echo "$SOURCE_SATA"; }

preflight() {
    local device=$1 target parent expected fuser_status=0
    target=$(target_for "$device"); parent=$(parent_for "$device"); expected=$(size_for "$device")
    [[ -b $target && -b $parent && $(realpath "$target") == "$target" ]] || return 2
    [[ $(lsblk -dnbo SIZE "$target") == "$expected" ]] || return 2
    [[ $(lsblk -dnro PKNAME "$target") == "${parent#/dev/}" ]] || return 2
    ! findmnt -rn -S "$target" >/dev/null || { echo "$target is mounted" >&2; return 2; }
    ! findmnt -rn -S "$parent" >/dev/null || { echo "$parent is mounted" >&2; return 2; }
    docker run --rm --privileged --pid=host --userns=host --network none -v /:/host:ro \
        "$TOOLS_IMAGE" chroot /host /usr/bin/fuser -s "$target" "$parent" || fuser_status=$?
    [[ $fuser_status == 1 ]] || { echo "$target or $parent is in use" >&2; return 2; }
}

format_target() {
    local target=$1 mode=$2
    local args=(-F -q -t ext4 -b 4096 -I 256 -E nodiscard,lazy_itable_init=0,lazy_journal_init=0)
    [[ $mode == ext4 ]] || args+=(-O '^has_journal')
    docker run --rm --network none --device "$target:/dev/benchmark-target:rwm" \
        "$TOOLS_IMAGE" mkfs.ext4 "${args[@]}" /dev/benchmark-target
}

inject_guest() {
    local image=$1 cfg=$2
    for name in filebench.cfg rebuild-fio.sh filebench; do
        debugfs -w -R "rm /$name" "$image" >/dev/null 2>&1 || true
    done
    debugfs -w -R "write $cfg /filebench.cfg" "$image" >/dev/null
    debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$image" >/dev/null
    debugfs -w -R "write $FILEBENCH /filebench" "$image" >/dev/null
}

run_guest() {
    local point=$1 target=$2 device=$3 case_name=$4
    local queues qemu_name status cpu
    queues=$(queues_for "$device")
    qemu_name="filebench-${device}-$$"
    cp -- "$MQ_HELPER" "$point/qmp-helper.py"
    local iothreads=()
    for ((queue=0; queue<queues; queue++)); do iothreads+=(-object "iothread,id=iothread$queue"); done
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
        "${iothreads[@]}" >"$point/qemu-container-id.txt"
    trap 'docker rm -f "$qemu_name" >/dev/null 2>&1 || true' RETURN
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-add-device /benchmark/qmp.sock "$queues" \
        >"$point/qmp-device-add.json"
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query /benchmark/qmp.sock \
        >"$point/qmp-iothreads.json"
    python3 - "$point/qmp-iothreads.json" "$TOOLS_IMAGE" "$queues" <<'PY' >"$point/iothread-affinity.log"
import json, subprocess, sys
threads = {row["id"]: row["thread-id"] for row in json.load(open(sys.argv[1]))}
queues = int(sys.argv[3])
cpus = (60, 61, 62, 63) if queues == 4 else (63,)
for queue, cpu in enumerate(cpus):
    command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
               sys.argv[2], "taskset", "-pc", str(cpu), str(threads[f"iothread{queue}"])]
    print(subprocess.check_output(command, text=True).strip())
PY
    docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-cont /benchmark/qmp.sock \
        >"$point/qmp-cont.json"
    docker wait "$qemu_name" >"$point/qemu-exit.txt"
    docker logs "$qemu_name" >"$point/qemu.log" 2>&1
    docker inspect "$qemu_name" >"$point/qemu.inspect.json"
    status=$(<"$point/qemu-exit.txt")
    [[ $status == 0 || $status == 33 ]] || { echo "$case_name: QEMU status $status" >&2; return 2; }
    docker rm "$qemu_name" >/dev/null
    trap - RETURN
}

run_point() {
    local device=$1 mode=$2 workload=$3 repeat=$4
    local case_name="${device}-${mode}-${workload}-r${repeat}"
    local point="$ROOT_ART/$case_name" target source cfg check=0
    point="$ROOT_ART/$case_name"; target=$(target_for "$device"); source=$(source_for "$device")
    if [[ -f $point/result.json ]]; then echo "SKIP $case_name"; return; fi
    [[ ! -e $point ]] || { echo "incomplete point exists: $point" >&2; return 2; }
    for file in selected.iso ext2.img exfat.img; do [[ -f $source/$file ]] || return 2; done
    mkdir "$point"
    cp --reflink=auto "$source/selected.iso" "$point/selected.iso"
    cp --reflink=auto "$source/ext2.img" "$point/ext2.img"
    cp --reflink=auto "$source/exfat.img" "$point/exfat.img"
    cfg="$point/filebench.cfg"
    printf 'CASE=%q\nDEVICE=%q\nMODE=%q\nWORKLOAD=%q\nRUNTIME=%q\nNFILES=%q\n' \
        "$case_name" "$device" "$mode" "$workload" "${TEST_RUNTIME:-60}" "${TEST_NFILES:-10000}" >"$cfg"
    python3 - "$point/case.json" "$case_name" "$device" "$mode" "$workload" "$repeat" <<'PY'
import json, sys
path, case, device, mode, workload, repeat = sys.argv[1:]
row = dict(case=case, device=device, mode=mode, workload=workload, repeat=int(repeat),
           threads=16, nfiles=10000, filesize_kib=128, dataset_mib=1250,
           dram_cache_mib=128 if mode == "ext4" else 64,
           nv_data_cache_mib=0 if mode == "ext4" else 64)
open(path, "w").write(json.dumps(row, indent=2) + "\n")
PY
    inject_guest "$point/ext2.img" "$cfg"
    preflight "$device"
    format_target "$target" "$mode" >"$point/mkfs.log" 2>&1
    echo "RUN $case_name"
    run_guest "$point" "$target" "$device" "$case_name"
    docker run --rm --network none --device "$target:/dev/benchmark-target:r" \
        "$TOOLS_IMAGE" e2fsck -fn /dev/benchmark-target >"$point/e2fsck.log" 2>&1 || check=$?
    printf '%s\n' "$check" >"$point/e2fsck.status"
    [[ $check == 0 ]] || return 2
    python3 "$SCRIPT_DIR/parse.py" "$ROOT_ART" --case "$case_name"
    rm -f "$point/selected.iso" "$point/ext2.img" "$point/exfat.img" \
        "$point/qmp-helper.py" "$point/qmp.sock"
}

if [[ -n ${CASES:-} ]]; then
    for case_name in $CASES; do
        device=${case_name%%-*}
        rest=${case_name#*-}; mode=${rest%%-*}; rest=${rest#*-}
        repeat=${rest##*-r}; workload=${rest%-r*}
        run_point "$device" "$mode" "$workload" "$repeat"
    done
else
    for device in nvme sata; do
        for mode in ext4 rnvpce; do
            for workload in fileserver varmail webserver-10r1w webserver-1r1w; do
                for repeat in 1 2 3; do run_point "$device" "$mode" "$workload" "$repeat"; done
            done
        done
    done
fi

python3 "$SCRIPT_DIR/parse.py" "$ROOT_ART"
python3 "$SCRIPT_DIR/summarize.py" "$ROOT_ART" | tee "$ROOT_ART/summary.txt"
python3 "$SCRIPT_DIR/plot.py" "$ROOT_ART"
