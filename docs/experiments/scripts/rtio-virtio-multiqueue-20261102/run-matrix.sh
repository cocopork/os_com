#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$SCRIPT_DIR/run-validation.sh"
authorize_target "${1:-}"
[[ $# == 1 ]]
ROOT_ART=${ROOT_ART:?set ROOT_ART to the external matrix artifact directory}
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to the fingerprinted input images}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
# A second invocation must not race formatting, image edits or the same device.
exec 9>"$ROOT_ART/matrix.lock"
flock -n 9 || { echo "matrix already running" >&2; exit 2; }
for queues in ${MATRIX_QUEUES:-1 4}; do
    configure_queues "$queues"
    while read -r workload bs jobs; do
        case_name="q${queues}-${workload}-${bs}-j${jobs}"
        point="$ROOT_ART/$case_name"
        inputs="$ROOT_ART/input-$case_name"
        mkdir "$inputs"
        for file in selected.iso ext2.img exfat.img; do
            cp --reflink=auto "$SOURCE_ART/$file" "$inputs/$file"
        done
        cat > "$inputs/case.cfg" <<EOF
CASE=$case_name
MODE=rnvpce
AXIS=rtio-virtio-multiqueue
WORKLOAD=$workload
BS=$bs
JOBS=$jobs
TOTAL_MIB=2048
EXPECTED_BYTES=2147483648
DISK_JOURNAL=0
RUNTIME_JOURNAL=0
RUNTIME=300
EOF
        for guest_file in rebuild-fio.cfg rebuild-fio.sh; do
            debugfs -w -R "rm /$guest_file" "$inputs/ext2.img" >/dev/null 2>&1 || true
        done
        debugfs -w -R "write $inputs/case.cfg /rebuild-fio.cfg" "$inputs/ext2.img" > "$inputs/inject-config.log" 2>&1
        debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$inputs/ext2.img" > "$inputs/inject-guest.log" 2>&1
        status=0
        ART="$point" SOURCE_ART="$inputs" QUEUES="$queues" FORMAT_TARGET=1 TOOLS_IMAGE="$TOOLS_IMAGE" \
            bash "$SCRIPT_DIR/run-validation.sh" "--authorized-destructive-target=$TARGET" \
            > "$ROOT_ART/$case_name-runner.log" 2>&1 || status=$?
        printf '%s\n' "$status" > "$ROOT_ART/$case_name-runner.status"
        [[ $status == 0 || $status == 3 ]] || { echo "$case_name: runner failed ($status)" >&2; exit 2; }
        # All helper/QEMU containers have been stopped and captured before this read-only check.
        python3 "$SCRIPT_DIR/summarize.py" target-preflight "$TARGET" > "$point/post-target.json"
        check_status=0
        docker run --rm --network none --device "$TARGET:/dev/benchmark-target:r" "$TOOLS_IMAGE" \
            e2fsck -fn /dev/benchmark-target > "$point/e2fsck.log" 2>&1 || check_status=$?
        printf '%s\n' "$check_status" > "$point/e2fsck.status"
        python3 "$SCRIPT_DIR/results.py" "$point" > "$point/result.json"
        echo "$case_name correctness=pass mechanism_status=$status"
    done <<< "${MATRIX_SPECS:-randwrite 4k 1
randwrite 4k 16
write 128k 1
write 128k 16}"
done
