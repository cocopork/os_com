> **Archived:** 2026-09-27. Historical debugging study. **Replaced by:** [AchillesFS implementation reference](../../rNVPC-rTIO/implementation.md); current status: [STATUS.md](../../STATUS.md).
>
> **Source date:** 2026-09-24. **Archive reason:** the original diagnostic path was superseded. **Citation use:** causal diagnosis and test design only; no raw logs are retained.

# Dirty-Page Accounting Residue Diagnosis

The historical investigation found that a global dirty-page summary could retain
counts after journaled directory pages reached their own durability boundary.
That caused later tests to inherit state and made pressure control observe a
count that no longer represented flushable PageCache data.

The durable lesson is to distinguish a delayed cleanup from a true accounting
leak: inspect in-flight writeback state alongside the aggregate count, and
assign an explicit clean/accounting transition when another subsystem owns
persistence. Test fixtures may expose residual state, but they must not hide a
production accounting error.

Any follow-on fix must prove the journaled and nojournal paths separately,
include failed-I/O and teardown cases, and use deterministic synchronization.
The current implementation boundary and validation status remain in
[`STATUS.md`](../../STATUS.md).
