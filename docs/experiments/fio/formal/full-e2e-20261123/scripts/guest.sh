#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg

FIO=/benchmark/bin/fio
FIO_JSON=/tmp/full-e2e-fio.json
MNT=/full-e2e-ext4
NATIVE_OPTIONS=delalloc,rnvpc=off,rnvpc_extend=off,rtio=off,pagecache_capacity_mb=$PAGECACHE_CAPACITY_MB

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "FULL_E2E_ERROR case=$CASE message=$1"; exit 2; }

case "$MODE" in
    ext4) OPTIONS=$NATIVE_OPTIONS ;;
    rnvpc) OPTIONS=delalloc,rnvpc=on,rnvpc_extend=off,rtio=on ;;
    rnvpce) OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on,pagecache_capacity_mb=$PAGECACHE_CAPACITY_MB ;;
    *) fail invalid-mode ;;
esac
case "$WORKLOAD" in
    overwrite-randwrite) FIO_RW=randwrite ;;
    append-write) FIO_RW=write ;;
    cold-randread) FIO_RW=randread ;;
    *) fail invalid-workload ;;
esac
if [ "$RUNTIME_SECONDS" -eq 0 ]; then
    [ "$EXPECTED_BYTES" -eq "$((PER_FILE_MIB * JOBS * 1048576))" ] || fail byte-contract
fi

mkdir -p "$MNT"
prepare_status=0
if [ "$PREPARE" -eq 1 ]; then
    mount -t ext4 -o "$NATIVE_OPTIONS" /dev/vdc "$MNT"
    mkdir -p "$MNT/data"
    file_id=0
    while [ "$file_id" -lt "$JOBS" ]; do
        "$FIO" --name="prepare-$file_id" --rw=write --bs=128k --ioengine=psync \
            --direct=1 --numjobs=1 --thread=1 --iodepth=1 \
            --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
            --filename="$MNT/data/f.$file_id" --size="${PER_FILE_MIB}M" \
            --fallocate=none --output-format=json \
            >"/tmp/full-e2e-prepare-$file_id.json" || prepare_status=$?
        [ "$prepare_status" -eq 0 ] || break
        file_id=$((file_id + 1))
    done
    sync
    umount "$MNT"
    [ "$prepare_status" -eq 0 ] || fail prepare-failed
fi

mount -t ext4 -o "$OPTIONS" /dev/vdc "$MNT"
mkdir -p "$MNT/data"
echo "FULL_E2E_CONFIG case=$CASE device=$DEVICE mode=$MODE workload=$WORKLOAD axis=$AXIS bs=$BS jobs=$JOBS per_file_mib=$PER_FILE_MIB expected_bytes=$EXPECTED_BYTES repeat=$REPEAT fsync_blocks=$FSYNC_BLOCKS"
echo "FULL_E2E_MOUNT options=$OPTIONS"
cat /proc/cmdline

rm -f "$FIO_JSON"
T0=$(now_ns)
fio_status=0
run_fio() {
    "$FIO" --name="$CASE" --rw="$FIO_RW" --bs="$BS" --ioengine=psync \
        --direct="${DIRECT_IO:-0}" --fsync="$FSYNC_BLOCKS" --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --fallocate=none --create_on_open=1 --randrepeat=1 --randseed=20261117 \
        "$@" --output-format=json --output="$FIO_JSON"
}
if [ "$RUNTIME_SECONDS" -gt 0 ]; then
    run_fio --size="${PER_FILE_MIB}M" --time_based --runtime="${RUNTIME_SECONDS}s" || fio_status=$?
else
    run_fio --size="${PER_FILE_MIB}M" --filesize="${PER_FILE_MIB}M" || fio_status=$?
fi
T1=$(now_ns)
sync_status=0
if [ "$FIO_RW" != randread ]; then
    sync || sync_status=$?
fi
T2=$(now_ns)
umount_status=0
umount "$MNT" || umount_status=$?

echo "FULL_E2E_FIO_START $CASE"
cat "$FIO_JSON"
echo "FULL_E2E_FIO_END $CASE"
echo "FULL_E2E_COMPLETE case=$CASE fio_status=$fio_status sync_status=$sync_status umount_status=$umount_status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2"
[ "$fio_status" -eq 0 ] && [ "$sync_status" -eq 0 ] && [ "$umount_status" -eq 0 ]
