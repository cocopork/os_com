#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  一站式：rNVPC / rNVPC-extend 带宽 sweep（njobs 1~32 步长 4）
# =========================================================================
#
#  口径（与 plots/20260921_180514_aster-ext4-baseline/summary.csv 同轴）：
#    device   : sata (/dev/sda4) | nvme (/dev/nvme3n1p2)
#    fs       : rnvpc（CoW+WAL）| rnvpc_extend（soft update）
#               --with-ext4 可再加 ext4 作同会话对照
#    workload : randwrite-4k | seqwrite-4k
#    mode     : orderless（buffered 带宽口径；--with-fsync 可加 fsync 模式）
#    njobs    : 1,4,8,12,16,20,24,28,32（= 1~32 步长 4，9 点）
#
#  测试点 = device × fs × workload × mode × njobs
#    默认  2 × 2 × 2 × 1 × 9 = 72 点
#    +ext4 2 × 3 × 2 × 1 × 9 = 108 点
#    +fsync 翻倍             = 144 / 216 点
#
#  耗时：每点 ≈ 20–45s（boot+mkfs+mount ~5s，fio ~5–30s，teardown ~5s），
#        72 点 ≈ 30–60 min，再加 1 次内核构建（~1–3 min）。
#
# 【用法】
#   bash run_rnvpc_bw_sweep.sh --dry-run          # 只看矩阵与点数，不跑
#   bash run_rnvpc_bw_sweep.sh                    # 跑，并打印汇总表
#   bash run_rnvpc_bw_sweep.sh --with-ext4         # 加 ext4 对照（108 点）
#   bash run_rnvpc_bw_sweep.sh --njobs 1,8,32     # 自定义 sweep 轴
#   bash run_rnvpc_bw_sweep.sh --runtime 20       # 每点 fio 墙钟上限（默认 30s）
#
# 【产出】
#   results/<ts>_aster-ext4-baseline/summary.csv      原始每点结果（harness 产出）
#   results/<ts>_aster-ext4-baseline/bw_sweep.csv     本次 sweep 的精简表（含加速比）
#   logs/baseline_run.log                             全部轮次拼接日志
#
# 【必须串行】真实盘性能实验一次只能跑一个 boot；脚本会拒绝在有 QEMU 在跑时启动。
# =========================================================================

set -euo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

TEST_DIR_HOST="${OS_COM_IMPL_ROOT}/test/aster-ext4-baseline-test"
CONTAINER="asterinas-dev"
CONTAINER_TEST_DIR="/root/impl/test/aster-ext4-baseline-test"
HARNESS="run-baseline.sh"

# ---- 默认 sweep 配置（改这里就够了）---------------------------------------
DEVICES="sata,nvme"
FS_LIST="rnvpc,rnvpc_extend"
WORKLOADS="randwrite-4k,seqwrite-4k"
MODES="orderless"
NJOBS="1,4,8,12,16,20,24,28,32"

RUNTIME=30          # 每点 fio 墙钟上限（秒）
TIMEOUT=300         # 单点 QEMU 墙钟上限
SMP=64              # 必须 > max(njobs)=32，否则把「线程扩展性」测成「调度排队」
# 总地址范围（MB）。
# 【为什么是 256 而不是 harness 默认的 2048】20260924_133554 那批用了 2048，结果
# 7/72 个点被 timeout 杀掉，现场是 `rNVPC NVBH allocation rejected: ENOSPC`——
# 2048 MiB 范围是 64 MiB NV 池的 32 倍，池要不停回收；高并发（nj8/16/24/28/32）
# 下前台退到池满就回压卡死。256 与已验证的 A/B（20260924_032853 等）口径一致。
# 真要拍大范围：必须同时放大池（--wr-capacity-mb 256 --rnvpc-kcmd "rnvpc_size=256M ..."）。
FILE_RANGE_MB=256
# rNVPC 池容量档：WRCAP 必须与内核 cmdline 的 rnvpc_size 对齐（容量额度一致）。
RNVPC_SIZE="64M"
RNVPC_META="16M"
RNVPC_DATA="48M"
WRCAP_MB=64
RNVPC_KCMD="rnvpc_size=${RNVPC_SIZE} rnvpc_meta_reserve=${RNVPC_META} rnvpc_data_limit=${RNVPC_DATA} rnvpc_sim_mode=fast"

DRY_RUN=false
WITH_EXT4=false
WITH_FSYNC=false
ALLOW_CONCURRENT=false
# 代码冻住后重跑时自动带上 --skip-build（省一次 ~1 分钟的内核构建）；
# harness 会自己校验"内核树 + guest 脚本"指纹，不一致就拒绝并让你去掉这个开关。
SKIP_ARG=()
[ -f "${TEST_DIR_HOST}/.build-stamp" ] && SKIP_ARG=(--skip-build)

usage() { sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'; exit 0; }

while [ $# -gt 0 ]; do
    case "$1" in
        --devices)        DEVICES="$2"; shift 2 ;;
        --njobs)          NJOBS="$2"; shift 2 ;;
        --workloads)      WORKLOADS="$2"; shift 2 ;;
        --runtime)        RUNTIME="$2"; shift 2 ;;
        --timeout)        TIMEOUT="$2"; shift 2 ;;
        --file-range-mb)  FILE_RANGE_MB="$2"; shift 2 ;;
        --wr-capacity-mb) WRCAP_MB="$2"; shift 2 ;;
        --rnvpc-kcmd)     RNVPC_KCMD="$2"; shift 2 ;;
        --with-ext4)      WITH_EXT4=true; shift ;;
        --with-fsync)     WITH_FSYNC=true; shift ;;
        --allow-concurrent) ALLOW_CONCURRENT=true; shift ;;
        --dry-run|-n)     DRY_RUN=true; shift ;;
        -h|--help)        usage ;;
        *) echo "Unknown arg: $1 (use --help)" >&2; exit 2 ;;
    esac
done

[ "$WITH_EXT4" = true ] && FS_LIST="ext4,rnvpc,rnvpc_extend"
[ "$WITH_FSYNC" = true ] && MODES="orderless,fsync"

# ---- 点数 ---------------------------------------------------------------
n_dev=$(awk -F, '{print NF}' <<<"$DEVICES")
n_fs=$(awk -F, '{print NF}' <<<"$FS_LIST")
n_wl=$(awk -F, '{print NF}' <<<"$WORKLOADS")
n_md=$(awk -F, '{print NF}' <<<"$MODES")
n_nj=$(awk -F, '{print NF}' <<<"$NJOBS")
POINTS=$((n_dev * n_fs * n_wl * n_md * n_nj))
EST_MIN=$((POINTS * 30 / 60 + 3))
[ "$POINTS" -lt 30 ] && EST_MIN=$((POINTS * 20 / 60 + 3))

echo "================================================================"
echo " rNVPC 带宽 sweep"
echo "   device   : $DEVICES"
echo "   fs       : $FS_LIST"
echo "   workload : $WORKLOADS"
echo "   mode     : $MODES"
echo "   njobs    : $NJOBS   ($n_nj 点)"
echo "   → 测试点 = ${n_dev}×${n_fs}×${n_wl}×${n_md}×${n_nj} = ${POINTS}"
echo "   → 预计墙钟 ≈ ${EST_MIN} 分钟（含 1 次内核构建）"
echo "   → rNVPC 档: wr-capacity=${WRCAP_MB}MiB  kcmd=\"$RNVPC_KCMD\""
echo "================================================================"

if [ "$DRY_RUN" = true ]; then
    echo "[dry-run] 交由 harness 打印矩阵（不会真正跑）"
    docker exec -w /root/asterinas "$CONTAINER" \
        bash "$CONTAINER_TEST_DIR/$HARNESS" \
        --devices "$DEVICES" --workloads "$WORKLOADS" --modes "$MODES" \
        --njobs "$NJOBS" --fs "$FS_LIST" --runtime "$RUNTIME" --smp "$SMP" \
        --timeout "$TIMEOUT" --file-range-mb "$FILE_RANGE_MB" \
        --wr-capacity-mb "$WRCAP_MB" --rnvpc-kcmd "$RNVPC_KCMD" --dry-run
    exit 0
fi

# ---- 前置检查 -----------------------------------------------------------
# 【为什么只看容器】asterinas 的 QEMU 跑在容器里（独立 PID namespace），宿主上的
# qemu 属于 research1 等常驻实验，两边相互看不见。用宿主 pgrep 会误拦别人的 VM。
# 【为什么过滤僵尸】docker exec 退出的 QEMU 会变成 <defunct>（不占资源、不碰盘），
# `ps` 的 state 字段以 Z 开头，直接排除。
if [ "$ALLOW_CONCURRENT" != true ]; then
    RUNNING="$(docker exec "$CONTAINER" bash -c \
        "ps -eo stat=,args= 2>/dev/null | grep 'qemu-sys[t]em' | grep -v '^Z' || true" 2>/dev/null)"
    if [ -n "$RUNNING" ]; then
        echo "error: 容器 $CONTAINER 里已有 asterinas QEMU 在跑（真实盘实验必须串行）：" >&2
        echo "$RUNNING" | sed 's/^/   /' >&2
        echo "       等它结束；若是残留：docker exec $CONTAINER pkill -9 -f 'qemu-sys[t]em'" >&2
        echo "       （确认可并存时用 --allow-concurrent 跳过本检查）" >&2
        exit 2
    fi
fi

if ! docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    echo "error: 容器 $CONTAINER 不在运行（docker start $CONTAINER）。" >&2
    exit 2
fi

# 宿主上别人的常驻 VM：不拦，但要说清楚测量背景（性能数字只在同背景下可比）。
HOST_QEMU=$(pgrep -c -f 'qemu-sys[t]em' 2>/dev/null || true)
HOST_QEMU=${HOST_QEMU:-0}
if [ "$HOST_QEMU" -gt 0 ]; then
    echo "[warn] 宿主上另有 ${HOST_QEMU} 个 qemu-system（应属 research1 等常驻实验，不拦本次）。"
    echo "       当前负载：$(uptime | sed 's/.*load average/load average/')"
    echo "       → 与历史数据同背景可比；若要绝对干净的带宽，先确认它们空闲。"
fi

# 结果目录：宿主侧预创建（归属当前用户），目录名经 --timestamp 传给容器，
# 保证两侧一致、事后好找。
TS="$(date +%Y%m%d_%H%M%S)"
RESULTS_DIR="${TEST_DIR_HOST}/results/${TS}_aster-ext4-baseline"
mkdir -p "${RESULTS_DIR}/logs"
echo "[run] 结果目录: ${RESULTS_DIR}"
echo "[run] 全量日志: ${RESULTS_DIR}/logs/baseline_run.log"
echo "[run] 开始时间: $(date '+%F %T')  （串行跑 ${POINTS} 点，中途可 tail 日志看进度）"

# ---- 跑 -----------------------------------------------------------------
set +e
docker exec \
    -e NIX_CONFIG="substitute = false" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -w /root/asterinas "$CONTAINER" \
    bash "$CONTAINER_TEST_DIR/$HARNESS" \
    --devices "$DEVICES" --workloads "$WORKLOADS" --modes "$MODES" \
    --njobs "$NJOBS" --fs "$FS_LIST" --runtime "$RUNTIME" --smp "$SMP" \
    --timeout "$TIMEOUT" --file-range-mb "$FILE_RANGE_MB" \
    --wr-capacity-mb "$WRCAP_MB" --rnvpc-kcmd "$RNVPC_KCMD" \
    --timestamp "$TS" --confirm-destroy \
    "${SKIP_ARG[@]}" \
    2>&1 | tee "${RESULTS_DIR}/logs/sweep_driver.log"
RC=${PIPESTATUS[0]}
set -e

echo "[run] 结束时间: $(date '+%F %T')  harness 退出码=$RC"
[ -f "${RESULTS_DIR}/summary.csv" ] || { echo "error: 没有 summary.csv，看 sweep_driver.log" >&2; exit "$RC"; }

# ---- 汇总 ---------------------------------------------------------------
python3 - "$RESULTS_DIR" <<'PY'
import csv, os, sys
from collections import defaultdict

d = sys.argv[1]
rows = list(csv.DictReader(open(os.path.join(d, "summary.csv"))))
if not rows:
    print("summary.csv 为空"); sys.exit(0)

def f(r, k):
    try: return float(r[k])
    except Exception: return float("nan")

order_wl = sorted({r["workload"] for r in rows})
order_dev = sorted({r["device"] for r in rows})
order_fs = sorted({r["fs"] for r in rows})
nj = sorted({int(r["njobs"]) for r in rows})

print()
print("=" * 104)
print(" busy_bw (MiB/s) —— 括号内 iops；valid=0 表示该点无效")
print("=" * 104)
hdr = f"{'device':6s} {'workload':13s} {'nj':>3s} " + " ".join(f"{fs:>22s}" for fs in order_fs)
print(hdr); print("-" * len(hdr))
for dev in order_dev:
    for wl in order_wl:
        for n in nj:
            cells = []
            for fs in order_fs:
                r = next((x for x in rows if x["device"] == dev and x["workload"] == wl
                          and x["fs"] == fs and int(x["njobs"]) == n), None)
                if r is None or r.get("valid", "1") != "1":
                    cells.append(f"{'-':>22s}")
                else:
                    cells.append(f"{f(r,'busy_bw_mib_s'):>10.1f}({f(r,'iops'):>8.0f})")
            print(f"{dev:6s} {wl:13s} {n:>3d} " + " ".join(cells))
    print("-" * len(hdr))

# 精简表 + 加速比（rnvpc_extend / rnvpc）
out = os.path.join(d, "bw_sweep.csv")
with open(out, "w", newline="") as fh:
    wr = csv.writer(fh)
    wr.writerow(["device", "workload", "njobs", "rnvpc_busy_bw", "rnvpc_iops",
                 "rnvpc_extend_busy_bw", "rnvpc_extend_iops", "speedup_extend_over_rnvpc"])
    if {"rnvpc", "rnvpc_extend"} <= set(order_fs):
        for dev in order_dev:
            for wl in order_wl:
                for n in nj:
                    g = {}
                    for fs in ("rnvpc", "rnvpc_extend"):
                        r = next((x for x in rows if x["device"] == dev and x["workload"] == wl
                                  and x["fs"] == fs and int(x["njobs"]) == n), None)
                        g[fs] = r
                    a = f(g["rnvpc"], "busy_bw_mib_s") if g["rnvpc"] else float("nan")
                    b = f(g["rnvpc_extend"], "busy_bw_mib_s") if g["rnvpc_extend"] else float("nan")
                    wr.writerow([dev, wl, n,
                                 f"{a:.2f}" if a == a else "", f(g["rnvpc"], "iops") if g["rnvpc"] else "",
                                 f"{b:.2f}" if b == b else "", f(g["rnvpc_extend"], "iops") if g["rnvpc_extend"] else "",
                                 f"{b/a:.3f}" if a == a and a > 0 and b == b else ""])
print(f"\n精简表（含 rnvpc_extend/rnvpc 加速比）→ {out}")

invalid = [r for r in rows if r.get("valid", "1") != "1"]
if invalid:
    print(f"\n⚠ {len(invalid)} 个点无效（valid=0）：")
    for r in invalid:
        print(f"   {r['device']:5s} {r['fs']:13s} {r['workload']:13s} nj{r['njobs']:>3s}  note={r.get('note','')}")
PY

echo
echo "================================================================"
echo " 完成。结果目录："
echo "   ${RESULTS_DIR}/summary.csv    （原始每点）"
echo "   ${RESULTS_DIR}/bw_sweep.csv   （精简表 + 加速比）"
echo "   ${RESULTS_DIR}/logs/          （baseline_run.log + 各点串口日志）"
echo "================================================================"
exit "$RC"
