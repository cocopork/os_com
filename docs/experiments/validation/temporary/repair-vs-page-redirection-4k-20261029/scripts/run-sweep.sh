#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

BASE=${BASE:-/home/zn/asterinas-build-env/repair-vs-page-redirection-4k-20261029}
RUNNER=${RUNNER:-/home/zn/asterinas-build-env/dirty-migration-extents-20261029/run-qemu.sh}
TOOLS_IMAGE=os-com-recovery-tools:20261017
TARGET=/dev/nvme1n1p1

run_point() {
    local variant=$1
    local jobs=$2
    local art="$BASE/$variant"
    local case_name="rnvpce-write4k-j${jobs}-2048m"

    if [[ -e "$art/${case_name}.log" ]]; then
        if grep -q "BUFFER_COMPLETE $case_name" "$art/${case_name}.log"; then
            if [[ ! -e "$art/${case_name}-e2fsck.log" ]]; then
                sg docker -c "docker run --rm --network none --device $TARGET:/dev/benchmark-target:rwm $TOOLS_IMAGE e2fsck -fn /dev/benchmark-target" \
                    > "$art/${case_name}-e2fsck.log" 2>&1
            fi
            return 0
        fi
        echo "refusing to overwrite incomplete $art/${case_name}.log" >&2
        return 2
    fi

    printf 'CASE=%s\nVARIANT=rnvpce\nRW=write\nBS=4k\nJOBS=%s\nTOTAL_MB=2048\nRUNTIME=180\n' \
        "$case_name" "$jobs" > "$art/case.cfg"

    set +e
    ART="$art" "$RUNNER" --authorized-target > "$art/${case_name}.log" 2>&1
    local qemu_status=$?
    set -e
    if [[ $qemu_status -ne 0 && $qemu_status -ne 33 ]]; then
        return "$qemu_status"
    fi
    sg docker -c "docker run --rm --network none --device $TARGET:/dev/benchmark-target:rwm $TOOLS_IMAGE e2fsck -fn /dev/benchmark-target" \
        > "$art/${case_name}-e2fsck.log" 2>&1
}

for variant in repair-image current; do
    for jobs in 1 2 4 8 16; do
        run_point "$variant" "$jobs"
    done
done
