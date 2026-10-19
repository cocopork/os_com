# rNVPCe Reverse-Index Recovery Scaling

This experiment measures the cost of rebuilding a volatile NVPage-to-extent
reverse index from the real persistent rNVPCe extent trees.

## Matrix

- NVPool: 16 GiB DRAM-backed persistent image (`fast` mode).
- EXT4 image: 24 GiB sparse image with at least 1.2 million inodes.
- Target file counts: `1`, `1024`, and `1048576`.
- File data is fixed at 4 GiB: one 4 GiB file, 1024 4 MiB files, or
  1,048,576 4 KiB files.
- Recovery samples: five rebuilds after one population run per target.
- VM memory defaults to 64 GiB so the 16 GiB durable image and the million-file
  VFS state do not exhaust Asterinas physical frames.

Each point first creates and allocates the complete file matrix on plain EXT4
in the privileged experiment container, then boots Asterinas, mounts with
rNVPCe, and overwrites those existing blocks. Host-side preparation avoids
measuring Asterinas's unrelated million-inode allocation cost. This keeps file
creation, directory updates, and SSD block-allocation records out of the
measured NVPool state. The recovery variable is therefore the number of real
inode/extent roots at a fixed data-page count, rather than metadata generated
while constructing the dataset. After unmount, each sample constructs an
independent temporary `Rnvpc` over the same backend bytes;
its object table, PageIndex, reachability sets, extent traversal, and reverse
map are all rebuilt from persistent descriptors and fixed inode roots. This is
a same-VM modeled restart, not a VM power-loss test.

`rnvpc_recovery_bench=on` is an experiment-only kernel switch. It suppresses
the 75% high-water wait and rTIO worker so objects remain resident, while the
normal total/data hard limits remain enforced.

## Run

```bash
./run.sh
```

Useful checks:

```bash
./run.sh --dry-run
./run.sh --smoke
./run.sh --targets 1 --repeats 2 --mem 32G
```

Results are written to `results/<timestamp>-rnvpce-reverse-recovery/`:

- `raw.csv`: every recovery sample and phase counter.
- `summary.csv`: median/min/max phase times grouped by target file count.
- `logs/`: build and serial logs.

The timed phases separate descriptor scan, volatile object/PageIndex rebuild,
metadata-root discovery, pending-mutation repair, extent traversal, reverse-map
insertion, allocation-record scan, unreachable-object reclamation, and final
PageIndex restoration.
