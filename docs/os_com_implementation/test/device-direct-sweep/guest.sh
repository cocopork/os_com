#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu
MNT=/rebuild-ext4
FIO=/benchmark/bin/fio
mkdir -p "$MNT"
mount -t ext4 -o delalloc,rnvpc=off,rnvpc_extend=off,rtio=off /dev/vdc "$MNT"
"$FIO" --version
cat /proc/mounts
# Each thread has a separate inode; round per-thread size down to whole MiB.
run_fio() {
    "$FIO" --name="$1" --rw="$2" --bs="$3" --numjobs="$4" \
        --directory="$MNT/data" --filename_format='f.$jobnum' \
        --size="$((2048 / $4))M" --filesize="$((2048 / $4))M" \
        --ioengine=psync --direct=1 --iodepth=1 --thread=1 \
        --cpus_allowed=0-31 --cpus_allowed_policy=split \
        --group_reporting=1 --fallocate=none --create_on_open=1 \
        --fsync=0 --randrepeat=1 --randseed=20261018 --runtime=600 \
        --output-format=json
}
for axis in threads bs; do
    if [ "$axis" = threads ]; then values='1 4 8 12 16 20 24 28 32'; else values='4 8 16 32 64 128 256 512 1024'; fi
    for value in $values; do
        if [ "$axis" = threads ]; then jobs=$value; bs=4k; else jobs=1; bs=${value}k; fi
        # For the bs sweep all sizes use the same fully initialized files.
        if [ "$axis" = threads ] || [ "$value" = 4 ]; then
            rm -rf "$MNT/data"
            mkdir "$MNT/data"
            echo "SWEEP_PREPARE_START $axis $value"
            run_fio prepare write 128k "$jobs"
            sync
            echo "SWEEP_PREPARE_END $axis $value"
        fi
        for rw in randwrite randread write read; do
            case_id="$axis-$value-$rw"
            echo "SWEEP_START $case_id"
            run_fio "$case_id" "$rw" "$bs" "$jobs"
            echo "SWEEP_END $case_id"
            sync
        done
    done
done
umount "$MNT"
echo SWEEP_COMPLETE
