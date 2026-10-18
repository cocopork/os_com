#!/bin/sh
# SPDX-License-Identifier: MPL-2.0

set -eu

PROFILE="${1:-ext4-orderless}"
DATA_MB="${2:-32}"
REPEAT="${3:-1}"

DEV="/dev/vdc"
MNT="/mnt/remount"
TEST_FILE="${MNT}/payload"
FIO="/benchmark/bin/fio"
MKE2FS="/benchmark/fio/aster_ext4_remount/mkfs/mke2fs"
MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"

case "$PROFILE" in
    ext4-orderless)
        MOUNT_OPTS="writeback_capacity_mb=64"
        ;;
    ext4-rNVPCe)
        MOUNT_OPTS="writeback_capacity_mb=64,rnvpc_extend=on,rtio=on"
        ;;
    *)
        echo "error: unknown profile '$PROFILE'" >&2
        exit 2
        ;;
esac

now_ns() {
    value="$(date +%s%N 2>/dev/null || true)"
    case "$value" in
        *N*|"") awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime ;;
        *) echo "$value" ;;
    esac
}

mkdir -p "$MNT"
umount "$MNT" 2>/dev/null || true
"$MKE2FS" $MKE2FS_ARGS "$DEV"
mount -t ext4 -o "$MOUNT_OPTS" "$DEV" "$MNT"

"$FIO" \
    --name=prepare-remount \
    --ioengine=psync \
    --thread=1 \
    --numjobs=1 \
    --iodepth=1 \
    --direct=0 \
    --fsync=0 \
    --rw=write \
    --bs=4k \
    --size="${DATA_MB}M" \
    --filename="$TEST_FILE" \
    --verify=crc32c \
    --do_verify=0 \
    --fallocate=none \
    --create_on_open=1 \
    --output-format=normal

# This is deliberately a clean-remount experiment: unmount drains all dirty state.
umount "$MNT"
echo "=== REMOUNT_FIRST_UNMOUNT_COMPLETE profile=${PROFILE} data_mb=${DATA_MB} repeat=${REPEAT} ==="

echo "=== REMOUNT_PROFILE_BEGIN profile=${PROFILE} data_mb=${DATA_MB} repeat=${REPEAT} ==="
START_NS="$(now_ns)"
mount -t ext4 -o "${MOUNT_OPTS},mount_profile=on" "$DEV" "$MNT"
END_NS="$(now_ns)"

EXPECTED_SIZE=$((DATA_MB * 1024 * 1024))
ACTUAL_SIZE="$(stat -c %s "$TEST_FILE" 2>/dev/null || echo 0)"
VALID=0
[ "$ACTUAL_SIZE" -eq "$EXPECTED_SIZE" ] && VALID=1

VERIFY_OK=0
if [ "$VALID" -eq 1 ] && "$FIO" \
    --name=verify-remount \
    --ioengine=psync \
    --thread=1 \
    --numjobs=1 \
    --iodepth=1 \
    --direct=0 \
    --rw=read \
    --bs=4k \
    --size="${DATA_MB}M" \
    --filename="$TEST_FILE" \
    --verify=crc32c \
    --do_verify=1 \
    --output-format=normal; then
    VERIFY_OK=1
fi

echo "=== REMOUNT_RESULT profile=${PROFILE} data_mb=${DATA_MB} repeat=${REPEAT} remount_ns=$((END_NS - START_NS)) file_size=${ACTUAL_SIZE} valid=${VALID} verify_ok=${VERIFY_OK} ==="

umount "$MNT"
echo "=== REMOUNT_FINAL_UNMOUNT_COMPLETE profile=${PROFILE} data_mb=${DATA_MB} repeat=${REPEAT} ==="
echo "=== REMOUNT_RECOVERY_COMPLETE profile=${PROFILE} data_mb=${DATA_MB} repeat=${REPEAT} valid=${VALID} verify_ok=${VERIFY_OK} ==="

[ "$VALID" -eq 1 ] && [ "$VERIFY_OK" -eq 1 ]
