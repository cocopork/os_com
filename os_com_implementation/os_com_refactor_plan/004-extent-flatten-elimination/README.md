# 004 Extent Flatten Elimination

## Goal
Bound rare extent range-edit paths that still flatten and rebuild whole trees.

## Current Status
**Partial.** The source study identifies bounded hot paths and remaining rare rebuilds; [`STATUS.md`](../../STATUS.md) has no fresh runtime validation.

## Implemented Result
No additional completion is claimed. Common edits and rare range operations must remain distinct when evaluating follow-on work.

## Verification Evidence
No new test execution is recorded.

## Remaining Work
Characterize each rebuild caller, implement local surgery where safe, and test extent ordering, merge/split, truncate, and error rollback.

## Historical Notes
Whole-tree rebuilds are a performance concern, not proof of incorrectness.
