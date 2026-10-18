# Correctness

For a new or semantically changed mechanism, name the authority copy at each
fault boundary and define recovery from persistent state rather than runtime
pointers. Give each state a legal entry, exit, owner, and reclaim condition.

Build a state-machine test matrix for success, failure, retry, and crash/restart.
Check lock order across foreground, writeback, and completion paths. Completion
handlers should make short, non-blocking transitions; defer work that may wait or
take contended allocator or filesystem locks.

Model re-dirty after submission explicitly. Pair reusable-object identity with a
generation, and validate identity plus release atomically so a delayed completion
cannot reclaim a new owner. Crash validation must prove reconstruction reaches
the authority copy and excludes stale volatile state.
