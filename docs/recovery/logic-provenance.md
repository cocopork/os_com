# Logic reconstruction provenance

These are evidence-based reconstructions, not exact recovered Git blobs. No original evidence files were changed.

## cow.rs

Copied the old cost-simulation implementation from session records `dcb1b7883b33` (2026-09-28 05:35:29 UTC, command output cow lines 1–380) and `91f28c16274c` (2026-09-28 11:22:33 UTC, command output remainder from displayed line 340). The latter starts at actual earlier-version line 346; a ten-line exact overlap located the join. Installed first 345 lines of the former, then all 193 cow lines of the latter. The first assembly restored the structural module but omitted WAL capacity helpers and completion accounting; see the review correction below. Formatting is normalized separately. Raw candidate `39356371...00059682.txt` corroborates the header and context interfaces. No new algorithms.

**Known design gap:** this is the old completion-retired, DRAM-only 1024-record WAL model. The retained wal.rs is also explicitly this model and provides only its APIs. It does NOT implement the final persistent foreground-checkpoint WAL. Session evidence from the separate Sep28 foreground-checkpoint branch imports pcache_layout, pcache_store and persistent_btree; none is present in this retained subtree. Implementing that branch requires recovering its consistent dependency set. Replacing only cow.rs would be false restoration. In particular, SSD completion here DOES retire legacy WAL; this must not be described as final rNVPC semantics.

## extend_cow.rs

Header copied from Oct12 record `b2b9a3575a32` (head -80). Production body copied from raw candidate `44aa561b70e28db55266dae263fc4c4a21b57726a2a0dd85d7bd583b82692f6d__00121321.txt`, from `use core::sync::atomic` through the second successful-release branch of `try_release_nv_page`. The displaced tail of the same candidate supplies the remainder through the final impl brace. One displaced statement, `self.counters.lock().pages_reclaimed += 1;`, is restored with indentation from its intact text immediately after a corrupted comment boundary. This is copied evidence, not a synthesized behavior.

Cross-check: installed lines 313–435 exactly equal Oct12 record `5075b97bef6a`; installed lines 260–305 exactly equal Oct6 numbered record `849943f1940d`. The compatibility implementation uses NVBH reference publication and sequence-aware release. It is not the final direct-NV range-write implementation: retained write_transaction::RangeWriteBatch remains authoritative for that path, and no call sites were altered to select this old CoW path.

## recover.rs

Copied Oct12 command outputs from records:

- `0197cdbf643eab81e377bd6295ced19d97135a35131d47ffe6d5c2d96b4e694a`: recover lines 1–219 (duplicate sed boundary removed).
- `646ffadaf7e3d8b4af519b726a5793ee70d13116254de61e6a11267ed17925e3`: recover lines 220–329.
- `90e04e48e0ece8023fb29f56e9ef26c7a38e2143da71e8791bd0a8eb83239eb5`: recover lines 330 onward through immediately before `reclaim_unreachable` (duplicate line 420 removed).
- Complete `reclaim_unreachable` copied from raw candidate `b09852ecc2ef795b3e2fc1655ccfb6e3c8e96b1376ebfd9fb941bab534df4067__00067164.txt`, stopping at the following binary boundary.

The older full nv_pool/recover.rs was inspected as a reference but not used because it lacks fixed inode roots, pending extent repair, reachability, generation identity, reverse-map rebuilding, and reservation reconciliation present in these later sessions. No new recovery algorithms.

## Validation and limits

All three assembled files pass Rust 2024 parsing via rustfmt +nightly-2026-04-03 inside os-com-recovery-dev. This proves syntactic completeness only. Parent integration runs compilation/tests; no runtime crash-safety or performance claim follows from parsing. Raw fragment selection alone does not establish exact checkpoint identity. The legacy cow completion race/lock behavior is preserved evidence and remains subject to review, not silently redesigned.

Sources reside under `/home/zn/zn/recovery_Rust_EXT4/research_recovery_20261016/new-session-records.jsonl` and `/home/zn/asterinas-build-env/rnvpc-candidate-search/selected/`. Positioned unversioned views were not blindly used: their conflict placeholders merge incompatible revisions.

## Review correction: coherent legacy WAL capacity accounting

Review found that the first cow assembly joined old pre-conversion code to a later session tail where WAL accounting was removed for the separate persistent-WAL work. Restored `charge_wal_delta` and `release_wal_delta` verbatim from the earlier session `a3400d96e00f` (2026-09-28 05:36:14 UTC, cow lines 500–575), corroborated by versioned positioned view `8a939120e.rs.partial` lines 363 and 377. Restored the `if retired > 0 { self.release_wal_delta(retired); }` completion call from session `b4346cc48d5b` (05:36:34 UTC), corroborated by positioned line 310. These changes reconstruct the existing legacy accounting chain; no new accounting algorithm was introduced. This corrects the earlier overly broad claim of full module completeness from syntax parsing. Rustfmt parsing was repeated after the fix; full type/runtime validation remains with integration.
