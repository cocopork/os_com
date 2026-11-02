#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if [[ ${1:-} == --self-test && $# -eq 1 ]]; then
    exec python3 -m unittest discover -s "$SCRIPT_DIR" -p test_harness.py -v
fi

case ${1:-} in
    --authorized-destructive-target=/dev/nvme1n1p1|\
    --authorized-destructive-target=/dev/nvme1n1p2|\
    --authorized-destructive-target=/dev/nvme1n1p3) TARGET=${1#*=} ;;
    *) echo "pass --authorized-destructive-target=/dev/nvme1n1p[123] for a named 256 GiB test partition" >&2; exit 2 ;;
esac
[[ $# -eq 1 ]] || { echo "unexpected arguments" >&2; exit 2; }
ART=${ART:?set ART to a new external artifact directory}
[[ ! -e "$ART" ]] || { echo "refusing to reuse artifact directory: $ART" >&2; exit 2; }
SOURCE_ART=${SOURCE_ART:?set SOURCE_ART to a directory containing selected.iso, ext2.img and exfat.img}
TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}
QEMU_BIN=${QEMU_BIN:-qemu-system-x86_64}
QUEUES=4
for file in selected.iso ext2.img exfat.img; do
    [[ -f "$SOURCE_ART/$file" ]] || { echo "missing input: $SOURCE_ART/$file" >&2; exit 2; }
done

target_json=$(python3 "$SCRIPT_DIR/summarize.py" target-preflight "$TARGET")
parent=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["parent"])' <<< "$target_json")
[[ -d "/sys/block/$parent/mq" ]] || { echo "missing NVMe mq sysfs map" >&2; exit 2; }
allowed_cpus=$(taskset -pc $$ | sed 's/.*: //')

mkdir -p "$ART"
printf '%s\n' "$target_json" > "$ART/target.json"
for file in selected.iso ext2.img exfat.img; do
    cp --reflink=auto -- "$SOURCE_ART/$file" "$ART/$file"
done
cp -- "$SCRIPT_DIR/summarize.py" "$ART/summarize.py"
sha256sum "$ART"/{selected.iso,ext2.img,exfat.img} > "$ART/input.sha256"
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
cleanup() {
    docker rm -f "$qemu_name" "$trace_name" >/dev/null 2>&1 || true
}
trap cleanup EXIT

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
docker run -d --name "$trace_name" --privileged --pid=host --network none \
    -v /:/host:ro "$TOOLS_IMAGE" \
    chroot /host /usr/bin/bpftrace "$ART/trace-nvme-hctx.bt" > "$ART/trace-container-id.txt"
trace_ready=0
for _ in {1..50}; do
    if docker logs "$trace_name" 2>&1 | rg -q '^Attaching 1 probe'; then
        trace_ready=1
        break
    fi
    [[ $(docker inspect -f '{{.State.Running}}' "$trace_name") == true ]] || break
    sleep 0.1
done
[[ $trace_ready -eq 1 ]] || {
    docker logs "$trace_name" > "$ART/trace.raw" 2>&1 || true
    echo "NVMe trace did not attach" >&2
    exit 2
}
docker exec "$qemu_name" python3 /benchmark/summarize.py qmp-cont /benchmark/qmp.sock > "$ART/qmp-cont.json"

docker wait "$qemu_name" > "$ART/qemu-exit.txt"
docker logs "$qemu_name" > "$ART/qemu.log" 2>&1
docker stop --signal=INT "$trace_name" >/dev/null || true
docker logs "$trace_name" > "$ART/trace.raw" 2>&1
qemu_status=$(< "$ART/qemu-exit.txt")
[[ $qemu_status == 0 || $qemu_status == 33 ]] || {
    echo "QEMU exited with status $qemu_status" >&2
    exit 2
}
python3 "$SCRIPT_DIR/summarize.py" trace-counts "$ART/trace.raw" > "$ART/hctx-counts.json"
python3 - "$ART/hctx-counts.json" <<'PY'
import json
import sys
result = json.load(open(sys.argv[1]))
if not result["accepted"]:
    raise SystemExit("multi-hctx acceptance failed: see hctx-counts.json")
PY
