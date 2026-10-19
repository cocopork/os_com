#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  EXT4 性能矩阵（32 点）一键运行器 —— os_com 仓库，真实盘后端
# =========================================================================
#
# 矩阵：
#   mod       ∈ { default, nojournal }
#   backend   ∈ { sata, nvme }
#   njobs     ∈ { 1, 16 }
#   workload  ∈ { random-4k, sequential-128k }
#   writetype ∈ { buffer, fsync }
#   固定：writeback_capacity_mb=64（os_com 忽略未知 option）、size=256M
#   共 2×2×2×2×2 = 32 个测试点。
#
# 【真实盘后端映射】
#   sata → /dev/sda4
#   nvme → /dev/nvme3n1p2
#   两者都通过 XFSTESTS_TEST_IMAGE 直通为 guest 的 virtio-blk /dev/vdc
#   （cache=none, aio=threads）；backend 仅用于结果命名。
#
# 每个点独立 VM boot：guest 内静态 mke2fs 格式化后跑 fio，30s time_based。
# 结果串行独占宿主机，不得与其他 QEMU/性能实验并发。
#
# 用法（宿主机）：
#   bash /home/zn/Ant/Rust_EXT4/os_com_implementation/test/ext4-journal-compare/run_perf_matrix_img.sh \
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
RESULT_MD="${RESULT_DIR_CTR}/perfomance.md"

# 矩阵维度
MODS=(default nojournal)
NJOBS_LIST=(1 16)
WORKLOADS=(random-4k sequential-128k)
WRITETYPES=(buffer fsync)
# 真实盘后端：backend 标签 → 宿主机设备（直通为 guest /dev/vdc）
declare -A DEVICE=([sata]="/dev/sda4" [nvme]="/dev/nvme3n1p2")

# 可选的 --only 过滤（空格分隔的 "mod backend workload njobs writetype" 元组）
ONLY_FILTER=()

# 原始参数：docker self-entry 需要把参数原样传给容器内的脚本重新解析
# （解析循环里的 shift 会清空 $@，不能直接复用）。
ORIG_ARGS=("$@")

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
    idx=0
    for backend in sata nvme; do
        for mod in "${MODS[@]}"; do
            for nj in "${NJOBS_LIST[@]}"; do
                for wl in "${WORKLOADS[@]}"; do
                    for wt in "${WRITETYPES[@]}"; do
                        match_only "$mod" "$backend" "$wl" "$nj" "$wt" || continue
                        idx=$((idx + 1))
                        echo "DRY-RUN: #${idx} mod=${mod} backend=${backend}(${DEVICE[$backend]}) workload=${wl} nj=${nj} wt=${wt}"
                    done
                done
            done
        done
    done
    exit 0
fi

# ── docker self-entry ─────────────────────────────────────────────
if [[ ! -f /.dockerenv ]]; then
    echo "=== EXT4 perf matrix (os_com, real-disk backend, smp=${SMP}) ==="
    # 真实盘安全前置检查：目标设备必须未挂载、未被占用
    for dev in "${DEVICE[@]}"; do
        if findmnt -rn "$dev" &>/dev/null; then
            echo "ERROR: $dev is mounted — refusing to proceed" >&2
            exit 1
        fi
        if fuser -v "$dev" &>/dev/null 2>&1; then
            echo "ERROR: $dev is in use by another process" >&2
            exit 1
        fi
    done
    mkdir -p "${IMPL_DIR_HOST}"
    docker volume create asterinas-cargo  >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true
    # 注意：不用 -it（TTY 在管道/后台环境下会让 QEMU 收到 SIGTTOU/SIGSTOP 而
    # 挂起）；-i + </dev/null 让 stdio chardev 的 stdin 立即 EOF，与
    # run_single_round.sh 的做法一致。
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
        bash "${TEST_DIR_CTR}/run_perf_matrix_img.sh" "${ORIG_ARGS[@]}" < /dev/null
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
    # 测试盘是真实设备（XFSTESTS_TEST_IMAGE 直通），每轮 guest 内重新 mke2fs。
    # 只准备辅助镜像。
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

# 从运行日志按 FIO_RESULT tag 精确提取段并解析关键指标，输出 TSV 行：
#   iops  bw_mib_s  avg_lat_usec  p50  p90  p99  p999
# benchmark 输出走 QEMU stdio（make run_kernel 的 stdout → RUN_LOG），
# 而不是 qemu-serial.log（那里只有 kernel log）。
# 按 tag 精确匹配：fio 失败（无该段）时输出全 NA，而不是回退到上一轮数据。
parse_fio() {
    local log="$1" fio_tag="$2"
    local sec
    sec=$(awk -v tag="$fio_tag" '
        index($0, "=== FIO_RESULT: " tag " ===") > 0 {collect=1; buf=""}
        collect {buf=buf $0 "\n"}
        /=== END_FIO_RESULT ===/{collect=0}
        END {printf "%s", buf}
    ' "$log" 2>/dev/null)

    # 把 fio 延迟数值（nsec/usec/msec）统一折算为 usec，保留 2 位小数。
    to_usec() {
        local num="$1" unit="$2"
        [[ -z "$num" ]] && { echo ""; return; }
        case "$unit" in
            nsec) awk "BEGIN{printf \"%.2f\", $num/1000}" ;;
            msec) awk "BEGIN{printf \"%.2f\", $num*1000}" ;;
            *)    awk "BEGIN{printf \"%.2f\", $num}" ;;
        esac
    }

    # IOPS：展开 k 后缀（fio 的 IOPS=58.9k 表示 58900）。
    local iops_num iops_k iops
    iops_num=$(echo "$sec" | grep -a 'write: IOPS=' | head -1 | sed -n 's/.*IOPS=\([0-9.]*\)k\{0,1\}.*/\1/p')
    iops_k=$(echo "$sec" | grep -a 'write: IOPS=' | head -1 | grep -o 'IOPS=[0-9.]*k' | grep -o 'k$')
    if [[ "$iops_k" == "k" && -n "$iops_num" ]]; then
        iops=$(awk "BEGIN{printf \"%.0f\", $iops_num*1000}")
    else
        iops="$iops_num"
    fi

    # BW：归一化到 MiB/s（fio 1024 进制单位 KiB/MiB/GiB）。
    # 行形如 "write: IOPS=536, BW=2144KiB/s ..."，故匹配 "BW=" 而非 "write: BW="。
    local bw_num bw_unit bw_mib
    bw_num=$(echo "$sec" | grep -a 'BW=' | head -1 | sed -n 's/.*BW=\([0-9.]*\)\(KiB\|MiB\|GiB\)\/s.*/\1/p')
    bw_unit=$(echo "$sec" | grep -a 'BW=' | head -1 | sed -n 's/.*BW=[0-9.]*\(KiB\|MiB\|GiB\)\/s.*/\1/p')
    case "$bw_unit" in
        KiB) bw_mib=$(awk "BEGIN{printf \"%.2f\", $bw_num/1024}") ;;
        GiB) bw_mib=$(awk "BEGIN{printf \"%.2f\", $bw_num*1024}") ;;
        MiB) bw_mib="$bw_num" ;;
        *) bw_mib="" ;;
    esac

    # 平均延迟：取 "lat (unit): ... avg=..." 行，统一折算 usec。
    # 注意 fio 输出 "avg= 8.83"（= 后可能带空格）。
    local lat_line lat_unit avg_raw avg_lat
    lat_line=$(echo "$sec" | grep -a ' lat (' | head -1)
    lat_unit=$(echo "$lat_line" | sed -n 's/.* lat (\([a-z]*\)).*/\1/p')
    avg_raw=$(echo "$lat_line" | sed -n 's/.*avg= *\([0-9.]*\).*/\1/p')
    avg_lat=$(to_usec "$avg_raw" "$lat_unit")

    # percentile：单位取自 "clat percentiles (unit):" 段标题，统一折算 usec。
    # --percentile_list=50:90:99:99.9 的输出形如
    #   | 50.0th=[   25], 90.0th=[   34], 99.0th=[   62], 99.9th=[  111]
    local pct_unit p50 p90 p99 p999
    pct_unit=$(echo "$sec" | grep -a 'clat percentiles' | head -1 | sed -n 's/.*clat percentiles (\([a-z]*\)).*/\1/p')
    [[ -z "$pct_unit" ]] && pct_unit="usec"
    p50=$(to_usec "$(echo "$sec" | grep -a '50\.0th=' | head -1 | sed -n 's/.*50\.0th=\[ *\([0-9]*\)\].*/\1/p')" "$pct_unit")
    p90=$(to_usec "$(echo "$sec" | grep -a '90\.0th=' | head -1 | sed -n 's/.*90\.0th=\[ *\([0-9]*\)\].*/\1/p')" "$pct_unit")
    p99=$(to_usec "$(echo "$sec" | grep -a '99\.0th=' | head -1 | sed -n 's/.*99\.0th=\[ *\([0-9]*\)\].*/\1/p')" "$pct_unit")
    p999=$(to_usec "$(echo "$sec" | grep -a '99\.9th=' | head -1 | sed -n 's/.*99\.9th=\[ *\([0-9]*\)\].*/\1/p')" "$pct_unit")

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s' "${iops:-NA}" "${bw_mib:-NA}" "${avg_lat:-NA}" \
        "${p50:-NA}" "${p90:-NA}" "${p99:-NA}" "${p999:-NA}"
}

write_result_header() {
    local kernel_commit
    # docker run --rm 的新容器没有 safe.directory 配置，root 访问宿主 UID 的
    # 仓库会触发 "dubious ownership"，故显式 -c safe.directory。
    kernel_commit=$(git -C "${REPO_CTR}" -c safe.directory="${REPO_CTR}" rev-parse --short HEAD 2>/dev/null || echo unknown)
    cat > "${RESULT_MD}" <<EOF
# EXT4 性能矩阵（32 点）—— os_com，真实盘后端

## 配置

| 项 | 值 |
|----|----|
| 仓库 | os_com @ \`${kernel_commit}\` |
| 后端 | 真实盘（sata→/dev/sda4；nvme→/dev/nvme3n1p2），virtio-blk 直通 guest /dev/vdc（cache=none, aio=threads） |
| CPU/内存 | smp=${SMP}, mem=${MEM} |
| 固定配置 | size=256M（所有 job 文件大小之和，每 job 独占文件 = 256M/njobs）, writeback_capacity_mb=64（os_com 忽略未知 option）, runtime=30s, ramp=5s |
| workload 语义 | random-4k 先 precondition 再 overwrite；sequential-128k 直接顺序写满 256M |

## 结果

| # | mod | backend | workload | njobs | writetype | IOPS | BW(MiB/s) | avg lat(us) | p50(us) | p90(us) | p99(us) | p99.9(us) |
|---|-----|---------|----------|-------|-----------|------|-----------|-------------|---------|---------|---------|-----------|
EOF
}

append_result_row() {
    local idx="$1" mod="$2" backend="$3" wl="$4" nj="$5" wt="$6"
    local -a fields
    IFS=$'\t' read -r -a fields <<<"$7"
    printf '| %d | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n' \
        "$idx" "$mod" "$backend" "$wl" "$nj" "$wt" \
        "${fields[0]}" "${fields[1]}" "${fields[2]}" "${fields[3]}" "${fields[4]}" "${fields[5]}" "${fields[6]}" \
        >> "${RESULT_MD}"
}

write_result_footer() {
    cat >> "${RESULT_MD}" <<EOF

## 说明

- 后端为宿主机真实盘：sata→/dev/sda4、nvme→/dev/nvme3n1p2，经 virtio-blk 直通 guest /dev/vdc（cache=none, aio=threads），每轮 guest 重新 mke2fs。
- size=256M 为所有 job 文件大小之和（每 job 独占文件，256M/njobs）。
- os_com 当前 002 后台写回为周期 1s 触发（无 dirty 水位），writeback_capacity_mb=64 作为未知 option 被忽略。
EOF
}

run_one() {
    local idx="$1" mod="$2" backend="$3" wl="$4" nj="$5" wt="$6"
    local dev="${DEVICE[$backend]}"
    local tag="${wl}-nj${nj}-${wt}-${mod}-${backend}"
    local fio_tag="${wl}_nj${nj}_${wt}_${mod}_${backend}"
    local serial_log="${RESULT_DIR_CTR}/perf-matrix-${tag}.log"

    echo ""
    echo "[matrix] === round ${idx}/32: mod=${mod} backend=${backend}(${dev}) workload=${wl} nj=${nj} wt=${wt} ===" | tee -a "${RUN_LOG}"

    cd "${REPO_CTR}"
    # 真实盘直通：XFSTESTS_TEST_IMAGE 指向宿主机设备，guest 内为 /dev/vdc
    # （virtio-blk，cache=none, aio=threads）。每轮 guest 重新 mke2fs。
    # NETDEV=none：fio 本地 IO 不需要网络，禁用随机 hostfwd 端口转发，避免
    # 连续 boot 时随机端口撞 TIME_WAIT/并发实验导致 QEMU 启动失败（NA 行）。
    XFSTESTS_TEST_IMAGE="${dev}" \
    XFSTESTS_TEST_CACHE="none" \
    XFSTESTS_TEST_AIO="threads" \
    ATTACH_XFSTESTS_IMAGES=true \
    NETDEV=none \
    timeout "${PER_ROUND_TIMEOUT}" make run_kernel \
        "BENCHMARK=fio/${BENCHMARK_JOB_NAME}" \
        "BENCHMARK_ARGS=${mod} ${backend} ${wl} ${nj} ${wt}" \
        "SMP=${SMP}" \
        "MEM=${MEM}" 2>&1 \
        | grep -vE "warning: unstable feature specified|warning: \`[^\`]+\` \((lib|bin \"[^\"]+\")\) generated 1 warning|^  = note: this feature is not stably supported|^  \||grub-mkrescue: warning: cannot open directory \`/usr/local/grub/share/locale\`" \
        | tee -a "${RUN_LOG}" || true

    cp "${REPO_CTR}/qemu-serial.log" "${serial_log}" 2>/dev/null || true

    local parsed
    parsed=$(parse_fio "${RUN_LOG}" "${fio_tag}")
    echo "[matrix] parse: iops,bw_mib,avg_lat,p50,p90,p99,p999 = ${parsed}" | tee -a "${RUN_LOG}"
    append_result_row "$idx" "$mod" "$backend" "$wl" "$nj" "$wt" "$parsed"
    echo "[matrix] --- round done: ${tag} ---" | tee -a "${RUN_LOG}"
}

echo ""
echo "==============================================================="
echo "  EXT4 perf matrix (os_com, real-disk backend, smp=${SMP})"
echo "  ${#ONLY_FILTER[@]} filter(s), dry_run=${DRY_RUN}"
echo "==============================================================="

mkdir -p "${RESULT_DIR_CTR}"
: > "${RUN_LOG}"

if [[ "${DRY_RUN}" == true ]]; then
    idx=0
    for backend in sata nvme; do
        for mod in "${MODS[@]}"; do
            for nj in "${NJOBS_LIST[@]}"; do
                for wl in "${WORKLOADS[@]}"; do
                    for wt in "${WRITETYPES[@]}"; do
                        match_only "$mod" "$backend" "$wl" "$nj" "$wt" || continue
                        idx=$((idx + 1))
                        echo "DRY-RUN: #${idx} mod=${mod} backend=${backend}(${DEVICE[$backend]}) workload=${wl} nj=${nj} wt=${wt}"
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
write_result_header

idx=0
for backend in sata nvme; do
    for mod in "${MODS[@]}"; do
        for nj in "${NJOBS_LIST[@]}"; do
            for wl in "${WORKLOADS[@]}"; do
                for wt in "${WRITETYPES[@]}"; do
                    match_only "$mod" "$backend" "$wl" "$nj" "$wt" || continue
                    idx=$((idx + 1))
                    run_one "$idx" "$mod" "$backend" "$wl" "$nj" "$wt"
                done
            done
        done
    done
done

write_result_footer

# re-own results to the host user
if [[ -n "${HOST_UID}" && -n "${HOST_GID}" ]]; then
    chown "${HOST_UID}:${HOST_GID}" "${RUN_LOG}" "${RESULT_MD}" "${RESULT_DIR_CTR}"/perf-matrix-*.log 2>/dev/null || true
fi

echo ""
echo "================================================================"
echo "  Done: ${idx}/32 rounds completed"
echo "  Log:    ${RUN_LOG}"
echo "  Result: ${RESULT_MD}"
echo "================================================================"
