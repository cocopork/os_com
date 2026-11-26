#!/bin/bash
set -uo pipefail

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export LD_LIBRARY_PATH=/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
exec >/dev/console 2>&1

mkdir -p /proc /sys /dev /run /tmp /mnt/test
mountpoint -q /proc || mount -t proc proc /proc
mountpoint -q /sys || mount -t sysfs sysfs /sys
mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev
mountpoint -q /run || mount -t tmpfs tmpfs /run
mountpoint -q /tmp || mount -t tmpfs tmpfs /tmp

echo "NVLOG_GUEST_BEGIN"
uname -a
echo "CMDLINE=$(cat /proc/cmdline)"

udevadm trigger --action=add || true
udevadm settle --timeout=20 || true

echo "NVLOG_NDCTL_BEFORE"
ndctl list -RNDv || true
echo "NVLOG_DAXCTL_BEFORE"
daxctl list -DMu || true

cmdline_value() {
    local key=$1 token
    for token in $(cat /proc/cmdline); do
        case "$token" in
            "$key"=*) printf '%s\n' "${token#*=}"; return 0 ;;
        esac
    done
    return 1
}

if ! daxctl list -D | grep -q '"chardev"'; then
    echo "NVLOG_CREATE_DEVDAX"
    ndctl create-namespace --reconfig=namespace0.0 --mode=devdax \
        --map=dev --align=2M --force || true
    udevadm settle --timeout=20 || true
fi

nvpc_pages=$(cmdline_value nvpc_pages || printf '%s\n' 16384)
nvpcctl config \
    extend_lru=1 absorb_syn=1 promote_level=4 \
    demote_before_promote=1 nvpc_lru_evict=1 nvpc_sz="$nvpc_pages" \
    rebuild=0 force_start=1
config_status=$?
echo "NVLOG_CONFIG_STATUS=$config_status"

echo "NVLOG_RECONFIGURE"
if [[ $config_status -eq 0 ]]; then
    daxctl reconfigure-device --mode=nvpc all --no-online
    reconfigure_status=$?
else
    reconfigure_status=1
fi
echo "NVLOG_RECONFIGURE_STATUS=$reconfigure_status"
daxctl list -DMu || true

smoke_status=0
if grep -qw 'nvlog_smoke=1' /proc/cmdline; then
    /opt/nvlog-smoke.sh
    smoke_status=$?
    echo "NVLOG_SMOKE_STATUS=$smoke_status"
fi

if [[ $(cmdline_value nvlog_workload || true) == mixed ]]; then
    /opt/nvlog-mixed.sh
    smoke_status=$?
    echo "NVLOG_MIXED_STATUS=$smoke_status"
fi

if [[ $(cmdline_value nvlog_workload || true) == fio ]]; then
    /opt/nvlog-fio.sh
    smoke_status=$?
    echo "NVLOG_FIO_STATUS=$smoke_status"
fi

if [[ $reconfigure_status -eq 0 && $config_status -eq 0 && $smoke_status -eq 0 ]]; then
    echo "NVLOG_GUEST_PASS"
    exit_code=0
else
    echo "NVLOG_GUEST_FAIL"
    exit_code=1
fi

sync
echo "NVLOG_GUEST_END=$exit_code"
poweroff -f
sleep 10
echo o >/proc/sysrq-trigger
