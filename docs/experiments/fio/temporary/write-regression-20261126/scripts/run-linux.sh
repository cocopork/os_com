#!/bin/bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(realpath "$SCRIPT_DIR/../../../../../..")
QMP_HELPER=$(realpath "$REPO_ROOT/docs/experiments/validation/temporary/rtio-virtio-multiqueue-20261102/scripts/summarize.py")
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
BUILD_ROOT=${BUILD_ROOT:-/home/zn/asterinas-build-env/full-e2e-nvlog-sync-write-20261125/build}
RESULT_ROOT=${RESULT_ROOT:-/home/zn/asterinas-build-env/write-regression-20261126/linux}
TARGET=/dev/nvme1n1p2
EXPECTED_SIZE=274877906944
QEMU_SMP=${QEMU_SMP:-40}
PIN_VCPUS=${PIN_VCPUS:-1}
SPECS=${SPECS:-'overwrite-randwrite:4k:1:2048 overwrite-randwrite:4k:16:128 overwrite-randwrite:4k:32:64 append-write:128k:1:2048 append-write:128k:16:128'}
REPEATS=${REPEATS:-'1 2 3'}

mkdir -p "$RESULT_ROOT"
exec 9>"$RESULT_ROOT/sweep.lock"
flock -n 9 || { echo "Linux write-regression sweep already running" >&2; exit 2; }

preflight() {
    local fuser_status=0
    [[ -b $TARGET && $(realpath "$TARGET") == "$TARGET" ]] || return 2
    [[ $(lsblk -dnbo SIZE "$TARGET") == "$EXPECTED_SIZE" ]] || return 2
    [[ -z $(lsblk -dnro MOUNTPOINTS "$TARGET") ]] || return 2
    [[ -z $(ls -A /sys/class/block/${TARGET#/dev/}/holders) ]] || return 2
    timeout --signal=TERM --kill-after=5 60 docker run --rm --privileged --pid=host \
        --userns=host --network none -v /:/host:ro "$TOOLS_IMAGE" \
        chroot /host /usr/bin/fuser -s "$TARGET" /dev/nvme1n1 || fuser_status=$?
    [[ $fuser_status == 1 ]]
}

parse_result() {
    python3 - "$1" "$2" "$3" <<'PY'
import json, re, sys
from pathlib import Path
log = Path(sys.argv[1]).read_text(errors="replace").replace("\r", "")
case, operation = sys.argv[2:]
begin = f"WRITE_REGRESSION_FIO_START {case}\n"
end = f"WRITE_REGRESSION_FIO_END {case}"
if begin not in log or end not in log:
    raise SystemExit("missing fio output markers")
payload = log.split(begin, 1)[1].split(end, 1)[0]
fio, _ = json.JSONDecoder().raw_decode(payload[payload.index("{"):])
measured = fio["jobs"][0][operation]
match = re.search(rf"^WRITE_REGRESSION_COMPLETE case={re.escape(case)} (.*)$", log, re.M)
fields = dict(re.findall(r"(\w+)=([^\s]+)", match.group(1))) if match else {}
if any(fields.get(name) != "0" for name in ("fio_status", "sync_status", "umount_status")):
    raise SystemExit("guest lifecycle failed")
expected = int(fields["expected_bytes"])
if int(measured["io_bytes"]) != expected:
    raise SystemExit("incomplete fio byte count")
elapsed = (int(fields["sync_end_ns"]) - int(fields["start_ns"])) / 1e9
print(json.dumps({
    "case": case,
    "system": "linux-ext4",
    "fio_mib_s": int(measured["bw_bytes"]) / 1048576,
    "including_sync_mib_s": expected / 1048576 / elapsed,
    "iops": float(measured["iops"]),
    "mean_latency_us": float(measured["lat_ns"]["mean"]) / 1000,
    "p99_latency_us": float(measured["clat_ns"]["percentile"]["99.000000"]) / 1000,
}, indent=2))
PY
}

for repeat in $REPEATS; do
    for spec in $SPECS; do
        IFS=: read -r workload bs jobs per_file_mib <<<"$spec"
        case_name="linux-ext4-${workload}-${bs}-j${jobs}-r${repeat}"
        point="$RESULT_ROOT/$case_name"
        expected_bytes=$((per_file_mib * jobs * 1024 * 1024))
        operation=write
        [[ ! -e $point ]] || { echo "SKIP existing $case_name"; continue; }
        preflight
        mkdir "$point"
        cp --reflink=auto "$BUILD_ROOT/rootfs-v5.ext4" "$point/rootfs.ext4"
        cp --reflink=auto "$BUILD_ROOT/bzImage" "$point/bzImage"
        cp -- "$QMP_HELPER" "$point/qmp-helper.py"
        debugfs -w -R 'rm /linux-write-regression.sh' "$point/rootfs.ext4" >/dev/null 2>&1 || true
        debugfs -w -R "write $SCRIPT_DIR/linux-guest.sh /linux-write-regression.sh" "$point/rootfs.ext4" >/dev/null
        debugfs -w -R 'set_inode_field /linux-write-regression.sh mode 0100755' "$point/rootfs.ext4" >/dev/null
        docker run --rm --network none --device "$TARGET:/dev/target:rwm" "$TOOLS_IMAGE" \
            mkfs.ext4 -F -b 4096 /dev/target >"$point/mkfs.log" 2>&1

        qemu_name="linux-write-$$"
        docker run -d --name "$qemu_name" --pid=host --network none --device /dev/kvm \
            --device "$TARGET:/dev/benchmark-target:rwm" -v "$point:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
            timeout --signal=TERM --kill-after=30 7200 qemu-system-x86_64 \
            -enable-kvm -cpu host -machine q35 -smp "$QEMU_SMP" -m 8G -mem-prealloc -S \
            -qmp unix:/benchmark/qmp.sock,server=on,wait=off \
            -kernel /benchmark/bzImage \
            -append "root=/dev/vda rw init=/linux-write-regression.sh console=ttyS0 panic=-1 write_case=$case_name write_workload=$workload write_bs=$bs write_jobs=$jobs write_per_file_mib=$per_file_mib write_expected_bytes=$expected_bytes" \
            -drive file=/benchmark/rootfs.ext4,if=none,format=raw,id=root,snapshot=on \
            -device virtio-blk-pci,drive=root \
            -drive file=/dev/benchmark-target,if=none,format=raw,id=target,cache=none,aio=threads \
            -device pcie-root-port,id=target-port,chassis=1,slot=1 \
            -object iothread,id=iothread0 -object iothread,id=iothread1 \
            -object iothread,id=iothread2 -object iothread,id=iothread3 \
            -object iothread,id=iothread4 -object iothread,id=iothread5 \
            -object iothread,id=iothread6 -object iothread,id=iothread7 \
            -nographic -no-reboot >"$point/container-id.txt"
        trap 'docker rm -f "$qemu_name" >/dev/null 2>&1 || true' EXIT
        docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-add-device /benchmark/qmp.sock 8 >"$point/qmp-add.json"
        docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query /benchmark/qmp.sock >"$point/qmp-iothreads.json"
        docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-query-cpus /benchmark/qmp.sock >"$point/qmp-cpus.json"
        python3 - "$point/qmp-iothreads.json" "$point/qmp-cpus.json" "$TOOLS_IMAGE" "$PIN_VCPUS" <<'PY' >"$point/affinity.log"
import json, subprocess, sys
threads = {row["id"]: row["thread-id"] for row in json.load(open(sys.argv[1]))}
vcpus = sorted(json.load(open(sys.argv[2])), key=lambda row: row["cpu-index"])
image, pin = sys.argv[3], sys.argv[4] == "1"
for queue, cpu in enumerate(range(56, 64)):
    print(subprocess.check_output(["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none", image, "taskset", "-pc", str(cpu), str(threads[f"iothread{queue}"])], text=True).strip())
if pin:
    host_cpus = list(range(56)) + list(range(64, 120))
    for row, cpu in zip(vcpus, host_cpus):
        print(subprocess.check_output(["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none", image, "taskset", "-pc", str(cpu), str(row["thread-id"])], text=True).strip())
PY
        docker exec "$qemu_name" python3 /benchmark/qmp-helper.py qmp-cont /benchmark/qmp.sock >"$point/qmp-cont.json"
        docker wait "$qemu_name" >"$point/qemu-exit.txt"
        docker logs "$qemu_name" >"$point/qemu.log" 2>&1
        docker rm "$qemu_name" >/dev/null
        trap - EXIT
        check_status=0
        docker run --rm --network none --device "$TARGET:/dev/target:r" "$TOOLS_IMAGE" \
            e2fsck -fn /dev/target >"$point/e2fsck.log" 2>&1 || check_status=$?
        printf '%s\n' "$check_status" >"$point/e2fsck.status"
        [[ $check_status == 0 ]] || exit 2
        parse_result "$point/qemu.log" "$case_name" "$operation" >"$point/result.json"
        rm -f "$point/rootfs.ext4" "$point/bzImage" "$point/qmp-helper.py" "$point/qmp.sock"
        python3 - "$point/result.json" <<'PY'
import json, sys
r=json.load(open(sys.argv[1]))
print(f'{r["case"]}: {r["fio_mib_s"]:.2f} MiB/s sync={r["including_sync_mib_s"]:.2f}')
PY
    done
done
