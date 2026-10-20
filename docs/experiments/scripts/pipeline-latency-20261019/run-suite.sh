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

for variant in ext4 rnvpce; do
    for rw in randwrite write; do
        for jobs in 1 16; do
            case="$variant-$rw-j$jobs-2048m"
            if [ -f "$artifacts/$case.log" ] && \
                grep -q "BUFFER_COMPLETE $case .*umount_status=0" "$artifacts/$case.log" && \
                [ -f "$artifacts/$case-e2fsck.log" ]; then
                continue
            fi
            python3 "$harness/run.py" nvme \
                --artifacts "$artifacts" \
                --format-authorized \
                --variant "$variant" \
                --rw "$rw" \
                --jobs "$jobs" \
                --total-mib 2048 \
                --runtime 60
        done
    done
done

python3 "$harness/summarize.py" "$artifacts"
for log in "$artifacts"/*-j*-2048m.log; do
    "$script_dir/validate-metrics.sh" "$log"
done
python3 "$script_dir/analyze.py" "$artifacts"
