# Experiments

Record device limits, virtual-machine resources, cache capacity, working-set
size, code revision, dirty state, command, and instrumentation state. Size the
working set relative to the cache deliberately, isolate the target device and
runtime, and retain raw completion evidence with parser output.

Use repeats and state the aggregation rule. A timeout, silent exit, changed
configuration, missing completion marker, or performance-distorting
instrumentation excludes a point from formal comparison. Use `busy_bw_mib_s` as
the only formal throughput metric. Drain measurements are diagnostic evidence
for tail completion or writeback behavior, not formal throughput.
