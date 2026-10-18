#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  宏测试一站式入口：filebench + SQLite speedtest1
# =========================================================================
#
# 两个负载各有自己的 harness（各自可单独跑）：
#   test/filebench-test/run-filebench.sh   filebench：write-heavy/fileserver/varmail
#   test/sqlite-test/run-sqlite.sh         SQLite speedtest1 + integrity_check
# 本脚本把它们**串行**串起来（真实盘 + 单 QEMU，必须串行），共用同一套
# device/fs/容量参数，并在同一个时间戳下汇总产物。
#
#  口径：device = sata(/dev/sda4) | nvme(/dev/nvme3n1p2)
#        fs     = ext4 | rnvpc | rnvpc_extend（与 fio 基线同一套 mkfs/mount 参数）
#
#  默认测试点（可用参数改）：
#    filebench: 2 device × 3 fs × 3 workload × 2 threads = 36 点
#    sqlite   : 2 device × 3 fs × 1 size                 =  6 点 ×3 runs
#    --------------------------------------------------------------
#    合计 42 点（预计 1.5–3 小时；filebench 每点 ~1–2 min，sqlite 每点 2–5 min）
#
# 【用法】
#   bash test/macro-tests/run-macro-tests.sh --dry-run                 # 看两个矩阵与总点数
#   bash test/macro-tests/run-macro-tests.sh                            # 跑 filebench + sqlite
#   bash test/macro-tests/run-macro-tests.sh --only filebench           # 只跑 filebench
#   bash test/macro-tests/run-macro-tests.sh --only sqlite              # 只跑 sqlite
#   bash test/macro-tests/run-macro-tests.sh --fs ext4,ext2like --filebench-threads 8
#   bash test/macro-tests/run-macro-tests.sh --filebench-workloads varmail --sqlite-sizes 100,1000
#
# 【产物】test/macro-tests/results/<ts>_macro/manifest.md 里列出本次两批结果的路径与要点。
# =========================================================================

set -e

HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_ROOT="$(cd "${HARNESS_DIR}/.." && pwd)"
FB_HARNESS="${TEST_ROOT}/filebench-test/run-filebench.sh"
SQ_HARNESS="${TEST_ROOT}/sqlite-test/run-sqlite.sh"
CONTAINER="asterinas-dev"

DEVICES="sata,nvme"
# 默认两个档 = “rNVPCe / rNVPC 替身”：ext4（有 journal）+ ext2like（去 journal）。
# 真实现 rnvpc/rnvpc_extend 仍可用 --fs 显式指定。
FS_LIST="ext4,ext2like"
FB_WORKLOADS="write-heavy,fileserver,varmail"
FB_THREADS="4,8"
FB_RUNTIME=30
FB_NFILES=0
FB_FILESIZE="-"
SQ_SIZES="1000"
SQ_RUNS=3
WRCAP_MB=256
RNVPC_KCMD="rnvpc_size=256M rnvpc_meta_reserve=64M rnvpc_data_limit=192M rnvpc_sim_mode=fast"
ONLY="both"
DRY_RUN=false
CONFIRM=false
TIMESTAMP=""

while [ $# -gt 0 ]; do
    case "$1" in
        --devices)   DEVICES="$2"; shift 2 ;;
        --fs)        FS_LIST="$2"; shift 2 ;;
        --filebench-workloads) FB_WORKLOADS="$2"; shift 2 ;;
        --filebench-threads)   FB_THREADS="$2"; shift 2 ;;
        --filebench-runtime)   FB_RUNTIME="$2"; shift 2 ;;
        --filebench-nfiles)    FB_NFILES="$2"; shift 2 ;;
        --filebench-filesize)  FB_FILESIZE="$2"; shift 2 ;;
        --sqlite-sizes)        SQ_SIZES="$2"; shift 2 ;;
        --sqlite-runs)         SQ_RUNS="$2"; shift 2 ;;
        --wr-capacity-mb)      WRCAP_MB="$2"; shift 2 ;;
        --rnvpc-kcmd)          RNVPC_KCMD="$2"; shift 2 ;;
        --only)                ONLY="$2"; shift 2 ;;
        --timestamp)           TIMESTAMP="$2"; shift 2 ;;
        --dry-run|-n)          DRY_RUN=true; shift ;;
        --confirm-destroy|-y)  CONFIRM=true; shift ;;
        -h|--help) sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
done

case "$ONLY" in both|filebench|sqlite) ;; *) echo "--only 必须是 both|filebench|sqlite" >&2; exit 2 ;; esac
[ -z "$TIMESTAMP" ] && TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
MANIFEST_DIR="${HARNESS_DIR}/results/${TIMESTAMP}_macro"

n_dev=$(awk -F, '{print NF}' <<<"$DEVICES")
n_fs=$(awk -F, '{print NF}' <<<"$FS_LIST")
n_fbwl=$(awk -F, '{print NF}' <<<"$FB_WORKLOADS")
n_fbth=$(awk -F, '{print NF}' <<<"$FB_THREADS")
n_sqsz=$(awk -F, '{print NF}' <<<"$SQ_SIZES")
PTS_FB=$((n_dev * n_fs * n_fbwl * n_fbth))
PTS_SQ=$((n_dev * n_fs * n_sqsz))
PTS=0
[ "$ONLY" != "sqlite" ] && PTS=$((PTS + PTS_FB))
[ "$ONLY" != "filebench" ] && PTS=$((PTS + PTS_SQ))

echo "================================================================"
echo " 宏测试一站式（filebench + SQLite）"
echo "   device/fs : ${DEVICES} / ${FS_LIST}"
echo "   filebench : ${FB_WORKLOADS} × threads ${FB_THREADS} (runtime ${FB_RUNTIME}s)"
echo "   sqlite    : size ${SQ_SIZES} × runs ${SQ_RUNS}"
echo "   → filebench ${PTS_FB} 点 / sqlite ${PTS_SQ} 点"
echo "   → 本次执行（--only ${ONLY}）= ${PTS} 点"
echo "   → rNVPC 档: wr-capacity=${WRCAP_MB}MiB"
echo "================================================================"

if [ "$DRY_RUN" = true ]; then
    [ "$ONLY" != "sqlite" ] && bash "$FB_HARNESS" --dry-run --devices "$DEVICES" --fs "$FS_LIST" \
        --workloads "$FB_WORKLOADS" --threads "$FB_THREADS" --runtime "$FB_RUNTIME" \
        --nfiles "$FB_NFILES" --filesize "$FB_FILESIZE" --wr-capacity-mb "$WRCAP_MB" \
        --rnvpc-kcmd "$RNVPC_KCMD"
    [ "$ONLY" != "filebench" ] && bash "$SQ_HARNESS" --dry-run --devices "$DEVICES" --fs "$FS_LIST" \
        --sizes "$SQ_SIZES" --runs "$SQ_RUNS" --wr-capacity-mb "$WRCAP_MB" \
        --rnvpc-kcmd "$RNVPC_KCMD"
    exit 0
fi

if [ "$CONFIRM" != true ]; then
    echo "error: --confirm-destroy 必需（会格式化 /dev/sda4 / /dev/nvme3n1p2）" >&2
    exit 2
fi

# ── 前置检查 ──────────────────────────────────────────────────────
# 只看**容器内、非僵尸**的 QEMU：asterinas 的 QEMU 跑在容器里（独立 PID
# namespace），宿主上的 qemu 属于 research1 等常驻实验，用宿主 pgrep 会误拦
# （2026-09-24 踩过）。docker exec 退出的 QEMU 会变 <defunct>，state 以 Z 开头，
# 排除掉。
RUNNING="$(docker exec "$CONTAINER" bash -c \
    "ps -eo stat=,args= 2>/dev/null | grep 'qemu-sys[t]em' | grep -v '^Z' || true" 2>/dev/null || true)"
if [ -n "$RUNNING" ]; then
    echo "error: 容器 $CONTAINER 里已有 asterinas QEMU 在跑（必须串行）：" >&2
    echo "$RUNNING" | sed 's/^/   /' >&2
    echo "       等它结束；残留则 docker exec $CONTAINER pkill -9 -f 'qemu-sys[t]em'" >&2
    exit 2
fi
HOST_QEMU=$(pgrep -c -f 'qemu-sys[t]em' 2>/dev/null || true)
if [ "${HOST_QEMU:-0}" -gt 0 ]; then
    echo "[warn] 宿主上另有 ${HOST_QEMU} 个 qemu-system（常驻实验），只提示不阻塞。"
    echo "       当前负载：$(uptime | sed 's/.*load average/load average/')"
fi

mkdir -p "$MANIFEST_DIR"
MANIFEST="${MANIFEST_DIR}/manifest.md"
{
    echo "# 宏测试 ${TIMESTAMP}"
    echo
    echo "- device: \`${DEVICES}\`   fs: \`${FS_LIST}\`"
    echo "- filebench: workloads \`${FB_WORKLOADS}\` threads \`${FB_THREADS}\` runtime ${FB_RUNTIME}s nfiles ${FB_NFILES} filesize ${FB_FILESIZE}"
    echo "- sqlite: size \`${SQ_SIZES}\` runs ${SQ_RUNS}"
    echo "- rnpcp 档: wr-capacity=${WRCAP_MB}MiB, kcmd=\`${RNVPC_KCMD}\`"
    echo "- 预计点数: filebench ${PTS_FB} / sqlite ${PTS_SQ}（本次执行 ${ONLY}）"
    echo
    echo "## 结果"
} > "$MANIFEST"

RC=0
if [ "$ONLY" != "sqlite" ]; then
    echo ""
    echo "############ [1/$( [ "$ONLY" = both ] && echo 2 || echo 1 )] filebench ############"
    set +e
    bash "$FB_HARNESS" --devices "$DEVICES" --fs "$FS_LIST" \
        --workloads "$FB_WORKLOADS" --threads "$FB_THREADS" --runtime "$FB_RUNTIME" \
        --nfiles "$FB_NFILES" --filesize "$FB_FILESIZE" --wr-capacity-mb "$WRCAP_MB" \
        --rnvpc-kcmd "$RNVPC_KCMD" --timestamp "$TIMESTAMP" --confirm-destroy
    rc=$?
    set -e
    [ "$rc" -ne 0 ] && RC=$rc
    echo "- filebench: \`test/filebench-test/results/${TIMESTAMP}_filebench-macro/summary.csv\` (exit=$rc)" >> "$MANIFEST"
fi

if [ "$ONLY" != "filebench" ]; then
    echo ""
    echo "############ [$( [ "$ONLY" = both ] && echo 2/2 || echo 1/1 )] SQLite ############"
    set +e
    bash "$SQ_HARNESS" --devices "$DEVICES" --fs "$FS_LIST" \
        --sizes "$SQ_SIZES" --runs "$SQ_RUNS" --wr-capacity-mb "$WRCAP_MB" \
        --rnvpc-kcmd "$RNVPC_KCMD" --timestamp "$TIMESTAMP" --confirm-destroy
    rc=$?
    set -e
    [ "$rc" -ne 0 ] && RC=$rc
    echo "- sqlite: \`test/sqlite-test/results/${TIMESTAMP}_sqlite-macro/summary.csv\` (exit=$rc)" >> "$MANIFEST"
fi

echo ""
echo "================================================================"
echo " 宏测试结束（driver exit=${RC}）"
echo "   manifest: ${MANIFEST}"
echo "================================================================"
exit "$RC"
