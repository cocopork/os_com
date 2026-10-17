# Backend, configuration, and DRAM recovery provenance

These files are evidence-led adaptations, **not exact blob recoveries**. The candidate report found no exact target object. No original repository or evidence files were modified.

## Evidence locations

- Complete old implementation: `/home/zn/zn/Rust_EXT4/asterinas_ext4/kernel/core/src/fs/nv_pool/{backend,config,dram}.rs`.
- Candidate selection and dated change records: `/home/zn/asterinas-build-env/rnvpc-candidate-search/` (`REPORT.md`, `selection-manifest.json`, `backend.rs.changes.json`, `config.rs.changes.json`, `dram.rs.changes.json`).
- Raw root: `/home/zn/zn/recovery_Rust_EXT4/os_com_candidates_20261015T074727Z/candidates/`.
- Positioned views: `/home/zn/zn/recovery_Rust_EXT4/research_recovery_20261016/positioned-views/kernel/src/fs/rnvpc/`.

## backend.rs

The complete trait body was extracted from raw candidate `02_project_rust/95/95f57a62646f7a01c86611e520a65dcab0b9f316fa366848728a209a6524dfbe__00106241.txt` beginning at `pub(crate) trait NvMemoryBackend`. The file around it is mixed/corrupt, so only the bounded, intact body was used. This includes direct VmReader/VmWriter copying, DMA page handles, zeroing, range writes, and naturally aligned stores. The module header/imports were adapted from the complete old implementation; the `USegment` import is required by the recovered signature. The caller-coordination documentation comes from the October 12 05:53 session change. Existing `frame.rs` and `persist.rs` consume this exact method set.

## config.rs

The old complete configuration supplies the capacity validation and accessors, with NvPool naming adapted to Rnvpc. Canonical/legacy command-line declarations, one-time legacy warning, `select_param`, and `recovery_benchmark_enabled` are recovered from selected candidate `b0900771f68053955eec7b5159ed001ab49e7f49d7eba6dddefa354877ed0761__00050417.txt`. The intact `from_command_line` method comes from selected candidate `20bfbe2cc2745cb4ecb6da004b4b7824f5b4eddb95e1e9956b04e313e8cc1edc__00105237.c`. The FastNoPersist variant/parser and Model documentation follow the October 11 16:06 and October 12 05:47 change records.

The complete old code and selected config fragments require nonzero metadata reserve. An intermediate October 12 concurrent-backend test used zero, but the recovered 05:55:45 follow-up in `tests.rs.changes.json` changes that fixture to five total pages, one metadata page, and four data pages. This resolves the apparent conflict: no production validation change is needed. The restored local concurrent test uses that corrected split.

## dram.rs

The complete old implementation supplies segmented allocation, cross-chunk reads/writes, control words, bounds/epoch checks, crash snapshots, stats, and compiler-fence simulation. Naming changes are mechanical. Intact `copy_to_writer`, `data_page_segment`, `copy_from_reader`, `zero_page_range`, `store_page_u64`, and `write_data_once` bodies come from selected candidate `20bfbe2cc2745cb4ecb6da004b4b7824f5b4eddb95e1e9956b04e313e8cc1edc__00105237.c`. The intact `write_page_range` body comes from raw `04_rust_kernel_ext4/b3/b36dc82d834184e76198df2dbd6180f6b8595de41332141b3f25e2e49d0eec0d__00082000.txt`. Imports follow the selected candidate's DRAM header.

The four dated dram change records are applied semantically:

- FastNoPersist validates ranges/epoch then skips simulated persistence and counter increments.
- Durable image allocation/copy and the snapshot mutex exist only under `cfg(ktest)`.
- Normal `allocate` rejects Model, including ktest builds; explicit model fixtures remain available.
- Ordinary byte operations never acquire an image-wide mutex. In ktests a mutex guard is acquired only when a durable crash-model image actually exists. The test-only mutex is sleepable, matching recovered user-fault requirements.

Direct user copies preserve the recovered partial-copy contract: a nonempty copied prefix is returned on a later fault; a fault before copying any byte is returned as an error. No staging buffer or new rollback algorithm was introduced. Higher-level callers retain responsibility for pins/leases and persisting the returned prefix.

## New validation additions and limits

New local `recovery_tests` cover short direct copies and range bounds, final short allocation chunk/DMA page size, model-only allocation plus persisted-prefix restoration/stale handles, FastNoPersist validation/counters, and command-line precedence/parser behavior. The disjoint-page concurrency test is adapted from the recorded October 12 backend test (fixture corrected by the recovered follow-up noted above). These tests are appended within the owned modules, avoiding changes to other agents' recovered tests.

All three files pass Rust 2024 rustfmt parsing. This is syntax validation only. Full kernel typechecking and ktest execution are delegated to the parent integration run; no runtime pass is claimed here. A real user-page-fault test is still needed to exercise the recovered nonzero-prefix-on-fault branch dynamically; the new short-copy test exercises a short source/destination without creating a fault. No unsupported complex algorithm was substituted for missing evidence.
