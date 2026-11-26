# NVLog Comparison Experiments Implementation Plan

> **For Codex:** Execute this plan with `executing-plans`, preserving all existing dirty work in both repositories.

**Goal:** Run NVLog under Linux in a QEMU guest and add directly comparable NVLog results to the existing mixed-sync, fio full-e2e, and Filebench EXT4/rNVPCe-SU experiments on NVMe and SATA.

**Architecture:** Build the repository's Linux 5.15.125 kernel and modified ndctl/nvpcctl, boot it with a reproducible Linux root image and an emulated QEMU NVDIMM, and pass through the same physical target partitions used by the Asterinas experiments. Reuse the exact existing workload binaries/profiles and collect NVLog output in an independent evidence directory before merging only validated CSV/plots/reports into `docs/experiments/fio/formal/full-e2e-20261123/`.

**Comparability contract:** Use the same devices, vCPU/RAM, queue counts, datasets, seeds, durations, thread counts, and repeat counts as each existing experiment. Set NVLog NVM capacity exactly in pages. Approximate the Linux DRAM PageCache budget with a per-benchmark cgroup v2 memory limit derived from measured anonymous memory plus the target file-cache budget, record `memory.stat`, and flag points whose observed file cache deviates by more than 10%. State that Linux and Asterinas driver/PageCache implementations remain a whole-system difference.

---

## Global constraints

- Preserve the dirty NVLog kernel changes and record the base commit plus diff SHA-256.
- Preserve unrelated dirty changes in `os_com_rnvpc_rebuild`.
- Destructive operations may target only `/dev/nvme1n1p2` at exactly `274877906944` bytes and `/dev/sda4` at exactly `274877890560` bytes, after mount/swap/holder/open-user checks.
- Store live evidence outside git under `/home/zn/asterinas-build-env/nvlog-comparison-20261120/`.
- Never classify a point as successful unless benchmark, sync, unmount, QEMU exit, result parsing, and read-only `e2fsck -fn` all succeed.
- Preserve failed point directories; resume only complete, reparsable points.

## Task 1: Reproducible NVLog build and boot environment

Create `docs/experiments/fio/formal/full-e2e-20261123/scripts/nvlog/` with test-first validation for configuration generation, device preflight, result completeness, and cache-budget metadata. Build the repository kernel from `.config.example`, modified ndctl/daxctl, `nvpcctl`, a Linux guest root image, and record hashes/tool versions.

Verification: boot the custom kernel, detect the emulated NVDIMM, reconfigure it to `nvpc`, and make `nvpcctl config` and `nvpcctl open` succeed.

## Task 2: Destructive smoke validation

Run one NVMe and one SATA point with a small workload. Verify NVLog absorbs `write()+fsync()`, exact NVM size is applied, cache observations are recorded, the guest exits cleanly, targets are unmounted, and `e2fsck -fn` succeeds.

## Task 3: Mixed-sync matrix

Reuse `mixed_sync_bench.c`: 4 KiB random access, eight private files, R/W ratios 0/10, 3/7, 5/5, 7/3; sync ratios 0/20/40/60/80/100%; three repeats; NVMe eight queues and SATA one queue. Add 144 validated NVLog runs and update the existing comparison data and plots without deleting historical evidence.

## Task 4: Filebench matrix

Reuse the four existing 16-thread profiles with 10,000 files and three repeats on both devices. Add 24 validated NVLog runs and update comparison data, plots, and report.

## Task 5: fio full-e2e matrix

Reuse overwrite-randwrite, append-write, and cold buffered randread; thread and block-size sweeps; three repeats on both devices. Add 324 validated NVLog runs and update comparison data, plots, and report.

## Task 6: Final validation and report

Reparse all raw evidence, verify expected row/point counts, hashes, cache observations, QEMU topology, clean filesystems, and absence of mounted targets or leaked QEMU processes. Summarize NVLog versus rNVPCe-SU with explicit evidence/inference/limitations sections.
