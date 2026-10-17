# Evidence restoration progress

Baseline: recovery/rnvpc-rebuild, 49e7829d6. Original checkpoint/evidence untouched.

- Historical Git probe: 57 traversable commits; historical object IDs found but none of 11 required file blobs available.
- Candidate audit: 54,731 candidate files scanned without read errors; no exact target blobs, useful fragments for all 11 paths.
- Backend/config/DRAM: old full references + intact candidate methods + dated session changes assembled; syntax checked.
- Descriptor/index/metadata: assembled; 5 independent pure-data/layout tests pass.
- Cow/extend/recover: session/raw bodies assembled; syntax checked.
- Module entry/tests: assembled; 25 top-level tests included, later unsupported test archived with provenance.
- Independent review finished; WAL accounting omission corrected and scoped re-reviewed, test version mismatches corrected and re-reviewed.
- Production and ktest builds both fail with 17 missing EXT4 API errors; QEMU not reached. All 11 recovered files pass final rustfmt check. Logs and final outcome in REPORT.md.

## Decisions

Ruling: Preserve the evidenced legacy cow/WAL compatibility path and label its mismatch with the final persistent foreground-checkpoint design. The retained wal.rs matches it; implementing a different algorithm would require absent pcache_layout/pcache_store/persistent_btree dependencies. Cost if wrong: further version alignment and restoration are required before this path can be enabled as the final research design.

Ruling: Keep changes in an isolated working tree, without claiming exact blob recovery or merging into the checkpoint. All assembled files remain reviewable.

## Validation container

`os-com-rnvpc-rebuild`, image `os-com-recovery-tools:20261017`, Rust nightly-2026-04-03, original official Asterinas 0.18.0-20260618 toolchain. New tree mounted at /root/asterinas; evidence tree read-only.

Ruling: Preserve inherited recovery/legacy completion/snapshot defects in this evidence-restoration candidate and record them in review.md, instead of silently rewriting historical algorithms. Cost: candidate remains unsuitable for final-design or crash-safety claims; dedicated fixes and runtime tests remain required.

Ruling: Archive the later metadata-mutation-guard test outside the compiled module because its API is absent from retained checkpoint code; retain 25 matching tests. Cost: that later race/wakeup regression is not covered until its API is restored.
