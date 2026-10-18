#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# EXT4 性能矩阵（32 点）guest 脚本
# =================================
# Runs inside the QEMU benchmark VM (rdinit=/benchmark/common/bench_runner.sh).
#
# 参数：run.sh <mod> <backend> <workload> <njobs> <writetype> [runtime] [ramp]
#   mod       : default | nojournal      (mke2fs 特性集差异)
#   backend   : sata | nvme              (sata → /dev/vdc virtio-blk img；
#                                          nvme → /dev/nvme0n1 nvme img)
#   workload  : random-4k | sequential-128k
#   njobs     : 1 | 16
#   writetype : buffer | fsync           (fsync → --fsync=1；buffer → 无周期 fsync)
#   runtime   : fio time_based runtime（默认 30 秒）
#   ramp      : fio ramp_time（默认 5 秒）
#
# 固定配置：writeback_capacity_mb=64（mount option，os_com 宽松解析会忽略未知
# 选项——当前 os_com 002 后台写回为周期 1s，无 dirty 水位参数）、size=256M。
#
# size=256M 是【所有 job 文件大小之和】：每个 job 独占一个文件（filename 含
# $jobnum），每 job 文件大小 = 256M / numjobs（nj16 时每 job 16M）。
#
# random-4k 先 precondition（每个 job 顺序写满自己的文件，总和 256M）再
# overwrite 随机写，排除首次分配的干扰；sequential-128k 直接顺序写满 256M。
# 两者均 --time_based=1。

set -e

MOD="${1:-default}"
BACKEND="${2:-sata}"
WORKLOAD="${3:-random-4k}"
NUMJOBS="${4:-1}"
WRITETYPE="${5:-fsync}"
RUNTIME="${6:-30}"
RAMP="${7:-5}"

SIZE="256M"
TEST_FILE_DIR="/ext4"
FIO="/benchmark/bin/fio"
MKE2FS="/benchmark/fio/ext4_perf_matrix/mkfs/mke2fs"

# size=256M 是【所有 job 文件大小之和】：每个 job 独占一个文件
# （filename 含 $jobnum），每 job 文件大小 = 256M / numjobs。
case "$SIZE" in
    *M) SIZE_BYTES=$(( ${SIZE%M} * 1024 * 1024 )) ;;
    *G) SIZE_BYTES=$(( ${SIZE%G} * 1024 * 1024 * 1024 )) ;;
    *K) SIZE_BYTES=$(( ${SIZE%K} * 1024 )) ;;
    *) SIZE_BYTES=$SIZE ;;
esac
PER_JOB_SIZE=$(( SIZE_BYTES / NUMJOBS ))

# backend → guest 块设备映射（真实盘直通）：
#   sata(/dev/sda4) 与 nvme(/dev/nvme3n1p2) 都通过 XFSTESTS_TEST_IMAGE 直通为
#   guest 的 virtio-blk /dev/vdc；backend 仅用于结果命名。
DEV="/dev/vdc"

if [ "$MOD" = "nojournal" ]; then
    MKE2FS_ARGS="-F -q -t ext4 -b 4096 -O ^has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg"
else
    MKE2FS_ARGS="-F -q -t ext4 -b 4096 -O has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg"
fi

case "$WORKLOAD" in
    random-4k) RW=randwrite; BS=4K; PRECOND=1 ;;
    sequential-128k) RW=write; BS=128K; PRECOND=0 ;;
    *) echo "unknown workload: $WORKLOAD" >&2; exit 2 ;;
esac

SYNC_ARG=""
if [ "$WRITETYPE" = "fsync" ]; then
    SYNC_ARG="--fsync=1"
fi

mkdir -p /ext4

reset_disk() {
    umount /ext4 2>/dev/null || true
    if [ -x "${MKE2FS}" ]; then
        "${MKE2FS}" ${MKE2FS_ARGS} "$DEV"
    else
        echo "WARNING: static mke2fs missing at ${MKE2FS}" >&2
    fi
    # 矩阵固定配置：writeback_capacity_mb=64。os_com 宽松解析忽略未知 option，
    # 当前 002 后台写回是周期 1s 触发（无 dirty 水位）。
    mount -t ext4 -o writeback_capacity_mb=64 "$DEV" /ext4
}

precondition() {
    echo "[matrix] precondition_begin name=$WORKLOAD"
    $FIO --name="prepare-$WORKLOAD" --directory="$TEST_FILE_DIR" \
        --size="$PER_JOB_SIZE" --ioengine=sync --direct=0 --iodepth=1 \
        --rw=write --bs=128k --numjobs="$NUMJOBS" --nrfiles=1 \
        --filename_format='perf.$jobnum.$filenum' --fallocate=none --fsync_on_close=1
    sync
    echo "[matrix] precondition_end name=$WORKLOAD"
}

reset_disk
[ "$PRECOND" = "1" ] && precondition

echo "=== FIO_RESULT: ${WORKLOAD}_nj${NUMJOBS}_${WRITETYPE}_${MOD}_${BACKEND} ==="
echo "=== FIO_DEV: ${DEV} ==="
echo "=== FIO_CMD: -rw=${RW} -bs=${BS} -numjobs=${NUMJOBS} -size=${SIZE}(total) -per_job_size=${PER_JOB_SIZE}B -time_based=1 -runtime=${RUNTIME} -ramp_time=${RAMP} ${SYNC_ARG} ==="
$FIO --rw="$RW" --directory="$TEST_FILE_DIR" \
    --name="m-$WORKLOAD-nj$NUMJOBS-$WRITETYPE-$MOD" \
    --size="$PER_JOB_SIZE" --bs="$BS" --numjobs="$NUMJOBS" --ioengine=sync --direct=0 \
    --iodepth=1 --group_reporting=1 --nrfiles=1 \
    --filename_format='perf.$jobnum.$filenum' --fallocate=none \
    --percentile_list=50:90:99:99.9 \
    ${SYNC_ARG} --overwrite=1 --time_based=1 --runtime="$RUNTIME" --ramp_time="$RAMP"
echo "=== END_FIO_RESULT ==="

umount /ext4
echo ""
echo "=== EXT4_PERF_MATRIX_COMPLETE ==="
