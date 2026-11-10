#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg
. /benchmark/fio/raw_virtio_thread_sweep/cpu_selection.sh

FIO=/benchmark/bin/fio
FIO_JSON=/tmp/raw-virtio-thread-sweep.json
PER_JOB_MIB=$((TOTAL_MIB / JOBS))

[ -r /proc/uptime ] || mount -t proc proc /proc
[ -d /sys/block ] || mount -t sysfs sysfs /sys

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "THREAD_SWEEP_ERROR case=$CASE message=$1"; exit 2; }

[ "$MODE" = linux ] || fail wrong-mode
[ "$EXPECTED_BYTES" -eq "$((PER_JOB_MIB * JOBS * 1048576))" ] || fail expected-byte-contract
[ -b /dev/vdc ] || fail missing-target-device
FIO_CPUS=$(select_fio_cpus /sys/block/vdc/mq "$JOBS") || fail cpu-selection

echo "THREAD_SWEEP_CONFIG case=$CASE mode=$MODE jobs=$JOBS total_mib=$TOTAL_MIB"
echo "THREAD_SWEEP_CPUS $FIO_CPUS"
cat /proc/cmdline
for queue in /sys/block/vdc/mq/*; do
    echo "THREAD_SWEEP_MQ queue=${queue##*/} cpus=$(cat "$queue/cpu_list")"
done
grep -i virtio /proc/interrupts || true
T0=$(now_ns)
fio_status=0
"$FIO" --name="$CASE" --rw=randwrite --bs=4k --ioengine=psync \
    --filename=/dev/vdc --direct=1 --numjobs="$JOBS" --thread=1 --iodepth=1 \
    --cpus_allowed="$FIO_CPUS" --cpus_allowed_policy=split --group_reporting=1 \
    --size="${PER_JOB_MIB}M" --offset_increment="${PER_JOB_MIB}M" \
    --randrepeat=1 --randseed=20261103 \
    --output-format=json --output="$FIO_JSON" || fio_status=$?
T1=$(now_ns)

echo "THREAD_SWEEP_FIO_START $CASE"
cat "$FIO_JSON"
echo "THREAD_SWEEP_FIO_END $CASE"
echo "THREAD_SWEEP_COMPLETE case=$CASE mode=$MODE fio_status=$fio_status sync_status=0 umount_status=0 start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T1"
[ "$fio_status" -eq 0 ]
