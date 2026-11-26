#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg

FIO=/benchmark/bin/fio
FIO_JSON=/tmp/thread-sweep.json
MNT=/thread-sweep-ext4
PER_JOB_MIB=$((TOTAL_MIB / JOBS))

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "THREAD_SWEEP_ERROR case=$CASE message=$1"; exit 2; }

[ "$EXPECTED_BYTES" -eq "$((PER_JOB_MIB * JOBS * 1048576))" ] || fail expected-byte-contract

prepare_status=0
if [ "$MODE" = rnvpce ] || [ "$MODE" = rnvpce-warm ]; then
    mkdir -p "$MNT"
    mount -t ext4 -o delalloc,rnvpc=off,rnvpc_extend=off,rtio=off /dev/vdc "$MNT"
    mkdir -p "$MNT/data"
    "$FIO" --name=prepare --rw=write --bs=128k --ioengine=psync --direct=1 \
        --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="${PER_JOB_MIB}M" --fallocate=none --output-format=json \
        >/tmp/thread-sweep-prepare.json || prepare_status=$?
    sync
    umount "$MNT"
    [ "$prepare_status" -eq 0 ] || fail prepare-failed
    mount -t ext4 -o delalloc,rnvpc=off,rnvpc_extend=on,rtio=on /dev/vdc "$MNT"
elif [ "$MODE" != raw ]; then
    fail unknown-mode
fi

echo "THREAD_SWEEP_CONFIG case=$CASE mode=$MODE jobs=$JOBS total_mib=$TOTAL_MIB"
cat /proc/cmdline
if [ "$MODE" = rnvpce-warm ]; then
    warm_status=0
    "$FIO" --name="${CASE}-warmup" --rw=randwrite --bs=4k --ioengine=psync \
        --direct=0 --fsync=0 --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="${PER_JOB_MIB}M" --filesize="${PER_JOB_MIB}M" \
        --fallocate=none --randrepeat=1 --randseed=20261102 \
        --output-format=json --output=/tmp/thread-sweep-warmup.json || warm_status=$?
    sync
    [ "$warm_status" -eq 0 ] || fail warmup-failed
fi
rm -f "$FIO_JSON"
T0=$(now_ns)
fio_status=0
if [ "$MODE" = rnvpce ] || [ "$MODE" = rnvpce-warm ]; then
    "$FIO" --name="$CASE" --rw=randwrite --bs=4k --ioengine=psync \
        --direct=0 --fsync=0 --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="${PER_JOB_MIB}M" --filesize="${PER_JOB_MIB}M" \
        --fallocate=none --randrepeat=1 --randseed=20261103 \
        --output-format=json --output="$FIO_JSON" || fio_status=$?
else
    # A raw block device has no guest page-cache path. Split the first 2 GiB
    # into disjoint per-job regions so scaling does not come from LBA overlap.
    "$FIO" --name="$CASE" --rw=randwrite --bs=4k --ioengine=psync \
        --filename=/dev/vdc --direct=1 --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --size="${PER_JOB_MIB}M" --offset_increment="${PER_JOB_MIB}M" \
        --randrepeat=1 --randseed=20261103 \
        --output-format=json --output="$FIO_JSON" || fio_status=$?
fi
T1=$(now_ns)

sync_status=0
umount_status=0
T2=$T1
if [ "$MODE" = rnvpce ] || [ "$MODE" = rnvpce-warm ]; then
    sync || sync_status=$?
    T2=$(now_ns)
    umount "$MNT" || umount_status=$?
fi

echo "THREAD_SWEEP_FIO_START $CASE"
cat "$FIO_JSON"
echo "THREAD_SWEEP_FIO_END $CASE"
echo "THREAD_SWEEP_COMPLETE case=$CASE mode=$MODE fio_status=$fio_status sync_status=$sync_status umount_status=$umount_status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2"
[ "$fio_status" -eq 0 ] && [ "$sync_status" -eq 0 ] && [ "$umount_status" -eq 0 ]
