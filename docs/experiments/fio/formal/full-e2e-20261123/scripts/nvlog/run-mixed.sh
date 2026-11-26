#!/bin/bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
BUILD_ROOT=${BUILD_ROOT:-/home/zn/asterinas-build-env/nvlog-comparison-20261120/build}
RESULT_ROOT=${RESULT_ROOT:-/home/zn/asterinas-build-env/nvlog-comparison-20261120/mixed-sync}

mkdir -p "$RESULT_ROOT"
exec 9>"$RESULT_ROOT/sweep.lock"
flock -n 9 || { echo "NVLog mixed-sync sweep already running" >&2; exit 2; }

target_for() { [[ $1 == nvme ]] && echo /dev/nvme1n1p2 || echo /dev/sda4; }
parent_for() { [[ $1 == nvme ]] && echo /dev/nvme1n1 || echo /dev/sda; }
size_for() { [[ $1 == nvme ]] && echo 274877906944 || echo 274877890560; }

preflight() {
    local device=$1 target parent expected actual fuser_status=0
    target=$(target_for "$device"); parent=$(parent_for "$device"); expected=$(size_for "$device")
    [[ -b $target && -b $parent ]] || return 2
    [[ $(realpath "$target") == "$target" && $(realpath "$parent") == "$parent" ]] || return 2
    actual=$(lsblk -dnbo SIZE "$target")
    [[ $actual == "$expected" ]] || { echo "unexpected target size: $target $actual" >&2; return 2; }
    [[ $(lsblk -dnro PKNAME "$target") == "${parent#/dev/}" ]] || return 2
    [[ -z $(lsblk -dnro MOUNTPOINTS "$target") && -z $(lsblk -dnro MOUNTPOINTS "$parent") ]] || return 2
    [[ -d /sys/class/block/${target#/dev/}/holders && -z $(ls -A "/sys/class/block/${target#/dev/}/holders") ]] || return 2
    ! awk -v target="$target" '$1 == target { found=1 } END { exit !found }' /proc/swaps || return 2
    timeout --signal=TERM --kill-after=5 60 docker run --rm --privileged --pid=host \
        --userns=host --network none -v /:/host:ro "$TOOLS_IMAGE" \
        chroot /host /usr/bin/fuser -s "$target" "$parent" || fuser_status=$?
    [[ $fuser_status == 1 ]] || { echo "target or parent in use: $target" >&2; return 2; }
}

if [[ -n ${CASES:-} ]]; then
    point_rows=
    for case_name in $CASES; do
        point_rows+=$(python3 "$SCRIPT_DIR/mixed_matrix.py" config "$case_name")$'\n'
    done
else
    point_rows=$(python3 "$SCRIPT_DIR/mixed_matrix.py" list)
fi

while IFS=$'\t' read -r case_name device read_percent sync_percent repeat threads file_mib \
    file_bytes dataset_mib seed warmup runtime virtqueues dram_mib nv_mib; do
    [[ -n $case_name ]] || continue
    target=$(target_for "$device")
    point="$RESULT_ROOT/$case_name"
    if [[ -f $point/result.json ]] && python3 - "$point/result.json" <<'PY'
import json, sys
result = json.load(open(sys.argv[1]))
assert result.get("parse_valid") is True
assert all(result.get(k) == 0 for k in ("benchmark_exit", "sync_exit", "unmount_exit", "qemu_exit", "e2fsck_exit"))
PY
    then
        echo "SKIP complete $case_name"
        continue
    fi
    [[ ! -e $point ]] || { echo "incomplete point retained: $point" >&2; exit 2; }
    preflight "$device"
    mkdir -p "$point"
    truncate -s 258M "$point/nvdimm.img"

    qemu_status=0
    timeout --signal=TERM --kill-after=30 1200 docker run --rm --name "nvlog-mixed-$$" \
        --network none --device /dev/kvm --device "$target:/dev/benchmark-target:rwm" \
        -v "$BUILD_ROOT:/source:ro" -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
        qemu-system-x86_64 \
            -enable-kvm -cpu host -machine q35,accel=kvm,nvdimm=on \
            -smp 64 -m 8G,slots=4,maxmem=16G \
            -kernel /source/bzImage \
            -append "root=/dev/vda rw init=/opt/guest-init.sh console=ttyS0 panic=-1 nvlog_workload=mixed nvpc_pages=32768 mixed_case=$case_name mixed_device=$device read_percent=$read_percent sync_percent=$sync_percent repeat=$repeat" \
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

    python3 - "$point" "$case_name" "$device" "$read_percent" "$sync_percent" "$repeat" \
        "$qemu_status" "$e2fsck_status" <<'PY'
import json, pathlib, re, sys

point = pathlib.Path(sys.argv[1])
case, device = sys.argv[2:4]
read_percent, sync_percent, repeat, qemu_exit, e2fsck_exit = map(int, sys.argv[4:])
raw = point.joinpath("qemu.log").read_text(errors="replace").replace("\r", "")
begin = f"MIXED_SYNC_JSON_BEGIN case={case}\n"
end = f"MIXED_SYNC_JSON_END case={case}"
payload = {}
if begin in raw and end in raw:
    try:
        payload = json.loads(raw.split(begin, 1)[1].split(end, 1)[0].strip())
    except json.JSONDecodeError:
        payload = {}
complete = re.search(rf"^MIXED_SYNC_COMPLETE case={re.escape(case)} (.*)$", raw, re.M)
fields = dict(re.findall(r"([a-z_]+)=([^\s]+)", complete.group(1))) if complete else {}
benchmark_exit = int(fields.get("benchmark_status", 1))
sync_exit = int(fields.get("sync_status", 1))
unmount_exit = int(fields.get("umount_status", 1))
samples = []
for line in raw.splitlines():
    if line.startswith("MIXED_SYNC_CACHE_SAMPLE "):
        samples.append({k: int(v) for k, v in re.findall(r"(anon|file|file_dirty|file_writeback)=(\d+)", line)})
memory_events = {
    key.lower(): int(value)
    for key, value in re.findall(r"^MIXED_SYNC_MEMORY_EVENT_([A-Z_]+)=(\d+)$", raw, re.M)
}
parse_valid = (
    qemu_exit == 0 and e2fsck_exit == 0 and benchmark_exit == 0 and sync_exit == 0
    and unmount_exit == 0 and "MIXED_SYNC_PASS" in raw
    and "NVPC started with 32768 pages" in raw and payload.get("threads") == 8
    and memory_events.get("oom_kill", 0) == 0
)
result = {
    "case": case, "device": device, "mode": "nvlog", "read_percent": read_percent,
    "write_percent": 100-read_percent, "sync_percent": sync_percent, "repeat": repeat,
    "configured_dram_cache_mib": 128, "configured_nv_data_cache_mib": 128,
    "configured_total_data_cache_mib": 256, "cache_control": "memcg-approx",
    "memory_high_mib": 136, "memory_max_mib": 512, "memory_events": memory_events,
    "cache_file_peak_bytes": max((s.get("file", 0) for s in samples), default=0),
    "cache_anon_peak_bytes": max((s.get("anon", 0) for s in samples), default=0),
    "benchmark_exit": benchmark_exit, "sync_exit": sync_exit, "unmount_exit": unmount_exit,
    "qemu_exit": qemu_exit, "e2fsck_exit": e2fsck_exit, "parse_valid": parse_valid,
    **payload,
}
point.joinpath("cache-samples.json").write_text(json.dumps(samples, indent=2) + "\n")
point.joinpath("result.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
if not parse_valid:
    raise SystemExit(f"invalid point: {case}")
PY
    preflight "$device"
    # The NVDIMM image is runtime state, not benchmark evidence.
    truncate -s 0 "$point/nvdimm.img"
    echo "PASS $case_name"
done <<< "$point_rows"
