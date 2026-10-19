# EXT4 Journal Comparison Harness

## Purpose

Owns the legacy EXT4 journal/nojournal comparison, matrix, and single-point reproduction tools. Outputs remain diagnostic until accepted by `../../EXPERIMENTS.md`.

## Commands

- `./run_ext4_journal_compare.sh --dry-run`: print the journal comparison matrix.
- `./run_perf_matrix.sh --dry-run`: print the SATA/NVMe matrix.
- `./run_perf_matrix_img.sh --dry-run`: print the real-device image matrix.
- `./run_single_round.sh`: run one image-backed point.

## Output

Every runner writes to `results/<timestamp>_<scenario>/` in this directory. The repository-root `results/` path is retired.

## Safety

Full runs use Docker, KVM, and QEMU; device-backed variants format their configured devices. Dry-run modes do not enter Docker or create result directories.
