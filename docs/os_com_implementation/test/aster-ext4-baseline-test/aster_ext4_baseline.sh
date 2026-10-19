#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Asterinas EXT4 多设备 / 多线程性能 guest 脚本（os_com 版）
# =========================================================
# 测试口径对齐 research1/motivation/test_dir/linux-ext4-baseline-test：
#   fs       : ext4（带 journal 的默认 ext4）
#   workload : randwrite-4k | seqwrite-4k
#   mode     : orderless（direct=0,fsync=0） | fsync（direct=0,fsync=1）
#   njobs    : 线程数（1,4,8,12,16,20,24,28,32）
#   runtime  : fio 时间上限（秒，默认 30，不 time_based，遍历一次文件范围）
#   file_range_mb : 总地址范围（MB，默认 8192，按 njobs 均分并对齐 4K）
#
# 各 fio job 使用独立文件（--directory + filename_format='f.$jobnum'），
# fallocate=none（包含真实数据块 + 元数据分配路径）。
# dirty budget：mount -o writeback_capacity_mb=256（对齐 Linux 的 dirty 256MiB）。
#
# 计时：从 fio 开始到 fio 返回 + 最终 `sync` 完成（完整排空），
# 宿主侧用 `drain_bw = fio write bytes / 排空时间` 作为持久化吞吐。
#
# 输出标记：
#     === FIO_RESULT: <workload>_<mode>_nj<njobs> ===
#     === PHASE: boot_ns=<内核启动→脚本开始> format_ns=<mke2fs> mount_ns=<挂载> ===
#     ... raw fio JSON ...
#     === END_FIO_RESULT ===
#     === DRAIN: busy_ns=<fio 耗时> drain_ns=<含排空总耗时> ===
#     === ASTER_BASELINE_COMPLETE ===
#
# PHASE 行仅作诊断：若某轮被宿主侧 timeout 杀掉，可用它区分卡在 fio 之前还是之后。

set -e

# 参数（宿主 run-baseline.sh 经 BENCHMARK_ARGS 传入）
#   $1 dev_tag        设备标签（sata|nvme）：只用于让 FIO_RESULT 标记自描述，
#                     宿主解析不再依赖轮次顺序猜设备
#   $2 fs             fs profile：ext4 | ext2like（见下方 case）
#   $3 workload       randwrite-4k | seqwrite-4k
#   $4 mode           orderless（direct=0,fsync=0） | fsync（direct=0,fsync=1）
#   $5 njobs          线程数
#   $6 runtime        fio 时间上限（秒，默认 30，不 time_based，遍历一次文件范围）
#   $7 file_range_mb  总地址范围（MB，默认 8192，按 njobs 均分并对齐 4K）
#   $8 njmb           nojournal 方案 A 的落盘作用域：on|owner（默认，= 2）按执行流分桶、
#                     global（= 1）旧实现（每次 end 全量落盘）、off（= 0）不缓冲。
#   $9 njwb           nojournal 元数据写回方式：on/async（默认，= 2 异步落盘）、
#                     sync（= 1 同步 flush，旧行为）、off（= 0 不缓冲）。
#                     两者都**仅 A/B 诊断用**，只对 ext2like（nojournal）有意义。
DEV_TAG="${1:-dut}"
FS="${2:-ext4}"
WORKLOAD="${3:-randwrite-4k}"
MODE="${4:-orderless}"
NUMJOBS="${5:-1}"
RUNTIME_LIMIT="${6:-30}"
FILE_RANGE_MB="${7:-8192}"
NJMB="${8:-on}"
NJWB="${9:-on}"
# $10 writeback_capacity_mb（dirty 预算 MB，默认 256）。
#   rNVPC/rNVPC_extend 下必须与内核 cmdline 的 `rnvpc_size` 对齐（容量额度一致），
#   例如 1/4 缩放档：WRCAP_MB=64 + `rnvpc_size=64M rnvpc_meta_reserve=16M
#   rnvpc_data_limit=48M`。
WRCAP_MB="${10:-256}"

FIO="/benchmark/bin/fio"
MKE2FS="/benchmark/fio/aster_ext4_baseline/mkfs/mke2fs"
MNT="/mnt/dut"
DEV="/dev/vdc"

# ── fs profile ────────────────────────────────────────
# 对齐 research1/.../linux-ext4-baseline-test 的 profiles：
#   ext4     = ext4 + journal + extent  ↔ Linux `ext4-{fsync,orderless}`
#   ext2like = ext4 去掉 journal        ↔ Linux `ext2-{fsync,orderless}`
#
# ⚠️ ext2like 有两处**已知偏差**（务必知悉，否则结果会被误导）：
#   1) **extent 关不掉**：本实现要求 INCOMPAT_EXTENTS 必须存在，`-O ^extent` 的镜像会被
#      拒挂载（`super_block.rs`: "ext4 image without the extents feature"）。
#      所以 block 映射仍是 extent，而不是 ext2 的间接块映射。
#   2) **nojournal 下元数据写回是同步设备 IO**：`begin_op` 在无 journal 时返回
#      `OpHandle::none()`，`write_back_inode_desc(None)` 直接对设备做 read-modify-write；
#      而 Linux 的 nojournal 元数据是 buffer-cache write-behind（异步）。实测 rand-4k
#      带宽 9.64 vs journal 194.82 MiB/s（20x 退化）——见
#      os_com_implementation/os_com_refactor_plan/008-nojournal-delalloc-meta-writeback。
#      ⇒ **修 008 之前，ext2like 的数据由这个缺陷主导，不能用来评判 fs 设计。**
#   只改「journal 开/关」一个变量（保留 delalloc）：这样与 ext4 profile 的差异只在 WAL，
#   对比才干净；也让 008 的缺陷（在 delalloc 路径上）被真实暴露。
#   （ext2 额外还有「无延迟分配」，如需一并模拟可用 `--fs ext2like` 的 MOUNT_OPTS 加
#     nodelalloc——但那会引入第二个变量，默认不开。）
case "$FS" in
    ext4)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"
        # 默认 256 MiB（对齐 Linux）。但显式给了 `--wr-capacity-mb` 时必须同样生效：
        # 否则与 rnvpc* 的对比会带一个 4× 的脏页预算差，而**节流正是 rNVPCe 前台
        # `write_total` 里占比最大的一项**（1/4 档实测 51%/87%）。
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB}"
        ;;
    ext2like)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB}"
        ;;
    rnvpc)
        # I4：rNVPC（WAL + CoW，数据页退休到 NV）。池容量由内核 cmdline
        # （`RNVPC_KCMD` → `rnvpc_size` 等）提供；未配置则挂载即 EINVAL。
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},rnvpc=on"
        ;;
    rnvpc_extend)
        # I4：rNVPC-extend（同上，另用 per-mount DRAM B-tree 作 residency 权威）。
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},rnvpc_extend=on"
        ;;
    ext4_nomerge)
        # 「无奈的妥协」隔离档：有 journal + **聚合关闭**（每页 4 KiB 一个 BIO）。
        # 用途：把"聚合"效应从 journal 里隔离出来（vs 第 4 档 ext4）。
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_merge=off"
        ;;
    ext2like_nomerge)
        # 「rNVPC 替身」：无 journal + 聚合关闭 + 回刷仍异步。
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_merge=off"
        ;;
    ext4_sync)
        # 「对照组」：有 journal + 回刷线程**每发一个 BIO 就等它完成**（per-BIO）。
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_sync_wait=on"
        ;;
    ext2like_sync)
        # 同上，无 journal。
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_sync_wait=on"
        ;;
    *)
        echo "[baseline] unknown_fs fs=$FS (expect ext4|ext2like|ext4_sync|ext2like_sync|ext4_nomerge|ext2like_nomerge|rnvpc|rnvpc_extend)" >&2
        exit 2
        ;;
esac

now_ns() { awk '{printf "%.0f", $1 * 1000000000}' /proc/uptime; }

# nojournal 方案 A（008）的落盘作用域：同一内核内可切换，跨臂比较不会混进构建差异。
#   2/owner 按执行流分桶（默认）——别人的 `end` 不再刷掉自己未改完的节点；
#   1/global 旧实现（每次 `end` 全量 drain），保留作为 A/B 对照；
#   0/off    不缓冲，回到「每次重写都同步写设备」的原始行为。
case "$NJMB" in
    on|owner|2)  NJMB_VALUE=2 ;;
    global|1)    NJMB_VALUE=1 ;;
    off|0)       NJMB_VALUE=0 ;;
    *)           NJMB_VALUE=2
                 echo "WARNING: unknown njmb='$NJMB', fallback to owner(2)" >&2 ;;
esac
# 即使取默认值也显式写进挂载选项：日志里的 PARAM 行就能直接看出本轮是哪只臂。
MOUNT_OPTS="${MOUNT_OPTS},nojournal_meta_batch=${NJMB_VALUE}"
# 008 核心缺口：inode 表元数据 write-behind。
#   on/async（默认，= 2）缓冲 + **异步**落盘（专用回写线程）；
#   sync（= 1）缓冲 + 同步 flush（旧行为，A/B 对照臂）；off（= 0）不做缓冲。
# `$9` 的语法是 `<mode>[-each]`：`-each` = 009 的旧屏障行为（每次 fsync 各发一次设备
# flush），用于同内核 A/B。屏障模式搭在 njwb 字段里传，是因为 benchmark 框架没把第 10 个
# 参数送进 guest（实测 guest 打印的 `mount_opts` 里始终没有 `fsync_barrier=`）。
# 注：曾经用 `,each` 承载，同样收不到（怀疑框架对逗号另有处理），故用 `-each`。
NJWB_BARRIER=""
case "$NJWB" in
    *-each) NJWB_BARRIER=each; NJWB_MODE="${NJWB%-each}" ;;
    *)      NJWB_MODE="$NJWB" ;;
esac
case "$NJWB_MODE" in
    off|0)  NJWB_VALUE=0 ;;
    sync|1) NJWB_VALUE=1 ;;
    # 3 = 异步转交 + 元数据块异步 bio 提交 + 延后 reap（对齐数据页路径）
    asyncbio|bio|3) NJWB_VALUE=3 ;;
    *)      NJWB_VALUE=2 ;;
esac
MOUNT_OPTS="${MOUNT_OPTS},nojournal_meta_writeback=${NJWB_VALUE}"
[ "$NJWB_BARRIER" = "each" ] && MOUNT_OPTS="${MOUNT_OPTS},fsync_barrier=each"

PER_JOB_MB=$((FILE_RANGE_MB / NUMJOBS))
# 向下对齐到 4 KiB。
PER_JOB_MB=$((PER_JOB_MB / 4 * 4))

# 阶段计时（uptime 纳秒基准）：boot = 内核启动→脚本开始（含 init 开销），
# format = mke2fs，mount = 挂载。
T_START="$(now_ns)"

mkdir -p "$MNT"
umount "$MNT" 2>/dev/null || true
if [ -x "$MKE2FS" ]; then
    "$MKE2FS" $MKE2FS_ARGS "$DEV"
else
    echo "WARNING: static mke2fs missing at $MKE2FS; fallback to rm-clean" >&2
    rm -f "$MNT"/* 2>/dev/null || true
fi
T_FMT="$(now_ns)"

# dirty budget 256 MiB（对齐 Linux 的 dirty_bytes=256MiB）。
mount -t ext4 -o "$MOUNT_OPTS" "$DEV" "$MNT"
T_MNT="$(now_ns)"

# 在日志里留一条可 grep 的醒目告警：ext2like 目前受 008 缺陷主导，不能当 ext2 结论用。
# （同负载实测：randwrite-4k nj16 下 nojournal 比 journal 慢 5×~490×，见 008 文档；
#   不写具体数值进脚本，避免数字过期。）
if [ "$FS" = "ext2like" ]; then
    echo "=== WARNING ext2like: nojournal 元数据写回仍是同步设备 IO（Linux 为异步 write-behind），"
    echo "=== WARNING ext2like: 同负载实测 randwrite-4k 下比 journal 慢 5x~490x；本轮数据受"
    echo "=== WARNING ext2like: os_com_refactor_plan/008-nojournal-delalloc-meta-writeback 主导，"
    echo "=== WARNING ext2like: 不可用于评判 fs 设计或与 Linux ext2 直接对比。"
fi

RWMIXREAD=""
MIXED_READ=0
case "$WORKLOAD" in
    randwrite-4k)  RW=randwrite; BS=4k ;;
    randread-4k)   RW=randread;  BS=4k; MIXED_READ=1 ;;
    seqwrite-4k)   RW=write;     BS=4k ;;
    randrw-r25-4k) RW=randrw;    BS=4k; RWMIXREAD=25; MIXED_READ=1 ;;
    randrw-r50-4k) RW=randrw;    BS=4k; RWMIXREAD=50; MIXED_READ=1 ;;
    randrw-r75-4k) RW=randrw;    BS=4k; RWMIXREAD=75; MIXED_READ=1 ;;
    *) echo "[baseline] unknown_workload workload=$WORKLOAD" >&2; exit 2 ;;
esac

case "$MODE" in
    orderless) DIRECT=0; FSYNC=0 ;;
    fsync)     DIRECT=0; FSYNC=1 ;;
    *) echo "[baseline] unknown_mode mode=$MODE" >&2; exit 2 ;;
esac

# Mixed reads require allocated data. Prepare it outside the measured interval
# with direct I/O so the timed buffered read path starts with a cold PageCache.
# Keeping the mount alive is required by the current DRAM-backed rNVPC model:
# unmount/remount is a recovery boundary, not a cache-dropping primitive.
CREATE_ON_OPEN=1
if [ "$MIXED_READ" -eq 1 ]; then
    echo "=== PREPARE: prepare-mixed-read-dataset workload=${WORKLOAD} ==="
    "$FIO" --output-format=normal --name=prepare-mixed-read-dataset \
        --ioengine=psync --numjobs="$NUMJOBS" --iodepth=1 --thread=1 \
        --group_reporting=1 --direct=1 --fsync=0 --directory="$MNT" \
        --filename_format='f.$jobnum' --nrfiles=1 --filesize="$((PER_JOB_MB))M" \
        --rw=write --bs=1m --size="$((PER_JOB_MB))M" \
        --create_on_open=1 --fallocate=none
    CREATE_ON_OPEN=0
fi

T0="$(now_ns)"
# 标记自描述（dev + fs + workload + mode + njobs）：宿主解析不再依赖轮次顺序猜设备。
echo "=== FIO_RESULT: ${DEV_TAG}_${FS}_${WORKLOAD}_${MODE}_nj${NUMJOBS} ==="
# 本轮参数快照（含 A/B 开关），供事后归因，避免“几个月后不知道当时挂的什么选项”。
echo "=== PARAM: dev=${DEV_TAG} fs=${FS} workload=${WORKLOAD} mode=${MODE} njobs=${NUMJOBS} runtime=${RUNTIME_LIMIT} file_range_mb=${FILE_RANGE_MB} mount_opts=${MOUNT_OPTS} njwb_raw=${NJWB} ==="
echo "=== PHASE: boot_ns=${T_START} format_ns=$((T_FMT - T_START)) mount_ns=$((T_MNT - T_FMT)) ==="
FIO_STATUS=0
"$FIO" --output-format=json \
    --name="${WORKLOAD}-${MODE}-nj${NUMJOBS}" \
    --ioengine=psync \
    --numjobs="$NUMJOBS" \
    --iodepth=1 \
    --thread=1 \
    --group_reporting=1 \
    --runtime="$RUNTIME_LIMIT" \
    --direct="$DIRECT" \
    --fsync="$FSYNC" \
    --norandommap=0 \
    --directory="$MNT" \
    --filename_format='f.$jobnum' \
    --nrfiles=1 \
    --filesize="$((PER_JOB_MB))M" \
    --rw="$RW" \
    ${RWMIXREAD:+--rwmixread=$RWMIXREAD} \
    --bs="$BS" \
    --size="$((PER_JOB_MB))M" \
    --create_on_open="$CREATE_ON_OPEN" \
    --fallocate=none || FIO_STATUS=$?
echo "=== FIO_EXIT_STATUS: ${FIO_STATUS} ==="
echo "=== END_FIO_RESULT ==="
T1="$(now_ns)"

if [ "$FIO_STATUS" -ne 0 ]; then
    # A failed workload is still diagnostic evidence. Unmount before returning
    # the error so the filesystem stops its workers and emits the final latency
    # snapshot instead of losing the only failure-path measurements.
    umount "$MNT" 2>/dev/null || true
    echo "=== ASTER_BASELINE_COMPLETE ==="
    exit "$FIO_STATUS"
fi

# 最终排空：把 PageCache 里的脏数据全部刷到设备。
if ! sync -f "$MNT" 2>/dev/null; then
    sync
fi
T2="$(now_ns)"

echo "=== DRAIN: busy_ns=$((T1 - T0)) drain_ns=$((T2 - T0)) ==="

umount "$MNT"

echo ""
echo "=== ASTER_BASELINE_COMPLETE ==="
