#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  EXT4 性能矩阵（32 点）一键运行器 —— os_com 仓库，真实盘 sata/nvme
# =========================================================================
#
# 矩阵：
#   mod       ∈ { default, nojournal }
#   backend   ∈ { sata (/dev/sda3), nvme (/dev/nvme3n1p1) }
#   njobs     ∈ { 1, 16 }
#   workload  ∈ { random-4k, sequential-128k }
#   writetype ∈ { buffer, fsync }
#   固定：writeback_capacity_mb=64（os_com 忽略未知 option）、size=256M
#   共 2×2×2×2×2 = 32 个测试点。
#
# 每个点独立 VM boot（方案 B）：guest 内静态 mke2fs 格式化 /dev/vdc 后跑 fio。
# 结果串行独占宿主机，不得与其他 QEMU/性能实验并发。
#
# 用法（宿主机）：
#   bash /home/zn/Ant/Rust_EXT4/os_com_implementation/test/ext4-journal-compare/run_perf_matrix.sh \
#       [--only "mod backend workload njobs writetype" ...] [--dry-run]
#
# 产物：test/ext4-journal-compare/results/<timestamp>_ext4_perf_matrix/

set -e

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

REPO_HOST="${OS_COM_ROOT}"
REPO_CTR="/root/asterinas"
RESEARCH_HOST="/home/zn/Ant/Rust_EXT4/research1/implementation/evaluation"
RESEARCH_CTR="/root/ext4-eval"
TEST_DIRS_CTR="${RESEARCH_CTR}/test_dirs"
IMPL_DIR_HOST="${OS_COM_IMPL_ROOT}"
IMPL_DIR_CTR="/root/impl"
TEST_DIR_HOST="${IMPL_DIR_HOST}/test/ext4-journal-compare"
TEST_DIR_CTR="${IMPL_DIR_CTR}/test/ext4-journal-compare"

SMP=24
MEM="12G"
PER_ROUND_TIMEOUT=600   # 每点上限：boot(~60s) + mke2fs + fio(~40s) + 余量

BENCHMARK_JOB_NAME="ext4_perf_matrix"
BENCHMARK_SRC_DIR="${REPO_CTR}/test/initramfs/src/benchmark"
JOB_DIR="${BENCHMARK_SRC_DIR}/fio/${BENCHMARK_JOB_NAME}"
SUMMARY_YAML="${BENCHMARK_SRC_DIR}/fio/summary.yaml"
GUEST_SCRIPT="${TEST_DIR_CTR}/ext4_perf_matrix.sh"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULT_DIR_CTR="${TEST_DIR_CTR}/results/${TIMESTAMP}_ext4_perf_matrix"
RUN_LOG="${RESULT_DIR_CTR}/perf_matrix_run.log"

# 矩阵维度
MODS=(default nojournal)
NJOBS_LIST=(1 16)
WORKLOADS=(random-4k sequential-128k)
WRITETYPES=(buffer fsync)
declare -A DEVICE=([sata]="/dev/sda3" [nvme]="/dev/nvme3n1p1")

# 可选的 --only 过滤（空格分隔的 "mod backend workload njobs writetype" 元组）
ONLY_FILTER=()

# ── argument parsing ──────────────────────────────────────────────
DRY_RUN=false
while (($# > 0)); do
    case "$1" in
        --only)
            shift
            while (($# > 0)) && [[ "$1" != --* ]]; do
                ONLY_FILTER+=("$1")
                shift
            done
            ;;
        --dry-run) DRY_RUN=true; shift ;;
        *) echo "Usage: $0 [--only 'mod backend workload njobs writetype' ...] [--dry-run]" >&2; exit 2 ;;
    esac
done

match_only() {
    ((${#ONLY_FILTER[@]} == 0)) && return 0
    local key="$1 $2 $3 $4 $5"
    for f in "${ONLY_FILTER[@]}"; do
        [[ "$f" == "$key" ]] && return 0
    done
    return 1
}

if [[ "${DRY_RUN}" == true ]]; then
    for backend in sata nvme; do
        for mod in "${MODS[@]}"; do
            for nj in "${NJOBS_LIST[@]}"; do
                for wl in "${WORKLOADS[@]}"; do
                    for wt in "${WRITETYPES[@]}"; do
                        match_only "$mod" "$backend" "$wl" "$nj" "$wt" || continue
                        echo "DRY-RUN: mod=${mod} backend=${backend} workload=${wl} nj=${nj} wt=${wt} dev=${DEVICE[$backend]}"
                    done
                done
            done
        done
    done
    exit 0
fi

# ── docker self-entry ─────────────────────────────────────────────
if [[ ! -f /.dockerenv ]]; then
    echo "=== EXT4 perf matrix (os_com, sata+NVMe, smp=${SMP}) ==="
    for dev in "${DEVICE[@]}"; do
        if findmnt -rn "$dev" &>/dev/null; then
            echo "ERROR: $dev is mounted — refusing to proceed" >&2
            exit 1
        fi
    done
    mkdir -p "${IMPL_DIR_HOST}"
    docker volume create asterinas-cargo  >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true
    exec docker run -it --rm --privileged --network=host \
        --device=/dev/kvm \
        -v /dev:/dev \
        -v "${REPO_HOST}:${REPO_CTR}" \
        -v "${RESEARCH_HOST}:${RESEARCH_CTR}" \
        -v "${IMPL_DIR_HOST}:${IMPL_DIR_CTR}" \
        -v asterinas-cargo:/root/.cargo \
        -v asterinas-rustup:/root/.rustup \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        -w "${RESEARCH_CTR}" \
        asterinas/asterinas:0.18.0-20260618 \
        bash "${TEST_DIR_CTR}/run_perf_matrix.sh" "$@"
fi

# ── below runs inside Docker ──────────────────────────────────────
die() { echo "ERROR: $*" >&2; exit 1; }

install_job() {
    echo "[install] installing ${BENCHMARK_JOB_NAME} job..."
    mkdir -p "${JOB_DIR}"
    cp "${GUEST_SCRIPT}" "${JOB_DIR}/run.sh"
    chmod +x "${JOB_DIR}/run.sh"
    if [ -f "${TEST_DIRS_CTR}/mkfs-mke2fs-static" ]; then
        mkdir -p "${JOB_DIR}/mkfs"
        cp "${TEST_DIRS_CTR}/mkfs-mke2fs-static" "${JOB_DIR}/mkfs/mke2fs"
        chmod +x "${JOB_DIR}/mkfs/mke2fs"
    else
        echo "[install] WARNING: static mke2fs missing — per-round reformat disabled" >&2
    fi
    if ! grep -qF "${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}" 2>/dev/null; then
        sed -i "\$i\ \ - ${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}"
    fi
    echo "[install] done."
}

prep_disk() {
    local build_dir="${REPO_CTR}/test/initramfs/build"
    echo "[disk] ensuring auxiliary images (ext2.img + scratch)..."
    if [[ ! -f "${build_dir}/ext2.img" ]]; then
        truncate -s 2G "${build_dir}/ext2.img"
        mke2fs -t ext2 -F -O ^ext_attr -O ^resize_inode -O ^dir_index "${build_dir}/ext2.img"
    fi
    if [[ ! -f "${build_dir}/xfstests_scratch.img" ]]; then
        truncate -s 256M "${build_dir}/xfstests_scratch.img"
        mke2fs -F -q -t ext4 -b 4096 "${build_dir}/xfstests_scratch.img"
    fi
    sync
}

build_kernel() {
    echo "[build] compiling Asterinas kernel (make kernel)..."
    cd "${REPO_CTR}"
    make kernel "BENCHMARK=fio/${BENCHMARK_JOB_NAME}"
    echo "[build] done."
}

run_one() {
    local mod="$1" backend="$2" wl="$3" nj="$4" wt="$5"
    local dev="${DEVICE[$backend]}"
    local tag="${wl}-nj${nj}-${wt}-${mod}-${backend}"

    if findmnt -rn "$dev" &>/dev/null; then
        echo "[matrix] ERROR: $dev is mounted — skipping $tag" | tee -a "${RUN_LOG}"
        return 1
    fi

    echo ""
    echo "[matrix] === round: mod=${mod} backend=${backend} workload=${wl} nj=${nj} wt=${wt} dev=${dev} ===" | tee -a "${RUN_LOG}"

    cd "${REPO_CTR}"
    XFSTESTS_TEST_IMAGE="${dev}" \
    XFSTESTS_TEST_CACHE="none" \
    XFSTESTS_TEST_AIO="threads" \
    ATTACH_XFSTESTS_IMAGES=true \
    timeout "${PER_ROUND_TIMEOUT}" make run_kernel \
        "BENCHMARK=fio/${BENCHMARK_JOB_NAME}" \
        "BENCHMARK_ARGS=${mod} ${backend} ${wl} ${nj} ${wt}" \
        "SMP=${SMP}" \
        "MEM=${MEM}" 2>&1 \
        | grep -vE "warning: unstable feature specified|warning: \`[^\`]+\` \((lib|bin \"[^\"]+\")\) generated 1 warning|^  = note: this feature is not stably supported|^  \||grub-mkrescue: warning: cannot open directory \`/usr/local/grub/share/locale\`" \
        | tee -a "${RUN_LOG}" || true

    cp "${REPO_CTR}/qemu-serial.log" "${RESULT_DIR_CTR}/perf-matrix-${tag}.log" 2>/dev/null || true
    echo "[matrix] --- round done: ${tag} ---" | tee -a "${RUN_LOG}"
}

echo ""
echo "================================================================"
echo "  EXT4 perf matrix (os_com, sata+NVMe, smp=${SMP})"
echo "  ${#ONLY_FILTER[@]} filter(s), dry_run=${DRY_RUN}"
echo "================================================================"

mkdir -p "${RESULT_DIR_CTR}"
: > "${RUN_LOG}"

if [[ "${DRY_RUN}" == true ]]; then
    for backend in sata nvme; do
        for mod in "${MODS[@]}"; do
            for nj in "${NJOBS_LIST[@]}"; do
                for wl in "${WORKLOADS[@]}"; do
                    for wt in "${WRITETYPES[@]}"; do
                        match_only "$mod" "$backend" "$wl" "$nj" "$wt" || continue
                        echo "DRY-RUN: mod=${mod} backend=${backend} workload=${wl} nj=${nj} wt=${wt} dev=${DEVICE[$backend]}"
                    done
                done
            done
        done
    done
    exit 0
fi

install_job
prep_disk
build_kernel

TOTAL=0
DONE=0
for backend in sata nvme; do
    for mod in "${MODS[@]}"; do
        for nj in "${NJOBS_LIST[@]}"; do
            for wl in "${WORKLOADS[@]}"; do
                for wt in "${WRITETYPES[@]}"; do
                    match_only "$mod" "$backend" "$wl" "$nj" "$wt" || continue
                    TOTAL=$((TOTAL + 1))
                    if run_one "$mod" "$backend" "$wl" "$nj" "$wt"; then
                        DONE=$((DONE + 1))
                    fi
                done
            done
        done
    done
done

# re-own results to the host user
if [[ -n "${HOST_UID}" && -n "${HOST_GID}" ]]; then
    chown -R "${HOST_UID}:${HOST_GID}" "${RESULT_DIR_CTR}" 2>/dev/null || true
fi

echo ""
echo "================================================================"
echo "  Done: ${DONE}/${TOTAL} rounds completed"
echo "  Log:  ${RUN_LOG}"
echo "================================================================"
