#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/rebuild-fio.cfg
MNT=/rebuild-ext4
FIO=/benchmark/bin/fio
NATIVE_OPTIONS=delalloc,rnvpc=off,rnvpc_extend=off,rtio=off

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }
fail() { echo "PRELIM_ERROR case=$CASE message=$1"; exit 2; }

case "$MODE" in
    ext4) OPTIONS=$NATIVE_OPTIONS ;;
    rnvpc) OPTIONS=delalloc,rnvpc=on,rnvpc_extend=off,rtio=on ;;
    rnvpce) OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on ;;
    *) fail "unknown-mode" ;;
esac

[ "$EXPECTED_BYTES" -eq "$(((TOTAL_MIB / JOBS) * JOBS * 1048576))" ] || \
    fail "expected-byte-contract"
[ "$DISK_JOURNAL" -eq "$RUNTIME_JOURNAL" ] || fail "journal-contract"

mkdir -p "$MNT"
mount -t ext4 -o "$NATIVE_OPTIONS" /dev/vdc "$MNT"
mkdir -p "$MNT/data"
prepare_status=0
prepare_bytes=0
if [ "$WORKLOAD" = randwrite ]; then
    echo "PRELIM_PREPARE_START $CASE"
    "$FIO" --name=prepare --rw=write --bs=128k --ioengine=psync \
        --direct=1 --numjobs="$JOBS" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="$((TOTAL_MIB / JOBS))M" --fallocate=none --output-format=json \
        || prepare_status=$?
    sync
    prepare_bytes=$EXPECTED_BYTES
fi
umount "$MNT"
echo "PRELIM_PREPARE_COMPLETE $CASE bytes=$prepare_bytes status=$prepare_status"
[ "$prepare_status" -eq 0 ] || exit "$prepare_status"

mount -t ext4 -o "$OPTIONS" /dev/vdc "$MNT"
echo "PRELIM_CONFIG case=$CASE mode=$MODE axis=$AXIS workload=$WORKLOAD bs=$BS jobs=$JOBS total_mib=$TOTAL_MIB expected_bytes=$EXPECTED_BYTES"
# The disk feature is checked by the host. Runtime journal state follows the
# mounted mode and the verified feature: native loads JBD2; nojournal rNVPC
# modes cannot instantiate it.
echo "PRELIM_JOURNAL case=$CASE disk=$DISK_JOURNAL runtime=$RUNTIME_JOURNAL source=verified-disk-and-mount-contract"
echo "PRELIM_MOUNT case=$CASE options=$OPTIONS"
cat /proc/cmdline

T0=$(now_ns)
echo "PRELIM_FIO_START $CASE"
status=0
"$FIO" --name="$CASE" --rw="$WORKLOAD" --bs="$BS" --ioengine=psync \
    --direct=0 --fsync=0 --numjobs="$JOBS" --thread=1 --iodepth=1 \
    --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
    --directory="$MNT/data" --filename_format='f.$jobnum' \
    --size="$((TOTAL_MIB / JOBS))M" --filesize="$((TOTAL_MIB / JOBS))M" \
    --fallocate=none --randrepeat=1 --randseed=20261030 \
    --runtime="${RUNTIME:-1200}" --output-format=json || status=$?
echo "PRELIM_FIO_END $CASE"
T1=$(now_ns)
sync
T2=$(now_ns)
umount_status=0
umount "$MNT" || umount_status=$?
echo "PRELIM_COMPLETE $CASE fio_status=$status start_ns=$T0 fio_end_ns=$T1 sync_end_ns=$T2 umount_status=$umount_status"

