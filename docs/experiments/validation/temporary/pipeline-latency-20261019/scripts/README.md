# EXT4 and rNVPCe write-pipeline latency experiment

This experiment compares the same three stages on native EXT4 and
EXT4-rNVPCe: foreground buffered writes, background flush, and completion.
It uses the authorized `/dev/nvme1n1p1` partition, 64 MiB data limits, 2 GiB
workloads, one virtio-blk queue with 1024 descriptors, and the existing CPU
isolation harness.

Set `pipeline_profile::ENABLED` only in the diagnostic build. Build the
benchmark initramfs first, then use the same automatic guest entry point and
kernel configuration as the historical buffered-write experiments. CPU
isolation remains enabled; its serial trace is omitted because it can corrupt
fio's JSON stream.

```sh
sg docker -c 'docker exec -e BENCHMARK=fio -e SMP=64 \
  -w /root/asterinas/test/initramfs os-com-rnvpc-rebuild make build'
sg docker -c 'docker exec -e SMP=64 -e MEM=8G -e CONSOLE=ttyS0 \
  -w /root/asterinas/kernel os-com-rnvpc-rebuild cargo osdk build --release \
  --init-args="/ext2/rebuild-fio.sh" \
  --kcmd-args="console=ttyS0 ostd.log_level=error rnvpc=off \
  rnvpc_extend=on rtio=on rnvpc_size=128M rnvpc_meta_reserve=64M \
  rnvpc_data_limit=64M rnvpc_sim_mode=fast rtio_cpu_isolation=on"'
```

Place the ISO as `selected.iso` beside the harness `ext2.img` and `exfat.img`,
then run:

```sh
./run-suite.sh /home/zn/asterinas-build-env/pipeline-latency-20261019
```

`pipeline-metrics.csv` retains count, total, maximum, and histogram-derived
percentiles. `pipeline-stages.csv` provides the stage comparison. Stage totals
can overlap across threads and requests, so they are accumulated CPU/wait time
and must not be added to estimate wall-clock runtime.
