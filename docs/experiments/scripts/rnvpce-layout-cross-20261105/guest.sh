#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg

FIO=/benchmark/bin/fio
MNT=/layout-cross-ext4
FILE_MIB=1024
EXPECTED_BYTES=$((JOBS * FILE_MIB * 1048576))
FIO_JSON=/tmp/layout-cross.json

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "LAYOUT_CROSS_ERROR case=$CASE message=$1"; exit 2; }

case "$LAYOUT" in fragmented|contiguous) ;; *) fail invalid-layout ;; esac
case "$JOBS" in 1|2) ;; *) fail invalid-jobs ;; esac

mkdir -p "$MNT"
mount -t ext4 -o delalloc,rnvpc=off,rnvpc_extend=off,rtio=off /dev/vdc "$MNT"
mkdir -p "$MNT/data"

prepare_status=0
if [ "$LAYOUT" = fragmented ]; then
    "$FIO" --name=prepare-fragmented --rw=write --bs=128k --ioengine=psync --direct=1 \
        --numjobs=2 --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="${FILE_MIB}M" --fallocate=none --output-format=json \
        >/tmp/layout-cross-prepare.json || prepare_status=$?
else
    for file_id in 0 1; do
        "$FIO" --name="prepare-contiguous-$file_id" --rw=write --bs=128k \
            --ioengine=psync --direct=1 --numjobs=1 --thread=1 --iodepth=1 \
            --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
            --filename="$MNT/data/f.$file_id" --size="${FILE_MIB}M" \
            --fallocate=none --output-format=json \
            >"/tmp/layout-cross-prepare-$file_id.json" || prepare_status=$?
        [ "$prepare_status" -eq 0 ] || break
    done
fi
sync
umount "$MNT"
[ "$prepare_status" -eq 0 ] || fail prepare-failed

mount -t ext4 -o delalloc,rnvpc=off,rnvpc_extend=on,rtio=on /dev/vdc "$MNT"

run_random_write() {
    name=$1
    seed=$2
    output=$3
    if [ "$JOBS" -eq 1 ]; then
        "$FIO" --name="$name" --rw=randwrite --bs=4k --ioengine=psync \
            --direct=0 --fsync=0 --numjobs=1 --thread=1 --iodepth=1 \
            --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
            --filename="$MNT/data/f.0" --size="${FILE_MIB}M" \
            --filesize="${FILE_MIB}M" --fallocate=none --randrepeat=1 \
            --randseed="$seed" --output-format=json --output="$output"
    else
        "$FIO" --name="$name" --rw=randwrite --bs=4k --ioengine=psync \
            --direct=0 --fsync=0 --numjobs=2 --thread=1 --iodepth=1 \
            --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
            --directory="$MNT/data" --filename_format='f.$jobnum' \
            --size="${FILE_MIB}M" --filesize="${FILE_MIB}M" \
            --fallocate=none --randrepeat=1 --randseed="$seed" \
            --output-format=json --output="$output"
    fi
}

echo "LAYOUT_CROSS_CONFIG case=$CASE layout=$LAYOUT jobs=$JOBS file_mib=$FILE_MIB"
cat /proc/cmdline

warm_status=0
run_random_write "$CASE-warmup" 20261104 /tmp/layout-cross-warmup.json || warm_status=$?
sync
[ "$warm_status" -eq 0 ] || fail warmup-failed

rm -f "$FIO_JSON"
T0=$(now_ns)
fio_status=0
run_random_write "$CASE" 20261105 "$FIO_JSON" || fio_status=$?
T1=$(now_ns)

sync_status=0
umount_status=0
sync || sync_status=$?
T2=$(now_ns)
umount "$MNT" || umount_status=$?

echo "LAYOUT_CROSS_FIO_START $CASE"
cat "$FIO_JSON"
echo "LAYOUT_CROSS_FIO_END $CASE"
echo "LAYOUT_CROSS_COMPLETE case=$CASE layout=$LAYOUT jobs=$JOBS expected_bytes=$EXPECTED_BYTES fio_status=$fio_status sync_status=$sync_status umount_status=$umount_status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2"
[ "$fio_status" -eq 0 ] && [ "$sync_status" -eq 0 ] && [ "$umount_status" -eq 0 ]
