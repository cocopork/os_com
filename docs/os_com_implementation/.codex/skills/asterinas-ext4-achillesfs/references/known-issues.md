# Known Issues

- Completion callbacks must remain short and non-blocking; use explicit
  synchronization to test completion ordering. See the
  [implementation map](../../../../rNVPC-rTIO/implementation.md).
- Treat dirty marks and re-dirty after writeback as state transitions. Submission
  is not a reclaim boundary; failed completion retains retryable data. See the
  [design](../../../../rNVPC-rTIO/design.md).
- Keep common extent edits bounded. Whole-tree flatten/rebuild is a rare path
  requiring separate measurement. See [lessons](../../../../LESSONS.md).
- BIO merging must respect segment limits and preserve logical completion for
  every page. See [lessons](../../../../LESSONS.md).
- Backpressure belongs before lower-level write locks; waits must not turn
  completion into a global serialized stage. See the
  [design](../../../../rNVPC-rTIO/design.md).
- Stale container/build harness state, missing guest completion markers, and
  enabled tracing invalidate a formal result. See the
  [test guide](../../../../test/README.md) and
  [experiment authority](../../../../EXPERIMENTS.md).
