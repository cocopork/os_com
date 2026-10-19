# SATA / NVMe direct-I/O sweeps

Runs the current Asterinas EXT4 journal baseline on a physical partition exposed
through one virtio-blk queue. This measures the guest filesystem, virtio stack,
QEMU forwarding and physical device together.

- `psync`, `direct=1`, `iodepth=1`, `fsync=0`, independent file per thread.
- Thread sweep: 1,4,8,12,16,20,24,28,32 at 4 KiB.
- Block-size sweep: 4,8,16,32,64,128,256,512,1024 KiB at one thread.
- Four workloads per point: randwrite, randread, write, read.
- A sequential direct write initializes all files before measurement. Writes are
  overwrites; reads do not measure holes or unwritten extents. Each point is one
  full pass over approximately 2 GiB, without a timed warm-up. Per-thread size is
  floor(2048/jobs) MiB (2040–2048 MiB total). The 600-second runtime is only a
  safety cap; the parser rejects incomplete byte counts.
- One repetition is exploratory, not a steady-state or statistical result.
- 64 vCPUs / 8 GiB; fio CPUs 0–31 (split placement), general kernel 32–59,
  writeback 60, reserved 61, virtio submit 62, journal 63.
- QEMU KVM, host CPU, cache=none, aio=threads, virtio queue 1024,
  one queue, indirect descriptors off. No CPU pinning of QEMU on the host.
- rNVPC/rNVPCe/rTIO disabled; journal EXT4 with delalloc enabled.
- direct I/O bypasses guest page cache. fsync=0 does not promise power-loss
  durability or bypass the device's internal volatile write cache.

## Run

Requires a normal release ISO whose init command is `/ext2/rebuild-fio.sh` and
auxiliary `ext2.img`, `exfat.img` containing fio at `/benchmark/bin/fio`.
Place them as `selected.iso`, `ext2.img`, `exfat.img` in an artifact directory.
The runner installs `guest.sh` into that ext2 image. Run one device at a time;
never share these writable guest images between running VMs.

**The following command destroys the selected partition. Obtain explicit user
permission first.** The hardcoded allowlist is NVMe `/dev/nvme1n1p1` and SATA
`/dev/sda4`, both unmounted 256 GiB partitions.

```sh
python3 run.py nvme --format-authorized --artifacts /path/to/artifacts
# Only after separate authorization for /dev/sda4:
python3 run.py sata --format-authorized --artifacts /path/to/artifacts
python3 summarize.py /path/to/artifacts
```

Every run saves its ISO hash and source commit, serial log, and post-unmount
read-only e2fsck output. `summarize.py` exports complete points to CSV and plots
bandwidth/IOPS in PNG and SVG. A complete two-device run has 144 rows; progress
plots can be regenerated while a run is in progress. A failed guest run retains
its log and must be diagnosed before restarting in a new artifact directory.

Historical baseline scripts defaulted to other devices; do not copy their
NVMe target. `/dev/nvme3n1p2` is the current host root filesystem.
