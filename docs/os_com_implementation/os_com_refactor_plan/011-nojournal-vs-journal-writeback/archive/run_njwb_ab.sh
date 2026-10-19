#!/usr/bin/env bash
# njwb A/B：窗口落盘 `sync`（同步写设备，旧行为）vs `async`（异步转交脏块缓冲）。
# 只影响 nojournal 卷的后台写回路径；同一内核内切换，逐 repeat 交替两只臂。
# 用法：REPEATS=3 bash run_njwb_ab.sh
set -u
B="$(cd "$(dirname "$0")" && pwd)"
REPEATS="${REPEATS:-3}"
NJOBS="${NJOBS:-16}"
WORKLOAD="${WORKLOAD:-randwrite-4k}"
MODE="${MODE:-orderless}"
FS="${FS:-ext2like}"
COMMON=(--confirm-destroy --devices nvme --fs "${FS}"
        --workloads "${WORKLOAD}" --modes "${MODE}" --njobs "${NJOBS}")
for rep in $(seq 1 "${REPEATS}"); do
    for arm in ${ARMS:-sync async asyncbio}; do
        ts="$(date +%Y%m%d_%H%M%S)"
        echo "###### rep=${rep} njwb=${arm} ts=${ts} $(date -Is) ######"
        bash "${B}/run-baseline.sh" "${COMMON[@]}" --njwb "${arm}" --timestamp "${ts}"
        echo "###### rep=${rep} njwb=${arm} done rc=$? $(date -Is) ######"
    done
done
echo "###### all done $(date -Is) ######"
