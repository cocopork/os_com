#!/bin/bash
set -uo pipefail

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
exec >/dev/console 2>&1

cmdline_value() {
    local key=$1 token
    for token in $(cat /proc/cmdline); do
        case "$token" in "$key"=*) printf '%s\n' "${token#*=}"; return 0 ;; esac
    done
    return 1
}

target=/dev/vdb
mountpoint=/mnt/test
data_dir=$mountpoint/data
fio_json=/tmp/cold-read.json
status=0

mkdir -p /proc /sys /dev /run /tmp "$mountpoint"
mountpoint -q /proc || mount -t proc proc /proc
mountpoint -q /sys || mount -t sysfs sysfs /sys
mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev
mountpoint -q /run || mount -t tmpfs tmpfs /run
mountpoint -q /tmp || mount -t tmpfs tmpfs /tmp
udevadm trigger --action=add || true
udevadm settle --timeout=20 || true

case_name=$(cmdline_value cold_case)
bs=$(cmdline_value cold_bs)
jobs=$(cmdline_value cold_jobs)
per_file_mib=$(cmdline_value cold_per_file_mib)
expected_bytes=$(cmdline_value cold_expected_bytes)

echo "COLD_READ_BEGIN case=$case_name bs=$bs jobs=$jobs per_file_mib=$per_file_mib"
mount -t ext4 "$target" "$mountpoint" || status=1
mkdir -p "$data_dir"
if [[ $status -eq 0 ]]; then
    for file_id in $(seq 0 $((jobs - 1))); do
        fio --name="prepare-$file_id" --rw=write --bs=128k --ioengine=psync \
            --direct=1 --numjobs=1 --thread=1 --iodepth=1 \
            --filename="$data_dir/f.$file_id" --size="${per_file_mib}M" \
            --fallocate=none --output-format=json >"/tmp/prepare-$file_id.json" || status=1
        [[ $status -eq 0 ]] || break
    done
    sync || status=1
    umount "$mountpoint" || status=1
fi

if [[ $status -eq 0 ]]; then
    mount -t ext4 "$target" "$mountpoint" || status=1
    mkdir -p /sys/fs/cgroup
    mountpoint -q /sys/fs/cgroup || mount -t cgroup2 none /sys/fs/cgroup || status=1
    echo +memory >/sys/fs/cgroup/cgroup.subtree_control || status=1
    mkdir -p /sys/fs/cgroup/cold-read
    echo $((136 * 1024 * 1024)) >/sys/fs/cgroup/cold-read/memory.high || status=1
    echo $((512 * 1024 * 1024)) >/sys/fs/cgroup/cold-read/memory.max || status=1
    echo 0 >/sys/fs/cgroup/cold-read/memory.swap.max || status=1
    sync
    echo 3 >/proc/sys/vm/drop_caches
fi

fio_status=0
if [[ $status -eq 0 ]]; then
    bash -c 'echo $$ > /sys/fs/cgroup/cold-read/cgroup.procs; exec "$@"' bash \
        taskset -c 0-31 fio --name="$case_name" --rw=randread --bs="$bs" \
        --ioengine=psync --direct=0 --fsync=0 --numjobs="$jobs" --thread=1 --iodepth=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split --group_reporting=1 \
        --directory="$data_dir" --filename_format='f.$jobnum' \
        --size="${per_file_mib}M" --filesize="${per_file_mib}M" \
        --fallocate=none --randrepeat=1 --randseed=20261117 \
        --output-format=json --output="$fio_json" || fio_status=$?
else
    fio_status=1
fi

echo "COLD_READ_FIO_START $case_name"
[[ -f $fio_json ]] && cat "$fio_json"
echo "COLD_READ_FIO_END $case_name"
umount_status=0
umount "$mountpoint" || umount_status=$?
echo "COLD_READ_COMPLETE case=$case_name fio_status=$fio_status umount_status=$umount_status expected_bytes=$expected_bytes"
sync
poweroff -f
sleep 10
echo o >/proc/sysrq-trigger
