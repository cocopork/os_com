#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
authorize_target() {
    case ${1:-} in
        --authorized-destructive-target=/dev/nvme1n1p1|\
        --authorized-destructive-target=/dev/nvme1n1p2) TARGET=${1#*=} ;;
        *) echo "pass --authorized-destructive-target=/dev/nvme1n1p[12] for a named 256 GiB test partition" >&2; return 2 ;;
    esac
}

create_artifact() {
    local requested=$1 parent
    requested=$(realpath -m -- "$requested")
    parent=$(dirname -- "$requested")
    [[ -d $parent && -O $parent && -w $parent ]] || {
        echo "artifact parent must exist and be owned by the caller: $parent" >&2
        return 2
    }
    (umask 077; mkdir -- "$requested") || {
        echo "refusing existing or concurrent artifact directory: $requested" >&2
        return 2
    }
    ART=$requested
}

docker_local_preflight() {
    local endpoint context daemon_id
    if [[ -n ${DOCKER_CONTEXT:-} ]]; then
        endpoint=$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}')
    elif [[ -n ${DOCKER_HOST:-} ]]; then
        endpoint=$DOCKER_HOST
    else
        context=$(docker context show)
        endpoint=$(docker context inspect "$context" --format '{{.Endpoints.docker.Host}}')
    fi
    [[ $endpoint == unix:///* && -S ${endpoint#unix://} ]] || {
        echo "Docker endpoint must be a local Unix socket: $endpoint" >&2
        return 2
    }
    daemon_id=$(docker info --format '{{.ID}}')
    [[ -n $daemon_id ]] || { echo "Docker daemon identity unavailable" >&2; return 2; }
    printf '%s %s\n' "$endpoint" "$daemon_id"
}

confirm_docker_identity() {
    [[ $(docker_local_preflight) == "$docker_identity" ]] || {
        echo "Docker daemon changed before device exposure" >&2
        return 2
    }
}

authoritative_open_user_check() {
    local target=$1 parent=$2 target_id=$3 parent_id=$4 status
    confirm_docker_identity || return 2
    if docker run --rm --privileged --pid=host --userns=host --network none -v /:/host:ro "$TOOLS_IMAGE" \
        chroot /host /usr/bin/python3 "$ART/root_fuser_check.py" check "$target" "/dev/$parent" \
        "$target_id" "$parent_id" \
        > "$ART/open-users-root.json" 2> "$ART/open-users-root.stderr"; then
        status=0
    else
        status=$?
    fi
    printf '%s\n' "$status" > "$ART/open-users-root.status" || return 2
    python3 "$SCRIPT_DIR/root_fuser_check.py" validate-result \
        "$ART/open-users-root.json" "$ART/open-users-root.stderr" "$status" \
        "$target" "/dev/$parent" "$target_id" "$parent_id"
}

trigger_ready_read() {
    local target=$1 timeout_seconds=${2:-15}
    confirm_docker_identity || return 2
    timeout "$timeout_seconds" docker run --rm --network none --device "$target:/dev/readiness:r" \
        "$TOOLS_IMAGE" dd if=/dev/readiness of=/dev/null bs=4096 count=1 iflag=direct status=none
}

wait_trace_ready() {
    local trace_name=$1 parent=$2 target=$3 attempt=0 read_count=0 remaining status
    local started=$SECONDS deadline=$((SECONDS + 20))
    printf '{"parent":"%s","read_count":0,"read_attempts":0,"bytes_per_read":4096}\n' "$parent" > "$ART/readiness-reads.json" || return 2
    : > "$ART/readiness-read-attempts.jsonl" || return 2
    while ((SECONDS < deadline)); do
        [[ $(docker inspect -f '{{.State.Running}}' "$trace_name") == true ]] || return 2
        docker logs "$trace_name" > "$ART/trace.startup.log" 2>&1 || return 2
        remaining=$((deadline - SECONDS))
        ((remaining > 0)) || break
        if rg -Fxq "READY parent=$parent" "$ART/trace.startup.log"; then
            [[ $(docker inspect -f '{{.State.Running}}' "$trace_name") == true ]] || return 2
            return 0
        fi
        attempt=$((attempt + 1))
        printf '{"parent":"%s","read_count":%d,"read_attempts":%d,"bytes_per_read":4096}\n' "$parent" "$read_count" "$attempt" > "$ART/readiness-reads.json" || return 2
        printf '{"attempt":%d,"event":"started","elapsed_seconds":%d,"timeout_seconds":%d}\n' "$attempt" "$((SECONDS - started))" "$remaining" >> "$ART/readiness-read-attempts.jsonl" || return 2
        status=0
        trigger_ready_read "$target" "$remaining" || status=$?
        printf '{"attempt":%d,"event":"finished","elapsed_seconds":%d,"status":%d}\n' "$attempt" "$((SECONDS - started))" "$status" >> "$ART/readiness-read-attempts.jsonl" || return 2
        ((status == 0)) || return 2
        read_count=$((read_count + 1))
        printf '{"parent":"%s","read_count":%d,"read_attempts":%d,"bytes_per_read":4096}\n' "$parent" "$read_count" "$attempt" > "$ART/readiness-reads.json" || return 2
        # A read in the final second can emit READY without another loop iteration.
        docker logs "$trace_name" > "$ART/trace.startup.log" 2>&1 || return 2
        if rg -Fxq "READY parent=$parent" "$ART/trace.startup.log"; then
            [[ $(docker inspect -f '{{.State.Running}}' "$trace_name") == true ]] || return 2
            return 0
        fi
        ((SECONDS < deadline)) || break
        sleep 1
    done
    return 2
}

resume_guest() {
    docker exec "$1" python3 /benchmark/summarize.py qmp-cont /benchmark/qmp.sock > "$ART/qmp-cont.json"
}

gate_guest_on_trace() {
    wait_trace_ready "$1" "$2" "$3" || return 2
    resume_guest "$qemu_name"
}

monitor_guest_trace() {
    local qemu_name=$1 trace_name=$2
    while true; do
        [[ $(docker inspect -f '{{.State.Running}}' "$trace_name") == true ]] || return 2
        [[ $(docker inspect -f '{{.State.Running}}' "$qemu_name") == true ]] || return 0
        sleep 1
    done
}

require_complete_trace() {
    local status_file=$1 raw_file=$2 counts_file=$3
    [[ $(< "$status_file") == 0 ]] || {
        echo "NVMe tracer did not exit cleanly after SIGINT" >&2
        return 2
    }
    rg -Fxq 'TRACE_FINAL_BEGIN' "$raw_file" || {
        echo "NVMe tracer did not print final map boundary" >&2
        return 2
    }
    printf 'TRACE_FINAL_END\n' >> "$raw_file"
    python3 "$SCRIPT_DIR/summarize.py" trace-counts "$raw_file" > "$counts_file"
}

capture_and_remove() {
    local name=$1 log_file=$2 inspect_file=$3 wait_file=$4 finalized=$5
    local log_status=0 inspect_status=0 wait_status=0
    local running
    running=$(docker inspect -f '{{.State.Running}}' "$name") || running=unknown
    if [[ $running != false ]]; then
        echo "retaining container $name: state is $running; stop it and recover with docker logs/inspect/wait before docker rm" >&2
        return 2
    fi
    if [[ $finalized -eq 0 ]]; then
        docker logs "$name" > "$log_file" 2>&1 || log_status=$?
        docker inspect "$name" > "$inspect_file" 2>&1 || inspect_status=$?
        docker wait "$name" > "$wait_file" 2>&1 || wait_status=$?
    fi
    if [[ $log_status -ne 0 || $inspect_status -ne 0 || $wait_status -ne 0 || ! -f $log_file || ! -s $inspect_file || ! -s $wait_file ]]; then
        echo "retaining stopped container $name: evidence capture failed; recover with docker logs $name, docker inspect $name, and docker wait $name before docker rm $name" >&2
        return 2
    fi
    docker rm "$name" >/dev/null 2>&1 || {
        echo "retaining stopped container $name: docker rm failed; inspect it manually" >&2
        return 2
    }
}

cleanup() {
    local result=$?
    trap - EXIT
    set +e
    if [[ $qemu_created -eq 1 ]]; then
        if docker inspect "$qemu_name" >/dev/null 2>&1; then
            if [[ $(docker inspect -f '{{.State.Running}}' "$qemu_name") == true ]]; then
                docker stop --signal=TERM "$qemu_name" >/dev/null 2>&1 || docker kill "$qemu_name" >/dev/null 2>&1
            fi
        else
            echo "cannot inspect $qemu_name; retain it if present and recover with docker logs/inspect/wait before docker rm" >&2
            result=2
        fi
    fi
    if [[ $trace_created -eq 1 ]]; then
        if docker inspect "$trace_name" >/dev/null 2>&1; then
            if [[ $(docker inspect -f '{{.State.Running}}' "$trace_name") == true ]]; then
                docker stop --signal=INT "$trace_name" >/dev/null 2>&1 || docker kill "$trace_name" >/dev/null 2>&1
            fi
        else
            echo "cannot inspect $trace_name; retain it if present and recover with docker logs/inspect/wait before docker rm" >&2
            result=2
        fi
    fi
    if [[ $trace_created -eq 1 ]] && docker inspect "$trace_name" >/dev/null 2>&1; then
        capture_and_remove "$trace_name" "$ART/trace.raw" "$ART/trace.inspect.json" "$ART/trace.wait-status.txt" "$trace_finalized" || result=2
    fi
    if [[ $qemu_created -eq 1 ]] && docker inspect "$qemu_name" >/dev/null 2>&1; then
        capture_and_remove "$qemu_name" "$ART/qemu.log" "$ART/qemu.inspect.json" "$ART/qemu-exit.txt" "$qemu_finalized" || result=2
    fi
    exit "$result"
}

main() {
if [[ ${1:-} == --self-test && $# -eq 1 ]]; then
    python3 -m unittest discover -s "$SCRIPT_DIR" -p 'test_*.py' -v
    bash "$SCRIPT_DIR/test_runner.sh"
    return
fi
authorize_target "${1:-}" || return 2
[[ $# -eq 1 ]] || { echo "unexpected arguments" >&2; return 2; }
local requested_art=${ART:?set ART to a new external artifact directory}
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to a directory containing selected.iso, ext2.img and exfat.img}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
QEMU_BIN=${QEMU_BIN:-qemu-system-x86_64}
QUEUES=4
for file in selected.iso ext2.img exfat.img; do
    [[ -f "$SOURCE_ART/$file" ]] || { echo "missing input: $SOURCE_ART/$file" >&2; exit 2; }
done

target_json=$(python3 "$SCRIPT_DIR/summarize.py" target-preflight "$TARGET")
parent=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["parent"])' <<< "$target_json")
target_id=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["target_major_minor"])' <<< "$target_json")
parent_id=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["parent_major_minor"])' <<< "$target_json")
[[ -d "/sys/block/$parent/mq" ]] || { echo "missing NVMe mq sysfs map" >&2; exit 2; }
allowed_cpus=$(taskset -pc $$ | sed 's/.*: //')

create_artifact "$requested_art"
printf '%s\n' "$target_json" > "$ART/target.json"
for file in selected.iso ext2.img exfat.img; do
    cp --reflink=auto -- "$SOURCE_ART/$file" "$ART/$file"
done
cp -- "$SCRIPT_DIR/summarize.py" "$ART/summarize.py"
cp -- "$SCRIPT_DIR/root_fuser_check.py" "$ART/root_fuser_check.py"
sha256sum "$ART"/{selected.iso,ext2.img,exfat.img} > "$ART/input.sha256"
docker_identity=$(docker_local_preflight)
printf '%s\n' "$docker_identity" > "$ART/docker-daemon.txt"
docker image inspect "$TOOLS_IMAGE" --format '{{.Id}}' > "$ART/tools-image-id.txt"
docker run --rm --network none "$TOOLS_IMAGE" "$QEMU_BIN" -version > "$ART/qemu-version.txt"
docker run --rm --network none "$TOOLS_IMAGE" "$QEMU_BIN" -device virtio-blk-pci,help > "$ART/qemu-device-help.txt"
python3 "$SCRIPT_DIR/summarize.py" qemu-preflight-files "$ART/qemu-version.txt" "$ART/qemu-device-help.txt" > "$ART/qemu-preflight.json"
"$SCRIPT_DIR/select-hctx-cpus.sh" "/sys/block/$parent/mq" "$QUEUES" "$allowed_cpus" > "$ART/placements.json"
cp -a -- "/sys/block/$parent/mq" "$ART/sysfs-mq"
python3 "$SCRIPT_DIR/summarize.py" qemu-mapping "$QUEUES" > "$ART/qemu-vq-mapping.json"
sed "s/__TARGET_DISK__/$parent/g" "$SCRIPT_DIR/trace-nvme-hctx.bt" > "$ART/trace-nvme-hctx.bt"
uname -a > "$ART/kernel-version.txt"

qemu_name="rtio-mq-qemu-$$"
trace_name="rtio-mq-trace-$$"
qemu_finalized=0
trace_finalized=0
qemu_created=0
trace_created=0
trap cleanup EXIT

confirm_docker_identity || return 2
authoritative_open_user_check "$TARGET" "$parent" "$target_id" "$parent_id" || return 2
confirm_docker_identity || return 2
qemu_created=1
docker run -d --name "$qemu_name" --pid=host --network none \
    --device /dev/kvm --device "$TARGET:/dev/benchmark-target:rwm" \
    -v "$ART:/benchmark" -w /benchmark "$TOOLS_IMAGE" \
    "$QEMU_BIN" -enable-kvm -cpu host -machine q35 -smp 64 -m 8G \
    -S -qmp unix:/benchmark/qmp.sock,server=on,wait=off \
    -bios /root/ovmf/release/OVMF.fd -no-reboot -display none -serial stdio \
    -monitor none -nic none -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -cdrom /benchmark/selected.iso \
    -drive if=none,format=raw,id=bootdata,file=/benchmark/ext2.img \
    -device virtio-blk-pci,drive=bootdata,disable-legacy=on \
    -drive if=none,format=raw,id=aux,file=/benchmark/exfat.img \
    -device virtio-blk-pci,drive=aux,disable-legacy=on \
    -drive if=none,format=raw,id=target,file=/dev/benchmark-target,cache=none,aio=threads \
    -device pcie-root-port,id=target-port,chassis=1,slot=1 \
    -object iothread,id=iothread0 -object iothread,id=iothread1 \
    -object iothread,id=iothread2 -object iothread,id=iothread3 \
    > "$ART/qemu-container-id.txt"

docker exec "$qemu_name" python3 /benchmark/summarize.py qmp-add-device /benchmark/qmp.sock "$QUEUES" > "$ART/qmp-device-add.json"
docker exec "$qemu_name" python3 /benchmark/summarize.py qmp-query /benchmark/qmp.sock > "$ART/qmp-iothreads.json"
python3 "$SCRIPT_DIR/summarize.py" mapping-record "$ART/placements.json" "$ART/qmp-iothreads.json" > "$ART/mapping.json"
python3 - "$ART/mapping.json" "$TOOLS_IMAGE" <<'PY' > "$ART/affinity.log"
import json
import subprocess
import sys

for row in json.load(open(sys.argv[1])):
    command = ["docker", "run", "--rm", "--privileged", "--pid=host", "--network", "none",
               sys.argv[2], "taskset", "-pc",
               str(row["host_cpu"]), str(row["iothread_tid"])]
    print(subprocess.check_output(command, text=True).strip(), flush=True)
PY

# Compilation checks the exact probe and typed BTF fields before guest I/O begins.
docker run --rm --privileged --pid=host --network none -v /:/host:ro "$TOOLS_IMAGE" \
    chroot /host /usr/bin/bpftrace -d "$ART/trace-nvme-hctx.bt" \
    > "$ART/probe-preflight.log" 2>&1 || { echo "typed NVMe probe preflight failed" >&2; exit 2; }
docker run --rm --privileged --pid=host --network none -v /:/host:ro "$TOOLS_IMAGE" \
    chroot /host /usr/bin/bpftrace -l kprobe:nvme_queue_rq \
    > "$ART/probe-symbols.txt" 2>&1
rg -Fxq 'kprobe:nvme_queue_rq' "$ART/probe-symbols.txt" || {
    echo "nvme_queue_rq probe symbol unavailable" >&2
    exit 2
}
trace_created=1
docker run -d --name "$trace_name" --privileged --pid=host --network none \
    -v /:/host:ro "$TOOLS_IMAGE" \
    chroot /host /usr/bin/bpftrace "$ART/trace-nvme-hctx.bt" > "$ART/trace-container-id.txt"
gate_guest_on_trace "$trace_name" "$parent" "$TARGET" || {
    echo "NVMe trace did not observe the readiness read; guest remains paused" >&2
    return 2
}

monitor_guest_trace "$qemu_name" "$trace_name" || {
    echo "NVMe tracer exited before guest completion" >&2
    return 2
}
docker wait "$qemu_name" > "$ART/qemu-exit.txt"
docker logs "$qemu_name" > "$ART/qemu.log" 2>&1
docker inspect "$qemu_name" > "$ART/qemu.inspect.json"
qemu_finalized=1
[[ $(docker inspect -f '{{.State.Running}}' "$trace_name") == true ]] || {
    echo "NVMe tracer exited before controlled stop" >&2
    return 2
}
docker stop --signal=INT "$trace_name" >/dev/null
docker wait "$trace_name" > "$ART/trace.wait-status.txt"
docker logs "$trace_name" > "$ART/trace.raw" 2>&1
docker inspect "$trace_name" > "$ART/trace.inspect.json"
trace_finalized=1
qemu_status=$(< "$ART/qemu-exit.txt")
[[ $qemu_status == 0 || $qemu_status == 33 ]] || {
    echo "QEMU exited with status $qemu_status" >&2
    return 2
}
require_complete_trace "$ART/trace.wait-status.txt" "$ART/trace.raw" "$ART/hctx-counts.json"
python3 - "$ART/hctx-counts.json" <<'PY'
import json
import sys
result = json.load(open(sys.argv[1]))
if not result["accepted"]:
    raise SystemExit("multi-hctx acceptance failed: see hctx-counts.json")
PY
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
