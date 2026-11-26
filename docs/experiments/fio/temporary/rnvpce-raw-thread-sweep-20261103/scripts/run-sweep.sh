#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MQ_DIR=$(realpath "$SCRIPT_DIR/../rtio-virtio-multiqueue-20261102")
ROOT_ART=${ROOT_ART:?set ROOT_ART to an existing external artifact directory}
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to selected.iso/ext2.img/exfat.img}
TARGET=${TARGET:-/dev/nvme1n1p2}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
JOBS_LIST=${JOBS_LIST:-"1 2 4 8 16"}

[[ -d $ROOT_ART && -O $ROOT_ART && -w $ROOT_ART ]] || {
    echo "ROOT_ART must be an existing caller-owned writable directory" >&2
    exit 2
}
case $TARGET in /dev/nvme1n1p1|/dev/nvme1n1p2) ;; *) echo "unsupported target" >&2; exit 2 ;; esac

exec 9>"$ROOT_ART/sweep.lock"
flock -n 9 || { echo "sweep already running" >&2; exit 2; }

for mode in ${MODES:-rnvpce raw}; do
    case $mode in rnvpce|rnvpce-warm|raw|linux) ;; *) echo "unsupported mode: $mode" >&2; exit 2 ;; esac
    for jobs in $JOBS_LIST; do
        case_name="${mode}-randwrite-4k-j${jobs}"
        point="$ROOT_ART/$case_name"
        inputs="$ROOT_ART/input-$case_name"
        mkdir "$inputs"
        guest_kind=aster
        input_files=(selected.iso ext2.img exfat.img)
        if [[ $mode == linux ]]; then
            guest_kind=linux
            input_files=(vmlinuz initramfs.cpio.gz ext2.img exfat.img)
        fi
        for file in "${input_files[@]}"; do
            cp --reflink=auto "$SOURCE_ART/$file" "$inputs/$file"
        done
        cat >"$inputs/case.cfg" <<EOF
CASE=$case_name
MODE=$mode
JOBS=$jobs
TOTAL_MIB=2048
EXPECTED_BYTES=2147483648
EOF
        for guest_file in rebuild-fio.cfg rebuild-fio.sh; do
            debugfs -w -R "rm /$guest_file" "$inputs/ext2.img" >/dev/null 2>&1 || true
        done
        debugfs -w -R "write $inputs/case.cfg /rebuild-fio.cfg" "$inputs/ext2.img" >/dev/null
        if [[ $guest_kind == aster ]]; then
            debugfs -w -R "write $SCRIPT_DIR/guest.sh /rebuild-fio.sh" "$inputs/ext2.img" >/dev/null
        fi
        format=0
        check=0
        if [[ $mode == rnvpce || $mode == rnvpce-warm ]]; then format=1; check=1; fi
        status=0
        ART="$point" SOURCE_ART="$inputs" GUEST_KIND="$guest_kind" QUEUES=4 FORMAT_TARGET=$format CHECK_FILESYSTEM=$check \
            TOOLS_IMAGE="$TOOLS_IMAGE" bash "$MQ_DIR/run-validation.sh" \
            "--authorized-destructive-target=$TARGET" >"$ROOT_ART/$case_name-runner.log" 2>&1 || status=$?
        printf '%s\n' "$status" >"$ROOT_ART/$case_name-runner.status"
        # Status 3 means the run completed but fewer than four host hctx passed
        # the mechanism threshold. This is expected for low job counts and is
        # retained in the evidence rather than treated as an I/O failure.
        [[ $status == 0 || $status == 3 ]] || { echo "$case_name failed: $status" >&2; exit 2; }
        python3 "$SCRIPT_DIR/results.py" "$point" >"$point/result.json"
        python3 - "$point/result.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
print(f'{r["case"]}: {r["fio_mib_s"]:.2f} MiB/s, mean={r["mean_latency_us"]:.2f} us, p99={r["p99_latency_us"]:.2f} us')
PY
    done
done
