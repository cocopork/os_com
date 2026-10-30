#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

ROOT=${ROOT:-/mnt/data/zn/Rust_EXT4/os_com_rnvpc_rebuild}
ART=${ART:?set ART to an external artifact directory containing ext2.img and exfat.img}
LOG_LEVEL=${LOG_LEVEL:-error}
mkdir -p "$ART"

docker exec \
    -e CARGO_NET_OFFLINE=true \
    -e CARGO_BUILD_JOBS=16 \
    -e SMP=64 \
    -e MEM=8G \
    -e CONSOLE=ttyS0 \
    -w /root/asterinas/kernel \
    os-com-rnvpc-rebuild \
    cargo osdk build --release \
    --init-args="/ext2/rebuild-fio.sh" \
    --kcmd-args="console=ttyS0 ostd.log_level=$LOG_LEVEL rnvpc=off rnvpc_extend=off rtio=off rnvpc_size=128M rnvpc_meta_reserve=64M rnvpc_data_limit=64M rnvpc_sim_mode=fast rtio_cpu_isolation=on rtio_cpu_trace=on"

cp "$ROOT/target/osdk/aster-kernel-osdk-bin.iso" "$ART/selected.iso"
cp "$ROOT/target/x86_64-unknown-none/release/aster-kernel-osdk-bin" "$ART/current.elf"
sha256sum "$ART/selected.iso" "$ART/current.elf" > "$ART/SHA256SUMS"
