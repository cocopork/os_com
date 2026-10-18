# 009 Fsync Device Round Trips

## Goal
Identify reducible device waits in fsync while retaining EXT4 durability order.

## Current Status
**Planned.** [`STATUS.md`](../../STATUS.md) records buffered-I/O durability dispatch, but no committed optimization or fresh runtime validation for this specific task.

## Implemented Result
None claimed.

## Verification Evidence
The existing analysis is a static diagnosis, not retained formal evidence.

## Remaining Work
Trace submit and completion boundaries on the committed baseline, then test concurrent fsync, error propagation, journal order, and crash/recovery effects.

## Historical Notes
Avoid interpreting fewer waits as durable completion without an explicit persistence boundary.
