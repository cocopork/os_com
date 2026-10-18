#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# 单点复现脚本：只跑一个 (rw, bs, numjobs, journal_mode) 点，用于死锁复现。
# 后端用 ext4.img 镜像（不直通真实盘），timeout 180 防死锁。
#
# 用法：bash run_single_round.sh <rw> <bs> <numjobs> <journal_mode>
#   例：bash run_single_round.sh randwrite 4K 16 journal

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

RW="${1:-randwrite}"
BS="${2:-4K}"
NJ="${3:-16}"
JM="${4:-journal}"
RUNTIME="${5:-60}"

BENCHMARK_JOB_NAME="ext4_journal_compare"
BENCHMARK_SRC_DIR="${REPO_CTR}/test/initramfs/src/benchmark"
FIO_DIR="${BENCHMARK_SRC_DIR}/fio"
JOB_DIR="${FIO_DIR}/${BENCHMARK_JOB_NAME}"
SUMMARY_YAML="${FIO_DIR}/summary.yaml"
GUEST_SCRIPT="${TEST_DIR_CTR}/ext4_journal_compare.sh"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULT_DIR_CTR="${TEST_DIR_CTR}/results/${TIMESTAMP}_single_round"

# ── docker self-entry ────────────────────────────────────────────
if [[ ! -f /.dockerenv ]]; then
    echo "=== single round: ${RW} ${BS} nj${NJ} ${JM} (ext4.img, smp=${SMP}, runtime=${RUNTIME}) ==="
    mkdir -p "${IMPL_DIR_HOST}"
    docker volume create asterinas-cargo  >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true

    exec docker run -i --rm --privileged --network=host \
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
        bash "${TEST_DIR_CTR}/run_single_round.sh" "$@" < /dev/null
fi

# ── below runs inside Docker ─────────────────────────────────────
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
    fi
    if ! grep -qF "${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}" 2>/dev/null; then
        sed -i "\$i\ \ - ${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}"
    fi
    echo "[install] done."
}

prep_disk() {
    local build_dir="${REPO_CTR}/test/initramfs/build"
    echo "[disk] creating fresh ext4 test image (xfstests_test.img)..."
    rm -f "${build_dir}/xfstests_test.img"
    truncate -s 2G "${build_dir}/xfstests_test.img"
    mke2fs -F -q -t ext4 -b 4096 \
        -O has_journal,extent,filetype,'^metadata_csum','^dir_index','^64bit','^flex_bg' \
        "${build_dir}/xfstests_test.img"
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

install_job
build_kernel
prep_disk
mkdir -p "${RESULT_DIR_CTR}"

echo "[run] === round: ${RW} ${BS} nj${NJ} ${JM} (ext4.img, runtime=${RUNTIME}) ==="
cd "${REPO_CTR}"
ATTACH_XFSTESTS_IMAGES=true \
timeout 3600 make run_kernel \
    "BENCHMARK=fio/${BENCHMARK_JOB_NAME}" \
    "BENCHMARK_ARGS=${RW} ${BS} ${NJ} ${JM} ${RUNTIME}" \
    "SMP=${SMP}" \
    "MEM=${MEM}" 2>&1 \
    | grep -vE "warning: unstable feature specified|warning: \`[^\`]+\` \((lib|bin \"[^\"]+\")\) generated 1 warning|^  = note: this feature is not stably supported|^  \||grub-mkrescue: warning: cannot open directory \`/usr/local/grub/share/locale\`" \
    | tee "${RESULT_DIR_CTR}/single_round_${RW}-${BS}-nj${NJ}-${JM}.log" || {
        rc=$?
        echo "[run] round FAILED or TIMED OUT (rc=${rc}); see ${RESULT_DIR_CTR}/single_round_${RW}-${BS}-nj${NJ}-${JM}.log"
    }

cp "${REPO_CTR}/qemu-serial.log" "${RESULT_DIR_CTR}/qemu-serial-single_${RW}-${BS}-nj${NJ}-${JM}.log" 2>/dev/null || true
if [[ -n "${HOST_UID:-}" && -n "${HOST_GID:-}" ]]; then
    chown -R "${HOST_UID}:${HOST_GID}" "${RESULT_DIR_CTR}" 2>/dev/null || true
fi
echo "[run] done. results -> ${RESULT_DIR_CTR}"
