#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg

FIO=/benchmark/bin/fio
FIO_JSON=/tmp/overwrite-sweep.json
MNT=/overwrite-sweep-ext4
NATIVE_OPTIONS=delalloc,rnvpc=off,rnvpc_extend=off,rtio=off

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "OVERWRITE_SWEEP_ERROR case=$CASE message=$1"; exit 2; }

case "$MODE" in
    ext4) OPTIONS=$NATIVE_OPTIONS ;;
    rnvpce) OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on ;;
    *) fail invalid-mode ;;
esac
case "$WORKLOAD" in randwrite|write) ;; *) fail invalid-workload ;; esac
[ "$EXPECTED_BYTES" -eq "$((PER_FILE_MIB * JOBS * 1048576))" ] || fail byte-contract

mkdir -p "$MNT"
mount -t ext4 -o "$NATIVE_OPTIONS" /dev/vdc "$MNT"
mkdir -p "$MNT/data"
prepare_status=0
for file_id in $(seq 0 $((JOBS - 1))); do
    "$FIO" --name="prepare-$file_id" --rw=write --bs=128k --ioengine=psync \
        --direct=1 --numjobs=1 --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --filename="$MNT/data/f.$file_id" --size="${PER_FILE_MIB}M" \
        --fallocate=none --output-format=json \
        >"/tmp/overwrite-prepare-$file_id.json" || prepare_status=$?
    [ "$prepare_status" -eq 0 ] || break
done
sync
umount "$MNT"
[ "$prepare_status" -eq 0 ] || fail prepare-failed

mount -t ext4 -o "$OPTIONS" /dev/vdc "$MNT"
echo "OVERWRITE_SWEEP_CONFIG case=$CASE mode=$MODE axis=$AXIS workload=$WORKLOAD bs=$BS jobs=$JOBS per_file_mib=$PER_FILE_MIB expected_bytes=$EXPECTED_BYTES"
cat /proc/cmdline

rm -f "$FIO_JSON"
T0=$(now_ns)
fio_status=0
"$FIO" --name="$CASE" --rw="$WORKLOAD" --bs="$BS" --ioengine=psync \
    --direct=0 --fsync=0 --numjobs="$JOBS" --thread=1 --iodepth=1 \
    --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
    --directory="$MNT/data" --filename_format='f.$jobnum' \
    --size="${PER_FILE_MIB}M" --filesize="${PER_FILE_MIB}M" \
    --fallocate=none --randrepeat=1 --randseed=20261105 \
    --output-format=json --output="$FIO_JSON" || fio_status=$?
T1=$(now_ns)
sync_status=0
sync || sync_status=$?
T2=$(now_ns)
umount_status=0
umount "$MNT" || umount_status=$?

echo "OVERWRITE_SWEEP_FIO_START $CASE"
cat "$FIO_JSON"
echo "OVERWRITE_SWEEP_FIO_END $CASE"
echo "OVERWRITE_SWEEP_COMPLETE case=$CASE mode=$MODE axis=$AXIS workload=$WORKLOAD bs=$BS jobs=$JOBS expected_bytes=$EXPECTED_BYTES fio_status=$fio_status sync_status=$sync_status umount_status=$umount_status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2"
[ "$fio_status" -eq 0 ] && [ "$sync_status" -eq 0 ] && [ "$umount_status" -eq 0 ]
