#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  宏测试 harness：filebench（五档 writeback profile × sata/nvme）
# =========================================================================
#
# 口径与 os_com_implementation/test/aster-ext4-baseline-test/run-baseline.sh 对齐：
#   device : sata (/dev/sda4) | nvme (/dev/nvme3n1p2)   ← 真盘，每轮重新 mke2fs
#   fs     : 与 fio 基线一致的五档 writeback profile
#   workload: write-heavy | fileserver | varmail | seqwrite（filebench .f，guest 内生成）
#   threads: filebench $nthreads（= fio 的 njobs 维度的对应物）
#   runtime: 每轮 filebench `run` 秒数
#
#  测试点 = device × fs × workload × threads
#    默认 2 × 5 × 3 × 2 = 60 点（write-heavy/fileserver/varmail × threads 4,8）
#
# 【与 fio 基线的差别】fio 量的是带宽（MiB/s），filebench 量的是
#   **ops/s + MB/s + 平均延迟**——宏测试关心的是真实负载形态下的
#   "每秒完成多少文件操作"。两者都要有：fio 是微观、filebench 是宏观。
#
# 【前置 — 必须先做，否则 round 会在 guest 里 127 失败】
#   1) filebench 二进制要进 initramfs：在 os_com 侧
#        test/initramfs/nix/benchmark/default.nix 里
#          filebench = pkgsHostTarget.filebench;
#        并在 package 的 buildCommand 里加
#          cp -r ${filebench}/bin/filebench $out/bin/
#      （可选：把 -A benchmark.filebench 加进 test/initramfs/Makefile 的
#        x86_64_pkgs 预构建列表，省一次现场构建。）
#   2) 本目录的 guest 脚本会被自动装到
#        <repo>/test/initramfs/src/benchmark/filebench/aster_ext4_macro/run.sh
#      并自动登记进 fio/summary.yaml（与 run-baseline.sh 同机制，无需手工改）
#   3) 静态 mke2fs 从 fio job 借用（同一份二进制，保证与 fio 基线同口径）
#
# 【用法】
#   bash run-filebench.sh --dry-run                    # 只看矩阵与点数
#   bash run-filebench.sh                               # 跑（默认 60 点）
#   bash run-filebench.sh --fs ext4,rnvpc_extend --workloads write-heavy --threads 4
#   bash run-filebench.sh --threads 4,8,16 --runtime 60 --nfiles 3000
#   bash run-filebench.sh --devices nvme --workloads varmail --threads 8
#
# 【必须串行】真实盘 + 单 QEMU，一次只能跑一个 boot。
# =========================================================================

set -e

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

REPO_HOST="${OS_COM_ROOT}"
REPO_CTR="/root/asterinas"
IMPL_DIR_HOST="${OS_COM_IMPL_ROOT}"
IMPL_DIR_CTR="/root/impl"
TEST_DIR_CTR="${IMPL_DIR_CTR}/test/filebench-test"

SMP=64            # 必须 > max(threads)
# 16G：与 sqlite harness 保持一致（4G 下大负载会出现内核堆页分配失败）
MEM="16G"
RELEASE=1
TIMEOUT_SEC=180   # 用户约束：每个测试点最多 3 分钟
RUNTIME_LIMIT=60  # filebench `run` 秒数（1 分钟）
WRCAP_MB=256      # dirty 预算；rnvpc* 下必须与 RNVPC_KCMD 的 rnvpc_size 对齐
RNVPC_KCMD="rnvpc_size=256M rnvpc_meta_reserve=64M rnvpc_data_limit=192M rnvpc_sim_mode=fast"
NFILES=0          # 0 = 用 guest 里该 workload 的默认值
FILESIZE="-"      # "-" = 用 guest 里该 workload 的默认值
NJMB="on"
NJWB="on"
BARRIER="merge"

declare -A DEVICE_PATHS=([sata]="/dev/sda4" [nvme]="/dev/nvme3n1p2")
DEVICES="sata,nvme"
# 与 fio writeback 矩阵一致的五档；真实 rnvpc/rnvpc_extend 仍可显式指定。
FS_LIST_STR="ext2like_sync,ext4_sync,ext2like_nomerge,ext4,ext4_nomerge"
WORKLOADS="write-heavy,fileserver,varmail"
THREADS="4,8"

DRY_RUN=false
CONFIRM_DESTROY=false
TIMESTAMP=""

while [ $# -gt 0 ]; do
    case "$1" in
        --devices)   DEVICES="$2"; shift 2 ;;
        --fs)        FS_LIST_STR="$2"; shift 2 ;;
        --workloads) WORKLOADS="$2"; shift 2 ;;
        --threads)   THREADS="$2"; shift 2 ;;
        --runtime)   RUNTIME_LIMIT="$2"; shift 2 ;;
        --nfiles)    NFILES="$2"; shift 2 ;;
        --filesize)  FILESIZE="$2"; shift 2 ;;
        --timeout)   TIMEOUT_SEC="$2"; shift 2 ;;
        --mem)       MEM="$2"; shift 2 ;;
        --smp)       SMP="$2"; shift 2 ;;
        --njmb)      NJMB="$2"; shift 2 ;;
        --njwb)      NJWB="$2"; shift 2 ;;
        --wr-capacity-mb) WRCAP_MB="$2"; shift 2 ;;
        --rnvpc-kcmd) RNVPC_KCMD="$2"; shift 2 ;;
        --timestamp) TIMESTAMP="$2"; shift 2 ;;
        --dry-run|-n) DRY_RUN=true; shift ;;
        # 最小 smoke：单盘 × 五档 fs × write-heavy × 4 线程 × 10s（1 轮/点）。
        --smoke)     DEVICES="sata"; FS_LIST_STR="ext2like_sync,ext4_sync,ext2like_nomerge,ext4,ext4_nomerge"; WORKLOADS="write-heavy"; THREADS="4"; RUNTIME_LIMIT=10; shift ;;
        --confirm-destroy|-y) CONFIRM_DESTROY=true; shift ;;
        -h|--help) sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
done

IFS=',' read -r -a DEVICE_LIST <<< "$DEVICES"
IFS=',' read -r -a FS_LIST <<< "$FS_LIST_STR"
IFS=',' read -r -a WORKLOAD_LIST <<< "$WORKLOADS"
IFS=',' read -r -a THREAD_LIST <<< "$THREADS"
for fs in "${FS_LIST[@]}"; do
    case "$fs" in
        ext4|ext2like|ext4_sync|ext2like_sync|ext4_nomerge|ext2like_nomerge|rnvpc|rnvpc_extend) ;;
        *) echo "Unknown fs profile: $fs (expect ext4|ext2like|ext4_sync|ext2like_sync|ext4_nomerge|ext2like_nomerge|rnvpc|rnvpc_extend)" >&2; exit 2 ;;
    esac
done

NJWB_GUEST="$NJWB"
[ "$BARRIER" = "each" ] && NJWB_GUEST="${NJWB}-each"

max_th=0
for t in "${THREAD_LIST[@]}"; do [ "$t" -gt "$max_th" ] && max_th="$t"; done
if [ "$SMP" -le "$max_th" ]; then
    echo "error: SMP (${SMP}) 必须大于 max(threads) (${max_th})；用 --smp 提高" >&2
    exit 2
fi

N_POINTS=$(( ${#DEVICE_LIST[@]} * ${#FS_LIST[@]} * ${#WORKLOAD_LIST[@]} * ${#THREAD_LIST[@]} ))
[ -z "$TIMESTAMP" ] && TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULTS_DIR="${TEST_DIR_CTR}/results/${TIMESTAMP}_filebench-macro"
LOGS_DIR="${RESULTS_DIR}/logs"
ASTER_LOG="${LOGS_DIR}/baseline_run.log"
BENCHMARK_JOB_NAME="aster_ext4_macro"
JOB_DIR="${REPO_CTR}/test/initramfs/src/benchmark/filebench/${BENCHMARK_JOB_NAME}"
SUMMARY_YAML="${REPO_CTR}/test/initramfs/src/benchmark/fio/summary.yaml"
GUEST_SCRIPT="${TEST_DIR_CTR}/guest-run-filebench.sh"
PARSE_SCRIPT="${TEST_DIR_CTR}/parse-filebench.py"
SUMMARY_CSV="${RESULTS_DIR}/summary.csv"

if [[ "$DRY_RUN" == true ]]; then
    echo "=== DRY-RUN: filebench 宏测试 ==="
    echo "smp=${SMP} mem=${MEM} runtime=${RUNTIME_LIMIT}s timeout=${TIMEOUT_SEC}s"
    echo "fs=${FS_LIST_STR} workloads=${WORKLOADS} threads=${THREADS}"
    echo "nfiles=${NFILES} filesize=${FILESIZE} wr-capacity=${WRCAP_MB}MiB"
    echo "rnvpc kcmd: ${RNVPC_KCMD}"
    idx=0
    for dev in "${DEVICE_LIST[@]}"; do
    for fs in "${FS_LIST[@]}"; do
    for w in "${WORKLOAD_LIST[@]}"; do
    for t in "${THREAD_LIST[@]}"; do
        idx=$((idx + 1))
        echo "  #${idx}: device=${dev}(${DEVICE_PATHS[$dev]}) fs=${fs} workload=${w} threads=${t}"
    done; done; done; done
    echo "Total: ${idx} rounds"
    exit 0
fi

if [[ "$CONFIRM_DESTROY" == false ]]; then
    echo "error: --confirm-destroy is required (formats test devices)" >&2
    exit 2
fi

# ── docker self-entry ────────────────────────────────────────────
if [[ ! -f /.dockerenv ]]; then
    echo "=== filebench 宏测试 (host → Docker) ==="
    echo "Repo:  ${REPO_HOST} (os_com)   Image: asterinas/asterinas:0.18.0-20260618"
    for dev in "${DEVICE_LIST[@]}"; do
        p="${DEVICE_PATHS[$dev]}"
        if findmnt -rn "$p" &>/dev/null; then
            echo "ERROR: $p is mounted — refusing to proceed" >&2; exit 1
        fi
        if command -v fuser >/dev/null 2>&1 && fuser -v "$p" &>/dev/null; then
            echo "ERROR: $p is in use by another process" >&2; exit 1
        fi
    done
    mkdir -p "${IMPL_DIR_HOST}/test/filebench-test"
    docker volume create asterinas-cargo  >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true
    nix_config_env=()
    [ -n "${NIX_CONFIG:-}" ] && nix_config_env=(-e "NIX_CONFIG=${NIX_CONFIG}")
    # 必须把宿主代理透传进容器（理由同 run-sqlite.sh）：容器直连 cache.nixos.org 会 000。
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
        bash "${TEST_DIR_CTR}/run-filebench.sh" \
            --devices "$DEVICES" --fs "$FS_LIST_STR" --workloads "$WORKLOADS" \
            --threads "$THREADS" --runtime "$RUNTIME_LIMIT" --nfiles "$NFILES" \
            --filesize "$FILESIZE" --timeout "$TIMEOUT_SEC" --smp "$SMP" \
            --njmb "$NJMB" --njwb "$NJWB" --wr-capacity-mb "$WRCAP_MB" \
            --rnvpc-kcmd "$RNVPC_KCMD" --timestamp "$TIMESTAMP" \
            --confirm-destroy < /dev/null
fi

# ── below runs inside Docker ─────────────────────────────────────
install_job() {
    echo "[install] installing ${BENCHMARK_JOB_NAME} job..."
    if ! bash -n "${GUEST_SCRIPT}"; then
        echo "error: guest script has syntax errors: ${GUEST_SCRIPT}" >&2; exit 2
    fi
    mkdir -p "${JOB_DIR}"
    cp "${GUEST_SCRIPT}" "${JOB_DIR}/run.sh"
    chmod +x "${JOB_DIR}/run.sh"

    # 静态 mke2fs 从 fio job 借用（同一份二进制 = 与 fio 基线同口径）。
    local src_mke2fs="${REPO_CTR}/test/initramfs/src/benchmark/fio/ext4_journal_compare/mkfs/mke2fs"
    if [ -f "$src_mke2fs" ]; then
        mkdir -p "${JOB_DIR}/mkfs"
        cp "$src_mke2fs" "${JOB_DIR}/mkfs/mke2fs"
        chmod +x "${JOB_DIR}/mkfs/mke2fs"
        echo "[install] bundled static mke2fs"
    else
        echo "[install] WARNING: static mke2fs missing — 每轮重格式化会退化为 rm-clean" >&2
    fi

    # filebench 二进制随 job 打包（与 static mke2fs 同一机制，不需改 initramfs 源码）。
    # 用 research1 自建的 1.5-alpha3：nixpkgs 的 1.4.9.1 在本环境 worker 跑空（0 ops）。
    local src_fb="${IMPL_DIR_CTR}/test/filebench-test/bin/filebench"
    if [ -f "$src_fb" ]; then
        mkdir -p "${JOB_DIR}/bin"
        cp "$src_fb" "${JOB_DIR}/bin/filebench"
        chmod +x "${JOB_DIR}/bin/filebench"
        echo "[install] bundled filebench 1.5-alpha3 ($(stat -c%s "$src_fb") bytes)"
    else
        echo "[install] WARNING: 缺少 ${src_fb}；guest 会回退 /benchmark/bin/filebench（nixpkgs 1.4.9.1，大概率 0 ops）" >&2
    fi

    if ! grep -qF "${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}" 2>/dev/null; then
        sed -i "\$i\ \ - ${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}"
    fi
    echo "[install] done."
}

prep_disk() {
    local build_dir="${REPO_CTR}/test/initramfs/build"
    mkdir -p "$build_dir"
    # benchmark 默认 boot 需要 ext2/exfat 镜像；xfstests scratch 槽被我们借来做
    # 真实盘挂载（ATTACH_XFSTESTS_IMAGES=true），没有它框架会报缺图。
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
    make kernel "BENCHMARK=filebench/${BENCHMARK_JOB_NAME}" "RELEASE=${RELEASE}" \
        "RNVPC_KCMD=${RNVPC_KCMD}" >> "${LOGS_DIR}/build_kernel.log" 2>&1
    echo "[build] done."
}

run_matrix() {
    echo ""
    echo "================================================================"
    echo "  filebench 宏测试 (smp=${SMP})  points=${N_POINTS}"
    echo "================================================================"
    : > "${ASTER_LOG}"
    for dev in "${DEVICE_LIST[@]}"; do
        local dev_path="${DEVICE_PATHS[$dev]}"
        for fs in "${FS_LIST[@]}"; do
        for w in "${WORKLOAD_LIST[@]}"; do
        for t in "${THREAD_LIST[@]}"; do
            local tag="${dev}-${fs}-${w}-t${t}"
            echo "[filebench] === round: ${tag} ==="
            prep_disk
            cd "${REPO_CTR}"
            XFSTESTS_TEST_IMAGE="${dev_path}" \
            XFSTESTS_TEST_CACHE="none" \
            XFSTESTS_TEST_AIO="threads" \
            ATTACH_XFSTESTS_IMAGES=true \
            NETDEV=none \
            timeout "${TIMEOUT_SEC}" make run_kernel \
                "BENCHMARK=filebench/${BENCHMARK_JOB_NAME}" \
                "BENCHMARK_ARGS=${dev} ${fs} ${w} ${t} ${RUNTIME_LIMIT} ${NFILES} ${FILESIZE} ${NJMB} ${NJWB_GUEST} ${WRCAP_MB}" \
                "RELEASE=${RELEASE}" \
                "SMP=${SMP}" \
                "MEM=${MEM}" \
                "RNVPC_KCMD=${RNVPC_KCMD}" >> "${ASTER_LOG}" 2>&1 || true
            cp "${REPO_CTR}/qemu-serial.log" "${LOGS_DIR}/qemu-serial-${tag}.log" 2>/dev/null || true
            echo "[filebench] --- round done: ${tag} ---"
        done; done; done
    done
    echo "[filebench] log saved → ${ASTER_LOG}"
}

parse_summary() {
    if [ ! -f "${PARSE_SCRIPT}" ]; then
        echo "ERROR: ${PARSE_SCRIPT} not found — cannot parse summary" >&2; return 1
    fi
    python3 "${PARSE_SCRIPT}" "${ASTER_LOG}" "${SUMMARY_CSV}"
}

write_run_config() {
    cat > "${RESULTS_DIR}/run-config.txt" <<EOF
test=filebench
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
workload.write-heavy=nfiles:2048,filesize:64k,iosize:16k,appendsize:16k,prealloc:80pct
workload.fileserver=nfiles:3000,filesize:128k,iosize:1m,appendsize:16k,prealloc:80pct
workload.varmail=nfiles:1000,filesize:16k,iosize:16k,prealloc:100pct,fsync:per_operation
workloads=${WORKLOADS}
threads=${THREADS}
runtime_s=${RUNTIME_LIMIT}
nfiles=${NFILES}
filesize=${FILESIZE}
writeback_capacity_mb=${WRCAP_MB}
nojournal_meta_batch=${NJMB}
nojournal_meta_writeback=${NJWB}
smp=${SMP}
memory=${MEM}
timeout_s=${TIMEOUT_SEC}
EOF
}

# ── main ──────────────────────────────────────────────────────────
mkdir -p "${RESULTS_DIR}" "${LOGS_DIR}"
write_run_config
umask 002
chmod -R g+rwX "${RESULTS_DIR}" 2>/dev/null || true
install_job
build_kernel
run_matrix
if parse_summary; then
    echo "[filebench] summary → ${SUMMARY_CSV}"
else
    echo "[filebench] WARNING: summary parsing failed" >&2
fi
if [ -n "${HOST_UID:-}" ]; then
    chown -R "${HOST_UID}:${HOST_GID:-${HOST_UID}}" "${RESULTS_DIR}" 2>/dev/null || true
fi

echo ""
echo "================================================================"
echo " 完成。结果目录（宿主）："
echo "   ${IMPL_DIR_HOST}/test/filebench-test/results/${TIMESTAMP}_filebench-macro/"
echo "     summary.csv              每点 ops/s、MB/s、延迟、status"
echo "     logs/baseline_run.log    全部轮次（filebench 原始输出也在里面）"
echo "================================================================"
