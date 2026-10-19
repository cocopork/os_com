#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# Clean remount latency: ordinary EXT4 versus EXT4-rNVPCe.
# This is not a crash-reset or power-loss recovery experiment.

set -euo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

REPO_HOST="${OS_COM_ROOT}"
REPO_CTR="/root/asterinas"
IMPL_HOST="${OS_COM_IMPL_ROOT}"
IMPL_CTR="/root/impl"
TEST_REL="test/aster-ext4-baseline-test"
TEST_CTR="${REMOUNT_TEST_DIR:-${IMPL_CTR}/${TEST_REL}}"
RESULTS_ROOT="${REMOUNT_RESULTS_ROOT:-${TEST_CTR}/results}"
BENCHMARK="aster_ext4_remount"
JOB_DIR="${REPO_CTR}/test/initramfs/src/benchmark/fio/${BENCHMARK}"
SUMMARY_YAML="${REPO_CTR}/test/initramfs/src/benchmark/fio/summary.yaml"
IMAGE="${REPO_CTR}/test/initramfs/build/remount-recovery.img"
BASE_ISO="${REPO_CTR}/target/osdk/aster-kernel/aster-kernel-osdk-bin.iso"

PROFILES="ext4-orderless,ext4-rNVPCe"
SIZES="32,256,2048"
REPEATS=5
REPEAT_START=1
SMP=16
MEM="4G"
TIMEOUT_SEC=1800
SKIP_BUILD=false
PREBUILT=false
DRY_RUN=false
TIMESTAMP=""
RNVPC_KCMD="rnvpc_size=64M rnvpc_meta_reserve=16M rnvpc_data_limit=48M rnvpc_sim_mode=fast"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --profiles) PROFILES="$2"; shift 2 ;;
        --sizes) SIZES="$2"; shift 2 ;;
        --repeats) REPEATS="$2"; shift 2 ;;
        --repeat-start) REPEAT_START="$2"; shift 2 ;;
        --smp) SMP="$2"; shift 2 ;;
        --mem) MEM="$2"; shift 2 ;;
        --timeout) TIMEOUT_SEC="$2"; shift 2 ;;
        --timestamp) TIMESTAMP="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --prebuilt) PREBUILT=true; SKIP_BUILD=true; shift ;;
        --dry-run|-n) DRY_RUN=true; shift ;;
        *) echo "error: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

IFS=',' read -r -a PROFILE_LIST <<< "$PROFILES"
IFS=',' read -r -a SIZE_LIST <<< "$SIZES"
for profile in "${PROFILE_LIST[@]}"; do
    case "$profile" in
        ext4-orderless|ext4-rNVPCe) ;;
        *) echo "error: unknown profile '$profile'" >&2; exit 2 ;;
    esac
done

if [ "$DRY_RUN" = true ]; then
    echo "=== DRY-RUN: clean remount latency ==="
    echo "Image: 2.5 GiB sparse image  rNVPC: 64 MiB  smp=${SMP}  mem=${MEM}"
    index=0
    for profile in "${PROFILE_LIST[@]}"; do
        for size in "${SIZE_LIST[@]}"; do
            index=$((index + 1))
            echo "  #${index}: profile=${profile} data=${size}MiB repeats=${REPEATS}"
        done
    done
    echo "Total: ${index} logical points ($((index * REPEATS)) VM runs)"
    exit 0
fi

[ -z "$TIMESTAMP" ] && TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULTS_DIR="${RESULTS_ROOT}/${TIMESTAMP}-remount-recovery"
LOGS_DIR="${RESULTS_DIR}/logs"
COMBINED_LOG="${LOGS_DIR}/remount.log"
PREBUILT_ROOT="/tmp/aster-ext4-remount-${TIMESTAMP}"
RUN_ISO="${PREBUILT_ROOT}/run.iso"
GRUB_CFG="${PREBUILT_ROOT}/iso-root/boot/grub/grub.cfg"

if [ ! -f /.dockerenv ]; then
    HOST_RESULTS="${IMPL_HOST}/${TEST_REL}/results/${TIMESTAMP}-remount-recovery"
    mkdir -p "${HOST_RESULTS}/logs"
    docker volume create asterinas-cargo >/dev/null 2>&1 || true
    docker volume create asterinas-rustup >/dev/null 2>&1 || true
    skip_arg=()
    [ "$SKIP_BUILD" = true ] && skip_arg=(--skip-build)
    [ "$PREBUILT" = true ] && skip_arg=(--prebuilt)
    exec docker run -i --rm --privileged --network=host \
        --device=/dev/kvm \
        -v "${REPO_HOST}:${REPO_CTR}" \
        -v "${IMPL_HOST}:${IMPL_CTR}" \
        -v "${REPO_HOST}/.impl-host:${REPO_HOST}/.impl-host" \
        -v asterinas-cargo:/root/.cargo \
        -v asterinas-rustup:/root/.rustup \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        -w "${REPO_CTR}" \
        asterinas/asterinas:0.18.0-20260618 \
        bash "${TEST_CTR}/run-remount-recovery.sh" \
            --profiles "$PROFILES" --sizes "$SIZES" --repeats "$REPEATS" \
            --repeat-start "$REPEAT_START" \
            --smp "$SMP" --mem "$MEM" --timeout "$TIMEOUT_SEC" \
            --timestamp "$TIMESTAMP" "${skip_arg[@]}" </dev/null
fi

mkdir -p "$LOGS_DIR"
: > "$COMBINED_LOG"

mkdir -p "$JOB_DIR"
cp "${TEST_CTR}/remount_recovery_guest.sh" "${JOB_DIR}/run.sh"
chmod +x "${JOB_DIR}/run.sh"
mkdir -p "${JOB_DIR}/mkfs"
cp "${REPO_CTR}/test/initramfs/src/benchmark/fio/ext4_journal_compare/mkfs/mke2fs" "${JOB_DIR}/mkfs/mke2fs"
chmod +x "${JOB_DIR}/mkfs/mke2fs"
grep -qF "  - ${BENCHMARK}" "$SUMMARY_YAML" || sed -i "\$i\  - ${BENCHMARK}" "$SUMMARY_YAML"

if [ "$SKIP_BUILD" = false ]; then
    echo "[build] compiling once"
    make kernel "BENCHMARK=fio/${BENCHMARK}" RELEASE=1 SMP="$SMP" MEM="$MEM" \
        "RNVPC_KCMD=${RNVPC_KCMD}" >"${LOGS_DIR}/build.log" 2>&1
else
    echo "[build] skipped by request"
fi

prepare_prebuilt_iso() {
    local profile="$1"
    local size="$2"
    local repeat="$3"
    if [ ! -d "${PREBUILT_ROOT}/iso-root/boot/grub" ]; then
        mkdir -p "${PREBUILT_ROOT}/iso-root"
        xorriso -osirrox on -indev "$BASE_ISO" \
            -extract / "${PREBUILT_ROOT}/iso-root" >/dev/null 2>&1
    fi
    printf '%s\n' \
        'set timeout_style=hidden' \
        'set timeout=0' \
        "menuentry 'asterinas' {" \
        "    multiboot2 /boot/aster-kernel-osdk-bin SHELL=/bin/sh LOGNAME=root HOME=/ USER=root PATH=/bin:/benchmark init=/init ostd.log_level=error ext4.power_protected=0 console=hvc0 ${RNVPC_KCMD} -- sh -l /benchmark/common/bench_runner.sh fio/${BENCHMARK} asterinas ${profile} ${size} ${repeat}" \
        '    module2 --nounzip /boot/initramfs.cpio.gz' \
        '    boot' \
        '}' >"$GRUB_CFG"
    grub-mkrescue -o "$RUN_ISO" "${PREBUILT_ROOT}/iso-root" >/dev/null 2>&1
}

overall_status=0
for profile in "${PROFILE_LIST[@]}"; do
    for size in "${SIZE_LIST[@]}"; do
        repeat_end=$((REPEAT_START + REPEATS - 1))
        for repeat in $(seq "$REPEAT_START" "$repeat_end"); do
            tag="${profile}-${size}MiB-r${repeat}"
            echo "[run] ${tag}"
            truncate -s 0 "$IMAGE"
            truncate -s 2560M "$IMAGE"
            rm -f "${REPO_CTR}/qemu-serial.log" "${REPO_CTR}/qemu.log"
            set +e
            export XFSTESTS_TEST_IMAGE="$IMAGE"
            export XFSTESTS_TEST_CACHE=none
            export XFSTESTS_TEST_AIO=threads
            export ATTACH_XFSTESTS_IMAGES=true
            export NETDEV=none
            if [ "$PREBUILT" = true ]; then
                prepare_prebuilt_iso "$profile" "$size" "$repeat"
                qemu_args="$(BOOT_METHOD=grub-rescue-iso BOOT_PROTOCOL=multiboot2 \
                    SMP="$SMP" MEM="$MEM" "${REPO_CTR}/tools/qemu_args.sh" normal)"
                (
                    cd "$REPO_CTR"
                    eval "timeout \"$TIMEOUT_SEC\" qemu-system-x86_64 \
                        -drive file=\"${RUN_ISO}\",format=raw,index=2,media=cdrom \
                        -accel kvm ${qemu_args}"
                ) >>"$COMBINED_LOG" 2>&1 &
            else
                timeout "$TIMEOUT_SEC" make run_kernel \
                    "BENCHMARK=fio/${BENCHMARK}" \
                    "BENCHMARK_ARGS=${profile} ${size} ${repeat}" \
                    RELEASE=1 SMP="$SMP" MEM="$MEM" \
                    "RNVPC_KCMD=${RNVPC_KCMD}" >>"$COMBINED_LOG" 2>&1 &
            fi
            runner_pid=$!
            marker="REMOUNT_RECOVERY_COMPLETE profile=${profile} data_mb=${size} repeat=${repeat} valid=1 verify_ok=1"
            marker_seen=false
            while kill -0 "$runner_pid" 2>/dev/null; do
                if grep -qF "$marker" "${REPO_CTR}/qemu-serial.log" 2>/dev/null \
                    || grep -qF "$marker" "$COMBINED_LOG" 2>/dev/null; then
                    marker_seen=true
                    sleep 1
                    for qemu_pid in $(pgrep -f "qemu-system-x86_64.*remount-recovery.img"); do
                        kill "$qemu_pid" 2>/dev/null || true
                    done
                    break
                fi
                sleep 1
            done
            wait "$runner_pid"
            status=$?
            if grep -qF "$marker" "${REPO_CTR}/qemu-serial.log" 2>/dev/null \
                || grep -qF "$marker" "$COMBINED_LOG" 2>/dev/null; then
                marker_seen=true
            fi
            echo "[run] ${tag} runner_status=${status} marker_seen=${marker_seen}" >>"$COMBINED_LOG"
            if [ "$marker_seen" = true ]; then
                status=0
            elif [ "$status" -eq 0 ]; then
                echo "[warn] ${tag} runner exited without the guest completion marker" \
                    | tee -a "$COMBINED_LOG"
                status=1
            fi
            set -e
            cp "${REPO_CTR}/qemu-serial.log" "${LOGS_DIR}/serial-${tag}.log" 2>/dev/null || true
            cp "${REPO_CTR}/qemu.log" "${LOGS_DIR}/qemu-${tag}.log" 2>/dev/null || true
            if [ "$status" -ne 0 ]; then
                echo "[warn] ${tag} exited with status ${status}" | tee -a "$COMBINED_LOG"
                overall_status=1
            fi
        done
    done
done

python3 "${TEST_CTR}/parse_remount_recovery.py" \
    "$COMBINED_LOG" "${RESULTS_DIR}/raw.csv" "${RESULTS_DIR}/summary.csv"

if [ -n "${HOST_UID:-}" ]; then
    chown -R "${HOST_UID}:${HOST_GID:-${HOST_UID}}" "$RESULTS_DIR" 2>/dev/null || true
fi

echo "Results: ${RESULTS_DIR}/summary.csv"
exit "$overall_status"
