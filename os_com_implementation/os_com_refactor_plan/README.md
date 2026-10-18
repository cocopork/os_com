# EXT4 Refactor References

This index is a bounded map of proposed EXT4 work. Current implementation and validation facts are maintained only in [`../STATUS.md`](../STATUS.md).

| ID | Mechanism | Current status | Code evidence | Validation | Remaining work | Detail |
| --- | --- | --- | --- | --- | --- | --- |
| 001 | PageCache dirty-page index | Partial | Historical source study; no committed baseline named in `STATUS.md` | Runtime validation unverified | Confirm committed implementation and state-machine tests | [001](001-pagecache-dirty-index/README.md) |
| 002 | Background writeback | Partial | EXT4 writeback is in the committed baseline | Runtime validation unverified | Confirm daemon/throttle behavior and completion tests | [002](002-background-writeback/README.md) |
| 003 | Group-level allocation locks + in-memory free-space index + capacity reservations | Implemented / measured; no stable speedup | Rebuild 2026-10-17; independent group gates, hierarchical index and unified capacity ledger; no PA | 593 ktests; 12 fio samples; 4GiB remount CRC32C; 15 clean e2fsck runs | Investigate group/journal contention and sequential regression; complete supplemental fault matrix | [003](003-buddy-cache-allocator/README.md) |
| 004 | Extent flatten elimination | Partial | Existing source retains rare rebuild paths | Not run | Bound rare range-edit paths | [004](004-extent-flatten-elimination/README.md) |
| 005 | virtio-blk segment merge | Planned | Filesystem BIO merging is separate from virtio descriptors | Not run | Implement and measure driver-side merging | [005](005-virtio-blk-segment-merge/README.md) |
| 006 | virtio-blk multiqueue | Planned | Existing study identifies one-queue fallback | Not run | Evaluate MQ independently | [006](006-virtio-multiqueue/README.md) |
| 007 | Delalloc completion | Partial | Delalloc is in the committed EXT4 baseline | Runtime validation unverified | Resolve remaining regressions and run targeted recovery checks | [007](007-delalloc-completion/README.md) |
| 008 | Nojournal metadata writeback | Planned | Diagnostic source only | No accepted experiment | Design batching without changing durability rules | [008](008-nojournal-delalloc-meta-writeback/README.md) |
| 009 | fsync device round trips | Planned | Diagnostic source only | No accepted experiment | Confirm causes on committed baseline and test ordering | [009](009-fsync-device-roundtrips/README.md) |
| 010 | NVPool port | Superseded | Replaced by committed rNVPC/rNVPCe/rTIO baseline | Runtime validation unverified | Use AchillesFS references for follow-on work | [010](010-nvpool-port/README.md) |
| 011 | Nojournal versus journal writeback study | Partial | Historical diagnostic study | No accepted experiment | Reproduce with retained evidence before using conclusions | [011](011-nojournal-vs-journal-writeback/README.md) |

Status values are intentionally limited to `Complete`, `Partial`, `Planned`, `Superseded`, and `Blocked`. They do not promote historical plans or benchmark prose into completed claims.
