#!/bin/bash
# SPDX-License-Identifier: MPL-2.0

set -euo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

REPO_HOST="${OS_COM_ROOT}"
REPO_CTR="/root/asterinas"
IMPL_HOST="${OS_COM_IMPL_ROOT}"
IMPL_CTR="/root/impl"
TEST_REL="test/rnvpce-reverse-recovery"
TEST_CTR="${IMPL_CTR}/${TEST_REL}"
RESULTS_ROOT="${TEST_CTR}/results"
BENCHMARK="rnvpce_reverse_recovery"
JOB_DIR="${REPO_CTR}/test/initramfs/src/benchmark/fio/${BENCHMARK}"
IMAGE="${REPO_CTR}/test/initramfs/build/rnvpce-reverse-recovery.img"
BASE_ISO="${REPO_CTR}/target/osdk/aster-kernel/aster-kernel-osdk-bin.iso"

TARGETS="1,1024,1048576"
REPEATS=5
SMP=16
MEM="64G"
TIMEOUT_SEC=21600
SKIP_BUILD=false
DRY_RUN=false
TIMESTAMP=""
FILL_MIB=4096
RNVPC_KCMD="rnvpc_size=16G rnvpc_meta_reserve=512M rnvpc_data_limit=15872M rnvpc_sim_mode=fast rnvpc_recovery_bench=on"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --targets) TARGETS="$2"; shift 2 ;;
        --repeats) REPEATS="$2"; shift 2 ;;
        --smp) SMP="$2"; shift 2 ;;
        --mem) MEM="$2"; shift 2 ;;
        --timeout) TIMEOUT_SEC="$2"; shift 2 ;;
        --timestamp) TIMESTAMP="$2"; shift 2 ;;
        --fill-mib) FILL_MIB="$2"; shift 2 ;;
        --rnvpc-kcmd) RNVPC_KCMD="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --smoke)
            TARGETS=1
            REPEATS=2
            MEM=4G
            FILL_MIB=48
            RNVPC_KCMD="rnvpc_size=64M rnvpc_meta_reserve=4M rnvpc_data_limit=60M rnvpc_sim_mode=fast rnvpc_recovery_bench=on"
            shift
            ;;
        --dry-run|-n) DRY_RUN=true; shift ;;
        *) echo "error: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

IFS=',' read -r -a TARGET_LIST <<< "$TARGETS"
for target in "${TARGET_LIST[@]}"; do
    case "$target" in 1|1024|1048576) ;; *) echo "invalid target: $target" >&2; exit 2 ;; esac
done

if [ "$DRY_RUN" = true ]; then
    echo "=== DRY-RUN: rNVPCe reverse-index recovery scaling ==="
    echo "NVPool args: ${RNVPC_KCMD}"
    echo "Image: 24 GiB  smp=${SMP}  mem=${MEM}  repeats=${REPEATS}"
    index=0
    for target in "${TARGET_LIST[@]}"; do
        index=$((index + 1))
        echo "  #${index}: target_files=${target}; preallocate=${FILL_MIB}MiB on plain EXT4, overwrite through rNVPCe, then rebuild reverse index ${REPEATS} times"
    done
    echo "Total: ${index} population runs / $((index * REPEATS)) recovery samples"
    exit 0
fi

[ -z "$TIMESTAMP" ] && TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULTS_DIR="${RESULTS_ROOT}/${TIMESTAMP}-rnvpce-reverse-recovery"
LOGS_DIR="${RESULTS_DIR}/logs"
COMBINED_LOG="${LOGS_DIR}/combined.log"
PREBUILT_ROOT="/tmp/rnvpce-reverse-recovery-${TIMESTAMP}"
RUN_ISO="${PREBUILT_ROOT}/run.iso"
GRUB_CFG="${PREBUILT_ROOT}/iso-root/boot/grub/grub.cfg"
DEFAULT_IMAGES="${PREBUILT_ROOT}/default-images"
DATASET_BUILDER="${PREBUILT_ROOT}/create_dataset"
HOST_MNT="${PREBUILT_ROOT}/host-mnt"

if [ ! -f /.dockerenv ]; then
    mkdir -p "${IMPL_HOST}/${TEST_REL}/results"
    docker volume create asterinas-cargo >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true
    skip_arg=()
    [ "$SKIP_BUILD" = true ] && skip_arg=(--skip-build)
    exec docker run -i --rm --privileged --network=none --device=/dev/kvm \
        -v "${REPO_HOST}:${REPO_CTR}" -v "${IMPL_HOST}:${IMPL_CTR}" \
        -v asterinas-cargo:/root/.cargo -v asterinas-rustup:/root/.rustup \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" -w "$REPO_CTR" \
        asterinas/dev:0.18.1-20260805 \
        bash "${TEST_CTR}/run.sh" --targets "$TARGETS" --repeats "$REPEATS" \
            --smp "$SMP" --mem "$MEM" --timeout "$TIMEOUT_SEC" \
            --timestamp "$TIMESTAMP" --fill-mib "$FILL_MIB" \
            --rnvpc-kcmd "$RNVPC_KCMD" "${skip_arg[@]}" </dev/null
fi

mkdir -p "$LOGS_DIR"
: >"$COMBINED_LOG"
mkdir -p "$DEFAULT_IMAGES"
gcc -O2 -Wall -Wextra -Werror "${TEST_CTR}/create_dataset.c" -o "$DATASET_BUILDER"
# qemu_args.sh always attaches the repository's ext2/exfat/NVMe images. Give
# this experiment private sparse placeholders so a concurrent VM cannot lock
# shared images that the benchmark never uses.
truncate -s 64M "${DEFAULT_IMAGES}/ext2.img"
truncate -s 64M "${DEFAULT_IMAGES}/exfat.img"
truncate -s 64M "${DEFAULT_IMAGES}/nvme0n1.img"
truncate -s 64M "${DEFAULT_IMAGES}/xfstests_scratch.img"
mkdir -p "${JOB_DIR}/mkfs"
cp "${REPO_CTR}/test/initramfs/src/benchmark/fio/ext4_journal_compare/mkfs/mke2fs" \
    "${JOB_DIR}/mkfs/mke2fs"
chmod +x "${JOB_DIR}/run.sh" "${JOB_DIR}/mkfs/mke2fs"

if [ "$SKIP_BUILD" = false ]; then
    echo "[build] injecting benchmark into the existing initramfs and compiling once"
    BASE_INITRAMFS="$(find "${REPO_CTR}/target/osdk/aster-kernel" -maxdepth 1 \
        -type f -name '*-initramfs-image' | head -n 1)"
    if [ -z "$BASE_INITRAMFS" ] \
        && [ -f "${REPO_CTR}/target/osdk/aster-kernel/initramfs.cpio.gz" ]; then
        BASE_INITRAMFS="${REPO_CTR}/target/osdk/aster-kernel/initramfs.cpio.gz"
    fi
    [ -n "$BASE_INITRAMFS" ] || { echo "error: no reusable initramfs image" >&2; exit 1; }
    INITRAMFS_ROOT="${PREBUILT_ROOT}/initramfs-root"
    CUSTOM_INITRAMFS="${PREBUILT_ROOT}/initramfs.cpio.gz"
    mkdir -p "$INITRAMFS_ROOT"
    (
        cd "$INITRAMFS_ROOT"
        gzip -dc "$BASE_INITRAMFS" | cpio -idmu --quiet
        mkdir -p "benchmark/fio/${BENCHMARK}/mkfs"
        cp "${JOB_DIR}/run.sh" "benchmark/fio/${BENCHMARK}/run.sh"
        cp "benchmark/fio/aster_ext4_remount/mkfs/mke2fs" \
            "benchmark/fio/${BENCHMARK}/mkfs/mke2fs"
        chmod +x "benchmark/fio/${BENCHMARK}/run.sh" \
            "benchmark/fio/${BENCHMARK}/mkfs/mke2fs"
        find . -print0 | cpio --null -o --format=newc --quiet | gzip -1 >"$CUSTOM_INITRAMFS"
    )
    (
        cd "${REPO_CTR}/kernel"
        CARGO_NET_OFFLINE=true cargo osdk build --release \
            --boot-method=grub-rescue-iso --grub-boot-protocol=multiboot2 \
            --initramfs="$CUSTOM_INITRAMFS" --kcmd-args="$RNVPC_KCMD" \
            --init-args="/benchmark/common/bench_runner.sh fio/${BENCHMARK} asterinas 1 1 ${FILL_MIB}"
    ) >"${LOGS_DIR}/build.log" 2>&1
else
    echo "[build] skipped"
fi

mkdir -p "${PREBUILT_ROOT}/iso-root"
xorriso -osirrox on -indev "$BASE_ISO" -extract / "${PREBUILT_ROOT}/iso-root" >/dev/null 2>&1

prepare_iso() {
    local target="$1"
    printf '%s\n' \
        'set timeout_style=hidden' 'set timeout=0' "menuentry 'asterinas' {" \
        "    multiboot2 /boot/aster-kernel-osdk-bin SHELL=/bin/sh LOGNAME=root HOME=/ USER=root PATH=/bin:/benchmark init=/init ostd.log_level=error ext4.power_protected=0 console=hvc0 ${RNVPC_KCMD} -- sh -l /benchmark/common/bench_runner.sh fio/${BENCHMARK} asterinas ${target} ${REPEATS} ${FILL_MIB} 1" \
        '    module2 --nounzip /boot/initramfs.cpio.gz' '    boot' '}' >"$GRUB_CFG"
    grub-mkrescue -o "$RUN_ISO" "${PREBUILT_ROOT}/iso-root" >/dev/null 2>&1
}

prepare_ext4_image() {
    local target="$1"
    local inode_count=8192
    [ "$target" -eq 1048576 ] && inode_count=1200000
    truncate -s 0 "$IMAGE"
    truncate -s 24G "$IMAGE"
    /usr/sbin/mke2fs -F -q -t ext4 -b 4096 -I 256 -N "$inode_count" -E nodiscard \
        -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal \
        "$IMAGE"
    mkdir -p "$HOST_MNT"
    mount -o loop "$IMAGE" "$HOST_MNT"
    trap 'umount "$HOST_MNT" 2>/dev/null || true' EXIT INT TERM
    "$DATASET_BUILDER" "$HOST_MNT/dataset" "$target" "$FILL_MIB" \
        2>&1 | tee -a "$COMBINED_LOG"
    sync
    umount "$HOST_MNT"
    trap - EXIT INT TERM
}

overall_status=0
for target in "${TARGET_LIST[@]}"; do
    tag="files-${target}"
    echo "[run] ${tag}"
    prepare_ext4_image "$target"
    prepare_iso "$target"
    rm -f "${REPO_CTR}/qemu-serial.log" "${REPO_CTR}/qemu.log"
    export XFSTESTS_TEST_IMAGE="$IMAGE" XFSTESTS_TEST_CACHE=none XFSTESTS_TEST_AIO=threads
    export ATTACH_XFSTESTS_IMAGES=true NETDEV=none BOOT_METHOD=grub-rescue-iso BOOT_PROTOCOL=multiboot2
    qemu_args="$(SMP="$SMP" MEM="$MEM" "${REPO_CTR}/tools/qemu_args.sh" normal)"
    qemu_args="${qemu_args//.\/test\/initramfs\/build\/ext2.img/${DEFAULT_IMAGES}\/ext2.img}"
    qemu_args="${qemu_args//.\/test\/initramfs\/build\/exfat.img/${DEFAULT_IMAGES}\/exfat.img}"
    qemu_args="${qemu_args//.\/test\/initramfs\/build\/nvme0n1.img/${DEFAULT_IMAGES}\/nvme0n1.img}"
    qemu_args="${qemu_args//.\/test\/initramfs\/build\/xfstests_scratch.img/${DEFAULT_IMAGES}\/xfstests_scratch.img}"
    set +e
    (
        cd "$REPO_CTR"
        eval "timeout '${TIMEOUT_SEC}' qemu-system-x86_64 -drive file='${RUN_ISO}',format=raw,index=2,media=cdrom -accel kvm ${qemu_args}"
    ) >>"$COMBINED_LOG" 2>&1 &
    runner_pid=$!
    marker="RNVPC_REVERSE_RECOVERY_COMPLETE target_files=${target}"
    marker_seen=false
    while kill -0 "$runner_pid" 2>/dev/null; do
        if grep -qF "$marker" "${REPO_CTR}/qemu-serial.log" 2>/dev/null \
            || grep -qF "$marker" "$COMBINED_LOG" 2>/dev/null; then
            marker_seen=true
            sleep 1
            pkill -f "qemu-system-x86_64.*rnvpce-reverse-recovery.img" 2>/dev/null || true
            break
        fi
        sleep 2
    done
    wait "$runner_pid"
    status=$?
    set -e
    if grep -qF "$marker" "${REPO_CTR}/qemu-serial.log" 2>/dev/null \
        || grep -qF "$marker" "$COMBINED_LOG" 2>/dev/null; then
        marker_seen=true
    fi
    [ "$marker_seen" = true ] && status=0
    cp "${REPO_CTR}/qemu-serial.log" "${LOGS_DIR}/serial-${tag}.log" 2>/dev/null || true
    cp "${REPO_CTR}/qemu.log" "${LOGS_DIR}/qemu-${tag}.log" 2>/dev/null || true
    cat "${LOGS_DIR}/serial-${tag}.log" >>"$COMBINED_LOG" 2>/dev/null || true
    if [ "$status" -ne 0 ]; then
        echo "[warn] ${tag} failed status=${status}" | tee -a "$COMBINED_LOG"
        overall_status=1
    fi
done

python3 "${TEST_CTR}/parse_results.py" "$COMBINED_LOG" \
    "${RESULTS_DIR}/raw.csv" "${RESULTS_DIR}/summary.csv"

if [ -n "${HOST_UID:-}" ]; then
    chown -R "${HOST_UID}:${HOST_GID:-${HOST_UID}}" "$RESULTS_DIR" 2>/dev/null || true
fi
echo "Results: ${RESULTS_DIR}/summary.csv"
exit "$overall_status"
