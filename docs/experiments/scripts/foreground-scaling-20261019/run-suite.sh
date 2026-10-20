#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 ARTIFACT_DIR" >&2
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH= cd -- "$script_dir/../../../.." && pwd)
harness="$repo/docs/experiments/scripts/rnvpce-fix-20261018/harness"
artifacts=$1

run_case() {
    bs=$1
    jobs=$2
    case="rnvpce-write${bs}-j${jobs}-2048m"
    if [ -f "$artifacts/$case.log" ] && \
        grep -q "BUFFER_COMPLETE $case .*umount_status=0" "$artifacts/$case.log" && \
        [ -f "$artifacts/$case-e2fsck.log" ]; then
        return
    fi
    python3 "$harness/run.py" nvme \
        --artifacts "$artifacts" \
        --format-authorized \
        --variant rnvpce \
        --rw write \
        --bs "$bs" \
        --jobs "$jobs" \
        --total-mib 2048 \
        --runtime 60
}

for bs in 4k 16k 64k 128k; do
    run_case "$bs" 1
done
for jobs in 4 16; do
    run_case 4k "$jobs"
    run_case 128k "$jobs"
done

python3 "$harness/summarize.py" "$artifacts"
python3 "$script_dir/analyze.py" "$artifacts"
