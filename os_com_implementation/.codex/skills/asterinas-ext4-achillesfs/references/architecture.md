# Architecture

The source repository contains EXT4/JBD2 integration, PageCache dirtying,
delayed allocation, extent and metadata-cache paths, and background writeback.
This repository supplies the design, implementation map, evidence policy, and
harness guidance. Consult the live [design](../../../../rNVPC-rTIO/design.md)
and [implementation map](../../../../rNVPC-rTIO/implementation.md) for source
ownership rather than treating this reference as a status authority.

rNVPC owns per-inode WAL-protected CoW; rNVPCe owns soft-update publication; and
rTIO owns residency selection, inode-scoped writeback, and completion handling.
Persistent state is NV data, metadata, descriptors, backing addresses, and
recovery reachability. B-tree indexes, pointers, locks, counters, queues, and
worker state are volatile and must be rebuilt after recovery.
