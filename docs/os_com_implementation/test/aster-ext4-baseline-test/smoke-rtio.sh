#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# rNVPC / rNVPCe 快速 smoke：两个数据集档位 × {rnvpc, rnvpc_extend}
#
#   档位 small : file-range 64   MiB  < rnvpc_data_limit(192 MiB)
#                ⇒ 数据集可全驻留 NV，几乎不回刷 = "仅 NVDIMM" 行为
#   档位 large : file-range 1280 MiB  ≫ rnvpc_data_limit(192 MiB)
#                ⇒ 必然触发 NV→SSD 回刷 = "NVDIMM + SSD 都参与 IO" 行为
#
# 结果落点（bridge 已架好，零拷贝直接出现在宿主）：
#   os_com_implementation/test/aster-ext4-baseline-test/results/<ts>_aster-ext4-baseline/
#     logs/baseline_run.log                 ← 星绽全部输出（跨轮拼接）
#     logs/qemu-nvme-<fs>-randwrite-4k-orderless-nj8.log       ← 该测试点的 fio 数据
#     logs/qemu-serial-nvme-<fs>-randwrite-4k-orderless-nj8.log ← 该测试点的内核控制台
#     summary.csv                           ← 汇总
#
# 【用法】 bash smoke-rtio.sh [small|large|both]   （默认 both）
# =========================================================================
set -uo pipefail   # 故意不用 -e：某一档失败也要继续跑下一档

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

HOST_IMPL="${OS_COM_IMPL_ROOT}"
TEST_DIR=/root/impl/test/aster-ext4-baseline-test
CTR=asterinas-dev

# 容量对齐：rnvpc_size == writeback_capacity_mb(256M)；meta 64M + data 192M == 256M
KCMD="rnvpc_size=256M rnvpc_meta_reserve=64M rnvpc_data_limit=192M rnvpc_sim_mode=fast"
COMMON="--devices nvme --workloads randwrite-4k --modes orderless --njobs 8 \
--fs rnvpc,rnvpc_extend --runtime 30 --smp 64 --timeout 240 \
--njmb on --njwb on --barrier merge"

run_one() {
    local range="$1" name="$2"
    echo "############ 档位 $name (file-range-mb=$range) ############"
    docker exec \
        -e NIX_CONFIG="substitute = false" \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -w /root/asterinas "$CTR" \
        bash "$TEST_DIR/run-baseline.sh" \
        $COMMON --file-range-mb "$range" \
        --rnvpc-kcmd "$KCMD" --confirm-destroy < /dev/null \
        > "$HOST_IMPL/smoke-rtio-$name.log" 2>&1
    echo "  退出码=$?  运行器日志=$HOST_IMPL/smoke-rtio-$name.log"
    echo "  结果目录（本轮最新）:"
    ls -1dt "$HOST_IMPL/test/aster-ext4-baseline-test/results/"*/ 2>/dev/null | head -1
    echo "  summary.csv:"
    tail -n +1 "$(ls -1dt "$HOST_IMPL/test/aster-ext4-baseline-test/results/"*/ | head -1)summary.csv" 2>/dev/null | sed 's/^/    /'
}

case "${1:-both}" in
    small) run_one 64 small ;;
    large) run_one 1280 large ;;
    both)  run_one 64 small; run_one 1280 large ;;
    *) echo "用法: bash smoke-rtio.sh [small|large|both]" >&2; exit 2 ;;
esac
