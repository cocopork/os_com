# 002 Background Writeback

## Goal
Drain dirty EXT4 data asynchronously while preserving foreground durability and completion accounting.

## Current Status
**Partial.** [`STATUS.md`](../../STATUS.md) records committed EXT4 writeback mechanisms but marks runtime validation unverified; this document does not claim that every historical daemon phase is complete.

## Implemented Result
The committed baseline includes asynchronous writeback-related mechanisms. The required behavior remains bounded dirty selection, lock-free waits, and completion-boundary reclamation.

## Verification Evidence
Source inspection is the current evidence. Fresh worker, dirty-accounting, failure, and foreground-contention tests are unverified.

## Remaining Work
Reconcile worker ownership with the current source, validate pressure and shutdown behavior, then evaluate performance only with retained evidence.

## Historical Notes
Earlier phase measurements and tuning tables are diagnostic history, not formal results; `busy_bw_mib_s` evidence belongs in [`EXPERIMENTS.md`](../../EXPERIMENTS.md).
