# Later tests outside the retained production version

`metadata_mutation_guard.rs.reference` preserves the complete recovered test `rnvpce_metadata_mutation_guard_wakes_a_waiting_leaf_mutator`. Its source is `/home/zn/asterinas-build-env/rnvpc-candidate-search/tests.rs.changes.json`, timestamp `2026-10-13T04:40:00.410Z`, the added function in that patch. Formatting was normalized by rustfmt when originally reconstructed; the archived function body has not been weakened or replaced with a no-op.

The integration ktest compile reported two missing `lock_metadata_mutation` methods from this test. The retained exact `frame.rs` exposes publication-based `update_authoritative_metadata_in_place` but no per-leaf mutation-guard API. The similarly dated positioned test view `72cd1d5cc.rs.partial` also references `try_lock_metadata_mutation` and `update_authoritative_metadata_with_guard`, confirming this belongs to a later API family, not a simple spelling mismatch.

This reference is intentionally not included through a Rust module declaration or disabled test attribute. Running it requires recovering the matching later production implementation and its dependencies. No such implementation is invented here. The parent report must treat this as missing behavioral coverage, not a passing/skipped runtime test.
