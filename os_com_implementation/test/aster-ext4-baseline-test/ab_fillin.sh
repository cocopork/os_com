#!/bin/bash
# 补齐「144 点矩阵里 orderless 异常点」的**重测数据**：只跑默认臂（njmb=on/njwb=on），
# 每个点重复 REPEATS 次，用来回答"这些点快起来是多少"。
#
# 覆盖范围：
#   sata/randwrite-4k  nj 4,16,20,28,32   （异常点只有 nj20；其余 4 个作为同组对照）
#   sata/seqwrite-4k   nj 4,16,20,28,32   （异常点 nj4,16,28,32）
#   nvme/seqwrite-4k   nj 16,24,28,32     （异常点 nj16,24,28,32）
# nvme/randwrite-4k 的 nj12,16,20,28 已在三批 A/B 里各有 9 次观测，不必重跑。
#
# 用法：REPEATS=3 nohup bash ab_fillin.sh > ./ab_fillin.log 2>&1 &

set -u
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

B="${OS_COM_IMPL_ROOT}/test/aster-ext4-baseline-test"
REPEATS="${REPEATS:-3}"
MODE="${MODE:-orderless}"

run_one() {
    local ts="$1"; shift
    bash "${B}/run-baseline.sh" --confirm-destroy --njmb on --njwb on \
        --modes "${MODE}" --timestamp "${ts}" "$@"
}

echo "###### ab_fillin start $(date -Is) repeats=${REPEATS} ######"
for rep in $(seq 1 "${REPEATS}"); do
    echo ""
    echo "###### repeat=${rep} sata ######"
    run_one "$(date +%Y%m%d_%H%M%S)" --devices sata --fs ext2like \
        --workloads randwrite-4k,seqwrite-4k --njobs 4,16,20,28,32
    echo "###### repeat=${rep} nvme seqwrite ######"
    run_one "$(date +%Y%m%d_%H%M%S)" --devices nvme --fs ext2like \
        --workloads seqwrite-4k --njobs 16,24,28,32
done
echo ""
echo "###### ab_fillin all done $(date -Is) ######"
