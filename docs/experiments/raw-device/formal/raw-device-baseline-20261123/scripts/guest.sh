#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg

FIO=/benchmark/bin/fio
TARGET=/dev/vdc
PER_JOB_MIB=$((TOTAL_MIB / JOBS))

fail() {
    echo "RAW_DEVICE_ERROR case=$CASE message=$1"
    exit 2
}

select_linux_fio_cpus() {
    root=$1
    jobs=$2
    selected=
    remaining=$jobs
    position=1
    while [ "$remaining" -gt 0 ]; do
        progressed=0
        for queue in "$root"/*; do
            [ -f "$queue/cpu_list" ] || continue
            cpu=$(tr -d ' ' <"$queue/cpu_list" | tr ',' '\n' | sed -n "${position}p")
            [ -n "$cpu" ] || continue
            if [ -n "$selected" ]; then
                selected="$selected,$cpu"
            else
                selected=$cpu
            fi
            remaining=$((remaining - 1))
            progressed=1
            [ "$remaining" -eq 0 ] && break
        done
        [ "$progressed" -eq 1 ] || return 1
        position=$((position + 1))
    done
    printf '%s\n' "$selected"
}

[ -b "$TARGET" ] || fail missing-target-device
[ "$EXPECTED_BYTES" -eq "$((PER_JOB_MIB * JOBS * 1048576))" ] || fail expected-byte-contract
[ "$REPEATS" -eq 3 ] || fail unexpected-repeat-count

if [ "$SYSTEM" = linux ]; then
    [ -r /proc/uptime ] || mount -t proc proc /proc
    [ -d /sys/block ] || mount -t sysfs sysfs /sys
    FIO_CPUS=$(select_linux_fio_cpus /sys/block/vdc/mq "$JOBS") || fail cpu-selection
else
    [ "$SYSTEM" = asterinas ] || fail unknown-system
    FIO_CPUS=0-31
fi

echo "RAW_DEVICE_CONFIG case=$CASE system=$SYSTEM device=$DEVICE workload=$WORKLOAD axis=$AXIS bs=$BS jobs=$JOBS actual_mib=$ACTUAL_MIB repeats=$REPEATS"
echo "RAW_DEVICE_CPUS $FIO_CPUS"
cat /proc/cmdline
if [ -d /sys/block/vdc/mq ]; then
    for queue in /sys/block/vdc/mq/*; do
        echo "RAW_DEVICE_MQ queue=${queue##*/} cpus=$(cat "$queue/cpu_list")"
    done
fi

completed=0
fio_status=0
repeat=1
while [ "$repeat" -le "$REPEATS" ]; do
    output="/tmp/raw-device-${repeat}.json"
    rm -f "$output"
    "$FIO" --name="$CASE-r$repeat" --rw="$FIO_RW" --bs="$BS" --ioengine=psync \
        --filename="$TARGET" --direct=1 --invalidate=1 \
        --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed="$FIO_CPUS" --cpus_allowed_policy=split --group_reporting=1 \
        --size="${PER_JOB_MIB}M" --offset_increment="${PER_JOB_MIB}M" \
        --randrepeat=1 --randseed=20261121 \
        --output-format=json --output="$output" || fio_status=$?
    echo "RAW_DEVICE_FIO_START case=$CASE repeat=$repeat"
    cat "$output"
    echo "RAW_DEVICE_FIO_END case=$CASE repeat=$repeat"
    [ "$fio_status" -eq 0 ] || break
    completed=$repeat
    repeat=$((repeat + 1))
done

echo "RAW_DEVICE_COMPLETE case=$CASE fio_status=$fio_status completed_repeats=$completed"
[ "$fio_status" -eq 0 ] && [ "$completed" -eq "$REPEATS" ]

