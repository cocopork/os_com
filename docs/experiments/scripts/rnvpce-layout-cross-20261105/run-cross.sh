#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MQ_DIR=$(realpath "$SCRIPT_DIR/../rtio-virtio-multiqueue-20261102")
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing external artifact directory}
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to selected.iso/ext2.img/exfat.img}
TARGET=${TARGET:-/dev/nvme1n1p2}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || {
    echo "ROOT_ART must be an existing caller-owned writable directory" >&2
    exit 2
}
case $TARGET in /dev/nvme1n1p1|/dev/nvme1n1p2) ;; *) echo "unsupported target" >&2; exit 2 ;; esac

exec 9>"$ROOT_ART/cross.lock"
flock -n 9 || { echo "cross experiment already running" >&2; exit 2; }

for layout in fragmented contiguous; do
    for jobs in 1 2; do
        case_name="rnvpce-${layout}-j${jobs}"
        point="$ROOT_ART/$case_name"
        inputs="$ROOT_ART/input-$case_name"
        mkdir "$inputs"
        for file in selected.iso ext2.img exfat.img; do
            cp --reflink=auto "$SOURCE_ART/$file" "$inputs/$file"
        done
        cat >"$inputs/case.cfg" <<EOF
CASE=$case_name
LAYOUT=$layout
JOBS=$jobs
EOF
        for guest_file in rebuild-fio.cfg rebuild-fio.sh; do
            debugfs -w -R "rm /$guest_file" "$inputs/ext2.img" >/dev/null 2>&1 || true
        done
        debugfs -w -R "write $inputs/case.cfg /rebuild-fio.cfg" "$inputs/ext2.img" >/dev/null
        debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$inputs/ext2.img" >/dev/null

        status=0
        ART="$point" SOURCE_ART="$inputs" GUEST_KIND=aster QUEUES=4 \
            FORMAT_TARGET=1 CHECK_FILESYSTEM=1 TOOLS_IMAGE="$TOOLS_IMAGE" \
            bash "$MQ_DIR/run-validation.sh" \
            "--authorized-destructive-target=$TARGET" \
            >"$ROOT_ART/$case_name-runner.log" 2>&1 || status=$?
        printf '%s\n' "$status" >"$ROOT_ART/$case_name-runner.status"
        [[ $status == 0 || $status == 3 ]] || {
            echo "$case_name failed: $status" >&2
            exit 2
        }

        for file_id in 0 1; do
            docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" \
                "$TOOLS_IMAGE" debugfs -R "stat /data/f.$file_id" \
                /dev/benchmark-target >"$point/f.$file_id.stat" 2>&1
        done
        python3 "$SCRIPT_DIR/inspect_layout.py" \
            "$point/f.0.stat" "$point/f.1.stat" >"$point/layout.json"
        python3 "$SCRIPT_DIR/results.py" "$point" >"$point/result.json"
        python3 - "$point/result.json" "$point/layout.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
l = json.load(open(sys.argv[2]))
extents = "/".join(str(row["data_extents"]) for row in l)
print(f'{r["case"]}: {r["fio_mib_s"]:.2f} MiB/s, mean={r["mean_latency_us"]:.2f} us, p99={r["p99_latency_us"]:.2f} us, extents={extents}')
PY
    done
done
