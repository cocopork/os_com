#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  EXT4 journal vs nojournal 对比基准 — 一键运行器（真实盘 sda3）
# =========================================================================
#
# 【代码与容器环境】本脚本编译并运行 **os_com** 仓库
#   （/home/zn/Ant/Rust_EXT4/os_com → 容器内 /root/asterinas），使用镜像
#   asterinas/asterinas:0.18.0-20260618，cargo 缓存用独立的
#   asterinas-cargo / asterinas-rustup 命名卷。
#   —— 与 asterinas_ext4 仓库（asterinas/dev:0.18.1-20260805、asterinas-ext4-*
#   常驻容器）完全隔离，不共享代码/容器/缓存。
#   —— 唯一共享的是真实盘 /dev/sda3 与 /dev/kvm：两者必须分时复用，
#   同一时刻只能有一个仓库跑真实盘性能实验。
#
# 【做什么】从宿主机一键完成：进入 Docker → 编译 Asterinas 内核 → 在 QEMU
# （smp=24）中跑 ext4 buffered fio 矩阵：随机 4K 写 + 顺序 128K 写，
# numjobs ∈ {1,4,16}，每次 write 后 fsync=1，journal 与 nojournal 两个对象。
# 测试盘为真实盘 /dev/sda3（virtio-blk 直通，cache=none,aio=threads）。
#
# 【用法】宿主机上运行（无需 sudo，只需 docker 组权限；容器内 root 格式化真实盘）：
#   bash /home/zn/Ant/Rust_EXT4/os_com_implementation/test/ext4-journal-compare/run_ext4_journal_compare.sh
# 每次运行在 test/ext4-journal-compare/results/<时间戳>_ext4_journal_compare/ 子目录，
# journal_compare_run.log、每轮 qemu-serial / qemu log、summary.csv 都归档到该目录。
#
# 【前置】
#   - /dev/sda3 未挂载、未被占用（runner 会检查）
#   - Docker 可访问 /dev/kvm；x86_64 宿主；用户需在 docker 组（免 sudo）
#
# 【必须串行】性能基准一次只能跑一个 boot，不得与其他 QEMU/性能实验并发。
# =========================================================================

set -e

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

REPO_HOST="${OS_COM_ROOT}"
REPO_CTR="/root/asterinas"
RESEARCH_HOST="/home/zn/Ant/Rust_EXT4/research1/implementation/evaluation"
RESEARCH_CTR="/root/ext4-eval"
TEST_DIRS_HOST="${RESEARCH_HOST}/test_dirs"
TEST_DIRS_CTR="${RESEARCH_CTR}/test_dirs"
IMPL_DIR_HOST="${OS_COM_IMPL_ROOT}"
IMPL_DIR_CTR="/root/impl"
TEST_DIR_HOST="${IMPL_DIR_HOST}/test/ext4-journal-compare"
TEST_DIR_CTR="${IMPL_DIR_CTR}/test/ext4-journal-compare"

# TEST_DEVICE="/dev/nvme3n1p2"
TEST_DEVICE="/dev/sda4"
SMP=24
MEM="4G"

# ── argument parsing ──────────────────────────────────────────────
PRETEST=false
DRY_RUN=false
# journal | nojournal（默认 journal；用 `--journal-mode=nojournal` 跑 nojournal）
JOURNAL_MODE="journal"
for arg in "$@"; do
    case "${arg}" in
        --pretest|-p) PRETEST=true ;;
        --dry-run|-n) DRY_RUN=true ;;
        --journal-mode=*) JOURNAL_MODE="${arg#*=}" ;;
        *) echo "Usage: $0 [--pretest|-p] [--dry-run|-n] [--journal-mode=journal|nojournal]" >&2; exit 2 ;;
    esac
done
case "${JOURNAL_MODE}" in
    journal|nojournal) ;;
    *) echo "Unknown journal mode: ${JOURNAL_MODE} (expect journal|nojournal)" >&2; exit 2 ;;
esac

TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
# 每次运行一个带时间戳的结果子目录，所有 log 集中存放。
RESULTS_DIR="${TEST_DIR_CTR}/results/${TIMESTAMP}_ext4_journal_compare"
ASTER_LOG="${RESULTS_DIR}/journal_compare_run.log"

BENCHMARK_JOB_NAME="ext4_journal_compare"
BENCHMARK_SRC_DIR="${REPO_CTR}/test/initramfs/src/benchmark"
FIO_DIR="${BENCHMARK_SRC_DIR}/fio"
JOB_DIR="${FIO_DIR}/${BENCHMARK_JOB_NAME}"
SUMMARY_YAML="${FIO_DIR}/summary.yaml"
# guest 脚本与 runner 同目录（os_com_implementation 自包含）
GUEST_SCRIPT="${TEST_DIR_CTR}/ext4_journal_compare.sh"
# fio JSON 解析脚本 + 裸设备基线（用于计算相对带宽比例）。
PARSER_SCRIPT="${TEST_DIR_CTR}/parse_summary.py"
SUMMARY_CSV="${RESULTS_DIR}/summary.csv"
# 裸设备基线目录（容器内只读挂载 research1/motivation）；/dev/sda4 为 SATA。
RAW_BASELINE_DIR_CTR="/root/motivation/test_dir/vm-raw-device-baseline/results/20260917-154858-vm-raw-device-baseline/asterinas"
RAW_BACKEND="sata"
# RAW_BASELINE_DIR_CTR="/root/motivation/test_dir/vm-raw-device-baseline/results/20260917-154858-vm-raw-device-baseline/asterinas"
# RAW_BACKEND="sata"

# 测试矩阵：随机 4K 写 + 顺序 128K 写 × numjobs × journal/nojournal × 写模式
# 【写回扫描粒度实验 v2】numjobs × 每 job 文件数。每文件固定 4MB（extent 树
# 复杂度一致），time_based 跑满 60s 测稳态。验证假设：writeback 一轮扫描所有
# dirty inode 时，inode 数越多一轮越长、前台被 throttle 阻塞越久。
# 场景：numjobs × nrfiles（每 job 文件数）。总文件数 = numjobs × nrfiles。
SCENARIOS=(
    "16 1"     # nj16, 16 文件（每 job 1 文件，每文件 4MB）—— 对照
    "16 16"    # nj16, 256 文件（每 job 16 文件，每文件 4MB）—— journal 瓶颈
)
RW="randwrite"
BS="4K"
# JOURNAL_MODE 由参数解析段设置（默认 journal）。
WRITETYPE="buffer"
PREALLOC="0"
# fio runtime 上限（秒）：guest run.sh 的第 6 个位置参数。
RUNTIME_LIMIT="60"
# 构建 profile：1 = release（关闭 debug_assertions 的节点缓存验证读），0 = dev
RELEASE=1

# ── dry-run：只打印矩阵展开，不编译、不启动 Docker/QEMU ──────────
if [[ "${DRY_RUN}" == true ]]; then
    echo "=== DRY-RUN: writeback scan-granularity experiment ==="
    echo "Test dev: ${TEST_DEVICE}  smp=${SMP}  mem=${MEM}"
    idx=0
    for sc in "${SCENARIOS[@]}"; do
        set -- ${sc}
        nj="$1"; nf="$2"
        idx=$((idx + 1))
        echo "  #${idx}: rw=${RW} bs=${BS} numjobs=${nj} nrfiles=${nf} journal_mode=${JOURNAL_MODE} writetype=${WRITETYPE} prealloc=${PREALLOC}"
    done
    echo "Total: ${idx} rounds"
    exit 0
fi

# ── docker self-entry ────────────────────────────────────────────
if [[ ! -f /.dockerenv ]]; then
    echo "=== EXT4 journal vs nojournal compare (host → Docker) ==="
    echo "Repo:     ${REPO_HOST} (os_com)"
    echo "Image:    asterinas/asterinas:0.18.0-20260618"
    echo "Test dev: ${TEST_DEVICE}  smp=${SMP}"

    # ── 启动前清理残留 QEMU ────────────────────────────────────
    # 上次运行被 Ctrl+C 中断时，容器内的 QEMU 可能变成孤儿进程继续
    # 持有 ext2.img 的写锁，导致本轮 QEMU 启动报
    # "Failed to get write lock"。这里在宿主机侧清理占用者（宿主能
    # 看到容器内进程），避免锁冲突重演。ext2.img 只被 QEMU 使用。
    local_ext2_img="${REPO_HOST}/test/initramfs/build/ext2.img"
    if [[ -f "${local_ext2_img}" ]] && fuser "${local_ext2_img}" >/dev/null 2>&1; then
        echo "WARNING: ${local_ext2_img} is held by a stale process — killing it" >&2
        fuser -k -9 "${local_ext2_img}" 2>/dev/null || true
        sleep 2
        if fuser "${local_ext2_img}" >/dev/null 2>&1; then
            echo "ERROR: ${local_ext2_img} still in use after cleanup — aborting" >&2
            exit 1
        fi
    fi

    # 真实盘安全前置检查（宿主机侧）
    if findmnt -rn "${TEST_DEVICE}" &>/dev/null; then
        echo "ERROR: ${TEST_DEVICE} is mounted — refusing to proceed" >&2
        exit 1
    fi
    if fuser -v "${TEST_DEVICE}" &>/dev/null; then
        echo "ERROR: ${TEST_DEVICE} is in use by another process" >&2
        exit 1
    fi

    mkdir -p "${IMPL_DIR_HOST}"

    docker volume create asterinas-cargo  >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true

    # 注意：不用 -it（TTY 在管道/后台环境下会让 QEMU 收到 SIGTTOU/SIGSTOP 而
    # 挂起，现象为 qemu 进程状态 T、串口停在 banner）；-i + </dev/null 让 stdio
    # chardev 的 stdin 立即 EOF，与 run_single_round.sh / run_perf_matrix_img.sh
    # 的做法一致。
    exec docker run -i --rm --privileged --network=host \
        --device=/dev/kvm \
        -v /dev:/dev \
        -v "${REPO_HOST}:${REPO_CTR}" \
        -v "${RESEARCH_HOST}:${RESEARCH_CTR}" \
        -v "${IMPL_DIR_HOST}:${IMPL_DIR_CTR}" \
        -v /home/zn/Ant/Rust_EXT4/research1/motivation:/root/motivation:ro \
        -v asterinas-cargo:/root/.cargo \
        -v asterinas-rustup:/root/.rustup \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        -w "${RESEARCH_CTR}" \
        asterinas/asterinas:0.18.0-20260618 \
        bash "${TEST_DIR_CTR}/run_ext4_journal_compare.sh" "$@" < /dev/null
fi

# ── below runs inside Docker ─────────────────────────────────────
die() { echo "ERROR: $*" >&2; exit 1; }

# Install the benchmark job into the repo (refresh run.sh + static mke2fs).
install_job() {
    echo "[install] installing ${BENCHMARK_JOB_NAME} job..."
    mkdir -p "${JOB_DIR}"
    cp "${GUEST_SCRIPT}" "${JOB_DIR}/run.sh"
    chmod +x "${JOB_DIR}/run.sh"

    if [ -f "${TEST_DIRS_CTR}/mkfs-mke2fs-static" ]; then
        mkdir -p "${JOB_DIR}/mkfs"
        cp "${TEST_DIRS_CTR}/mkfs-mke2fs-static" "${JOB_DIR}/mkfs/mke2fs"
        chmod +x "${JOB_DIR}/mkfs/mke2fs"
        echo "[install] bundled static mke2fs"
    elif [ -x "${JOB_DIR}/mkfs/mke2fs" ]; then
        # 源（research1 的 test_dirs）不可用，但 job 目录里已有上次安装的副本——
        # guest 侧的 per-round reformat 仍然生效，**不影响本轮数据有效性**。
        echo "[install] static mke2fs source missing; keeping existing copy at ${JOB_DIR}/mkfs/mke2fs"
    else
        echo "[install] WARNING: no static mke2fs (source missing AND no existing copy) — per-round reformat disabled" >&2
    fi

    if ! grep -qF "${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}" 2>/dev/null; then
        sed -i "\$i\ \ - ${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}"
    fi
    echo "[install] done."
}

# Prepare the auxiliary disk images. The fio target is the REAL device
# (/dev/sda3, passed through QEMU as /dev/vdc via XFSTESTS_TEST_IMAGE), so we
# do NOT create a test image — the guest reformats the real device each round.
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
    echo "[build] compiling Asterinas kernel (make kernel, RELEASE=${RELEASE})..."
    cd "${REPO_CTR}"
    make kernel "BENCHMARK=fio/${BENCHMARK_JOB_NAME}" "RELEASE=${RELEASE}" >> "${RESULTS_DIR}/build_kernel.log" 2>&1
    echo "[build] done."
}

# Run one VM boot per (rw, bs, numjobs, journal_mode) round (方案 B).
run_aster() {
    echo ""
    echo "================================================================"
    echo "  ASTERINAS: ext4 journal vs nojournal (${TEST_DEVICE}, smp=${SMP})"
    echo "================================================================"
    : > "${ASTER_LOG}"
    for sc in "${SCENARIOS[@]}"; do
        set -- ${sc}
        local nj="$1" nf="$2"
        echo "[aster] === round: ${RW} ${BS} nj${nj} nf${nf} ${JOURNAL_MODE} ${WRITETYPE} pa${PREALLOC} ==="
        prep_disk
        cd "${REPO_CTR}"
        # 真实盘直通：XFSTESTS_TEST_IMAGE 指向 /dev/sda4，guest 内为 /dev/vdc。
        # NETDEV=none：fio 本地 IO 不需要网络，去掉 virtio-net 与
        # hostfwd 随机端口（连续 boot 时随机端口撞 TIME_WAIT/并发，
        # 会让 QEMU 启动失败——本轮丢数据就是它）。
        XFSTESTS_TEST_IMAGE="${TEST_DEVICE}" \
        XFSTESTS_TEST_CACHE="none" \
        XFSTESTS_TEST_AIO="threads" \
        ATTACH_XFSTESTS_IMAGES=true \
        NETDEV=none \
        timeout 180 make run_kernel \
            "BENCHMARK=fio/${BENCHMARK_JOB_NAME}" \
            "BENCHMARK_ARGS=${RW} ${BS} ${nj} ${JOURNAL_MODE} ${WRITETYPE} ${RUNTIME_LIMIT} ${PREALLOC} ${nf}" \
            "RELEASE=${RELEASE}" \
            "SMP=${SMP}" \
            "MEM=${MEM}" >> "${ASTER_LOG}" 2>&1 || true
        cp "${REPO_CTR}/qemu-serial.log" "${RESULTS_DIR}/qemu-serial-${RW}-${BS}-nj${nj}-nf${nf}-${JOURNAL_MODE}-${WRITETYPE}-pa${PREALLOC}.log" 2>/dev/null || true
        # stdio chardev 的原始 log（fio 输出所在），每轮归档一份。
        cp "${REPO_CTR}/qemu.log" "${RESULTS_DIR}/qemu-${RW}-${BS}-nj${nj}-nf${nf}-${JOURNAL_MODE}-${WRITETYPE}-pa${PREALLOC}.log" 2>/dev/null || true
        echo "[aster] --- round done: ${RW} ${BS} nj${nj} nf${nf} ${JOURNAL_MODE} ${WRITETYPE} pa${PREALLOC} ---"
    done
    echo "[aster] log saved → ${ASTER_LOG}"
    # 解析 fio JSON → summary.csv（含裸设备带宽比例）
    if python3 "${PARSER_SCRIPT}" "${ASTER_LOG}" "${RAW_BASELINE_DIR_CTR}" "${RAW_BACKEND}" "${SUMMARY_CSV}"; then
        echo "[aster] summary → ${SUMMARY_CSV}"
    else
        echo "[aster] WARNING: summary parsing failed" >&2
    fi
}

# ── main ────────────────────────────────────────────────────────
echo ""
echo "================================================================"
echo "  EXT4 journal vs nojournal compare (${TEST_DEVICE}, smp=${SMP})"
echo "================================================================"
echo "Mode:      $(${PRETEST} && echo PRE-TEST || echo ARCHIVE)"
echo "Timestamp: ${TIMESTAMP}"
echo ""

mkdir -p "${RESULTS_DIR}"

install_job
build_kernel
run_aster

# re-own results to the host user
if [[ -n "${HOST_UID}" && -n "${HOST_GID}" ]]; then
    chown -R "${HOST_UID}:${HOST_GID}" "${RESULTS_DIR}" 2>/dev/null || true
fi

echo ""
echo "================================================================"
echo "  Done"
echo "================================================================"
echo "Results dir:   ${RESULTS_DIR}"
echo "Summary csv:   ${SUMMARY_CSV}"
echo "Host dir:      ${TEST_DIR_HOST}/results/${TIMESTAMP}_ext4_journal_compare/"
echo "================================================================"
