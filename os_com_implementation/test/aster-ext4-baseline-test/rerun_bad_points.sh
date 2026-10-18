#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  坏点重测驱动：对 20260924_145908 批次里的 timeout / 低带点各重测 3 轮
# =========================================================================
#
# 为什么分组而不是逐点：每个点单独调一次 harness 就要重建一次内核（浪费），
# 所以按 (device, fs, workload) 分组、把多个 njobs 一次跑完；每组跑 3 遍 ⇒
# 每个点就有 3 个样本。**配置与母批次逐字一致**（file_range 256 / wr-capacity 64
# / rnvpc_size=64M），否则补进去的数不可比。
#
# 产物：
#   results/20260924_145908_aster-ext4-baseline/rerun/<dev>_<fs>_<wl>_rep<N>.csv
#   results/20260924_145908_aster-ext4-baseline/rerun/drv_*.log
# 之后由 merge_rerun.py 择优回填 summary.csv。
# =========================================================================

set -u

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

TEST_DIR="${OS_COM_IMPL_ROOT}/test/aster-ext4-baseline-test"
BASE="${TEST_DIR}/results/20260924_145908_aster-ext4-baseline"
OUT="${BASE}/rerun"
CONTAINER="asterinas-dev"
RUNS="${RUNS:-3}"
DRY_RUN=false
[ "${1:-}" = "--dry-run" ] && DRY_RUN=true
RNVPC_KCMD="rnvpc_size=64M rnvpc_meta_reserve=16M rnvpc_data_limit=48M rnvpc_sim_mode=fast"

# 坏点分组：device|fs|workload|njobs 列表
# 【名字别叫 GROUPS】`GROUPS` 是 bash 的**内建只读数组**（当前用户的组 ID 列表），
# 同名赋值会被静默忽略 —— 2026-09-24 就因此把 5 个组跑成了 (27 998 1002 1003 119)
# （= `id -G`），每个 invocation 空跑。
POINT_GROUPS=(
  "sata|rnvpc|randwrite-4k|24,28,32"
  "sata|rnvpc_extend|randwrite-4k|8,32"
  "sata|rnvpc_extend|seqwrite-4k|28"
  "nvme|rnvpc|randwrite-4k|4,12,24,32"
  "nvme|rnvpc_extend|randwrite-4k|16,28,32"
)

mkdir -p "$OUT"
cd "$TEST_DIR"

# 代码冻住后重跑：有构建印章就带 --skip-build（省 15 次内核构建）；harness 自己
# 校验指纹，不一致会硬失败。
SKIP_ARG=()
[ -f "${TEST_DIR}/.build-stamp" ] && SKIP_ARG=(--skip-build)
echo "[rerun] 构建策略：$([ ${#SKIP_ARG[@]} -gt 0 ] && echo '复用上次构建（--skip-build）' || echo '完整构建') + 后续 invocation 自动复用"

# 前置：设备不能被挂载着（真实盘实验）
for dev in sata nvme; do
    p=$([ "$dev" = sata ] && echo /dev/sda4 || echo /dev/nvme3n1p2)
    if findmnt -rn "$p" >/dev/null 2>&1; then
        echo "error: $p 已被挂载，拒绝开跑" >&2; exit 1
    fi
done
# 容器里不能有活着的 asterinas QEMU（残留会撞盘/撞端口）
if docker exec "$CONTAINER" bash -c "ps -eo stat=,args= 2>/dev/null | grep 'qemu-sys[t]em' | grep -v '^Z' || true" | grep -q .; then
    echo "error: 容器里有活着的 QEMU，先等它结束" >&2; exit 1
fi

echo "=== 坏点重测：${#POINT_GROUPS[@]} 组 × ${RUNS} 轮 ==="
for g in "${POINT_GROUPS[@]}"; do
    IFS='|' read -r dev fs wl njs <<< "$g"
    echo "   ${dev} / ${fs} / ${wl} / nj=${njs}"
done
echo

if [ "$DRY_RUN" = true ]; then
    echo "[dry-run] 不跑。预期轮数 = 组数(${#POINT_GROUPS[@]}) × 轮数(${RUNS})"
    exit 0
fi

t_start=$(date +%s)
for rep in $(seq 1 "$RUNS"); do
    for g in "${POINT_GROUPS[@]}"; do
        IFS='|' read -r dev fs wl njs <<< "$g"
        ts="rerun_r${rep}_$(date +%H%M%S)"
        stem="${dev}_${fs}_${wl}_rep${rep}"
        echo "[rerun] rep=${rep}/${RUNS}  ${dev}/${fs}/${wl} nj=${njs}  ts=${ts}  $(date '+%H:%M:%S')"
        docker exec -e NIX_CONFIG="substitute = false" \
            -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
            -w /root/asterinas "$CONTAINER" \
            bash /root/impl/test/aster-ext4-baseline-test/run-baseline.sh \
            --devices "$dev" --fs "$fs" --workloads "$wl" --modes orderless \
            --njobs "$njs" --runtime 30 --smp 64 --timeout 300 \
            --file-range-mb 256 --wr-capacity-mb 64 \
            --rnvpc-kcmd "$RNVPC_KCMD" \
            --timestamp "$ts" "${SKIP_ARG[@]}" --confirm-destroy \
            > "${OUT}/drv_${stem}.log" 2>&1
        rc=$?
        if [ -f "results/${ts}_aster-ext4-baseline/summary.csv" ]; then
            cp "results/${ts}_aster-ext4-baseline/summary.csv" "${OUT}/${stem}.csv"
            echo "         → ${stem}.csv（$(($(wc -l < "${OUT}/${stem}.csv")-1)) 个点）"
        else
            echo "         ⚠ 没有 summary.csv（driver rc=$rc），看 ${OUT}/drv_${stem}.log" >&2
            : > "${OUT}/${stem}.csv.missing"
        fi
    done
done
echo
echo "[rerun] 全部完成，用时 $(( ($(date +%s) - t_start) / 60 )) 分钟 → ${OUT}"
