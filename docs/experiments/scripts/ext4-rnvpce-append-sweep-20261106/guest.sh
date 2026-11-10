#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg

FIO=/benchmark/bin/fio
FIO_JSON=/tmp/append-sweep.json
MNT=/append-sweep-ext4

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "APPEND_SWEEP_ERROR case=$CASE message=$1"; exit 2; }

case "$MODE" in
    ext4) OPTIONS=delalloc,rnvpc=off,rnvpc_extend=off,rtio=off ;;
    rnvpce) OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on ;;
    *) fail invalid-mode ;;
esac
[ "$EXPECTED_BYTES" -eq "$((PER_FILE_MIB * JOBS * 1048576))" ] || fail byte-contract

mkdir -p "$MNT"
mount -t ext4 -o "$OPTIONS" /dev/vdc "$MNT"
mkdir -p "$MNT/data"
echo "APPEND_SWEEP_CONFIG case=$CASE mode=$MODE axis=$AXIS bs=$BS jobs=$JOBS per_file_mib=$PER_FILE_MIB expected_bytes=$EXPECTED_BYTES"
cat /proc/cmdline

rm -f "$FIO_JSON"
T0=$(now_ns)
fio_status=0
"$FIO" --name="$CASE" --rw=write --bs="$BS" --ioengine=psync \
    --direct=0 --fsync=0 --numjobs="$JOBS" --thread=1 --iodepth=1 \
    --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
    --directory="$MNT/data" --filename_format='f.$jobnum' \
    --size="${PER_FILE_MIB}M" --filesize="${PER_FILE_MIB}M" \
    --fallocate=none --create_on_open=1 \
    --output-format=json --output="$FIO_JSON" || fio_status=$?
T1=$(now_ns)
sync_status=0
sync || sync_status=$?
T2=$(now_ns)
umount_status=0
umount "$MNT" || umount_status=$?

echo "APPEND_SWEEP_FIO_START $CASE"
cat "$FIO_JSON"
echo "APPEND_SWEEP_FIO_END $CASE"
echo "APPEND_SWEEP_COMPLETE case=$CASE mode=$MODE axis=$AXIS bs=$BS jobs=$JOBS expected_bytes=$EXPECTED_BYTES fio_status=$fio_status sync_status=$sync_status umount_status=$umount_status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2"
[ "$fio_status" -eq 0 ] && [ "$sync_status" -eq 0 ] && [ "$umount_status" -eq 0 ]
