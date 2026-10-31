#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg
MNT=/rebuild-ext4
FIO=/benchmark/bin/fio
NATIVE_OPTIONS=delalloc,rnvpc=off,rnvpc_extend=off,rtio=off
RNVPCE_OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "OVERWRITE_ERROR case=$CASE message=$1"; exit 2; }

[ "$EXPECTED_BYTES" -eq "$(((TOTAL_MIB / JOBS) * JOBS * 1048576))" ] || \
    fail expected-byte-contract
[ "$PREPARE" -eq 0 ] || [ "$PREPARE" -eq 1 ] || fail invalid-prepare-mode

mkdir -p "$MNT"
mount -t ext4 -o "$NATIVE_OPTIONS" /dev/vdc "$MNT"
mkdir -p "$MNT/data"
prepare_bytes=0
if [ "$PREPARE" -eq 1 ]; then
    echo "OVERWRITE_PREPARE_START $CASE"
    "$FIO" --name=prepare --rw=write --bs=128k --ioengine=psync \
        --direct=1 --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="$((TOTAL_MIB / JOBS))M" --fallocate=none --output-format=json
    sync
    prepare_bytes=$EXPECTED_BYTES
fi
umount "$MNT"
echo "OVERWRITE_PREPARE_COMPLETE $CASE bytes=$prepare_bytes status=0"

mount -t ext4 -o "$RNVPCE_OPTIONS" /dev/vdc "$MNT"
echo "OVERWRITE_CONFIG case=$CASE prepare=$PREPARE bs=128k jobs=$JOBS total_mib=$TOTAL_MIB expected_bytes=$EXPECTED_BYTES"
echo "OVERWRITE_MOUNT case=$CASE options=$RNVPCE_OPTIONS"
cat /proc/cmdline

T0=$(now_ns)
echo "OVERWRITE_FIO_START $CASE"
status=0
"$FIO" --name="$CASE" --rw=write --bs=128k --ioengine=psync \
    --direct=0 --fsync=0 --numjobs="$JOBS" --thread=1 --iodepth=1 \
    --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
    --directory="$MNT/data" --filename_format='f.$jobnum' \
    --size="$((TOTAL_MIB / JOBS))M" --filesize="$((TOTAL_MIB / JOBS))M" \
    --fallocate=none --runtime=1200 --output-format=json || status=$?
echo "OVERWRITE_FIO_END $CASE"
T1=$(now_ns)
sync
T2=$(now_ns)
umount_status=0
umount "$MNT" || umount_status=$?
echo "OVERWRITE_COMPLETE $CASE fio_status=$status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2 umount_status=$umount_status"
