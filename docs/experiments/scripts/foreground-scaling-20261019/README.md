# rNVPCe foreground scaling diagnosis

This experiment separates request-size scaling from thread scaling in the
rNVPCe foreground write transaction. It runs 2 GiB buffered sequential writes
at 4K, 16K, 64K, and 128K with one job, then repeats 4K and 128K with 4 and 16
jobs. Every job writes a separate inode.

Build a diagnostic ISO with `LATENCY_ENABLED` in `ext4/stats.rs`, place it at
`ARTIFACT_DIR/selected.iso`, copy the standard `ext2.img` and `exfat.img`
fixtures there, and run:

```sh
./run-suite.sh ARTIFACT_DIR
```

The target is the previously authorized `/dev/nvme1n1p1`; every point formats
the partition and runs a post-unmount `e2fsck -fn`. Raw logs and generated CSV
stay outside Git.
