#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  Asterinas EXT4 多设备 / 多线程性能基线（os_com 版）— 一键运行器
# =========================================================================
#
# 测试口径对齐 research1/motivation/test_dir/linux-ext4-baseline-test
# （sync/orderless 矩阵）：
#   device   : sata (/dev/sda4) | nvme (/dev/nvme3n1p2)
#   fs       : ext4（ext4 + journal + extent）| ext2like（ext4 去 journal，模拟 ext2）
#              ⚠ ext2like 有两处已知偏差（extent 关不掉、nojournal 元数据为同步写），
#                详见 aster_ext4_baseline.sh 的 fs profile 说明与 008 任务文档
#   workload : randwrite-4k | seqwrite-4k
#   mode     : orderless（buffered，fsync=0） | fsync（每次 4k 写后 fsync）
#   njobs    : 1,4,8,12,16,20,24,28,32（线程数）
#
# 指标：drain_bw = fio write bytes /（fio 开始 → fio 返回 + 最终 sync 完成）。
# 报错/中途失败会跳过该点并继续。
#
# 环境隔离：编译并运行 **os_com**（/home/zn/Ant/Rust_EXT4/os_com → /root/asterinas），
# 镜像 asterinas/asterinas:0.18.0-20260618 + 独立 cargo/rustup 卷；结果写到
# os_com_implementation/test/aster-ext4-baseline-test/results/，不与 research1 混淆。
#
# 【用法】
#   bash run-baseline.sh --dry-run                       # 看矩阵
#   bash run-baseline.sh --confirm-destroy               # 跑完整矩阵
#   bash run-baseline.sh --confirm-destroy --devices sata --njobs 1,4 --workloads randwrite-4k --modes orderless
#   bash run-baseline.sh --confirm-destroy --timeout 300 # 单轮墙钟上限（默认 300s）
#   bash run-baseline.sh --confirm-destroy --fs ext4     # 只跑 ext4 profile（默认）
#   bash run-baseline.sh --confirm-destroy --fs ext4,ext2like  # 对齐 Linux 的 ext2/ext4 两组
#   bash run-baseline.sh --confirm-destroy --fs ext2like --njmb off     # A/B：不缓冲（0）
#   bash run-baseline.sh --confirm-destroy --fs ext2like --njmb global  # A/B：旧的全量落盘（1）
#                       默认 on = owner（2）：按执行流分桶，只在自己窗口归零时落盘
#
# 【结果目录】
#   results/<时间戳>_aster-ext4-baseline/          # 目录在**宿主侧预创建**，归属宿主用户
#     summary.csv                                  # 解析结果
#     logs/baseline_run.log                        # 全部轮次拼接日志（解析输入）
#     logs/build_kernel.log
#     logs/qemu-serial-<device>-<fs>-<workload>-<mode>-nj<N>.log
#     logs/qemu-<device>-<fs>-<workload>-<mode>-nj<N>.log
#
# 【必须串行】真实盘性能实验一次只能跑一个 boot。
# =========================================================================

set -e

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

REPO_HOST="${OS_COM_ROOT}"
REPO_CTR="/root/asterinas"
IMPL_DIR_HOST="${OS_COM_IMPL_ROOT}"
IMPL_DIR_CTR="/root/impl"
TEST_DIR_CTR="${IMPL_DIR_CTR}/test/aster-ext4-baseline-test"

# VM vCPU 数。**必须 >= 最大 njobs（32）**，否则 fio 线程超配 vCPU，把「线程扩展性」
# 测成「调度器排队」。取 64 给 32 线程留 2x 余量（宿主 128 核）。
# 经 tools/qemu_args.sh 的 `-smp` 传入，配套 `-cpu Icelake-Server,+x2apic` 支持高核数。
SMP=64
MEM="4G"
RELEASE=1
RUNTIME_LIMIT=30
# 单轮 `make run_kernel` 的墙钟上限。正常 orderless 轮 13-30s、fsync 轮 60-90s；
# 定太小会把「慢」误判成「挂」（曾被 180s 杀掉 10 轮，其中 1 轮 orderless）。
TIMEOUT_SEC=300
# 总地址范围（MB）。linux-ext4-baseline 用 8 GiB，但 Asterinas 的 PageCache 不主动
# 回收干净页，总范围超过 VM 内存会 OOM（fio "Cannot allocate memory" + 段错误）。
# 这里用 2 GiB（< 4 GiB MEM），保留「跨 job 均分、fallocate=none」的口径。
FILE_RANGE_MB=2048
# writeback_capacity_mb（dirty 预算 MB）。rNVPC/rNVPC_extend 下必须与 `rnvpc_size`
# 对齐；延迟分解实验的 1/4 缩放档用 64（配合 rnvpc_size=64M）。
WRCAP_MB=256

declare -A DEVICE_PATHS=([sata]="/dev/sda4" [nvme]="/dev/nvme3n1p2")
DEVICES_DEFAULT="sata,nvme"
WORKLOADS_DEFAULT="randwrite-4k,seqwrite-4k"
MODES_DEFAULT="orderless,fsync"
NJOBS_DEFAULT="1,4,8,12,16,20,24,28,32"
# fs profile：ext4 = ext4+journal+extent；ext2like = ext4 去 journal（模拟 ext2）。
# 两侧都跑 = 2 device × 2 fs × 2 workload × 2 mode × 9 njobs = 144 点，
# 与 Linux 基准 parameters.json 的 point_count=144 对齐。
FS_DEFAULT="ext4"

DEVICES="$DEVICES_DEFAULT"
WORKLOADS="$WORKLOADS_DEFAULT"
MODES="$MODES_DEFAULT"
NJOBS="$NJOBS_DEFAULT"
FS_LIST_STR="$FS_DEFAULT"
# nojournal 方案 A（008）A/B 开关：on|owner（默认，= 2，当前行为）| global（= 1，旧实现）
# | off（= 0，不缓冲）。只影响 ext2like。经挂载选项 `nojournal_meta_batch=` 生效，
# 不需要重新构建内核（同一内核内 A/B）。
NJMB="on"
# nojournal 元数据 write-behind（008 核心缺口）A/B 开关：on（默认，共享脏块缓冲）| off。
NJWB="on"
# fsync 设备 flush 屏障（009）A/B 开关：merge（默认，可合并屏障）| each（每次各发一次）。
BARRIER="merge"
# rNVPC 池容量（由 `make` 的 `--kcmd-args` 注入内核 cmdline）。空 = 不注入（原生）。
RNVPC_KCMD=""
# --skip-build：跳过 `make kernel`，直接用上次的构建产物。
# 【什么时候能用】代码（内核源码 + guest 脚本 + nix）冻结、只是重跑测量时。
# 【防呆】本脚本把"内核树状态 + 已安装 guest 脚本的 sha256"写成印章，--skip-build 时
# 必须与当前一致，否则**拒绝并让你去掉这个开关**——否则会拿旧内核跑出一批"幽灵数据"
# （曾经在 ktest 里吃过同类亏：以为测了新代码，其实跑的是旧镜像）。
SKIP_BUILD=false
DRY_RUN=false
CONFIRM_DESTROY=false
TIMESTAMP=""

while [ $# -gt 0 ]; do
    case "$1" in
        --devices) DEVICES="$2"; shift 2 ;;
        --workloads) WORKLOADS="$2"; shift 2 ;;
        --modes) MODES="$2"; shift 2 ;;
        --njobs) NJOBS="$2"; shift 2 ;;
        --fs) FS_LIST_STR="$2"; shift 2 ;;
        --runtime) RUNTIME_LIMIT="$2"; shift 2 ;;
        --file-range-mb) FILE_RANGE_MB="$2"; shift 2 ;;
        --smp) SMP="$2"; shift 2 ;;
        --timeout) TIMEOUT_SEC="$2"; shift 2 ;;
        --njmb) NJMB="$2"; shift 2 ;;
        --njwb) NJWB="$2"; shift 2 ;;
        --barrier) BARRIER="$2"; shift 2 ;;
        --rnvpc-kcmd) RNVPC_KCMD="$2"; shift 2 ;;
        --wr-capacity-mb) WRCAP_MB="$2"; shift 2 ;;
        --timestamp) TIMESTAMP="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --dry-run|-n) DRY_RUN=true; shift ;;
        --confirm-destroy|-y) CONFIRM_DESTROY=true; shift ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
done

IFS=',' read -r -a DEVICE_LIST <<< "$DEVICES"
IFS=',' read -r -a WORKLOAD_LIST <<< "$WORKLOADS"
IFS=',' read -r -a MODE_LIST <<< "$MODES"
IFS=',' read -r -a NJOB_LIST <<< "$NJOBS"
IFS=',' read -r -a FS_LIST <<< "$FS_LIST_STR"
for fs in "${FS_LIST[@]}"; do
    case "$fs" in
        ext4|ext2like|ext4_sync|ext2like_sync|ext4_nomerge|ext2like_nomerge|rnvpc|rnvpc_extend) ;;
        *) echo "Unknown fs profile: $fs (expect ext4|ext2like|ext4_sync|ext2like_sync|ext4_nomerge|ext2like_nomerge|rnvpc|rnvpc_extend)" >&2; exit 2 ;;
    esac
done

case "$NJMB" in
    on|owner|global|off) ;;
    *) echo "Unknown njmb: $NJMB (expect on|owner|global|off)" >&2; exit 2 ;;
esac
case "$NJWB" in
    on|async|sync|off|asyncbio) ;;
    *) echo "Unknown njwb: $NJWB (expect on|async|sync|off|asyncbio)" >&2; exit 2 ;;
esac
case "$BARRIER" in
    merge|each) ;;
    *) echo "Unknown barrier: $BARRIER (expect merge|each)" >&2; exit 2 ;;
esac
# 结果目录带开关后缀（只带**非默认**的那几项），避免事后分不清哪批是 A/B 的哪一只。
# 臂标签 = 后缀本身（`ab_compare.py` 直接按目录名解析），所以两个轴互不干扰。
NJMB_SUFFIX=""
[ "$NJMB" = "on" ] || NJMB_SUFFIX="${NJMB_SUFFIX}_njmb-${NJMB}"
[ "$NJWB" = "on" ] || NJMB_SUFFIX="${NJMB_SUFFIX}_njwb-${NJWB}"
[ "$BARRIER" = "merge" ] || NJMB_SUFFIX="${NJMB_SUFFIX}_barrier-${BARRIER}"

# 009：设备 flush 屏障模式**搭在 njwb 字段里**带给 guest，语法 `<mode>[-each]`。
# 原因：benchmark 框架没把第 10 个参数送进 guest（实测 guest 打印的 `mount_opts` 里
# 始终看不到 `fsync_barrier=`），而 `$9`（njwb）证明可达。
NJWB_GUEST="${NJWB}"
[ "$BARRIER" = "each" ] && NJWB_GUEST="${NJWB}-each"

# 与 linux-ext4-baseline-test 的 validate_vm_cpu_capacity 同口径：VM vCPU 必须**多于**
# 最大 fio 线程数，否则线程超配 vCPU，会把「线程扩展性」测成「调度器排队」。
max_nj=0
for nj in "${NJOB_LIST[@]}"; do
    [ "$nj" -gt "$max_nj" ] && max_nj="$nj"
done
if [ "$SMP" -le "$max_nj" ]; then
    echo "error: SMP (${SMP}) must exceed max njobs (${max_nj}); use --smp to raise it" >&2
    exit 2
fi

# TIMESTAMP 由宿主侧生成并经 --timestamp 传给容器，保证宿主预创建的目录名与容器内一致。
[ -z "$TIMESTAMP" ] && TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULTS_DIR="${TEST_DIR_CTR}/results/${TIMESTAMP}_aster-ext4-baseline${NJMB_SUFFIX}"
LOGS_DIR="${RESULTS_DIR}/logs"
ASTER_LOG="${LOGS_DIR}/baseline_run.log"

BENCHMARK_JOB_NAME="aster_ext4_baseline"
JOB_DIR="${REPO_CTR}/test/initramfs/src/benchmark/fio/${BENCHMARK_JOB_NAME}"
SUMMARY_YAML="${REPO_CTR}/test/initramfs/src/benchmark/fio/summary.yaml"
GUEST_SCRIPT="${TEST_DIR_CTR}/aster_ext4_baseline.sh"
PARSE_SCRIPT="${TEST_DIR_CTR}/parse_baseline.py"
SUMMARY_CSV="${RESULTS_DIR}/summary.csv"

if [[ "$DRY_RUN" == true ]]; then
    echo "=== DRY-RUN: aster-ext4-baseline (os_com, multi-device / multi-thread) ==="
    echo "smp=${SMP} mem=${MEM} runtime=${RUNTIME_LIMIT}s timeout=${TIMEOUT_SEC}s file_range=${FILE_RANGE_MB}MB fs=${FS_LIST_STR}"
    echo "nojournal_meta_batch=${NJMB} writeback=${NJWB} (results dir suffix: '${NJMB_SUFFIX}')"
    idx=0
    for dev in "${DEVICE_LIST[@]}"; do
        for fs in "${FS_LIST[@]}"; do
            for w in "${WORKLOAD_LIST[@]}"; do
                for m in "${MODE_LIST[@]}"; do
                    for nj in "${NJOB_LIST[@]}"; do
                        idx=$((idx + 1))
                        echo "  #${idx}: device=${dev}(${DEVICE_PATHS[$dev]}) fs=${fs} ${w} ${m} nj${nj}"
                    done
                done
            done
        done
    done
    echo "Total: ${idx} rounds"
    exit 0
fi

if [[ "$CONFIRM_DESTROY" == false ]]; then
    echo "error: --confirm-destroy is required (formats test devices)" >&2
    exit 2
fi

# ── docker self-entry ────────────────────────────────────────────
if [[ ! -f /.dockerenv ]]; then
    echo "=== Asterinas EXT4 multi-device / multi-thread baseline (host → Docker) ==="
    echo "Repo:  ${REPO_HOST} (os_com)"
    echo "Image: asterinas/asterinas:0.18.0-20260618"

    for dev in "${DEVICE_LIST[@]}"; do
        p="${DEVICE_PATHS[$dev]}"
        if findmnt -rn "$p" &>/dev/null; then
            echo "ERROR: $p is mounted — refusing to proceed" >&2; exit 1
        fi
        if command -v fuser >/dev/null 2>&1 && fuser -v "$p" &>/dev/null; then
            echo "ERROR: $p is in use by another process" >&2; exit 1
        fi
    done

    mkdir -p "${IMPL_DIR_HOST}/test/aster-ext4-baseline-test"
    # 结果目录在宿主侧预创建（含 logs/ 子目录），让产物一开始就归宿主用户。
    # 但 `results/` 若由 root 建过（权限 root:zns 且无组写位），宿主用户建不了子目录
    # ——这时**不能致命退出**：容器内是 root，可以自己建，末尾的 chown 会把属主还给
    # `HOST_UID:HOST_GID`。所以这里只做 best-effort 并提示。
    HOST_RESULTS_DIR="${IMPL_DIR_HOST}/test/aster-ext4-baseline-test/results/${TIMESTAMP}_aster-ext4-baseline${NJMB_SUFFIX}"
    if ! mkdir -p "${HOST_RESULTS_DIR}/logs" 2>/dev/null; then
        echo "[warn] 无法在宿主预建结果目录（父目录可能属 root 且无组写权限）："
        echo "[warn]   ${HOST_RESULTS_DIR}"
        echo "[warn] 改由容器内 root 创建；跑完的 chown 会把属主还给宿主用户。"
        echo "[warn] 若中途失败，事后需: sudo chown -R \$(id -u):\$(id -g) <该目录>"
    fi
    docker volume create asterinas-cargo  >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true

    # 宿主设了 `NIX_CONFIG` 就透传进容器（否则不传，保持原行为）。
    # 用途：本环境 substituter `cache.nixos.org` 被 TLS 拦截且证书过期（见
    # `os_com_implementation/nix-initramfs-network-blocker.md`），会使 nix-build
    # 在查询 narinfo 时**硬失败**。所有依赖均已在 `/nix/store` 时，
    # `NIX_CONFIG="substitute = false"` 可让 initramfs 完全离线构建。
    # 顶层脚本代码，不能用 `local`。
    nix_config_env=()
    [ -n "${NIX_CONFIG:-}" ] && nix_config_env=(-e "NIX_CONFIG=${NIX_CONFIG}")
    skip_arg=()
    [ "$SKIP_BUILD" = true ] && skip_arg=(--skip-build)

    exec docker run -i --rm --privileged --network=host \
        --device=/dev/kvm \
        -v /dev:/dev \
        -v "${REPO_HOST}:${REPO_CTR}" \
        -v "${IMPL_DIR_HOST}:${IMPL_DIR_CTR}" \
        -v "${REPO_HOST}/.impl-host:${REPO_HOST}/.impl-host" \
        -v asterinas-cargo:/root/.cargo \
        -v asterinas-rustup:/root/.rustup \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        "${nix_config_env[@]}" \
        -w "${REPO_CTR}" \
        asterinas/asterinas:0.18.0-20260618 \
        bash "${TEST_DIR_CTR}/run-baseline.sh" \
            --devices "$DEVICES" --workloads "$WORKLOADS" \
            --modes "$MODES" --njobs "$NJOBS" --fs "$FS_LIST_STR" \
            --runtime "$RUNTIME_LIMIT" --file-range-mb "$FILE_RANGE_MB" \
            --smp "$SMP" --timeout "$TIMEOUT_SEC" --njmb "$NJMB" --njwb "$NJWB" --barrier "$BARRIER" \
            --rnvpc-kcmd "$RNVPC_KCMD" \
            --wr-capacity-mb "$WRCAP_MB" \
            --timestamp "$TIMESTAMP" \
            "${skip_arg[@]}" \
            --confirm-destroy < /dev/null
fi

# ── below runs inside Docker ─────────────────────────────────────
install_job() {
    echo "[install] installing ${BENCHMARK_JOB_NAME} job..."
    # guest 脚本语法预检：语法错误会让它在 VM 里原地失败、整轮跑出 0 个数据点，
    # 而外层只看到「跑完了」——必须在这里硬失败（曾在 A/B 里静默烧掉一整轮实验）。
    if ! bash -n "${GUEST_SCRIPT}"; then
        echo "error: guest script has syntax errors: ${GUEST_SCRIPT}" >&2
        exit 2
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
        echo "[install] WARNING: static mke2fs missing — per-round reformat disabled" >&2
    fi

    if ! grep -qF "${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}" 2>/dev/null; then
        sed -i "\$i\ \ - ${BENCHMARK_JOB_NAME}" "${SUMMARY_YAML}"
    fi
    # 供 --skip-build 校验：记录"本次装进去的 guest 脚本"的 sha256。
    GUEST_SHA="$(sha256sum "${JOB_DIR}/run.sh" | awk '{print $1}')"
    echo "[install] guest sha256=${GUEST_SHA:0:16}"
    echo "[install] done."
}

# 内核树状态指纹：HEAD + 工作区改动列表。源码/nix/未提交改动变了就必须重建。
kernel_state() {
    local head dirty
    head="$(git -C "${REPO_CTR}" rev-parse HEAD 2>/dev/null || echo no-git)"
    dirty="$(git -C "${REPO_CTR}" status --porcelain 2>/dev/null | sha256sum | awk '{print $1}')"
    echo "${head:0:12}-${dirty:0:16}"
}

prep_disk() {
    local build_dir="${REPO_CTR}/test/initramfs/build"
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
    # 印章放在**固定路径**（不是 results/<ts>/）：每次 invocation 都是新结果目录，
    # 印章放那里就永远“找不到上次构建” ⇒ --skip-build 永远失效。
    local stamp="${TEST_DIR_CTR}/.build-stamp"
    local want="${GUEST_SHA:0:16}|$(kernel_state)"
    if [ "$SKIP_BUILD" = true ]; then
        if [ ! -f "$stamp" ]; then
            echo "error: --skip-build 但找不到上次构建印章（${stamp}）——先不带 --skip-build 跑一次" >&2
            exit 2
        fi
        local have
        have="$(cat "$stamp")"
        if [ "$have" != "$want" ]; then
            echo "error: --skip-build 被拒绝 —— 内核树或 guest 脚本已变：" >&2
            echo "        上次构建: ${have}" >&2
            echo "        当前状态: ${want}" >&2
            echo "        （去掉 --skip-build 重跑，让它重新编译）" >&2
            exit 2
        fi
        echo "[build] skipped（--skip-build，印章 ${have} 匹配）"
        return
    fi
    echo "[build] compiling Asterinas kernel (make kernel, RELEASE=${RELEASE})..."
    cd "${REPO_CTR}"
    make kernel "BENCHMARK=fio/${BENCHMARK_JOB_NAME}" "RELEASE=${RELEASE}" \
        "RNVPC_KCMD=${RNVPC_KCMD}" \
        >> "${LOGS_DIR}/build_kernel.log" 2>&1
    printf '%s' "$want" > "$stamp"
    echo "[build] done（印章 ${want}）"
}

run_matrix() {
    echo ""
    echo "================================================================"
    echo "  ASTERINAS: ext4 multi-device baseline (smp=${SMP})"
    echo "  nojournal_meta_batch=${NJMB}  nojournal_meta_writeback=${NJWB}"
    echo "================================================================"
    : > "${ASTER_LOG}"
    for dev in "${DEVICE_LIST[@]}"; do
        local dev_path="${DEVICE_PATHS[$dev]}"
        for fs in "${FS_LIST[@]}"; do
        for w in "${WORKLOAD_LIST[@]}"; do
            for m in "${MODE_LIST[@]}"; do
                for nj in "${NJOB_LIST[@]}"; do
                    local tag="${dev}-${fs}-${w}-${m}-nj${nj}"
                    local completed_before completed_after run_status
                    echo "[aster] === round: ${tag} ==="
                    prep_disk
                    cd "${REPO_CTR}"
                    completed_before="$(grep -cF '=== END_FIO_RESULT ===' "${ASTER_LOG}" || true)"
                    if XFSTESTS_TEST_IMAGE="${dev_path}" \
                        XFSTESTS_TEST_CACHE="none" \
                        XFSTESTS_TEST_AIO="threads" \
                        ATTACH_XFSTESTS_IMAGES=true \
                        NETDEV=none \
                        timeout "${TIMEOUT_SEC}" make run_kernel \
                            "BENCHMARK=fio/${BENCHMARK_JOB_NAME}" \
                            "BENCHMARK_ARGS=${dev} ${fs} ${w} ${m} ${nj} ${RUNTIME_LIMIT} ${FILE_RANGE_MB} ${NJMB} ${NJWB_GUEST} ${WRCAP_MB}" \
                            "RELEASE=${RELEASE}" \
                            "SMP=${SMP}" \
                            "MEM=${MEM}" \
                            "RNVPC_KCMD=${RNVPC_KCMD}" >> "${ASTER_LOG}" 2>&1; then
                        run_status=0
                    else
                        run_status=$?
                    fi
                    cp "${REPO_CTR}/qemu-serial.log" "${LOGS_DIR}/qemu-serial-${tag}.log" 2>/dev/null || true
                    cp "${REPO_CTR}/qemu.log" "${LOGS_DIR}/qemu-${tag}.log" 2>/dev/null || true
                    if [ "$run_status" -ne 0 ]; then
                        echo "error: QEMU round failed: ${tag} (status=${run_status})" >&2
                        return "$run_status"
                    fi
                    completed_after="$(grep -cF '=== END_FIO_RESULT ===' "${ASTER_LOG}" || true)"
                    if [ "$completed_after" -le "$completed_before" ]; then
                        echo "error: guest did not emit END_FIO_RESULT: ${tag}" >&2
                        return 1
                    fi
                    echo "[aster] --- round done: ${tag} ---"
                done
            done
        done
        done
    done
    echo "[aster] log saved → ${ASTER_LOG}"
}

# 解析统一委托给 parse_baseline.py（单一实现）。
# 此前本脚本内嵌一份 python 解析副本，与独立脚本各自演进；且副本用
# END_FIO_RESULT 作段边界，遇缺标记轮次会跨轮错配（相邻轮 write_mib 完全相同）。
# 现在只保留一份实现，口径（含 10 MiB 有效性阈值、valid/note 列）天然一致。
parse_summary() {
    if [ ! -f "${PARSE_SCRIPT}" ]; then
        echo "ERROR: ${PARSE_SCRIPT} not found — cannot parse summary" >&2
        return 1
    fi
    python3 "${PARSE_SCRIPT}" "${ASTER_LOG}" "${SUMMARY_CSV}"
}

# ── main ──────────────────────────────────────────────────────────
mkdir -p "${RESULTS_DIR}" "${LOGS_DIR}"
# 本脚本在容器内以 root 运行，产物经 bind mount 落到宿主，宿主用户默认拿不到写权限。
# umask 002 + setgid 目录 → 新文件对同组可写；末尾再把属主还给 HOST_UID/HOST_GID。
umask 002
chmod -R g+rwX "${RESULTS_DIR}" 2>/dev/null || true
install_job
build_kernel
run_matrix
if parse_summary; then
    echo "[aster] summary → ${SUMMARY_CSV}"
else
    echo "[aster] WARNING: summary parsing failed" >&2
fi

# 把结果目录属主还给宿主用户（HOST_UID/HOST_GID 由 docker run -e 传入），
# 否则宿主用户无法覆盖 summary.csv 等产物。
if [ -n "${HOST_UID:-}" ]; then
    chown -R "${HOST_UID}:${HOST_GID:-${HOST_UID}}" "${RESULTS_DIR}" 2>/dev/null || true
fi

echo ""
echo "================================================================"
echo "  Done"
echo "================================================================"
echo "Results dir: ${RESULTS_DIR}"
echo "Host dir:    ${IMPL_DIR_HOST}/test/aster-ext4-baseline-test/results/$(basename "${RESULTS_DIR}")"
echo "================================================================"
