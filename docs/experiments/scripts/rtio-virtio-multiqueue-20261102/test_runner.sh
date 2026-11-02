#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$SCRIPT_DIR/run-validation.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
expect_rejected() {
    if "$@"; then
        fail "accepted: $*"
    fi
}

expect_rejected authorize_target --authorized-destructive-target=/dev/nvme1n1p3
expect_rejected authorize_target --authorized-destructive-target=/dev/nvme0n1p1
expect_rejected authorize_target --authorized-destructive-target=/dev/../dev/nvme1n1p1
authorize_target --authorized-destructive-target=/dev/nvme1n1p1
[[ $TARGET == /dev/nvme1n1p1 ]] || fail "p1 authorization"
authorize_target --authorized-destructive-target=/dev/nvme1n1p2
[[ $TARGET == /dev/nvme1n1p2 ]] || fail "p2 authorization"

test_root=$(mktemp -d)
trap 'rm -rf "$test_root"' EXIT
mkdir "$test_root/evidence"
expect_rejected create_artifact "$test_root/evidence"
[[ -d $test_root/evidence ]] || fail "collision removed existing evidence"
create_artifact "$test_root/new/../fresh"
[[ $ART == "$test_root/fresh" && -d $ART ]] || fail "artifact path not canonical"

docker() {
    case "$*" in
        'context show') echo remote ;;
        *'context inspect remote'*) echo ssh://benchmark-host ;;
        *'info --format'*) echo daemon-A ;;
        *) return 1 ;;
    esac
}
expect_rejected docker_local_preflight
DOCKER_HOST=tcp://benchmark-host:2375
expect_rejected docker_local_preflight
unset DOCKER_HOST
DOCKER_CONTEXT=remote
expect_rejected docker_local_preflight
unset DOCKER_CONTEXT
docker_identity='unix:///tmp/fake.sock daemon-A'
docker_local_preflight() { echo 'unix:///tmp/fake.sock daemon-B'; }
expect_rejected confirm_docker_identity
docker_local_preflight() { echo "$docker_identity"; }
confirm_docker_identity

docker() {
    case "$*" in
        'logs trace') printf 'Attaching 1 probe...\nBEGIN\n@hctx[0]: 10\n@hctx[1]: 10\n@hctx[2]: 10\n@hctx[3]: 10\n' ;;
        *'inspect -f '*'.State.Running'*' trace') echo false ;;
        *) return 1 ;;
    esac
}
trigger_ready_read() { echo read >> "$test_root/events"; }
resume_guest() { echo cont >> "$test_root/events"; }
expect_rejected gate_guest_on_trace trace nvme1n1 /dev/nvme1n1p1
[[ ! -e $test_root/events || $(< "$test_root/events") != *cont* ]] || fail "resumed after tracer banner"

docker() {
    case "$*" in
        'logs trace') printf 'Attaching 1 probe...\nREADY parent=nvme1n1\n' ;;
        *'inspect -f '*'.State.Running'*' trace') echo true ;;
        *) return 1 ;;
    esac
}
qemu_name=qemu
gate_guest_on_trace trace nvme1n1 /dev/nvme1n1p1
[[ $(< "$test_root/events") == *read*cont* ]] || fail "readiness did not precede guest resume"

docker() {
    case "$*" in
        *'inspect -f '*'.State.Running'*' qemu') echo true ;;
        *'inspect -f '*'.State.Running'*' trace') echo false ;;
        *) return 1 ;;
    esac
}
expect_rejected monitor_guest_trace qemu trace
printf 'Attaching 1 probe...\nREADY parent=nvme1n1\n@hctx[0]: 10\n@hctx[1]: 10\n@hctx[2]: 10\n@hctx[3]: 10\n' > "$test_root/partial.raw"
expect_rejected python3 "$SCRIPT_DIR/summarize.py" trace-counts "$test_root/partial.raw"
printf '130\n' > "$test_root/trace.status"
printf 'READY parent=nvme1n1\nTRACE_FINAL_BEGIN\n@hctx[0]: 10\n@hctx[1]: 10\n@hctx[2]: 10\n@hctx[3]: 10\n' > "$test_root/stopped.raw"
declare -F require_complete_trace >/dev/null || fail "missing trace finalization gate"
expect_rejected require_complete_trace "$test_root/trace.status" "$test_root/stopped.raw" "$test_root/counts.json"
[[ ! -e $test_root/counts.json ]] || fail "accepted nonzero tracer exit"
printf '0\n' > "$test_root/trace.status"
require_complete_trace "$test_root/trace.status" "$test_root/stopped.raw" "$test_root/counts.json"
python3 - "$test_root/counts.json" <<'PY' || fail "complete trace was not accepted"
import json
import sys
assert json.load(open(sys.argv[1]))["accepted"]
PY

(
    ART="$test_root/capture"
    mkdir "$ART"
    qemu_name=qemu
    trace_name=trace
    qemu_finalized=0
    trace_finalized=0
    docker() {
        case "$1" in
            inspect)
                if [[ ${2:-} == -f ]]; then echo false; else echo '{"State":{"ExitCode":1}}'; fi ;;
            logs) echo "log from $2" ;;
            wait) echo 1 ;;
            rm)
                [[ -s "$ART/trace.raw" && -s "$ART/trace.inspect.json" && -s "$ART/trace.wait-status.txt" ]] || fail "removed tracer before evidence"
                [[ -s "$ART/qemu.log" && -s "$ART/qemu.inspect.json" && -s "$ART/qemu-exit.txt" ]] || [[ $2 == trace ]] || fail "removed QEMU before evidence" ;;
            *) return 1 ;;
        esac
    }
    cleanup
)
[[ -s "$test_root/capture/trace.raw" && -s "$test_root/capture/qemu.log" ]] || fail "failure evidence missing"
echo "shell orchestration fixtures passed"
