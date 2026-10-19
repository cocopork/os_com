#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu
. /ext2/rebuild-fio.cfg
MNT=/rebuild-ext4
FIO=/benchmark/bin/fio
now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
mkdir -p "$MNT"
mount -t ext4 -o delalloc,rnvpc=off,rnvpc_extend=off,rtio=off /dev/vdc "$MNT"
mkdir "$MNT/data"
if [ "$RW" = randwrite ]; then
    echo "BUFFER_PREPARE_START $CASE"
    "$FIO" --name=prepare --rw=write --bs=128k --ioengine=psync \
        --direct=1 --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="$((TOTAL_MB / JOBS))M" --fallocate=none --output-format=json
    sync
    echo "BUFFER_PREPARE_END $CASE"
fi
umount "$MNT"
if [ "$VARIANT" = rnvpce ]; then
    OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on
else
    OPTIONS=delalloc,rnvpc=off,rnvpc_extend=off,rtio=off
fi
mount -t ext4 -o "$OPTIONS" /dev/vdc "$MNT"
echo "BUFFER_CONFIG $CASE mount=$OPTIONS direct=0 jobs=$JOBS total_mib=$TOTAL_MB"
cat /proc/cmdline
T0=$(now_ns)
echo "BUFFER_FIO_START $CASE"
status=0
"$FIO" --name="$CASE" --rw="$RW" --bs="$BS" --ioengine=psync \
    --direct=0 --fsync=0 --numjobs="$JOBS" --thread=1 --iodepth=1 \
    --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
    --directory="$MNT/data" --filename_format='f.$jobnum' \
    --size="$((TOTAL_MB / JOBS))M" --filesize="$((TOTAL_MB / JOBS))M" \
    --fallocate=none --randrepeat=1 --randseed=20261018 \
    --runtime="${RUNTIME:-60}" --output-format=json || status=$?
echo "BUFFER_FIO_END $CASE"
T1=$(now_ns)
echo "BUFFER_DRAIN_START $CASE fio_status=$status start_ns=$T0 fio_end_ns=$T1"
sync
T2=$(now_ns)
umount_status=0
umount "$MNT" || umount_status=$?
echo "BUFFER_COMPLETE $CASE fio_status=$status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2 umount_status=$umount_status"
