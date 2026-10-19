# NVLog Fsync Baseline Design

## Goal

Run NVLog against the `aster-ext4-baseline-test` fsync matrix on the two
dedicated test partitions and emit results compatible with the existing
baseline analysis pipeline.

## Scope

The matrix contains 36 points:

- devices: SATA `/dev/sda4` and NVMe `/dev/nvme1n1p2`;
- workloads: `randwrite-4k` and `seqwrite-4k`;
- mode: `fsync` only (`direct=0`, `fsync=1`);
- jobs: `1,4,8,12,16,20,24,28,32`;
- one measured run per point.

Each point uses the existing baseline fio settings: `psync`, `iodepth=1`, one
file per job, `fallocate=none`, a 2 GiB aggregate file range, and a 30-second
runtime without `time_based`. The VM uses 64 vCPUs and 4 GiB RAM. The NVLog
effective pool is 256 MiB (`65536` 4 KiB pages), matching the baseline dirty
writeback capacity.

## Architecture

A dedicated host controller under `test/aster-ext4-baseline-test` owns matrix
expansion, device validation, artifact staging, serial capture, result parsing,
and summary generation. It reuses the already validated NVLog kernel,
initramfs, module metadata generation, and NVDIMM setup from
`research1/evaluation/comparision-objs/NVLog` without changing that framework's
device allowlist or application workloads.

A container-side launcher creates one 512 MiB PMem backing file per point,
injects the guest script into a run-local initramfs, verifies the block-device
major/minor identity, and starts QEMU with the selected physical partition as a
single-queue virtio-blk device. A guest-side script initializes NVLog, formats
and mounts EXT4, opens NVLog, runs exactly one fio point, drains with `sync`,
closes NVLog, unmounts, and powers off.

Every test point boots a fresh VM and reformats the selected test partition.
Points run strictly serially.

## Safety

Real execution requires `--confirm-destroy`. The controller accepts only
`/dev/sda4` and `/dev/nvme1n1p2`, rejects mounted devices, snapshots each
device's major/minor number, and passes that identity to the container. The
container resolves the same identity through `/sys/dev/block` before QEMU can
start. No wildcard device path or caller-provided alternate device is allowed.

The controller refuses to start when another live QEMU command line already
references either test partition. Cleanup is run-ID scoped and removes only
the corresponding temporary initramfs, PMem file, and QEMU process.

## Timing And Validity

Guest markers delimit fio JSON and record monotonic start, fio completion, and
post-`sync` completion times. The parser reports:

- `write_mib` from fio bytes;
- `busy_ms` from fio start through fio completion;
- `drain_ms` from fio start through final sync;
- `busy_bw_mib_s` and `drain_bw_mib_s`;
- write IOPS;
- `valid=1` only when fio reports no error, at least 10 MiB was written, the
  drain marker exists, QEMU exits successfully, and the serial log contains no
  panic, oops, BUG, soft lockup, or RCU stall signature.

Each point has a 180-second wall-clock limit. A timeout or invalid point is
recorded and the matrix continues.

## Outputs

Results are written to
`test/aster-ext4-baseline-test/results/<timestamp>_nvlog-fsync-baseline/`:

- `parameters.json`: complete matrix and runtime configuration;
- `summary.csv`: the existing baseline column schema, with `fs=nvlog` and
  `mode=fsync`;
- `result_table.txt`: readable per-point status;
- `logs/serial-<device>-<workload>-fsync-nj<N>.log`;
- `fio/<device>-<workload>-fsync-nj<N>.json` for successfully parsed fio output;
- `runner.log`: controller progress and failures.

The original NVLog and Asterinas baseline result directories are not modified.

## Verification

Unit tests cover matrix expansion, exact fio arguments, device allowlisting,
device-in-use rejection, marker parsing, kernel-failure detection, timeout
recording, and summary schema. A dry run must report exactly 36 points. Before
the full matrix, one SATA `seqwrite-4k/fsync/nj1` smoke point must complete and
produce a valid summary row. The full run starts only after this smoke passes.
