# Experiment Evidence

This document is the acceptance boundary for performance evidence. Current
implementation progress remains in [`STATUS.md`](STATUS.md). A formal result
must preserve its workload, environment, revision, instrumentation state,
completion marker, and `busy_bw_mib_s` data in a retained evidence package.

## Accepted Evidence

There is no accepted experiment in the current inventory. [`STATUS.md`](STATUS.md)
records that no retained `summary.csv`, completion marker, configuration, and
result provenance have been verified together. Task 4 therefore creates no
`evidence/<experiment>/` package: historical result directories and prose
cannot support a current formal throughput conclusion.

When evidence is accepted, its entry will identify the purpose, compared
objects, exact workload, device and virtual-machine environment, code revision,
instrumentation state, accepted `busy_bw_mib_s` result path, conclusion, and
limitation. The required package shape is `evidence/<experiment>/summary.csv`.

## Diagnostic Evidence

Historical FIO, Filebench, SQLite, and writeback investigations remain useful
for reproducing a symptom or choosing an instrumentation point, but they are
diagnostic evidence only until a retained package is accepted. In particular,
drain measurements are diagnostic for tail completion and writeback behavior;
they are not formal throughput results.

Useful diagnostic references include:

- [`ext4_performance.md`](ext4_performance.md) for prior root-cause studies and rejected hypotheses.
- [`test/aster-ext4-baseline-test/README.md`](test/aster-ext4-baseline-test/README.md) for the FIO matrix, parser semantics, and current harness limitations.
- [`rNVPC-rTIO/rtio-flush-merge-and-thread-merge.md`](rNVPC-rTIO/rtio-flush-merge-and-thread-merge.md) for the bounded rTIO BIO-merge diagnosis.

## Pending Experiments

The pending five-profile FIO matrix compares `ext2like_sync`, `ext4_sync`,
`ext2like_nomerge`, `ext4`, and `ext4_nomerge` on both SATA and NVMe. The matrix
uses `randwrite-4k` and `seqwrite-4k` orderless workloads, records the mounted
options in the serial log, and treats the rNVPC/rNVPCe labels as surrogates.

Before accepting a point, retain a final package with the exact command,
device, cache and working-set settings, virtual-machine resources, code commit
and dirty state, instrumentation setting, raw completion marker, and a
`busy_bw_mib_s` value. Use repeated observations and report the chosen
aggregation rule; a smoke run or a single observation is not a formal result.

## Acceptance Rules

An experiment is accepted only when the retained package is complete,
reproducible, and linked here. Incomplete output, a timeout, a silent guest
exit, a missing completion marker, changed configuration, or enabled
performance-distorting instrumentation excludes a point from formal comparison.

Use same-session A/B comparisons when isolating a mechanism. Do not attribute
cross-session variation to code changes without an equivalent baseline in the
same environment.
