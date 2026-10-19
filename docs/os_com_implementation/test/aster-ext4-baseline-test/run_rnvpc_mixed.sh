#!/bin/bash
# Run the four-point rNVPCe pure-write versus mixed-I/O comparison.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
LATENCY_SOURCE="$ROOT/os_com/kernel/src/fs/fs_impls/ext4/fs.rs"
CONTAINER="asterinas-dev"
CONTAINER_TEST_DIR="/root/impl/test/aster-ext4-baseline-test"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULT_DIR="$HERE/results/${TIMESTAMP}_aster-ext4-baseline"
CONTAINER_RESULT_DIR="$CONTAINER_TEST_DIR/results/${TIMESTAMP}_aster-ext4-baseline"
WORKLOADS="randwrite-4k,randrw-r25-4k,randrw-r50-4k,randrw-r75-4k"

if ! grep -q 'const LATENCY_ENABLED: bool = false;' "$LATENCY_SOURCE"; then
    echo "error: LATENCY_ENABLED must be false before performance testing" >&2
    exit 2
fi

DRY_ARG=()
CONFIRM_ARG=(--confirm-destroy)
if [ "${1:-}" = "--dry-run" ]; then
    DRY_ARG=(--dry-run)
    CONFIRM_ARG=()
fi

if ! docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    echo "error: persistent build container $CONTAINER is not running" >&2
    exit 2
fi

if [ ${#DRY_ARG[@]} -eq 0 ]; then
    active_qemu="$(docker exec "$CONTAINER" ps -eo pid=,args= | \
        awk 'index($0, "qemu-system") && index($0, "/dev/nvme3n1p2") { print $1 }')"
    if [ -n "$active_qemu" ]; then
        echo "error: QEMU pid(s) $active_qemu already uses /dev/nvme3n1p2" >&2
        exit 2
    fi
fi

# The persistent container predates this harness and /root/impl is not a bind
# mount. Copy through /tmp and install atomically because docker cp does not
# reliably replace these pre-existing files in this container.
sync_harness_file() {
    local source="$1" destination="$2" temporary
    temporary="/tmp/$(basename "$destination").rnvpc-mixed"
    docker cp "$source" "$CONTAINER:$temporary"
    docker exec "$CONTAINER" install -m 755 "$temporary" "$destination"

    local host_sha container_sha
    host_sha="$(sha256sum "$source" | awk '{print $1}')"
    container_sha="$(docker exec "$CONTAINER" sha256sum "$destination" | awk '{print $1}')"
    if [ "$host_sha" != "$container_sha" ]; then
        echo "error: container harness checksum mismatch: $destination" >&2
        exit 2
    fi
}

docker exec "$CONTAINER" mkdir -p "$CONTAINER_TEST_DIR"
sync_harness_file "$HERE/aster_ext4_baseline.sh" \
    "$CONTAINER_TEST_DIR/aster_ext4_baseline.sh"
sync_harness_file "$HERE/run-baseline.sh" \
    "$CONTAINER_TEST_DIR/run-baseline.sh"
sync_harness_file "$HERE/parse_baseline.py" \
    "$CONTAINER_TEST_DIR/parse_baseline.py"
docker exec "$CONTAINER" grep -qF randrw-r25-4k \
    "$CONTAINER_TEST_DIR/aster_ext4_baseline.sh"

copy_results_back() {
    # The nested baseline runner bind-mounts the implementation tree, so normal
    # runs already leave results on the host. Copy only for legacy container-only
    # runs; this also avoids a redundant docker cp race at process teardown.
    if [ -f "$RESULT_DIR/logs/baseline_run.log" ]; then
        return
    fi
    if docker exec "$CONTAINER" test -d "$CONTAINER_RESULT_DIR"; then
        mkdir -p "$RESULT_DIR"
        docker cp "$CONTAINER:$CONTAINER_RESULT_DIR/." "$RESULT_DIR/"
    fi
}
trap copy_results_back EXIT

docker exec \
    -e NIX_CONFIG="substitute = false" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -w /root/asterinas "$CONTAINER" \
    bash "$CONTAINER_TEST_DIR/run-baseline.sh" \
    --devices nvme \
    --fs rnvpc_extend \
    --workloads "$WORKLOADS" \
    --modes orderless \
    --njobs 8 \
    --runtime 30 \
    --timeout 600 \
    --file-range-mb 256 \
    --wr-capacity-mb 128 \
    --rnvpc-kcmd "rnvpc_size=128M rnvpc_meta_reserve=32M rnvpc_data_limit=96M rnvpc_sim_mode=fast" \
    --smp 24 \
    --timestamp "$TIMESTAMP" \
    "${DRY_ARG[@]}" \
    "${CONFIRM_ARG[@]}"

if [ ${#DRY_ARG[@]} -eq 0 ]; then
    copy_results_back
    trap - EXIT
    python3 "$HERE/parse_mixed.py" \
        "$RESULT_DIR/logs/baseline_run.log" \
        "$RESULT_DIR/mixed-summary.csv"
    echo "mixed summary: $RESULT_DIR/mixed-summary.csv"
fi
