---
name: filesystem-development
description: "Design filesystem mechanisms, diagnose correctness or I/O performance, and plan reproducible storage experiments."
---

# Filesystem Development

Use this skill for new filesystem mechanisms, semantic changes to existing
mechanisms, correctness debugging, I/O-path performance analysis, or experiment
planning. Keep conclusions bounded by observed evidence.

For a new or semantically changed mechanism, define or update its fault model,
persistence boundary, and authority, not each implementation edit that preserves
the mechanism's semantics. State the durable copy, recovery
source, state transitions, and the failure boundary that makes data reclaimable.

Choose the relevant guide before proposing a change or conclusion:

- [Correctness](references/correctness.md): state, locking, completion, reuse,
  and crash/recovery reasoning.
- [Performance](references/performance.md): locate cost across the I/O path
  before optimizing.
- [Experiments](references/experiments.md): retain reproducible evidence and
  use `busy_bw_mib_s` for formal throughput.

Structure the result as: hypothesis, evidence, change, correctness gate,
performance gate, and residual risk.
