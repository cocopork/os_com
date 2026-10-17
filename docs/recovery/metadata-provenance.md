# Descriptor, index, and metadata recovery provenance

Scope: only `kernel/src/fs/rnvpc/{descriptor,index,metadata}.rs` plus this note. These are evidence-led reconstructions, **not byte-exact recovered files**. Existing `layout.rs`, `persist.rs`, `frame.rs`, `extent_mutation.rs`, and `write_transaction.rs` were read for contract verification and not edited.

## Evidence locations

Raw source root: `/home/zn/zn/recovery_Rust_EXT4/os_com_candidates_20261015T074727Z/candidates`. The hexadecimal prefixes below identify SHA256-named raw candidates uniquely. Session view root: `/home/zn/zn/recovery_Rust_EXT4/research_recovery_20261016/positioned-views/kernel/src/fs/rnvpc`. Session views are sparse and versioned; only explicitly intact item ranges were used, never gap comments as source.

## descriptor.rs

- `b09852ecc2ef795b3e2fc1655ccfb6e3c8e96b1376ebfd9fb941bab534df4067__00067164.txt`: from `NV_DESCRIPTOR_MAGIC` through the end of `transition_to`, including the full 72-byte C-layout descriptor, kind/state discriminants, constructors, allocation sequence, and both migration flags. This raw candidate already contains the two 2026-10-09 migration-credit patches; they were not applied twice.
- `14c04aa5df3764dfd2246189707ae90d30373c78cd9e541a1f3dc33237fda6fb__00041147.txt`: `is_valid`, `encode`, `decode`, and `scan_descriptors`, ending after the scan loop. Independent `2fa0bcb9373311fd83a7c85bac4e620bf0f5620490a933d72df765165d36a89c__00067493.txt` confirms the same scan suffix truncation.
- Minimal reconstructed text: SPDX/import preamble (`aster_block::BLOCK_SIZE`, the same constant used by the exact frame module), and final truncated `found` return expression plus closing brace of the test-only scanner. No persisted field or numeric tag was invented.
- New regression test: size/alignment, encode/decode round trip, wrong size/magic/kind/state, forbidden state transition.

## index.rs

- `581961376aa2ac0931eb525edcd7f807ca0337c11cd111ad17324896428be97a__00059681.txt`: intact imports and `PageKey` definition/constructor.
- `fc881a10bb4a16257e32623f23ebb955f36a99bdb2559b111c651ac02138bf75__00068350.txt`: contiguous `ReverseExtentRef` through the closing `PageIndex` implementation, stopping before binary contamination.
- Applied the recorded 2026-10-12 06:04:29.510Z patch (`exec-b542e7df-0869-4522-9d99-632e06bf8835`, record SHA256 `b5f293a8e32b2e7fa9fcd957e215657dfdb0f93124ea27626ecb56fd9802ecc5`) changing the packed reverse locator to `NonZeroU64` and asserting eight-byte `Option<ReverseExtentRef>`; corroborated by the index session view. This is volatile state, not a persisted-layout change.
- Minimal new text: SPDX preamble. New regression test covers packing limits and overflow rejection.

## metadata.rs

- `a6e46a3453098228a54735c08b58279537e28e220006a492c52029cb4f93dd49__00016937.c`: latest preamble, magic/version, kind enum, full 64-byte header declaration, constructor, `kind`, and `decode_kind`. Crucially this version has **SpaceReservation = 3, Allocation = 7**; older views using Allocation = 3 are not compatible and were not selected for the kind enum.
- `e8b43c73c4834a87662362f0ff22f6b9afa1914de0ef53063caff8c9f0a855ed__00016936.txt`: header getters/validation suffix, full `PersistentObjectRef`, reservation structure/constructor/decode/validation; later intact pending-mutation getters, state helper, size/alignment/offset assertions, and trait implementations. Binary suffix excluded.
- `b1d5d101543cc086ec02a8c2fbe9cca8c867d58ac6e5cc4068a5a0669e9198a4__00122700.txt`: reservation getter suffix, reservation size assertions, complete allocation structure and implementation.
- `metadata.rs/6d9a1d689.rs.partial`: intact range from `EXTENT_ENTRY_UNWRITTEN` through the `ExtentRecordEntry` size assertions. This supplies NV-data flag/tagged reference logic; unlike surrounding portions of this sparse view, this item range contains no gaps or conflicts. Its fields match later raw entry prefixes (`a80cd6d2cd2b1ccd404c233245a7dd1b4205d64082f04b06d758b9dea72a2bba__00051186.txt`, `b1d5d101...`).
- `7eac678a5905ab2c029fbe79d2a91c583ff408a2d85e316c626c9d31b70fa7cc__00031035.txt`: uninterrupted current extent capacity, boxed wrapper, extent structure/constructors/validation/getters; inode structure/constructors/getters/update helpers and offsets; metadata traits; mutation phase/operation enum (including NodeRewrite); 208-byte mutation structure; node-rewrite constructor and split constructor prefix.
- `e70eae2c0a4af1b01f59bb04ae60383f1220b956bead84d731a3ac0d6fe33511__00031038.txt`: split-constructor continuation, root-split constructor, phase update, repair references, decode, and validation prefix up to the operation match. The split seam is independently corroborated by `metadata.rs/21a1e2d60.rs.partial`.
- `metadata.rs/21a1e2d60.rs.partial`: intact operation-specific validation match for NodeRewrite/NodeSplit/RootSplit, replacing a torn raw expression. Followed by the intact raw getter suffix from `e8b43...`.
- Minimal reconstructed seams: header `fs_id` method signature/return token (both surrounding fragments present, also identical in older session header definitions); reservation `header()` signature/body at a torn boundary (identical standard getter in session/raw other records); `Ok(())` and closing validation brace between the intact match and getter suffix (also visible in `e8b43...`). No new storage fields, sizes, offsets, enum tags, constructors, or recovery algorithms were introduced.
- The older 176-byte pending mark and old phase/operation byte fields were not selected: exact `frame.rs` persists a naturally aligned state word and current mutation code requires repair images and NodeRewrite.

## Persisted schema contracts

| Record | Size | Alignment | Important fields/offsets |
|---|---:|---:|---|
| NvDescriptor | 72 | 8 | Existing layout embeds this unchanged |
| PersistentMetadataHeader | 64 | 8 | generation at 40 |
| PersistentObjectRef | 16 | 8 | slot-plus-one, allocation sequence |
| SpaceReservationRecord | 128 | 8 | kind 3 |
| AllocationRecord | 128 | 8 | transitional kind 7 |
| ExtentRecordEntry | 32 | 8 | flags: unwritten=1, child=2, NV data=4 |
| ExtentNodeRecord | 4096 | 8 | 124 entries |
| InodeRecord | 128 | 8 | root=64, file size=80, block count=88 |
| PendingExtentMutationRecord | 208 | 8 | state=64; phase low byte, operation next byte |

Offsets in production are recovered `offset_of!` expressions, not hard-coded guesses. Newly authored tests assert the recovered numeric schema and exercise bad sizes/magic, malformed persistent references, over-consumption, invalid mutation state/repair references, extent overlap, unknown flags, and null-reference constructor rejection.

## Limits

Exact byte-for-byte provenance for the entire reconstructed files is not claimed. The selected code preserves recovered behavior, including the distinction that descriptor `decode` checks size/magic/version while `is_valid` additionally checks kind/state/length. No speculative hardening of other validation semantics has been mixed into recovery. Full boot/crash behavior requires integration testing beyond these pure-data tests.

## Validation performed

- All three files parsed/formatted successfully with container `rustfmt +nightly-2026-04-03 --edition 2024`.
- A temporary host harness `/tmp/rnvpc-metadata-harness` copied the actual recovered modules plus the unchanged `id.rs`, used the repository's real `ostd-pod`/derive implementation and `zerocopy`, and supplied only kernel prelude/error/block-size plumbing. `cargo +nightly-2026-04-03 test --offline` in `os-com-recovery-dev` passed **5 tests, 0 failures**, including compile-time Pod/layout assertions. This is type/layout/pure-data validation, not kernel integration or crash testing.
- Initial harness compile lacked its direct `zerocopy` dependency required by the real Pod derive; adding that harness dependency resolved the errors without modifying recovered source.
