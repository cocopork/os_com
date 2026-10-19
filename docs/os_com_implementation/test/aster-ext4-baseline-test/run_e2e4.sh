#!/usr/bin/env bash
# ext2like vs ext4 对比：nj16 randwrite-4k orderless，默认挂载选项（无臂后缀，可被 latency_decomp.py 识别）
# 用法：REPEATS=3 bash run_e2e4.sh
set -u
B="$(cd "$(dirname "$0")" && pwd)"
REPEATS="${REPEATS:-3}"
NJOBS="${NJOBS:-16}"
WORKLOAD="${WORKLOAD:-randwrite-4k}"
MODE="${MODE:-orderless}"
COMMON=(--confirm-destroy --devices nvme --fs ext2like,ext4
        --workloads "${WORKLOAD}" --modes "${MODE}" --njobs "${NJOBS}")
for rep in $(seq 1 "${REPEATS}"); do
    ts="$(date +%Y%m%d_%H%M%S)"
    echo "###### rep=${rep} ts=${ts} $(date -Is) ######"
    bash "${B}/run-baseline.sh" "${COMMON[@]}" --timestamp "${ts}"
    echo "###### rep=${rep} done $(date -Is) ######"
done
echo "###### all done $(date -Is) ######"
