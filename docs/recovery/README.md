# Rebuild development baseline

As of 2026-10-17, this branch is the starting point for implementing the missing
functionality. Exact reconstruction of the pre-crash source is no longer the
development goal. Recovered sessions and designs remain evidence of intended
behavior, not a requirement to reproduce historical code byte for byte.

Read [WAL-EXT4-INTEGRATION.md](WAL-EXT4-INTEGRATION.md) for the recovered
implementation, validation, and known limits. This directory is limited to
source-recovery provenance, manifests, and reconstruction reviews. Subsequent
functional and performance work lives under
[`docs/experiments`](../experiments/reports/README.md).

The checked source manifest matches the snapshot that built a normal kernel ISO
and passed 56 targeted KVM tests (0 failed, 678 filtered out). This is not a full
kernel regression result or evidence of physical persistent-memory durability.

## Remaining work

- Restore or reimplement native EXT4 background writeback, dirty/in-flight
  accounting and throttling, delayed allocation, and its reservations.
- Implement the PageCache dirty-only index and matching owner, redirty,
  writeback-generation, and detached-completion protocol.
- Close the rNVPCe SSD-allocation-to-NV-target crash ownership gap; runtime
  rollback/error latching currently only bounds failures during one boot.
- Implement graph-aware NV truncate/deletion and related lifecycle operations;
  unsupported native-only mutations are currently rejected.
- Reconcile rNVPC foreground durability overhead and rTIO batch allocation with
  the intended experimental comparison, then measure rather than reuse old results.
- Add retained-image startup recovery and broaden journal/nojournal, sustained
  over-capacity, sync/unmount/remount, concurrency, and fault tests.

The current DRAM fast backend implements protocol operations, not real NVDIMM
cache-flush instructions or calibrated hardware latency. Model restart fixtures
are ktest-only.
