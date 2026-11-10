# Experiment scripts

This directory contains the scripts needed to build, run, parse, and summarize
the experiments documented in [`../reports`](../reports/). Generated output is
ignored throughout `docs/experiments`; write raw artifacts to
`/home/zn/asterinas-build-env/` and commit only scripts or Markdown reports.

## Default performance-file preparation

Unless a test explicitly measures first-write or SSD allocation-metadata work,
its timed workload must use a pre-created low-fragmentation file. Multi-job
tests use disjoint ranges of that file by default. Tests that require separate
inodes must create their files sequentially. Never use concurrent preparation
for a thread-scaling baseline: it can create thousands of interleaved extents
and turn the sweep into a file-layout experiment.

Each filesystem performance artifact must include the prepared file size,
data-extent count and extent-index-block count. See the authoritative
[performance sweep policy](../../os_com_implementation/test/preliminary-performance-sweep/README.md)
and the [layout cross experiment](../reports/RNVPCE-LAYOUT-CROSS-2026-11-05.md).
