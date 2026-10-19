#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# EXT4 journal vs nojournal 对比基准（标准测试）
# ===============================================
# Runs inside the QEMU benchmark VM (rdinit=/benchmark/common/bench_runner.sh).
# 参数：run.sh <rw> <bs> <numjobs> <journal_mode> <writetype> [runtime_limit]
#   rw            : write | randwrite
#   bs            : 4K | 128K
#   numjobs       : 1 | 4 | 16
#   journal_mode  : journal | nojournal
#   writetype     : buffer | direct | fsync
#       buffer → direct=0, fsync=0（buffered 无 fsync）
#       direct → direct=1, fsync=0（O_DIRECT 绕过 page cache）
#       fsync  → direct=0, fsync=1（buffered + 每写后 fsync）
#   runtime_limit : fio 时间上限（秒，默认 60）
#
# 每个 job 写满 128M 即停（size-based）。每个组合由独立 VM boot 运行（方案 B）。
#
# 输出用括号标记，供宿主解析：
#     === FIO_RESULT: <rw>_<bs>_nj<numjobs>_<journal_mode>_<writetype> ===
#     ... raw fio output ...
#     === END_FIO_RESULT ===

set -e

RW="${1:-write}"
BS="${2:-128K}"
NUMJOBS="${3:-1}"
JOURNAL_MODE="${4:-journal}"
WRITETYPE="${5:-fsync}"
# 第 6 个参数：fio runtime 时间上限（秒）。默认 60。
# 死锁复现需长跑（旧第 5 轮无 runtime 上限，约 30 分钟才触发）。
RUNTIME_LIMIT="${6:-60}"
# 第 7 个参数：prealloc_blocks=N（0 = 关闭）。写进 FIO_RESULT 标记，
# 同一轮次的多组 prealloc 配置才不会被宿主侧当成重复轮次。
PREALLOC_BLOCKS="${7:-0}"
# 第 8 个参数：nrfiles（每个 job 的文件数）。写进 FIO_RESULT 标记。
# 总数据量固定 256MB：每 job size = 256MB / numjobs，由 nrfiles 个文件平分。
NRFILES="${8:-1}"

TEST_FILE_DIR="/ext4"
FIO="/benchmark/bin/fio"
MKE2FS="/benchmark/fio/ext4_journal_compare/mkfs/mke2fs"

# journal / nojournal 特性集：nojournal 去掉 has_journal 位。
# 其余特性与 ext4_buffered_sweep 一致，保证 Asterinas 与 Linux 都能挂载。
if [ "$JOURNAL_MODE" = "nojournal" ]; then
    MKE2FS_ARGS="-F -q -t ext4 -b 4096 -O ^has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg"
else
    MKE2FS_ARGS="-F -q -t ext4 -b 4096 -O has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg"
fi

# writetype → direct / fsync 开关
case "$WRITETYPE" in
    buffer) DIRECT=0; FSYNC=0 ;;
    direct) DIRECT=1; FSYNC=0 ;;
    fsync|*) DIRECT=0; FSYNC=1 ;;
esac

# fallocate=none 保持 delalloc 路径诚实。fsync 开关按 writetype。
FIO_COMMON="-ioengine=psync -direct=${DIRECT} -numjobs=${NUMJOBS} --group_reporting --fallocate=none"
if [ "$FSYNC" = "1" ]; then
    FIO_COMMON="${FIO_COMMON} --fsync=1"
fi

# 每文件固定 4MB（extent 树复杂度一致），time_based 跑满 runtime。
# size = 4MB × nrfiles 作为一轮的循环单位，写满后重新随机写，
# 60s 内持续写（覆盖）+ writeback 持续参与，测稳态带宽。
FILE_SIZE_MB="4"
SIZE_PER_JOB="$((FILE_SIZE_MB * NRFILES))M"

mkdir -p /ext4

reset_disk() {
    umount /ext4 2>/dev/null || true
    if [ -x "${MKE2FS}" ]; then
        "${MKE2FS}" ${MKE2FS_ARGS} /dev/vdc
    else
        echo "WARNING: static mke2fs missing at ${MKE2FS}; fallback to rm-clean" >&2
        rm -f "${TEST_FILE_DIR}"/* 2>/dev/null || true
    fi
    # writeback_capacity_mb=N 启用 PageCache 脏页容量上限与前台 write 节流：
    # 脏页超 high 水线时前台 write 阻塞，等后台 writeback 刷盘释放容量。
    # 不配置时水线全为 0，throttle 为 no-op，buffer 写会一路写满内存（带宽
    # 远超设备上限）。
    # prealloc_blocks=N 启用 delalloc 写回预分配：每个 dirty 范围的上限向上
    # 对齐到 N 块，超额部分保持 unwritten，后续落在同一窗口内的写命中
    # 预分配不再进分配器。0（默认）= 关闭。
    mount -t ext4 \
        -o writeback_capacity_mb="${WRITEBACK_CAPACITY_MB:-64},prealloc_blocks=${PREALLOC_BLOCKS}" \
        /dev/vdc /ext4
}

reset_disk

echo "=== FIO_RESULT: ${RW}_${BS}_nj${NUMJOBS}_${JOURNAL_MODE}_${WRITETYPE}_pa${PREALLOC_BLOCKS}_nf${NRFILES} ==="
# fio 以 JSON 输出（--output-format=json），宿主侧 parse_summary.py 解析该 JSON
# 生成 summary.csv。JSON 走 stdio chardev → make run_kernel stdout → journal log。
$FIO --output-format=json -rw="${RW}" -directory="${TEST_FILE_DIR}" \
    -name="jc-${RW}-${BS}-nj${NUMJOBS}-${JOURNAL_MODE}-${WRITETYPE}" \
    -size="${SIZE_PER_JOB}" -bs="${BS}" -nrfiles="${NRFILES}" \
    -filesize="${FILE_SIZE_MB}M" --time_based=1 \
    ${FIO_COMMON} -runtime="${RUNTIME_LIMIT}"
echo "=== END_FIO_RESULT ==="

umount /ext4

echo ""
echo "=== EXT4_JOURNAL_COMPARE_COMPLETE ==="
