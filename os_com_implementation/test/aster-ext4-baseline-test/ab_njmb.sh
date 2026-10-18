#!/bin/bash
# 008 方案 A（nojournal extent 节点批量缓冲）**判定性 A/B** 驱动脚本。
#
# 目的：144 点基线矩阵里 ext2like/orderless 出现 12 个异常低态（紧致落在 12.2-13.0 MiB/s），
# 需要判定「异常是否由方案 A 引起」。此前那次 A/B 是**跨构建**比较（改编译期常量要重建），
# 且每格只测 1 次，被双稳噪声淹没（同配置散在 0.41 / 2.11 / 40.38），无法判定。
#
# 本次设计：
#   - 方案 A 已改成**挂载选项** `nojournal_meta_batch=0|1`，故 on/off 用**同一个内核**，
#     不存在构建差异；
#   - 逐 repeat **交替**两只臂（off,on,off,on,off,on），抵消宿主负载 / 设备内部状态的漂移；
#   - 每格 3 次重复：异常低态是"稳定吸引子"而非高斯噪声，单次测量区分不了"状态"与"噪声"。
#
# 靶点：nvme/ext2like/randwrite-4k/orderless
#   nj12/16/20/28 = 基线里的慢态（12.2-13.0 MiB/s，只写 ~390 MiB）
#   nj8 /nj24     = 基线里的快态（251 / 218 MiB/s，写满 2048 MiB）→ 作对照，检验
#                   「关掉方案 A 会不会把快态也弄坏」
#
# 判据（结合 parse_baseline.py 的中位 drain_bw）：
#   off 臂慢态依旧 ~12.5  → 方案 A 无责，异常由 008 未实现项（缺元数据 write-behind）主导
#   off 臂慢态消失（≥200）→ 方案 A 有害（合并收益被全局 end 提前 flush 摧毁），应默认关掉
#   off 臂快态也被弄慢     → 方案 A 有害且必要，需改实现（per-inode 作用域）而非直接关
#
# 用法：nohup bash ab_njmb.sh > ./ab_njmb.log 2>&1 &

set -u
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

B="${OS_COM_IMPL_ROOT}/test/aster-ext4-baseline-test"

# 臂：off(0) 不缓冲 / global(1) 旧实现（每次 end 全量落盘）/ on(2) owner 分桶（默认）
ARMS="${ARMS:-off on}"
REPEATS="${REPEATS:-3}"
NJOBS="${NJOBS:-8,12,16,20,24,28}"
DEV="${DEV:-nvme}"
FS="${FS:-ext2like}"
WORKLOAD="${WORKLOAD:-randwrite-4k}"
MODE="${MODE:-orderless}"

COMMON=(--confirm-destroy --devices "${DEV}" --fs "${FS}"
        --workloads "${WORKLOAD}" --modes "${MODE}" --njobs "${NJOBS}")

# 臂 → 命令行。接受两类臂名：
#   on / off / global / owner   → `--njmb <arm>`（方案 A 的落盘作用域）
#   wb-on / wb-off              → `--njwb on|off`（元数据 write-behind，008 核心缺口）
run_arm() {
    local arm="$1" ts="$2"
    case "$arm" in
        wb-on)  bash "${B}/run-baseline.sh" "${COMMON[@]}" --njwb on    --timestamp "${ts}" ;;
        wb-sync) bash "${B}/run-baseline.sh" "${COMMON[@]}" --njwb sync --timestamp "${ts}" ;;
        wb-off) bash "${B}/run-baseline.sh" "${COMMON[@]}" --njwb off  --timestamp "${ts}" ;;
        barrier-merge) bash "${B}/run-baseline.sh" "${COMMON[@]}" --barrier merge --timestamp "${ts}" ;;
        barrier-each)  bash "${B}/run-baseline.sh" "${COMMON[@]}" --barrier each  --timestamp "${ts}" ;;
        *)      bash "${B}/run-baseline.sh" "${COMMON[@]}" --njmb "${arm}" --timestamp "${ts}" ;;
    esac
}

echo "###### ab_njmb start $(date -Is) ######"
echo "###### arms='${ARMS}' repeats=${REPEATS} njobs=${NJOBS} ######"
echo "###### dev=${DEV} fs=${FS} workload=${WORKLOAD} mode=${MODE} ######"
for rep in $(seq 1 "${REPEATS}"); do
    for arm in ${ARMS}; do
        ts="$(date +%Y%m%d_%H%M%S)"
        echo ""
        echo "###### repeat=${rep} arm=${arm} timestamp=${ts} $(date -Is) ######"
        run_arm "${arm}" "${ts}"
        echo "###### repeat=${rep} arm=${arm} done rc=$? $(date -Is) ######"
    done
done
echo ""
echo "###### ab_njmb all done $(date -Is) ######"
