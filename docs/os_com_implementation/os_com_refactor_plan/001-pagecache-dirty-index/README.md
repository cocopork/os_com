# 001 PageCache Dirty-Page Index

## Goal
Traverse dirty pages directly for writeback rather than scanning present pages.

## Current Status
**Partial.** The historical design identifies XArray marks and owner-safe state changes; [`STATUS.md`](../../STATUS.md) does not record a committed dirty-index baseline or runtime validation.

## Implemented Result
No completed result is asserted by this plan. The proposed boundary is a VMO dirty mark maintained with page-state transitions.

## Verification Evidence
No fresh test is recorded. Any implementation needs sparse-dirty traversal, clean/redirty, and writeback-failure state-machine coverage.

## Remaining Work
Confirm the committed PageCache interface, implement only its matching mark protocol, and validate normal and detached completion paths before enabling a background user.

## Historical Notes
The earlier reverse-mapping proposal was selected to avoid changing backend callers. It is a design input, not current implementation status.
