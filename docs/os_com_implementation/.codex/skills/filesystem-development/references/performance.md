# Performance

Locate the dominant cost before changing a mechanism. Decompose the path into
memory lookup/copy, allocation and metadata work, BIO construction, submission,
device completion, and post-I/O accounting or wakeups. Measure one boundary at a
time and compare an equivalent baseline in the same environment.

Avoid work that serializes an otherwise asynchronous path: waiting per page,
heavy completion callbacks, coarse global locks, and unbounded dirty scans.
Coalesce only contiguous work that respects device and segment limits, then
verify logical completion for every covered unit. Keep common extent edits
bounded and in place; isolate rare flatten/rebuild paths and measure them
separately.
