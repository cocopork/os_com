> **Archived:** 2026-09-27. Superseded diagnostic experiment narrative; retained for methodology only. **Accepted replacement evidence:** none; the current acceptance boundary is [EXPERIMENTS.md](../../EXPERIMENTS.md).
>
> **Source date:** 2026-09-24. **Archive reason:** its conclusions and intermediate observations are not accepted formal evidence. **Citation use:** methodology only.

# AchillesFS Sequential-Write Diagnostic Method

This historical study is not a performance result and contains no raw logs or
intermediate datasets. Its durable methodological lessons are:

- Compare candidate paths only with identical writeback-capacity, workload,
  device, guest, and instrumentation settings.
- Treat latency instrumentation as a diagnostic tool; do not use instrumented
  observations as absolute-performance conclusions.
- Separate foreground throttle time, retirement/publication work, allocation
  waits, and writeback completion when locating a bottleneck.
- A capacity or in-flight limit can amplify a completion-rate difference into
  foreground latency; test that causal chain before attributing it to metadata
  allocation.
- Repeated observations and complete provenance are required before accepting a
  result. Formal throughput, when accepted, uses `busy_bw_mib_s`.

Current performance acceptance and retained evidence are defined by
[`EXPERIMENTS.md`](../../EXPERIMENTS.md); current implementation status is in
[`STATUS.md`](../../STATUS.md).
