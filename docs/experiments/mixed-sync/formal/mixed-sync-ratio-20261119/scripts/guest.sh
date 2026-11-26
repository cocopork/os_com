#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

CONFIG_PATH=${CONFIG_PATH:-/ext2/mixed-sync.cfg}
BENCHMARK=${BENCHMARK:-/ext2/mixed_sync_bench}
MOUNT_POINT=${MOUNT_POINT:-/mixed-sync-mnt}
TARGET_DEVICE=${TARGET_DEVICE:-/dev/vdc}
RESULT_JSON=${RESULT_JSON:-/tmp/mixed-sync-result.json}
MIXED_SYNC_AUDIT=${MIXED_SYNC_AUDIT:-0}

. "$CONFIG_PATH"

fail() {
    echo "MIXED_SYNC_ERROR case=$CASE message=$1"
    exit 2
}

action() {
    echo "ACTION $*"
}

mount_filesystem() {
    phase=$1
    filesystem=$2
    options=$3
    action "${phase}_MOUNT type=$filesystem device=$TARGET_DEVICE options=${options:-none}"
    [ "$MIXED_SYNC_AUDIT" -eq 1 ] && return 0
    if [ -n "$options" ]; then
        mount -t "$filesystem" -o "$options" "$TARGET_DEVICE" "$MOUNT_POINT"
    else
        mount -t "$filesystem" "$TARGET_DEVICE" "$MOUNT_POINT"
    fi
}

do_sync() {
    phase=$1
    action "${phase}_SYNC"
    [ "$MIXED_SYNC_AUDIT" -eq 1 ] || sync
}

do_umount() {
    phase=$1
    action "${phase}_UMOUNT"
    [ "$MIXED_SYNC_AUDIT" -eq 1 ] || umount "$MOUNT_POINT"
}

prewrite_file() {
    thread_id=$1
    path="$MOUNT_POINT/data/f.$thread_id"
    action "PREWRITE_FILE thread=$thread_id path=$path bytes=$FILE_BYTES"
    [ "$MIXED_SYNC_AUDIT" -eq 1 ] && return 0
    dd if=/dev/zero of="$path" bs=1048576 count="$FILE_MIB" conv=fsync status=none
    actual_bytes=$(stat -c %s "$path")
    [ "$actual_bytes" -eq "$FILE_BYTES" ] || fail "prewrite-size-$thread_id"
    allocated_blocks=$(stat -c %b "$path")
    [ "$((allocated_blocks * 512))" -ge "$FILE_BYTES" ] || \
        fail "prewrite-allocation-$thread_id"
}

case "$MODE" in
    ext2)
        PREPARE_TYPE=ext2
        PREPARE_OPTIONS=
        MEASURE_TYPE=ext2
        MEASURE_OPTIONS=
        CACHE_CONTROL=uncontrolled
        ;;
    ext4)
        PREPARE_TYPE=ext4
        PREPARE_OPTIONS="delalloc,rnvpc=off,rnvpc_extend=off,rtio=off,pagecache_capacity_mb=256"
        MEASURE_TYPE=ext4
        MEASURE_OPTIONS=$PREPARE_OPTIONS
        CACHE_CONTROL=strict
        ;;
    rnvpce)
        PREPARE_TYPE=ext4
        PREPARE_OPTIONS="delalloc,rnvpc=off,rnvpc_extend=off,rtio=off,pagecache_capacity_mb=256"
        MEASURE_TYPE=ext4
        MEASURE_OPTIONS="delalloc,rnvpc=off,rnvpc_extend=on,rtio=on,pagecache_capacity_mb=128"
        CACHE_CONTROL=strict
        ;;
    *) fail invalid-mode ;;
esac

[ "$THREADS" -eq 8 ] || fail invalid-thread-count
[ "$FILE_MIB" -eq 320 ] || fail invalid-file-size
[ "$DATASET_MIB" -eq 2560 ] || fail invalid-dataset-size
[ "$FILE_BYTES" -eq 335544320 ] || fail invalid-file-bytes
[ "$DRAM_CACHE_MIB" -eq 128 ] || [ "$DRAM_CACHE_MIB" -eq 256 ] || fail invalid-dram-cache
[ "$NV_DATA_CACHE_MIB" -eq 0 ] || [ "$NV_DATA_CACHE_MIB" -eq 128 ] || fail invalid-nv-cache

if [ "$MIXED_SYNC_AUDIT" -ne 1 ]; then
    mkdir -p "$MOUNT_POINT"
fi

mount_filesystem PREPARE "$PREPARE_TYPE" "$PREPARE_OPTIONS"
if [ "$MIXED_SYNC_AUDIT" -ne 1 ]; then
    mkdir -p "$MOUNT_POINT/data"
fi
thread_id=0
while [ "$thread_id" -lt "$THREADS" ]; do
    prewrite_file "$thread_id"
    thread_id=$((thread_id + 1))
done
do_sync PREPARE
do_umount PREPARE

mount_filesystem MEASURE "$MEASURE_TYPE" "$MEASURE_OPTIONS"
echo "MIXED_SYNC_CONFIG case=$CASE device=$DEVICE mode=$MODE read_percent=$READ_PERCENT sync_percent=$SYNC_PERCENT repeat=$REPEAT threads=$THREADS file_mib=$FILE_MIB dataset_mib=$DATASET_MIB seed=$SEED warmup_seconds=$WARMUP_SECONDS runtime_seconds=$RUNTIME_SECONDS cache_control=$CACHE_CONTROL configured_dram_cache_mib=$DRAM_CACHE_MIB configured_nv_data_cache_mib=$NV_DATA_CACHE_MIB configured_total_data_cache_mib=$((DRAM_CACHE_MIB + NV_DATA_CACHE_MIB))"
if [ "$CACHE_CONTROL" = uncontrolled ]; then
    echo "MIXED_SYNC_CACHE_NOTE mode=ext2 configured_total_data_cache_mib=256 actual_capacity_control=unavailable"
fi
if [ "$MIXED_SYNC_AUDIT" -ne 1 ]; then
    cat /proc/cmdline
    grep -E '^(MemTotal|MemFree|MemAvailable|Cached):' /proc/meminfo || true
fi

action "BENCHMARK cpus=0-7 directory=$MOUNT_POINT/data threads=$THREADS file_bytes=$FILE_BYTES read_percent=$READ_PERCENT sync_percent=$SYNC_PERCENT seed=$SEED warmup_seconds=$WARMUP_SECONDS runtime_seconds=$RUNTIME_SECONDS"
benchmark_status=0
if [ "$MIXED_SYNC_AUDIT" -ne 1 ]; then
    taskset -c 0-7 "$BENCHMARK" \
        --directory "$MOUNT_POINT/data" \
        --threads "$THREADS" \
        --file-bytes "$FILE_BYTES" \
        --read-percent "$READ_PERCENT" \
        --sync-percent "$SYNC_PERCENT" \
        --seed "$SEED" \
        --warmup-seconds "$WARMUP_SECONDS" \
        --runtime-seconds "$RUNTIME_SECONDS" \
        >"$RESULT_JSON" || benchmark_status=$?
    echo "MIXED_SYNC_JSON_BEGIN case=$CASE"
    cat "$RESULT_JSON"
    echo "MIXED_SYNC_JSON_END case=$CASE"
fi

sync_status=0
action FINAL_SYNC
if [ "$MIXED_SYNC_AUDIT" -ne 1 ]; then
    sync || sync_status=$?
fi
umount_status=0
action FINAL_UMOUNT
if [ "$MIXED_SYNC_AUDIT" -ne 1 ]; then
    umount "$MOUNT_POINT" || umount_status=$?
fi
echo "MIXED_SYNC_COMPLETE case=$CASE benchmark_status=$benchmark_status sync_status=$sync_status umount_status=$umount_status"
[ "$benchmark_status" -eq 0 ] && [ "$sync_status" -eq 0 ] && [ "$umount_status" -eq 0 ]
