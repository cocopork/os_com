#!/bin/bash
set -uo pipefail

cmdline_value() {
    local key=$1 token
    for token in $(cat /proc/cmdline); do
        case "$token" in
            "$key"=*) printf '%s\n' "${token#*=}"; return 0 ;;
        esac
    done
    return 1
}

case_name=$(cmdline_value mixed_case)
device=$(cmdline_value mixed_device)
read_percent=$(cmdline_value read_percent)
sync_percent=$(cmdline_value sync_percent)
repeat=$(cmdline_value repeat)
target=/dev/vdb
mountpoint=/mnt/test
data_dir=$mountpoint/data
result=/tmp/mixed-result.json
status=0

echo "MIXED_SYNC_BEGIN case=$case_name"
mkfs.ext4 -F -b 4096 "$target" || status=1
if [[ $status -eq 0 ]]; then
    mount -t ext4 "$target" "$mountpoint" || status=1
fi
if [[ $status -eq 0 ]]; then
    mkdir -p "$data_dir"
    for thread in $(seq 0 7); do
        echo "ACTION PREWRITE_FILE thread=$thread path=$data_dir/f.$thread bytes=335544320"
        dd if=/dev/zero of="$data_dir/f.$thread" bs=1M count=320 conv=fsync status=none || status=1
    done
    sync || status=1
    umount "$mountpoint" || status=1
fi
if [[ $status -eq 0 ]]; then
    mount -t ext4 "$target" "$mountpoint" || status=1
    nvpcctl open "$mountpoint" strict || status=1
fi

echo "MIXED_SYNC_CONFIG case=$case_name device=$device mode=nvlog read_percent=$read_percent sync_percent=$sync_percent repeat=$repeat threads=8 file_mib=320 dataset_mib=2560 seed=20261119 warmup_seconds=10 runtime_seconds=30 cache_control=memcg-approx configured_dram_cache_mib=128 configured_nv_data_cache_mib=128 configured_total_data_cache_mib=256"

if [[ $status -eq 0 ]]; then
    mkdir -p /sys/fs/cgroup
    mountpoint -q /sys/fs/cgroup || mount -t cgroup2 none /sys/fs/cgroup || status=1
    echo "MIXED_SYNC_CGROUP_MOUNTS"
    grep cgroup /proc/self/mountinfo || true
    echo "MIXED_SYNC_CGROUP_CONTROLLERS=$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || true)"
    echo +memory >/sys/fs/cgroup/cgroup.subtree_control || status=1
    echo "MIXED_SYNC_CGROUP_SUBTREE=$(cat /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true)"
fi
if [[ $status -eq 0 ]]; then
    mkdir -p /sys/fs/cgroup/nvlog-benchmark
    echo $((136 * 1024 * 1024)) >/sys/fs/cgroup/nvlog-benchmark/memory.high || status=1
    echo $((512 * 1024 * 1024)) >/sys/fs/cgroup/nvlog-benchmark/memory.max || status=1
    echo 0 >/sys/fs/cgroup/nvlog-benchmark/memory.swap.max || status=1
    sync
    echo 3 >/proc/sys/vm/drop_caches
    # Keep warning/error diagnostics on the serial console while preventing
    # normal kernel info messages from being interleaved with result JSON.
    dmesg -n 4
fi

benchmark_status=0
if [[ $status -eq 0 ]]; then
    bash -c 'echo $$ > /sys/fs/cgroup/nvlog-benchmark/cgroup.procs; exec taskset -c 0-7 /usr/local/bin/mixed_sync_bench "$@"' bash \
        --directory "$data_dir" --threads 8 --file-bytes 335544320 \
        --read-percent "$read_percent" --sync-percent "$sync_percent" \
        --seed 20261119 --warmup-seconds 10 --runtime-seconds 30 \
        >"$result" &
    benchmark_pid=$!
    while kill -0 "$benchmark_pid" 2>/dev/null; do
        printf 'MIXED_SYNC_CACHE_SAMPLE time=%(%s)T ' -1
        awk '$1 == "anon" || $1 == "file" || $1 == "file_dirty" || $1 == "file_writeback" { printf "%s=%s ", $1, $2 } END { print "" }' \
            /sys/fs/cgroup/nvlog-benchmark/memory.stat
        sleep 1
    done
    wait "$benchmark_pid" || benchmark_status=$?
else
    benchmark_status=1
fi

echo "MIXED_SYNC_JSON_BEGIN case=$case_name"
[[ -f $result ]] && cat "$result"
echo "MIXED_SYNC_JSON_END case=$case_name"
awk '{ printf "MIXED_SYNC_MEMORY_EVENT_%s=%s\n", toupper($1), $2 }' \
    /sys/fs/cgroup/nvlog-benchmark/memory.events
nvpcctl usage || true

sync_status=0
sync || sync_status=$?
close_status=0
nvpcctl close "$mountpoint" || close_status=$?
umount_status=0
umount "$mountpoint" || umount_status=$?
echo "MIXED_SYNC_COMPLETE case=$case_name benchmark_status=$benchmark_status sync_status=$sync_status close_status=$close_status umount_status=$umount_status"

if [[ $benchmark_status -eq 0 && $sync_status -eq 0 && $close_status -eq 0 && $umount_status -eq 0 ]]; then
    echo "MIXED_SYNC_PASS case=$case_name"
    exit 0
fi
echo "MIXED_SYNC_FAIL case=$case_name"
exit 1
