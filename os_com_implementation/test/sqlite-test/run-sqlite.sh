#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  宏测试 harness：SQLite speedtest1（五档 writeback profile × sata/nvme）
# =========================================================================
#
# 口径与 fio 基线（aster-ext4-baseline-test/run-baseline.sh）对齐：
#   device : sata (/dev/sda4) | nvme (/dev/nvme3n1p2)   真盘，每轮重新 mke2fs
#   fs     : 与 fio 基线一致的五档 writeback profile
#   size   : speedtest1 --size（默认 1000）；可用 --sizes 做多档
#   runs   : 同一配置重复轮数（默认 3；每轮重建库文件）
#
#  测试点 = device × fs × size
#    默认 2 × 5 × 1 = 10 点
#
# 【这是一条"性能 + 数据无损"绑定的负载】每轮跑完都对库文件做
#   `PRAGMA integrity_check`，只有 ok 才算该点成立（与 P9 计分板口径一致）。
#
# ⚠️ 当前 Asterinas **预期**在 speedtest1 上崩：test 150 (CREATE INDEX) 的
#   reserialize mega-buffer 堆耗尽（P9 `p9-perf-basket` / `extent-flatten-storm`）。
#   harness 会把 crash 如实记进 summary.csv（status=crash + 崩点末段输出），
#   **不要**把它当脚本故障：崩点本身就是本轮要测的 before 值。
#
# 【前置】无额外二进制需求 —— `sqlite-speedtest1` 与 `sqlite3` 已由
#   test/initramfs/nix/benchmark/default.nix 打进 initramfs；静态 mke2fs 从 fio job
#   借用。job 目录（sqlite/aster_ext4_macro）与 summary.yaml 登记由本脚本自动完成。
#
# 【用法】
#   bash run-sqlite.sh --dry-run                     # 只看矩阵与点数
#   bash run-sqlite.sh                                # 跑（默认 10 点）
#   bash run-sqlite.sh --fs ext4,rnvpc_extend --sizes 100,1000
#   bash run-sqlite.sh --devices nvme --runs 5
#
# 【必须串行】真实盘 + 单 QEMU，一次只能一个 boot。
# =========================================================================

set -e

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

REPO_HOST="${OS_COM_ROOT}"
REPO_CTR="/root/asterinas"
IMPL_DIR_HOST="${OS_COM_IMPL_ROOT}"
IMPL_DIR_CTR="/root/impl"
TEST_DIR_CTR="${IMPL_DIR_CTR}/test/sqlite-test"

SMP=64
# 16G：4G 下 speedtest1 --size 1000 会把 guest 内存吃爆
# （内核报 `Failed to allocate a large slot` + `Heap allocation error size: 0x1000`），
# 并伴随 sqlite 进程 rc=1/rc=139 崩。宿主 125G 总量，取 16G 留余量。
MEM="16G"
RELEASE=1
TIMEOUT_SEC=180          # 用户约束：每个测试点最多 3 分钟
RUNS=1
SIZES="1000"
WRCAP_MB=256
RNVPC_KCMD="rnvpc_size=256M rnvpc_meta_reserve=64M rnvpc_data_limit=192M rnvpc_sim_mode=fast"
NJMB="on"
NJWB="on"

declare -A DEVICE_PATHS=([sata]="/dev/sda4" [nvme]="/dev/nvme3n1p2")
DEVICES="sata,nvme"
# 与 fio writeback 矩阵一致的五档；真实 rnvpc/rnvpc_extend 仍可显式指定。
FS_LIST_STR="ext2like_sync,ext4_sync,ext2like_nomerge,ext4,ext4_nomerge"

DRY_RUN=false
CONFIRM_DESTROY=false
TIMESTAMP=""

while [ $# -gt 0 ]; do
    case "$1" in
        --devices)   DEVICES="$2"; shift 2 ;;
        --fs)        FS_LIST_STR="$2"; shift 2 ;;
        --sizes)     SIZES="$2"; shift 2 ;;
        --runs)      RUNS="$2"; shift 2 ;;
        --timeout)   TIMEOUT_SEC="$2"; shift 2 ;;
        --mem)       MEM="$2"; shift 2 ;;
        --smp)       SMP="$2"; shift 2 ;;
        --njmb)      NJMB="$2"; shift 2 ;;
        --njwb)      NJWB="$2"; shift 2 ;;
        --wr-capacity-mb) WRCAP_MB="$2"; shift 2 ;;
        --rnvpc-kcmd) RNVPC_KCMD="$2"; shift 2 ;;
        --timestamp) TIMESTAMP="$2"; shift 2 ;;
        --dry-run|-n) DRY_RUN=true; shift ;;
        # 最小 smoke：单盘 × 五档 fs × size=100 × 1 轮，只验证“能不能跑通”。
        # 注意：后面的 --devices/--fs/--sizes/--runs 可覆盖它（顺序后者胜）。
        --smoke)     DEVICES="sata"; FS_LIST_STR="ext2like_sync,ext4_sync,ext2like_nomerge,ext4,ext4_nomerge"; SIZES="100"; RUNS=1; TIMEOUT_SEC=180; shift ;;
        --confirm-destroy|-y) CONFIRM_DESTROY=true; shift ;;
        -h|--help) sed -n '2,50p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
done

IFS=',' read -r -a DEVICE_LIST <<< "$DEVICES"
IFS=',' read -r -a FS_LIST <<< "$FS_LIST_STR"
IFS=',' read -r -a SIZE_LIST <<< "$SIZES"
for fs in "${FS_LIST[@]}"; do
    case "$fs" in
        ext4|ext2like|ext4_sync|ext2like_sync|ext4_nomerge|ext2like_nomerge|rnvpc|rnvpc_extend) ;;
        *) echo "Unknown fs profile: $fs (expect ext4|ext2like|ext4_sync|ext2like_sync|ext4_nomerge|ext2like_nomerge|rnvpc|rnvpc_extend)" >&2; exit 2 ;;
    esac
done

N_POINTS=$(( ${#DEVICE_LIST[@]} * ${#FS_LIST[@]} * ${#SIZE_LIST[@]} ))
[ -z "$TIMESTAMP" ] && TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULTS_DIR="${TEST_DIR_CTR}/results/${TIMESTAMP}_sqlite-macro"
LOGS_DIR="${RESULTS_DIR}/logs"
ASTER_LOG="${LOGS_DIR}/baseline_run.log"
BENCHMARK_JOB_NAME="aster_ext4_macro"
JOB_DIR="${REPO_CTR}/test/initramfs/src/benchmark/sqlite/${BENCHMARK_JOB_NAME}"
SUMMARY_YAML="${REPO_CTR}/test/initramfs/src/benchmark/fio/summary.yaml"
GUEST_SCRIPT="${TEST_DIR_CTR}/guest-run-sqlite.sh"
PARSE_SCRIPT="${TEST_DIR_CTR}/parse-sqlite.py"
SUMMARY_CSV="${RESULTS_DIR}/summary.csv"

if [[ "$DRY_RUN" == true ]]; then
    echo "=== DRY-RUN: SQLite speedtest1 宏测试 ==="
    echo "smp=${SMP} mem=${MEM} timeout=${TIMEOUT_SEC}s runs=${RUNS} sizes=${SIZES}"
    echo "fs=${FS_LIST_STR} wr-capacity=${WRCAP_MB}MiB"
    echo "rnvpc kcmd: ${RNVPC_KCMD}"
    idx=0
    for dev in "${DEVICE_LIST[@]}"; do
    for fs in "${FS_LIST[@]}"; do
    for sz in "${SIZE_LIST[@]}"; do
        idx=$((idx + 1))
        echo "  #${idx}: device=${dev}(${DEVICE_PATHS[$dev]}) fs=${fs} size=${sz} runs=${RUNS}"
    done; done; done
    echo "Total: ${idx} rounds × ${RUNS} runs"
    exit 0
fi

if [[ "$CONFIRM_DESTROY" == false ]]; then
    echo "error: --confirm-destroy is required (formats test devices)" >&2
    exit 2
fi

if [[ ! -f /.dockerenv ]]; then
    echo "=== SQLite 宏测试 (host → Docker) ==="
    for dev in "${DEVICE_LIST[@]}"; do
        p="${DEVICE_PATHS[$dev]}"
        if findmnt -rn "$p" &>/dev/null; then
            echo "ERROR: $p is mounted — refusing to proceed" >&2; exit 1
        fi
        if command -v fuser >/dev/null 2>&1 && fuser -v "$p" &>/dev/null; then
            echo "ERROR: $p is in use by another process" >&2; exit 1
        fi
    done
    mkdir -p "${IMPL_DIR_HOST}/test/sqlite-test"
    docker volume create asterinas-cargo  >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true
    nix_config_env=()
    [ -n "${NIX_CONFIG:-}" ] && nix_config_env=(-e "NIX_CONFIG=${NIX_CONFIG}")
    # 必须把宿主代理透传进容器：本机网络下容器内直连 cache.nixos.org 会超时(000)，
    # 一旦 nix 不能替换就会退化到源码构建（bash52-004 补丁在 ftpmirror 已 404 → 整条链挂）。
    proxy_env=()
    for v in http_proxy https_proxy HTTP_PROXY HTTPS_PROXY no_proxy NO_PROXY; do
        [ -n "${!v:-}" ] && proxy_env+=(-e "$v=${!v}")
    done
    exec docker run -i --rm --privileged --network=host \
        --device=/dev/kvm \
        -v /dev:/dev \
        -v "${REPO_HOST}:${REPO_CTR}" \
        -v "${IMPL_DIR_HOST}:${IMPL_DIR_CTR}" \
        -v asterinas-cargo:/root/.cargo \
        -v asterinas-rustup:/root/.rustup \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        "${nix_config_env[@]}" \
        "${proxy_env[@]}" \
        -w "${REPO_CTR}" \
        asterinas/asterinas:0.18.0-20260618 \
        bash "${TEST_DIR_CTR}/run-sqlite.sh" \
            --devices "$DEVICES" --fs "$FS_LIST_STR" --sizes "$SIZES" \
            --runs "$RUNS" --timeout "$TIMEOUT_SEC" --smp "$SMP" \
            --njmb "$NJMB" --njwb "$NJWB" --wr-capacity-mb "$WRCAP_MB" \
            --rnvpc-kcmd "$RNVPC_KCMD" --timestamp "$TIMESTAMP" \
            --confirm-destroy < /dev/null
fi

install_job() {
    echo "[install] installing sqlite/${BENCHMARK_JOB_NAME} job..."
    if ! bash -n "${GUEST_SCRIPT}"; then
        echo "error: guest script has syntax errors: ${GUEST_SCRIPT}" >&2; exit 2
    fi
    mkdir -p "${JOB_DIR}"
    cp "${GUEST_SCRIPT}" "${JOB_DIR}/run.sh"
    chmod +x "${JOB_DIR}/run.sh"

    local src_mke2fs="${REPO_CTR}/test/initramfs/src/benchmark/fio/ext4_journal_compare/mkfs/mke2fs"
    if [ -f "$src_mke2fs" ]; then
        mkdir -p "${JOB_DIR}/mkfs"
        cp "$src_mke2fs" "${JOB_DIR}/mkfs/mke2fs"
        chmod +x "${JOB_DIR}/mkfs/mke2fs"
        echo "[install] bundled static mke2fs"
    else
        echo "[install] WARNING: static mke2fs missing — 每轮重格式化退化为 rm-clean" >&2
    fi

    if ! grep -qF "${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}" 2>/dev/null; then
        sed -i "\$i\ \ - ${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}"
    fi
    echo "[install] done."
}

prep_disk() {
    local build_dir="${REPO_CTR}/test/initramfs/build"
    mkdir -p "$build_dir"
    if [ ! -f "${build_dir}/ext2.img" ]; then
        truncate -s 2G "${build_dir}/ext2.img"
        mke2fs -t ext2 -F -O ^ext_attr -O ^resize_inode -O ^dir_index "${build_dir}/ext2.img" 2>/dev/null || true
    fi
    if [ ! -f "${build_dir}/xfstests_scratch.img" ]; then
        truncate -s 256M "${build_dir}/xfstests_scratch.img"
        mke2fs -F -q -t ext4 -b 4096 "${build_dir}/xfstests_scratch.img" 2>/dev/null || true
    fi
    sync
}

build_kernel() {
    echo "[build] compiling Asterinas kernel (make kernel, RELEASE=${RELEASE})..."
    cd "${REPO_CTR}"
    make kernel "BENCHMARK=sqlite/${BENCHMARK_JOB_NAME}" "RELEASE=${RELEASE}" \
        "RNVPC_KCMD=${RNVPC_KCMD}" >> "${LOGS_DIR}/build_kernel.log" 2>&1
    echo "[build] done."
}

run_matrix() {
    echo ""
    echo "================================================================"
    echo "  SQLite speedtest1 宏测试 (smp=${SMP})  points=${N_POINTS} × runs=${RUNS}"
    echo "================================================================"
    : > "${ASTER_LOG}"
    for dev in "${DEVICE_LIST[@]}"; do
        local dev_path="${DEVICE_PATHS[$dev]}"
        for fs in "${FS_LIST[@]}"; do
        for sz in "${SIZE_LIST[@]}"; do
            local tag="${dev}-${fs}-size${sz}"
            echo "[sqlite] === round: ${tag} ==="
            prep_disk
            cd "${REPO_CTR}"
            XFSTESTS_TEST_IMAGE="${dev_path}" \
            XFSTESTS_TEST_CACHE="none" \
            XFSTESTS_TEST_AIO="threads" \
            ATTACH_XFSTESTS_IMAGES=true \
            NETDEV=none \
            timeout "${TIMEOUT_SEC}" make run_kernel \
                "BENCHMARK=sqlite/${BENCHMARK_JOB_NAME}" \
                "BENCHMARK_ARGS=${dev} ${fs} ${sz} ${RUNS} ${NJMB} ${NJWB} ${WRCAP_MB}" \
                "RELEASE=${RELEASE}" \
                "SMP=${SMP}" \
                "MEM=${MEM}" \
                "RNVPC_KCMD=${RNVPC_KCMD}" >> "${ASTER_LOG}" 2>&1 || true
            cp "${REPO_CTR}/qemu-serial.log" "${LOGS_DIR}/qemu-serial-${tag}.log" 2>/dev/null || true
            echo "[sqlite] --- round done: ${tag} ---"
        done; done
    done
    echo "[sqlite] log saved → ${ASTER_LOG}"
}

parse_summary() {
    if [ ! -f "${PARSE_SCRIPT}" ]; then
        echo "ERROR: ${PARSE_SCRIPT} not found — cannot parse summary" >&2; return 1
    fi
    python3 "${PARSE_SCRIPT}" "${ASTER_LOG}" "${SUMMARY_CSV}"
}

write_run_config() {
    cat > "${RESULTS_DIR}/run-config.txt" <<EOF
test=sqlite-speedtest1
timestamp=${TIMESTAMP}
devices=${DEVICES}
device_paths=sata:/dev/sda4,nvme:/dev/nvme3n1p2
profiles=${FS_LIST_STR}
profile.ext2like_sync=nojournal,wb_sync_wait=on
profile.ext4_sync=journal,wb_sync_wait=on
profile.ext2like_nomerge=nojournal,wb_merge=off
profile.ext4=journal,async,merge
profile.ext4_nomerge=journal,wb_merge=off
mkfs.common=ext4,block_size=4096,inode_size=256,extent,filetype,no_metadata_csum,no_dir_index,no_64bit,no_flex_bg,no_inline_data
mkfs.nojournal_extra=no_has_journal
benchmark=sqlite-speedtest1
integrity_check=PRAGMA_integrity_check
sizes=${SIZES}
runs=${RUNS}
writeback_capacity_mb=${WRCAP_MB}
nojournal_meta_batch=${NJMB}
nojournal_meta_writeback=${NJWB}
smp=${SMP}
memory=${MEM}
timeout_s=${TIMEOUT_SEC}
EOF
}

mkdir -p "${RESULTS_DIR}" "${LOGS_DIR}"
write_run_config
umask 002
chmod -R g+rwX "${RESULTS_DIR}" 2>/dev/null || true
install_job
build_kernel
run_matrix
if parse_summary; then
    echo "[sqlite] summary → ${SUMMARY_CSV}"
else
    echo "[sqlite] WARNING: summary parsing failed" >&2
fi
if [ -n "${HOST_UID:-}" ]; then
    chown -R "${HOST_UID}:${HOST_GID:-${HOST_UID}}" "${RESULTS_DIR}" 2>/dev/null || true
fi

echo ""
echo "================================================================"
echo " 完成。结果目录（宿主）："
echo "   ${IMPL_DIR_HOST}/test/sqlite-test/results/${TIMESTAMP}_sqlite-macro/"
echo "     summary.csv              每轮 status/耗时/integrity/crash_tail"
echo "     logs/baseline_run.log    全部轮次（speedtest1 原始输出在里面）"
echo "================================================================"
