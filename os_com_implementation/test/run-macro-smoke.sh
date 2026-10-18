#!/usr/bin/env bash
# 宏测试“最小 smoke”：只验证 filebench / sqlite 两条链路能不能跑通。
#
# 为什么需要它：宏测试每次都要重格式化真盘 + 起 QEMU，失败要十几分钟才知道。
# smoke 用最小矩阵（单盘、size=100、runtime=10s、每点 1 轮）把
#   job 安装 → initramfs 构建 → guest 启动 → mkfs/mount → 负载跑出 ops → 结果落盘
# 整条链路先验一遍，再上完整矩阵。
#
# 【口径】每个测试点**只跑 1 轮**（2026-09-24 起）。
#
# 【不打断正在跑的实验】默认会先等所有在跑的 harness（run-sqlite.sh /
# run-filebench.sh / run-baseline.sh 容器）结束再开始——真盘必须串行。
# 想不等就加 --no-wait。
#
# 用法：
#   bash run-macro-smoke.sh                 # 等当前实验结束 → filebench smoke → sqlite smoke
#   bash run-macro-smoke.sh --only sqlite
#   bash run-macro-smoke.sh --no-wait
#   bash run-macro-smoke.sh --dry-run
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS="${HERE}/results"
TS="$(date +%Y%m%d_%H%M%S)"
OUT="${RESULTS}/${TS}_smoke"

ONLY="both"
WAIT_FOR_CURRENT=true
DRY_RUN=false

while [ $# -gt 0 ]; do
    case "$1" in
        --only)      ONLY="$2"; shift 2 ;;
        --no-wait)   WAIT_FOR_CURRENT=false; shift ;;
        --dry-run|-n) DRY_RUN=true; shift ;;
        -h|--help)   sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$OUT"
say() { echo "[smoke] $*" | tee -a "${OUT}/smoke.log"; }

# ── 等在跑的 harness 结束（真盘串行约束）───────────────────────────────
wait_for_current() {
    local busy
    while :; do
        busy="$(docker ps --no-trunc --format '{{.Command}}' 2>/dev/null \
                | grep -cE 'run-sqlite\.sh|run-filebench\.sh|run-baseline\.sh' || true)"
        if [ "${busy:-0}" -eq 0 ]; then
            say "没有在跑的 harness 了，开始 smoke"
            return 0
        fi
        say "仍有 ${busy} 个 harness 在跑（真盘要串行），60s 后再看…"
        sleep 60
    done
}

run_filebench_smoke() {
    say "=== filebench smoke（sata × ext4/ext2like × write-heavy × 4线程 × 10s）==="
    if [ "$DRY_RUN" = true ]; then
        bash "${HERE}/filebench-test/run-filebench.sh" --smoke --dry-run 2>&1 | tee -a "${OUT}/smoke.log"
        return 0
    fi
    bash "${HERE}/filebench-test/run-filebench.sh" --smoke --confirm-destroy \
        > "${OUT}/filebench-smoke.log" 2>&1
    local rc=$?
    say "filebench smoke 退出码=${rc}（日志 ${OUT}/filebench-smoke.log）"
    return $rc
}

run_sqlite_smoke() {
    say "=== sqlite smoke（sata × ext4/ext2like × size=100 × 1 轮）==="
    if [ "$DRY_RUN" = true ]; then
        bash "${HERE}/sqlite-test/run-sqlite.sh" --smoke --dry-run 2>&1 | tee -a "${OUT}/smoke.log"
        return 0
    fi
    bash "${HERE}/sqlite-test/run-sqlite.sh" --smoke --confirm-destroy \
        > "${OUT}/sqlite-smoke.log" 2>&1
    local rc=$?
    say "sqlite smoke 退出码=${rc}（日志 ${OUT}/sqlite-smoke.log）"
    return $rc
}

say "宏测试 smoke 开始；产物目录 ${OUT}"
say "口径：每点 1 轮；--only=${ONLY}；wait_for_current=${WAIT_FOR_CURRENT}"

[ "$DRY_RUN" = true ] || [ "$WAIT_FOR_CURRENT" = false ] || wait_for_current

rc_fb=0; rc_sq=0
case "$ONLY" in
    both)     run_filebench_smoke; rc_fb=$?; run_sqlite_smoke; rc_sq=$? ;;
    filebench) run_filebench_smoke; rc_fb=$? ;;
    sqlite)   run_sqlite_smoke; rc_sq=$? ;;
    *) echo "--only 只能是 both|filebench|sqlite" >&2; exit 2 ;;
esac

say "---- smoke 汇总 ----"
say "filebench rc=${rc_fb}  sqlite rc=${rc_sq}"
for f in "${OUT}/filebench-smoke.log" "${OUT}/sqlite-smoke.log"; do
    [ -f "$f" ] || continue
    say "--- $(basename "$f") 关键行 ---"
    grep -aE "FILEBENCH_RESULT|SQLITE_RESULT|IO Summary|round:" "$f" | tail -8 | sed 's/^/    /' | tee -a "${OUT}/smoke.log"
done
[ $((rc_fb + rc_sq)) -eq 0 ] && say "SMOKE OK" || say "SMOKE 有失败项，见上面 rc"
exit $((rc_fb != 0 || rc_sq != 0))
