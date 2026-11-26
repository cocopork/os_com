#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MQ_DIR=$(realpath "$SCRIPT_DIR/../rtio-virtio-multiqueue-20261102")
OVERWRITE_DIR=$(realpath "$SCRIPT_DIR/../ext4-rnvpce-overwrite-sweep-20261105")
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing external artifact directory}
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to selected.iso/ext2.img/exfat.img}
TARGET=${TARGET:-/dev/nvme1n1p2}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || {
    echo "ROOT_ART must be an existing caller-owned writable directory" >&2
    exit 2
}
case $TARGET in /dev/nvme1n1p1|/dev/nvme1n1p2) ;; *) echo "unsupported target" >&2; exit 2 ;; esac

exec 9>"$ROOT_ART/sweep.lock"
flock -n 9 || { echo "append sweep already running" >&2; exit 2; }

list_points() {
    if [[ -n ${CASES:-} ]]; then
        for case_name in $CASES; do
            python3 "$SCRIPT_DIR/sweep.py" config "$case_name"
        done
    else
        python3 "$SCRIPT_DIR/sweep.py" list
    fi
}

while IFS=$'\t' read -r case_name mode axis bs jobs per_file_mib expected_bytes disk_format; do
    point="$ROOT_ART/$case_name"
    inputs="$ROOT_ART/input-$case_name"
    if [[ -f $point/result.json ]]; then
        python3 "$SCRIPT_DIR/sweep.py" parse "$point" "$case_name" >/dev/null
        echo "SKIP validated $case_name"
        continue
    fi
    mkdir "$inputs"
    for file in selected.iso ext2.img exfat.img; do
        cp --reflink=auto "$SOURCE_ART/$file" "$inputs/$file"
    done
    cat >"$inputs/case.cfg" <<EOF
CASE=$case_name
MODE=$mode
AXIS=$axis
BS=$bs
JOBS=$jobs
PER_FILE_MIB=$per_file_mib
EXPECTED_BYTES=$expected_bytes
EOF
    for guest_file in rebuild-fio.cfg rebuild-fio.sh; do
        debugfs -w -R "rm /$guest_file" "$inputs/ext2.img" >/dev/null 2>&1 || true
    done
    debugfs -w -R "write $inputs/case.cfg /rebuild-fio.cfg" "$inputs/ext2.img" >/dev/null
    debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$inputs/ext2.img" >/dev/null

    status=0
    ART="$point" SOURCE_ART="$inputs" GUEST_KIND=aster QUEUES=4 \
        FORMAT_TARGET=1 CHECK_FILESYSTEM=1 FILESYSTEM_FORMAT="$disk_format" \
        TOOLS_IMAGE="$TOOLS_IMAGE" bash "$MQ_DIR/run-validation.sh" \
        "--authorized-destructive-target=$TARGET" \
        >"$ROOT_ART/$case_name-runner.log" 2>&1 || status=$?
    printf '%s\n' "$status" >"$ROOT_ART/$case_name-runner.status"
    [[ $status == 0 || $status == 3 ]] || {
        echo "$case_name failed: $status" >&2
        exit 2
    }

    stat_files=()
    for file_id in $(seq 0 $((jobs - 1))); do
        stat_file="$point/f.$file_id.stat"
        docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
            "$TOOLS_IMAGE" debugfs -R "stat /data/f.$file_id" \
            /dev/benchmark-target >"$stat_file" 2>&1
        stat_files+=("$stat_file")
    done
    python3 "$OVERWRITE_DIR/inspect_layout.py" "${stat_files[@]}" >"$point/layout.json"
    python3 "$SCRIPT_DIR/sweep.py" parse "$point" "$case_name" >"$point/result.json"
    python3 - "$point/result.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
print(f'{r["case"]}: {r["fio_mib_s"]:.2f} MiB/s, sync={r["including_sync_mib_s"]:.2f}, mean={r["mean_latency_us"]:.2f} us, extents={r["layout"]["min_data_extents"]}-{r["layout"]["max_data_extents"]}')
PY
done < <(list_points)

python3 - "$ROOT_ART" <<'PY'
import csv, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
rows = [json.loads(path.read_text()) for path in sorted(root.glob("*/result.json"))]
if not rows:
    raise SystemExit("no validated results")
fields = [
    "case", "mode", "axis", "workload", "value", "bs_kib", "jobs", "actual_mib",
    "fio_mib_s", "including_sync_mib_s", "iops", "mean_latency_us", "p99_latency_us",
]
with (root / "results.csv").open("w", newline="") as output:
    writer = csv.DictWriter(output, fieldnames=fields, extrasaction="ignore")
    writer.writeheader()
    writer.writerows(rows)
print(f"validated {len(rows)} points")
PY
