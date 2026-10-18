#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  宏测试 guest 侧：SQLite speedtest1 在 ext4 / rNVPC / rNVPC-extend 上
# =========================================================================
#
# 由宿主 harness（run-sqlite.sh）安装为
#   /benchmark/sqlite/aster_ext4_macro/run.sh
#
# 【参数】
#   $1 DEV_TAG   设备标签（sata|nvme；仅日志）
#   $2 FS         ext4|rnvpc|rnvpc_extend
#   $3 SIZE       speedtest1 --size（0/空 = 1000）
#   $4 RUNS       重复轮数（每轮重建库文件）
#   $5 NJMB       nojournal_meta_batch: on|global|off
#   $6 NJWB       nojournal_meta_writeback: on|sync|off
#   $7 WRCAP_MB   writeback_capacity_mb（dirty 预算 MB）
#
# 【口径来源】移植自 os_com/test/initramfs/src/benchmark/sqlite/ext4_benchmarks/
#   run.sh（P9 计分板用例，test.md §6.2），差异只有：① 加了 fs 档与真盘/挂载
#   参数（与 fio 基线同口径）；② 支持多轮；③ 把"崩"当成一种合法结果如实记录。
#
# 【已知债 — 必须知道】speedtest1 在 Asterinas 上**预期会崩**：历史上崩于
#   test 150 (CREATE INDEX)，原因是 reserialize mega-buffer 堆耗尽（P9 台账
#   `p9-perf-basket` / `extent-flatten-storm`）。所以本脚本：
#     * 不因 speedtest1 非 0 退出而失败；
#     * 把最后若干行原始输出带回日志 —— **崩点本身就是结论**（P9 的 before 值）；
#     * 仍然跑 PRAGMA integrity_check，把"性能"与"数据无损"绑在一起。
#
# 【产出标记】（宿主解析只看这几行）
#   === SQLITE_PARAM:  dev=.. fs=.. size=.. runs=.. mount_opts=..
#   === SQLITE_RUN:    <tag> run=N status=ok|crash rc=.. elapsed_s=..
#   === SQLITE_INTEGRITY: <tag> run=N result=ok|corrupt|n/a|crash
#   === SQLITE_RESULT: <tag> status=.. runs_ok=.. total_elapsed_s=.. integrity=..
# =========================================================================

DEV_TAG="${1:-dut}"
FS="${2:-ext4}"
SIZE="${3:-1000}"
RUNS="${4:-3}"
NJMB="${5:-on}"
NJWB="${6:-on}"
WRCAP_MB="${7:-256}"

if [ -z "$SIZE" ] || [ "$SIZE" = "0" ]; then SIZE=1000; fi
if [ -z "$RUNS" ] || [ "$RUNS" = "0" ]; then RUNS=1; fi

SPEEDTEST="/benchmark/bin/sqlite-speedtest1"
SQLITE3="/benchmark/bin/sqlite3"
MKE2FS="/benchmark/sqlite/aster_ext4_macro/mkfs/mke2fs"
MNT="/mnt/dut"
DEV="/dev/vdc"
DB="$MNT/test.db"

MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"

case "$FS" in
    ext4)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB}"
        ;;
    ext2like)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB}"
        ;;
    ext4_nomerge)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_merge=off"
        ;;
    ext2like_nomerge)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_merge=off"
        ;;
    ext4_sync)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_sync_wait=on"
        ;;
    ext2like_sync)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_sync_wait=on"
        ;;
    rnvpc)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},rnvpc=on"
        ;;
    rnvpc_extend)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},rnvpc_extend=on"
        ;;
    *)
        echo "[sqlite] unknown_fs fs=$FS" >&2
        exit 2
        ;;
esac
case "$NJMB" in
    on|owner|2) NJMB_VALUE=2 ;;
    global|1)   NJMB_VALUE=1 ;;
    off|0)      NJMB_VALUE=0 ;;
    *)          NJMB_VALUE=2 ;;
esac
case "$NJWB" in
    off|0)  NJWB_VALUE=0 ;;
    sync|1) NJWB_VALUE=1 ;;
    asyncbio|bio|3) NJWB_VALUE=3 ;;
    *)      NJWB_VALUE=2 ;;
esac
MOUNT_OPTS="${MOUNT_OPTS},nojournal_meta_batch=${NJMB_VALUE}"
MOUNT_OPTS="${MOUNT_OPTS},nojournal_meta_writeback=${NJWB_VALUE}"

now_s() { awk '{printf "%.3f", $1}' /proc/uptime; }

echo "=== SQLITE_PARAM: dev=${DEV_TAG} fs=${FS} size=${SIZE} runs=${RUNS} mount_opts=${MOUNT_OPTS} ==="

# ---- 格式化 + 挂载 ------------------------------------------------------
mkdir -p "$MNT"
umount "$MNT" 2>/dev/null || true
if [ -x "$MKE2FS" ]; then
    "$MKE2FS" $MKE2FS_ARGS "$DEV" || { echo "[sqlite] mke2fs failed" >&2; exit 3; }
else
    echo "WARNING: static mke2fs missing at $MKE2FS; fallback to rm-clean" >&2
    rm -rf "${MNT:?}"/* 2>/dev/null || true
fi
mount -t ext4 -o "$MOUNT_OPTS" "$DEV" "$MNT" || { echo "[sqlite] mount failed: $MOUNT_OPTS" >&2; exit 3; }

# ---- 多轮 speedtest1 ----------------------------------------------------
RUNS_OK=0
TOTAL=0
INTEGRITY_LAST=n/a

run_i=1
while [ "$run_i" -le "$RUNS" ]; do
    rm -f "$DB" 2>/dev/null || true
    sync
    T0="$(now_s)"
    set +e
    "$SPEEDTEST" --size "$SIZE" "$DB" > /tmp/sqlite.out 2>&1
    ST_RC=$?
    set -e
    T1="$(now_s)"
    ELAPSED=$(awk -v a="$T0" -v b="$T1" 'BEGIN{printf "%.3f", b-a}')
    TOTAL=$(awk -v a="$TOTAL" -v b="$ELAPSED" 'BEGIN{printf "%.3f", a+b}')

    if [ "$ST_RC" -eq 0 ]; then
        ST_STATUS=ok
        RUNS_OK=$((RUNS_OK + 1))
    else
        ST_STATUS=crash
    fi
    echo "=== SQLITE_RUN: ${DEV_TAG}_${FS}_size${SIZE} run=${run_i} status=${ST_STATUS} rc=${ST_RC} elapsed_s=${ELAPSED} ==="

    # 崩点现场：末 15 行原始输出（P9 的 before 台账就靠它）。
    echo "--- SQLITE_RAW_BEGIN run=${run_i} rc=${ST_RC} ---"
    tail -15 /tmp/sqlite.out 2>/dev/null || true
    echo "--- SQLITE_RAW_END run=${run_i} ---"

    # 数据无损绑定：只有 integrity_check 返回 ok 才算这一轮成立。
    IG=crash
    if [ -x "$SQLITE3" ] && [ -f "$DB" ]; then
        set +e
        IG_OUT="$("$SQLITE3" "$DB" "PRAGMA integrity_check;" 2>&1 | tail -1)"
        set -e
        case "$IG_OUT" in
            ok) IG=ok ;;
            *)  IG=corrupt ;;
        esac
    fi
    INTEGRITY_LAST="$IG"
    echo "=== SQLITE_INTEGRITY: ${DEV_TAG}_${FS}_size${SIZE} run=${run_i} result=${IG} ==="

    run_i=$((run_i + 1))
done

sync
umount "$MNT" 2>/dev/null || true

if [ "$RUNS_OK" -eq "$RUNS" ] && [ "$INTEGRITY_LAST" = "ok" ]; then
    FINAL=ok
elif [ "$RUNS_OK" -eq 0 ]; then
    FINAL=crash
else
    FINAL=partial
fi

echo "=== SQLITE_RESULT: ${DEV_TAG}_${FS}_size${SIZE} status=${FINAL} runs_ok=${RUNS_OK}/${RUNS} total_elapsed_s=${TOTAL} integrity=${INTEGRITY_LAST} ==="
exit 0
