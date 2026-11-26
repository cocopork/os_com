#!/bin/bash
set -uo pipefail

cmdline_value() {
    local key=$1 token
    for token in $(cat /proc/cmdline); do
        case "$token" in "$key"=*) printf '%s\n' "${token#*=}"; return 0 ;; esac
    done
    return 1
}

case_name=$(cmdline_value fio_case)
device=$(cmdline_value fio_device)
workload=$(cmdline_value fio_workload)
axis=$(cmdline_value fio_axis)
bs=$(cmdline_value fio_bs)
jobs=$(cmdline_value fio_jobs)
per_file_mib=$(cmdline_value fio_per_file_mib)
expected_bytes=$(cmdline_value fio_expected_bytes)
prepare=$(cmdline_value fio_prepare)
repeat=$(cmdline_value fio_repeat)
runtime_seconds=$(cmdline_value fio_runtime_seconds)
fsync_blocks=$(cmdline_value fio_fsync_blocks)
target=/dev/vdb
mountpoint=/mnt/test
data_dir=$mountpoint/data
fio_json=/tmp/full-e2e-fio.json
status=0

case "$workload" in
    overwrite-randwrite) fio_rw=randwrite ;;
    append-write) fio_rw=write ;;
    cold-randread) fio_rw=randread ;;
    *) echo "FULL_E2E_ERROR case=$case_name message=invalid-workload"; exit 2 ;;
esac

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }

echo "FULL_E2E_BEGIN case=$case_name"
if [[ $prepare -eq 1 ]]; then
    mount -t ext4 "$target" "$mountpoint" || status=1
    mkdir -p "$data_dir"
    for file_id in $(seq 0 $((jobs - 1))); do
        fio --name="prepare-$file_id" --rw=write --bs=128k --ioengine=psync \
            --direct=1 --numjobs=1 --thread=1 --iodepth=1 \
            --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
            --filename="$data_dir/f.$file_id" --size="${per_file_mib}M" \
            --fallocate=none --output-format=json \
            >"/tmp/full-e2e-prepare-$file_id.json" || status=1
        [[ $status -eq 0 ]] || break
    done
    sync || status=1
    umount "$mountpoint" || status=1
fi

if [[ $status -eq 0 ]]; then
    mount -t ext4 "$target" "$mountpoint" || status=1
    nvpcctl open "$mountpoint" strict || status=1
fi

echo "FULL_E2E_CONFIG case=$case_name device=$device mode=nvlog workload=$workload axis=$axis bs=$bs jobs=$jobs per_file_mib=$per_file_mib expected_bytes=$expected_bytes repeat=$repeat fsync_blocks=$fsync_blocks configured_dram_cache_mib=128 configured_nv_data_cache_mib=128 configured_total_data_cache_mib=256"

if [[ $status -eq 0 ]]; then
    mkdir -p /sys/fs/cgroup
    mountpoint -q /sys/fs/cgroup || mount -t cgroup2 none /sys/fs/cgroup || status=1
    echo +memory >/sys/fs/cgroup/cgroup.subtree_control || status=1
    mkdir -p /sys/fs/cgroup/nvlog-fio
    echo $((136 * 1024 * 1024)) >/sys/fs/cgroup/nvlog-fio/memory.high || status=1
    echo $((512 * 1024 * 1024)) >/sys/fs/cgroup/nvlog-fio/memory.max || status=1
    echo 0 >/sys/fs/cgroup/nvlog-fio/memory.swap.max || status=1
    sync
    echo 3 >/proc/sys/vm/drop_caches
    dmesg -n 4
fi

mkdir -p "$data_dir"
rm -f "$fio_json"
start_ns=$(now_ns)
fio_status=0
if [[ $status -eq 0 ]]; then
    fio_runtime_args=(--size="${per_file_mib}M" --filesize="${per_file_mib}M")
    fio_launcher=(taskset -c 0-31 fio)
    if [[ $runtime_seconds -gt 0 ]]; then
        fio_runtime_args=(--size="${per_file_mib}M" --time_based --runtime="${runtime_seconds}s")
        fio_launcher=(timeout --preserve-status --signal=INT --kill-after=30 "${runtime_seconds}s" taskset -c 0-31 fio)
    fi
    bash -c 'echo $$ > /sys/fs/cgroup/nvlog-fio/cgroup.procs; exec "$@"' bash \
        "${fio_launcher[@]}" \
        --name="$case_name" --rw="$fio_rw" --bs="$bs" --ioengine=psync \
        --direct=0 --fsync="$fsync_blocks" --numjobs="$jobs" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$data_dir" --filename_format='f.$jobnum' \
        "${fio_runtime_args[@]}" \
        --fallocate=none --create_on_open=1 --randrepeat=1 --randseed=20261117 \
        --output-format=json --output="$fio_json" &
    fio_pid=$!
    while kill -0 "$fio_pid" 2>/dev/null; do
        printf 'FULL_E2E_CACHE_SAMPLE time=%(%s)T ' -1
        awk '$1 == "anon" || $1 == "file" || $1 == "file_dirty" || $1 == "file_writeback" { printf "%s=%s ", $1, $2 } END { print "" }' \
            /sys/fs/cgroup/nvlog-fio/memory.stat
        nvpcctl usage || true
        sleep 1
    done
    wait "$fio_pid" || fio_status=$?
else
    fio_status=1
fi
fio_end_ns=$(now_ns)

sync_status=0
if [[ $fio_rw != randread ]]; then sync || sync_status=$?; fi
sync_end_ns=$(now_ns)

echo "FULL_E2E_FIO_START $case_name"
[[ -f $fio_json ]] && cat "$fio_json"
echo "FULL_E2E_FIO_END $case_name"
awk '{ printf "FULL_E2E_MEMORY_EVENT_%s=%s\n", toupper($1), $2 }' \
    /sys/fs/cgroup/nvlog-fio/memory.events
nvpcctl usage || true

close_status=0
nvpcctl close "$mountpoint" || close_status=$?
umount_status=0
umount "$mountpoint" || umount_status=$?
echo "FULL_E2E_COMPLETE case=$case_name fio_status=$fio_status sync_status=$sync_status close_status=$close_status umount_status=$umount_status start_ns=$start_ns fio_end_ns=$fio_end_ns sync_end_ns=$sync_end_ns"

if [[ $fio_status -eq 0 && $sync_status -eq 0 && $close_status -eq 0 && $umount_status -eq 0 ]]; then
    echo "FULL_E2E_PASS case=$case_name"
    exit 0
fi
echo "FULL_E2E_FAIL case=$case_name"
exit 1
