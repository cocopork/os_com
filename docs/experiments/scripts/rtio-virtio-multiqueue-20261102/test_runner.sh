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
TOOLS_IMAGE=fixture-tools
root_fuser_status=0
root_fuser_marker=1
root_fuser_stderr=
docker() {
    case "$*" in
        *'run --rm --privileged --pid=host --userns=host --network none -v /:/host:ro'*'chroot /host /usr/bin/python3'*'root_fuser_check.py'*)
            : > "$ART/open-users-root.stdout"
            printf '%s' "$root_fuser_stderr" > "$ART/open-users-root.stderr"
            [[ $root_fuser_marker == 1 ]] && echo ROOT_FUSER_IDENTITY_VALIDATED
            return "$root_fuser_status" ;;
        *) return 125 ;;
    esac
}
declare -F authoritative_open_user_check >/dev/null || fail "missing authoritative open-user check"
expect_rejected authoritative_open_user_check /dev/nvme1n1p1 nvme1n1 259:1 259:0
root_fuser_status=1
authoritative_open_user_check /dev/nvme1n1p1 nvme1n1 259:1 259:0
root_fuser_stderr='lookup failed'
expect_rejected authoritative_open_user_check /dev/nvme1n1p1 nvme1n1 259:1 259:0
root_fuser_stderr=
root_fuser_marker=0
expect_rejected authoritative_open_user_check /dev/nvme1n1p1 nvme1n1 259:1 259:0
root_fuser_marker=1
root_fuser_status=125
expect_rejected authoritative_open_user_check /dev/nvme1n1p1 nvme1n1 259:1 259:0
sleep() { :; }

docker() {
    case "$*" in
        'logs trace') printf 'Attaching 2 probes...\nBEGIN\n@hctx[0]: 10\n@hctx[1]: 10\n@hctx[2]: 10\n@hctx[3]: 10\n' ;;
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
        'logs trace') printf 'Attaching 2 probes...\nREADY parent=nvme1n1\n' ;;
        *'inspect -f '*'.State.Running'*' trace') echo true ;;
        *) return 1 ;;
    esac
}
qemu_name=qemu
gate_guest_on_trace trace nvme1n1 /dev/nvme1n1p1
[[ $(< "$test_root/events") == *read*cont* ]] || fail "readiness did not precede guest resume"

readiness_reads=0
trigger_ready_read() { readiness_reads=$((readiness_reads + 1)); echo read >> "$test_root/events"; }
docker() {
    case "$*" in
        'logs trace')
            printf 'Attaching 2 probes...\n'
            if [[ $readiness_reads -ge 2 ]]; then printf 'READY parent=nvme1n1\n'; fi ;;
        *'inspect -f '*'.State.Running'*' trace') echo true ;;
        *) return 1 ;;
    esac
}
gate_guest_on_trace trace nvme1n1 /dev/nvme1n1p1
[[ $readiness_reads -eq 2 ]] || fail "delayed attachment did not retry the direct read"
python3 - "$ART/readiness-reads.json" <<'PY' || fail "read count evidence missing"
import json
import sys
assert json.load(open(sys.argv[1]))["read_count"] == 2
PY
cont_before=$(rg -c '^cont$' "$test_root/events")

readiness_reads=0
docker() {
    case "$*" in
        'logs trace') printf 'Attaching 2 probes...\n' ;;
        *'inspect -f '*'.State.Running'*' trace') echo true ;;
        *) return 1 ;;
    esac
}
expect_rejected gate_guest_on_trace trace nvme1n1 /dev/nvme1n1p1
[[ $readiness_reads -ge 2 && $readiness_reads -le 5 ]] || fail "readiness retry was not bounded"
[[ $(rg -c '^cont$' "$test_root/events") == "$cont_before" ]] || fail "resumed after readiness timeout"

readiness_reads=0
docker() {
    case "$*" in
        'logs trace') printf 'Attaching 2 probes...\n' ;;
        *'inspect -f '*'.State.Running'*' trace')
            [[ $readiness_reads -eq 0 ]] && echo true || echo false ;;
        *) return 1 ;;
    esac
}
expect_rejected gate_guest_on_trace trace nvme1n1 /dev/nvme1n1p1
[[ $(rg -c '^cont$' "$test_root/events") == "$cont_before" ]] || fail "resumed after tracer early exit"

previous_art=$ART
ART="$test_root/evidence-write-failure"
mkdir "$ART" "$ART/readiness-reads.json"
docker() {
    case "$*" in
        'logs trace') printf 'READY parent=nvme1n1\n' ;;
        *'inspect -f '*'.State.Running'*' trace') echo true ;;
        *) return 1 ;;
    esac
}
expect_rejected gate_guest_on_trace trace nvme1n1 /dev/nvme1n1p1
[[ $(rg -c '^cont$' "$test_root/events") == "$cont_before" ]] || fail "resumed without readiness-read evidence"
ART=$previous_art

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
    qemu_created=1
    trace_created=1
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

(
    ART="$test_root/capture-failure"
    mkdir "$ART"
    qemu_name=qemu
    trace_name=trace
    qemu_finalized=0
    trace_finalized=0
    qemu_created=1
    trace_created=1
    docker() {
        case "$1" in
            inspect)
                if [[ ${2:-} == -f ]]; then echo false; else echo '{"State":{"ExitCode":1}}'; fi ;;
            logs)
                if [[ $2 == trace ]]; then echo 'partial trace'; return 7; fi
                echo 'qemu log' ;;
            wait) echo 1 ;;
            rm) echo "$2" >> "$ART/removed" ;;
            *) return 1 ;;
        esac
    }
    cleanup
) 2> "$test_root/capture-failure.stderr" && fail "cleanup accepted missing trace logs"
[[ ! -e "$test_root/capture-failure/removed" || $(< "$test_root/capture-failure/removed") != *trace* ]] || fail "removed tracer after failed capture"
rg -q 'trace' "$test_root/capture-failure.stderr" || fail "missing retained-container recovery diagnostics"

(
    ART="$test_root/inspect-failure"
    mkdir "$ART"
    qemu_name=qemu
    trace_name=trace
    qemu_finalized=0
    trace_finalized=0
    qemu_created=1
    trace_created=1
    docker() {
        case "$1" in
            inspect) return 1 ;;
            rm) echo "$2" >> "$ART/removed" ;;
            *) return 1 ;;
        esac
    }
    cleanup
) 2> "$test_root/inspect-failure.stderr" && fail "cleanup accepted inaccessible container state"
[[ ! -e "$test_root/inspect-failure/removed" ]] || fail "removed container without inspect"
rg -q 'trace' "$test_root/inspect-failure.stderr" || fail "missing inspect-failure recovery diagnostics"
echo "shell orchestration fixtures passed"
