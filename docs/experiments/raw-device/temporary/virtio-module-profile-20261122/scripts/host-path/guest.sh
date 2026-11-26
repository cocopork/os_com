#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/virtio-direct.cfg

FIO=/benchmark/bin/fio
TARGET=/dev/vdc

[ -b "$TARGET" ] || { echo "missing target device"; exit 2; }
[ "$EXPECTED_BYTES" -eq "$((PER_JOB_MIB * JOBS * 1048576))" ] || exit 2

echo "VIRTIO_DIRECT_CONFIG case=$CASE mode=$MODE queues=$VIRTQUEUES iothreads=$IOTHREADS jobs=$JOBS"
cat /proc/cmdline

repeat=1
while [ "$repeat" -le 1 ]; do
    output="/tmp/virtio-direct-$repeat.json"
    "$FIO" --name="$CASE-r$repeat" --rw=randwrite --bs=4k --ioengine=psync \
        --filename="$TARGET" --direct=1 --invalidate=1 --numjobs="$JOBS" \
        --thread=1 --iodepth=1 --cpus_allowed=0-31 --cpus_allowed_policy=split \
        --group_reporting=1 --size="${PER_JOB_MIB}M" \
        --offset_increment="${PER_JOB_MIB}M" --randrepeat=1 --randseed=20261122 \
        --output-format=json --output="$output"
    echo "VIRTIO_DIRECT_FIO_START repeat=$repeat"
    cat "$output"
    echo "VIRTIO_DIRECT_FIO_END repeat=$repeat"
    repeat=$((repeat + 1))
done

echo "VIRTIO_DIRECT_COMPLETE case=$CASE"
