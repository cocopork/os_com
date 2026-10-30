// SPDX-License-Identifier: MPL-2.0

//! Recovered and adapted soft-update integration regressions.
//! See docs/recovery/rnvpce-integrity.md for provenance and validation limits.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ostd::prelude::ktest;

use super::{
    allocation_metadata::{
        ALLOCATION_BITMAP_CHUNK_BYTES, AllocationBitmapChunkRecord, AllocationGroupDirectoryRecord,
        AllocationGroupImage, AllocationMetadataCache, select_allocation_group_image,
    },
    backend::NvMemoryBackend,
    config::{NvSimMode, RnvpcConfig},
    data_page::DataPageState,
    descriptor::{NvDescriptor, NvObjectState, NvRedirectAuthority},
    dram::{DramNvBackend, DramNvImage},
    extent_mutation::ExtentMutation,
    fault::FaultPoint,
    frame::{EvictingDataPage, MigrationExtentIdentity, Rnvpc},
    id::{NvFrameId, NvPageId},
    index::PageKey,
    layout::{commit_offset, descriptor_offset},
    metadata::{
        ExtentDataRef, ExtentMutationPhase, ExtentNodeRecord, ExtentRecordEntry, InodeRecord,
        PENDING_EXTENT_MUTATION_LOGICAL_INDEX, PendingExtentMutationRecord, PersistentObjectRef,
    },
    redirect_index::{DescriptorHandle, NvExtentEntryState},
    write_transaction::{
        AuthoritySnapshot, RangeWriteBatch, RangeWritePage, SsdBaselineExtent, SsdMigrationPage,
        authoritative_inode_state, bootstrap_inode_authority, commit_range_write,
        consume_ssd_capacity_locked, current_page_mapping, mapped_nv_page_ref,
        publish_ssd_baseline_batch, publish_ssd_baseline_extent, publish_ssd_baseline_extents,
        range_write_new_pages, reserve_ssd_capacity, space_reservation,
        test_release_migration_credit_with, visit_ssd_extents,
    },
};
use crate::prelude::*;

const ALLOCATION_TEST_FS_ID: u64 = 0xa110_ca7e;
const ALLOCATION_TEST_GROUP: u32 = 7;

fn allocation_test_image(generation: u64, fill: u8, free_blocks: u32) -> AllocationGroupImage {
    let mut bitmap = [0u8; PAGE_SIZE];
    bitmap.fill(fill);
    AllocationGroupImage::new(generation, bitmap, free_blocks).unwrap()
}

fn allocation_test_chunks(
    generation: u64,
    image: &AllocationGroupImage,
) -> [AllocationBitmapChunkRecord; 2] {
    [
        AllocationBitmapChunkRecord::new(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            generation,
            0,
            &image.bitmap()[..ALLOCATION_BITMAP_CHUNK_BYTES],
        )
        .unwrap(),
        AllocationBitmapChunkRecord::new(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            generation,
            1,
            &image.bitmap()[ALLOCATION_BITMAP_CHUNK_BYTES..],
        )
        .unwrap(),
    ]
}

#[ktest]
fn rnvpce_allocation_bitmap_requires_two_matching_chunks() {
    let image = allocation_test_image(3, 0x5a, 91);
    let chunks = allocation_test_chunks(3, &image);
    let directory = AllocationGroupDirectoryRecord::resident_dirty(
        ALLOCATION_TEST_FS_ID,
        ALLOCATION_TEST_GROUP,
        3,
        image.free_blocks(),
        image.checksum(),
    )
    .unwrap();

    let selected = select_allocation_group_image(&directory, [&chunks[0], &chunks[1]]).unwrap();
    assert_eq!(selected, image);

    let mismatched = allocation_test_chunks(4, &image);
    assert_eq!(
        select_allocation_group_image(&directory, [&chunks[0], &mismatched[1]])
            .unwrap_err()
            .error(),
        Errno::EUCLEAN
    );
}

#[ktest]
fn rnvpce_allocation_directory_selects_only_complete_generation() {
    let old_image = allocation_test_image(8, 0x11, 113);
    let new_image = allocation_test_image(9, 0x22, 57);
    let old_chunks = allocation_test_chunks(8, &old_image);
    let _unreachable_new_chunks = allocation_test_chunks(9, &new_image);
    let old_directory = AllocationGroupDirectoryRecord::resident_dirty(
        ALLOCATION_TEST_FS_ID,
        ALLOCATION_TEST_GROUP,
        8,
        old_image.free_blocks(),
        old_image.checksum(),
    )
    .unwrap();

    assert_eq!(
        select_allocation_group_image(&old_directory, [&old_chunks[0], &old_chunks[1]]).unwrap(),
        old_image
    );
}

#[ktest]
fn rnvpce_allocation_update_crash_keeps_old_generation() {
    let old_image = allocation_test_image(12, 0x33, 100);
    let old_chunks = allocation_test_chunks(12, &old_image);
    let directory = AllocationGroupDirectoryRecord::resident_dirty(
        ALLOCATION_TEST_FS_ID,
        ALLOCATION_TEST_GROUP,
        12,
        old_image.free_blocks(),
        old_image.checksum(),
    )
    .unwrap();

    let recovered =
        select_allocation_group_image(&directory, [&old_chunks[0], &old_chunks[1]]).unwrap();
    assert_eq!(recovered.generation(), 12);
    assert_eq!(recovered, old_image);
}

#[ktest]
fn rnvpce_allocation_directory_rejects_corrupt_selected_chunk() {
    let image = allocation_test_image(21, 0x7e, 31);
    let mut chunks = allocation_test_chunks(21, &image);
    chunks[1].corrupt_payload_for_test(0);
    let directory = AllocationGroupDirectoryRecord::resident_dirty(
        ALLOCATION_TEST_FS_ID,
        ALLOCATION_TEST_GROUP,
        21,
        image.free_blocks(),
        image.checksum(),
    )
    .unwrap();

    assert_eq!(
        select_allocation_group_image(&directory, [&chunks[0], &chunks[1]])
            .unwrap_err()
            .error(),
        Errno::EUCLEAN
    );
}

#[ktest]
fn rnvpce_allocation_cache_recovers_selected_image() {
    let config = RnvpcConfig::from_bytes(
        Some(32 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let persistent_image = DramNvImage::allocate_model(config).unwrap();
    let pool = Arc::new(Rnvpc::format(Arc::new(persistent_image.attach()), config, None).unwrap());
    let expected = allocation_test_image(1, 0xa5, 47);
    AllocationMetadataCache::new(pool)
        .publish_initial(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            &expected,
            None,
        )
        .unwrap();

    let recovered =
        Arc::new(Rnvpc::recover(Arc::new(persistent_image.attach()), config, None).unwrap());
    let actual = AllocationMetadataCache::new(recovered)
        .selected_image(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap()
        .unwrap();
    assert_eq!(actual, expected);
}

#[ktest]
fn rnvpce_allocation_bitmap_updates_only_touched_words_and_keeps_chunk_refs() {
    let config = RnvpcConfig::from_bytes(
        Some(32 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let backend = Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
    let pool = Arc::new(Rnvpc::new(backend.clone(), config, None).unwrap());
    let cache = AllocationMetadataCache::new(pool.clone());
    let image = allocation_test_image(1, 0, 32768);
    cache
        .publish_initial(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP, &image, None)
        .unwrap();
    let before_refs = cache
        .resident_chunk_refs(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap();
    let before = backend.stats();
    let reads = pool.authoritative_metadata_reads();

    let mut bitmap = *image.bitmap();
    bitmap[8] = 0xff;
    cache
        .persist_allocated_range_with_chunks(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            64..72,
            &bitmap[8..16],
            before_refs,
        )
        .unwrap();

    assert_eq!(backend.stats().persist_bytes - before.persist_bytes, 8);
    assert_eq!(
        pool.authoritative_metadata_reads() - reads,
        0,
        "stable chunk refs must avoid full-page validation reads on the hot path"
    );
    assert_eq!(
        cache
            .resident_chunk_refs(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
            .unwrap(),
        before_refs
    );
    let selected = cache
        .selected_image(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap()
        .unwrap();
    assert_eq!(selected.bitmap()[8], 0xff);
    assert_eq!(selected.free_blocks(), 32760);
}

#[ktest]
fn rnvpce_allocation_bitmap_multiword_partial_persist_is_only_a_leak() {
    let config = RnvpcConfig::from_bytes(
        Some(32 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let backend = Arc::new(image.attach());
    let pool = Arc::new(Rnvpc::format(backend.clone(), config, None).unwrap());
    let cache = AllocationMetadataCache::new(pool);
    let initial = allocation_test_image(1, 0, 32768);
    cache
        .publish_initial(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP, &initial, None)
        .unwrap();
    let chunks = cache
        .resident_chunk_refs(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap();

    // This range crosses the two stable 2 KiB bitmap chunks. Persist the
    // first word, fail the second, then crash away the unpersisted working
    // image. Recovery may retain the first half as a leak, never the inverse.
    let mut bitmap = *initial.bitmap();
    bitmap[2047] = 0xf0;
    bitmap[2048] = 0x0f;
    backend.fail_data_persist_after(1);
    let failure = cache
        .persist_allocated_range_with_chunks(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            16380..16388,
            &bitmap[2040..2056],
            chunks,
        )
        .unwrap_err();
    assert!(failure.is_uncertain());

    drop(cache);
    drop(backend);
    image.simulate_crash();
    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let selected = AllocationMetadataCache::new(recovered)
        .selected_image(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap()
        .unwrap();
    assert_eq!(selected.bitmap()[2047], 0xf0);
    assert_eq!(selected.bitmap()[2048], 0);
    assert_eq!(selected.free_blocks(), 32764);
}

#[ktest]
fn rnvpce_allocation_bitmap_free_persist_failure_keeps_range_allocated() {
    let config = RnvpcConfig::from_bytes(
        Some(32 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let backend = Arc::new(image.attach());
    let pool = Arc::new(Rnvpc::format(backend.clone(), config, None).unwrap());
    let cache = AllocationMetadataCache::new(pool);
    let initial = allocation_test_image(1, 0, 32768);
    cache
        .publish_initial(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP, &initial, None)
        .unwrap();
    let chunks = cache
        .resident_chunk_refs(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap();
    let mut allocated_word = [0u8; 8];
    allocated_word[0] = 1;
    cache
        .persist_allocated_range_with_chunks(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            0..1,
            &allocated_word,
            chunks,
        )
        .unwrap();

    backend.fail_data_persist_after(0);
    let failure = cache
        .persist_freed_range_with_chunks(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            0..1,
            &[0; 8],
            chunks,
        )
        .unwrap_err();
    assert!(failure.is_uncertain());

    drop(cache);
    drop(backend);
    image.simulate_crash();
    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let selected = AllocationMetadataCache::new(recovered)
        .selected_image(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap()
        .unwrap();
    assert_eq!(selected.bitmap()[0] & 1, 1);
    assert_eq!(selected.free_blocks(), 32767);
}

#[ktest]
fn rnvpce_allocation_bitmap_recovery_derives_count_from_words() {
    let config = RnvpcConfig::from_bytes(
        Some(32 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let persistent_image = DramNvImage::allocate_model(config).unwrap();
    let pool = Arc::new(Rnvpc::format(Arc::new(persistent_image.attach()), config, None).unwrap());
    let cache = AllocationMetadataCache::new(pool);
    let image = allocation_test_image(1, 0, 32768);
    cache
        .publish_initial(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP, &image, None)
        .unwrap();
    let mut bitmap = *image.bitmap();
    bitmap[0] = 0x0f;
    cache
        .persist_allocated_range(
            ALLOCATION_TEST_FS_ID,
            ALLOCATION_TEST_GROUP,
            0..4,
            &bitmap[..8],
        )
        .unwrap();

    let recovered =
        Arc::new(Rnvpc::recover(Arc::new(persistent_image.attach()), config, None).unwrap());
    let selected = AllocationMetadataCache::new(recovered)
        .selected_image(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP)
        .unwrap()
        .unwrap();
    assert_eq!(selected.free_blocks(), 32764);
}

#[ktest]
fn rnvpce_stale_allocation_snapshot_is_not_reported_as_corruption() {
    let config = RnvpcConfig::from_bytes(
        Some(32 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();
    let backend: Arc<dyn NvMemoryBackend> =
        Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
    let pool = Arc::new(Rnvpc::new(backend, config, None).unwrap());
    let cache = AllocationMetadataCache::new(pool.clone());
    let first = allocation_test_image(1, 0x35, 71);
    let stale = cache
        .publish_initial(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP, &first, None)
        .unwrap();
    let second = allocation_test_image(2, 0x53, 69);
    cache
        .publish_update(ALLOCATION_TEST_FS_ID, ALLOCATION_TEST_GROUP, &second, None)
        .unwrap();

    assert_eq!(
        pool.persistent_metadata_kind(stale).unwrap_err().error(),
        Errno::ESTALE
    );
    assert_eq!(
        cache.resident_groups(ALLOCATION_TEST_FS_ID).unwrap().len(),
        1
    );
}

#[ktest]
fn rnvpce_allocation_authority_handoff_requires_exact_generation() {
    let config = RnvpcConfig::from_bytes(
        Some(32 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(16 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let backend: Arc<dyn NvMemoryBackend> =
        Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
    let pool = Arc::new(Rnvpc::new(backend, config, None).unwrap());
    let cache = AllocationMetadataCache::new(pool);
    let first = allocation_test_image(1, 0x35, 71);
    cache
        .publish_initial(
            ALLOCATION_TEST_FS_ID + 1,
            ALLOCATION_TEST_GROUP,
            &first,
            None,
        )
        .unwrap();

    assert_eq!(
        cache
            .finish_authority_handoff(
                ALLOCATION_TEST_FS_ID + 1,
                ALLOCATION_TEST_GROUP,
                first.generation() + 1,
            )
            .unwrap_err()
            .error(),
        Errno::ESTALE
    );
    cache
        .finish_authority_handoff(
            ALLOCATION_TEST_FS_ID + 1,
            ALLOCATION_TEST_GROUP,
            first.generation(),
        )
        .unwrap();
    assert!(
        cache
            .selected_image(ALLOCATION_TEST_FS_ID + 1, ALLOCATION_TEST_GROUP)
            .unwrap()
            .is_none()
    );

    let second_generation = cache
        .publish_dirty_bitmap(
            ALLOCATION_TEST_FS_ID + 1,
            ALLOCATION_TEST_GROUP,
            *first.bitmap(),
            first.free_blocks(),
        )
        .unwrap();
    assert_eq!(second_generation, 2);
}

fn test_pool(
    total_pages: usize,
    meta_reserve_pages: usize,
    data_limit_pages: usize,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Arc<Rnvpc> {
    let config = RnvpcConfig::from_bytes(
        Some(total_pages * PAGE_SIZE),
        Some(meta_reserve_pages * PAGE_SIZE),
        Some(data_limit_pages * PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();
    let backend: Arc<dyn NvMemoryBackend> =
        Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
    Arc::new(Rnvpc::new(backend, config, wake).unwrap())
}

#[ktest]
fn extent_target_word_crash_is_always_old_or_new() {
    const FS_ID: u64 = 0xf401;
    const INODE: u64 = 17;
    for (fault, expected_block) in [
        (FaultPoint::BeforeContentPersist, 100),
        (FaultPoint::AfterVisibilityPersist, 200),
    ] {
        let (image, config, pool) = restartable_pool(8);
        let mutation = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();
        let old = ExtentRecordEntry::leaf(0, 1, 100, false).unwrap();
        let node = ExtentNodeRecord::new(
            FS_ID,
            INODE,
            1,
            1,
            0,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[old],
        )
        .unwrap();
        let initialized = mutation
            .persist_initialized_child(PageKey::new(FS_ID, INODE, 0x8000), &node, None)
            .unwrap();
        let reference = initialized.reference();
        let inode = InodeRecord::new(
            FS_ID,
            INODE,
            1,
            reference,
            PAGE_SIZE as u64,
            (PAGE_SIZE / 512) as u64,
            0o100644,
            0,
        )
        .unwrap();
        pool.create_fixed_inode(&inode, None).unwrap();
        let guard = pool.lock_metadata_mutation(reference).unwrap();
        let replacement = ExtentRecordEntry::leaf(0, 1, 200, false).unwrap();
        assert!(
            pool.persist_extent_target_with_guard(
                &guard,
                reference,
                0,
                old.target(),
                replacement.target(),
                Some(fault),
            )
            .is_err()
        );
        drop(guard);
        drop(mutation);
        drop(pool);

        image.simulate_crash();
        let backend = image.attach();
        let slot = usize::try_from(reference.slot().unwrap()).unwrap();
        let mut bytes = [0u8; PAGE_SIZE];
        backend
            .read_page(NvPageId::try_from(slot).unwrap(), &mut bytes)
            .unwrap();
        let recovered = ExtentNodeRecord::decode(&bytes).unwrap();
        assert_eq!(
            recovered.entry(0).unwrap().data_ref().unwrap(),
            ExtentDataRef::Ssd(expected_block)
        );
        drop(backend);
        let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
        assert_eq!(
            current_page_mapping(&recovered, FS_ID, INODE, 0)
                .unwrap()
                .unwrap()
                .ssd_block(),
            expected_block
        );
    }
}

#[ktest]
fn extent_target_update_does_not_read_full_metadata_page() {
    const FS_ID: u64 = 0xf405;
    const INODE: u64 = 21;
    let (image, _config, pool) = restartable_pool(8);
    let mutation = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();
    let old = ExtentRecordEntry::leaf(0, 1, 100, false).unwrap();
    let node = ExtentNodeRecord::new(
        FS_ID,
        INODE,
        1,
        1,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &[old],
    )
    .unwrap();
    let initialized = mutation
        .persist_initialized_child(PageKey::new(FS_ID, INODE, 0x8004), &node, None)
        .unwrap();
    let reference = initialized.reference();
    let guard = pool.lock_metadata_mutation(reference).unwrap();
    let replacement = ExtentRecordEntry::leaf(0, 1, 200, false).unwrap();
    let observer = image.attach();
    let before = observer.stats();

    pool.persist_extent_target_with_guard(
        &guard,
        reference,
        0,
        old.target(),
        replacement.target(),
        None,
    )
    .unwrap();

    let after = observer.stats();
    assert_eq!(after.full_page_reads - before.full_page_reads, 0);
    assert_eq!(after.range_reads - before.range_reads, 1);
}

// The v5 Native layout never stores NV targets in extent leaves. The older
// mixed-residency crash scenario is superseded by the descriptor selector and
// SSD-only FAST/FAIR crash tests above and below.
#[cfg(any())]
fn logical_extent_split_survives_every_persist_boundary() {
    const FS_ID: u64 = 0xf402;
    const INODE: u64 = 18;
    const LOGICAL: u64 = 4;
    for stage in 0..=11 {
        let (image, config, pool) = restartable_pool(12);
        let mut reservation = pool.reserve_data_pages(1).unwrap();
        let page = pool
            .stage_reserved_data_page(&mut reservation, &[0x6d; PAGE_SIZE])
            .unwrap();
        pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
            .unwrap();
        pool.replace_data_page_in_owner_at(page, PageKey::new(FS_ID, INODE, LOGICAL), 104, None)
            .unwrap();
        let data_ref = pool.persistent_data_ref(page).unwrap();

        let mutation = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();
        let old = ExtentRecordEntry::leaf(0, 8, 100, false).unwrap();
        let node = ExtentNodeRecord::new(
            FS_ID,
            INODE,
            1,
            2,
            0,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[old],
        )
        .unwrap();
        let initialized = mutation
            .persist_initialized_child(PageKey::new(FS_ID, INODE, 0x8001), &node, None)
            .unwrap();
        let node_ref = initialized.reference();
        let inode = InodeRecord::new(
            FS_ID,
            INODE,
            1,
            node_ref,
            8 * PAGE_SIZE as u64,
            (8 * PAGE_SIZE / 512) as u64,
            0o100644,
            0,
        )
        .unwrap();
        pool.create_fixed_inode(&inode, None).unwrap();
        let guard = pool.lock_metadata_mutation(node_ref).unwrap();
        let page_ssd = ExtentRecordEntry::leaf(LOGICAL, 1, 104, false).unwrap();
        let right = ExtentRecordEntry::leaf(LOGICAL + 1, 3, 105, false).unwrap();

        let first_fault = match stage {
            1 => Some(FaultPoint::AfterExtentPreparedPersist),
            2 => Some(FaultPoint::AfterDependencyPersist),
            3 => Some(FaultPoint::AfterVisibilityPersist),
            4 => Some(FaultPoint::AfterContentPersist),
            _ => None,
        };
        if stage >= 1 {
            let result =
                pool.append_extent_entry_with_guard(&guard, node_ref, page_ssd, first_fault);
            if (1..=4).contains(&stage) {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
        }
        if stage >= 5 {
            let second_fault = match stage {
                5 => Some(FaultPoint::AfterExtentPreparedPersist),
                6 => Some(FaultPoint::AfterDependencyPersist),
                7 => Some(FaultPoint::AfterVisibilityPersist),
                8 => Some(FaultPoint::AfterContentPersist),
                _ => None,
            };
            let result = pool.append_extent_entry_with_guard(&guard, node_ref, right, second_fault);
            if (5..=8).contains(&stage) {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
        }
        if stage >= 9 {
            let result = pool.persist_extent_range_with_guard(
                &guard,
                node_ref,
                0,
                old.range_tail(),
                4,
                (stage == 9).then_some(FaultPoint::AfterExtentChildPersist),
            );
            if stage == 9 {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
        }
        if stage >= 10 {
            let result = pool.persist_extent_target_with_guard(
                &guard,
                node_ref,
                1,
                page_ssd.target(),
                ExtentRecordEntry::nv_leaf(LOGICAL, data_ref, false)
                    .unwrap()
                    .target(),
                Some(if stage == 10 {
                    FaultPoint::BeforeContentPersist
                } else {
                    FaultPoint::AfterVisibilityPersist
                }),
            );
            assert!(result.is_err());
        }
        drop(guard);
        drop(mutation);
        drop(pool);

        image.simulate_crash();
        let backend = image.attach();
        let node_slot = usize::try_from(node_ref.slot().unwrap()).unwrap();
        let mut bytes = [0u8; PAGE_SIZE];
        backend
            .read_page(NvPageId::try_from(node_slot).unwrap(), &mut bytes)
            .unwrap();
        let recovered = ExtentNodeRecord::decode(&bytes).unwrap();
        for logical in 0..8 {
            let mut found = None;
            for entry in recovered.entries().unwrap() {
                if entry.logical_block() > logical
                    || logical >= entry.logical_block() + entry.length() as u64
                {
                    continue;
                }
                let value = match entry.data_ref().unwrap() {
                    ExtentDataRef::Ssd(block) => (0, block + logical - entry.logical_block()),
                    ExtentDataRef::Nv(_) => (1, entry.nv_slot_at(logical).unwrap()),
                };
                assert!(found.is_none_or(|previous| previous == value));
                found = Some(value);
            }
            let expected = if stage == 11 && logical == LOGICAL {
                (1, data_ref.slot_plus_one())
            } else {
                (0, 100 + logical)
            };
            assert_eq!(found, Some(expected), "stage={stage}, logical={logical}");
        }
        if stage == 11 {
            let mut data = [0u8; PAGE_SIZE];
            backend.read_page(page, &mut data).unwrap();
            assert_eq!(data, [0x6d; PAGE_SIZE]);
        }
        drop(backend);

        let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
        for logical in 0..8 {
            let mapping = current_page_mapping(&recovered, FS_ID, INODE, logical)
                .unwrap()
                .unwrap();
            assert_eq!(mapping.ssd_block(), 100 + logical, "stage={stage}");
        }
        if stage == 11 {
            let reference = mapped_nv_page_ref(&recovered, FS_ID, INODE, LOGICAL)
                .unwrap()
                .unwrap();
            let page = NvPageId::try_from(reference.slot().unwrap() as usize).unwrap();
            let pin = recovered.pin_data_page(page).unwrap();
            let mut data = [0u8; PAGE_SIZE];
            pin.read(&mut data).unwrap();
            assert_eq!(data, [0x6d; PAGE_SIZE]);
        }
    }
}

#[ktest]
fn fair_node_split_survives_every_persist_boundary() {
    const FS_ID: u64 = 0xf403;
    const INODE: u64 = 19;
    for stage in 0..=7 {
        let (image, config, pool) = restartable_pool(16);
        let mutation = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();
        let entries = [
            ExtentRecordEntry::leaf(0, 2, 100, false).unwrap(),
            ExtentRecordEntry::leaf(2, 2, 102, false).unwrap(),
            ExtentRecordEntry::leaf(4, 2, 104, false).unwrap(),
            ExtentRecordEntry::leaf(6, 2, 106, false).unwrap(),
        ];
        let left_record = ExtentNodeRecord::new(
            FS_ID,
            INODE,
            1,
            10,
            0,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &entries,
        )
        .unwrap();
        let left = mutation
            .persist_initialized_child(PageKey::new(FS_ID, INODE, 0x8100), &left_record, None)
            .unwrap()
            .reference();
        let right_record = ExtentNodeRecord::new(
            FS_ID,
            INODE,
            2,
            11,
            0,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &entries[2..],
        )
        .unwrap();
        let right = mutation
            .persist_initialized_child(PageKey::new(FS_ID, INODE, 0x8101), &right_record, None)
            .unwrap()
            .reference();
        let parent_record = ExtentNodeRecord::new(
            FS_ID,
            INODE,
            1,
            12,
            1,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[ExtentRecordEntry::child(0, left).unwrap()],
        )
        .unwrap();
        let parent_node = mutation
            .persist_initialized_child(PageKey::new(FS_ID, INODE, 0x8102), &parent_record, None)
            .unwrap();
        let parent = parent_node.reference();
        let inode_record = InodeRecord::new(
            FS_ID,
            INODE,
            1,
            parent,
            8 * PAGE_SIZE as u64,
            (8 * PAGE_SIZE / 512) as u64,
            0o100644,
            0,
        )
        .unwrap();
        pool.create_fixed_inode(&inode_record, None).unwrap();
        let left_guard = pool.lock_metadata_mutation(left).unwrap();
        let parent_guard = pool.lock_metadata_mutation(parent).unwrap();

        if stage >= 1 {
            let result = pool.persist_extent_sibling_with_guard(
                &left_guard,
                left,
                None,
                right,
                Some(if stage == 1 {
                    FaultPoint::BeforeContentPersist
                } else {
                    FaultPoint::AfterExtentChildPersist
                }),
            );
            assert!(result.is_err());
        }
        if stage >= 3 {
            let result = pool.shrink_extent_entry_count_with_guard(
                &left_guard,
                left,
                4,
                2,
                (stage == 3).then_some(FaultPoint::AfterContentPersist),
            );
            if stage == 3 {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
        }
        if stage >= 4 {
            let fault = match stage {
                4 => FaultPoint::AfterExtentPreparedPersist,
                5 => FaultPoint::AfterDependencyPersist,
                6 => FaultPoint::AfterVisibilityPersist,
                _ => FaultPoint::AfterContentPersist,
            };
            assert!(
                pool.append_extent_entry_with_guard(
                    &parent_guard,
                    parent,
                    ExtentRecordEntry::child(4, right).unwrap(),
                    Some(fault),
                )
                .is_err()
            );
        }
        drop(parent_guard);
        drop(left_guard);
        drop(mutation);
        drop(pool);

        image.simulate_crash();
        let backend = image.attach();
        let read_node = |reference: PersistentObjectRef| {
            let mut bytes = [0u8; PAGE_SIZE];
            backend
                .read_page(
                    NvPageId::try_from(usize::try_from(reference.slot().unwrap()).unwrap())
                        .unwrap(),
                    &mut bytes,
                )
                .unwrap();
            ExtentNodeRecord::decode(&bytes).unwrap()
        };
        let parent_node = read_node(parent);
        let left_node = read_node(left);
        let right_node = read_node(right);
        if let Some(slot) = left_node.right_sibling_slot() {
            assert_eq!(slot, right.slot_plus_one());
        }
        for logical in 0..8 {
            let route = parent_node
                .entries()
                .unwrap()
                .iter()
                .filter(|entry| entry.logical_block() <= logical)
                .max_by_key(|entry| entry.logical_block())
                .unwrap()
                .child_slot()
                .unwrap();
            let node = if route == right.slot_plus_one()
                || (route == left.slot_plus_one()
                    && left_node.right_sibling_slot() == Some(right.slot_plus_one())
                    && logical >= 4)
            {
                &right_node
            } else {
                assert_eq!(route, left.slot_plus_one());
                &left_node
            };
            let entry = node
                .entries()
                .unwrap()
                .iter()
                .find(|entry| {
                    entry.logical_block() <= logical
                        && logical < entry.logical_block() + entry.length() as u64
                })
                .unwrap();
            let ExtentDataRef::Ssd(block) = entry.data_ref().unwrap() else {
                panic!("FAIR test contains only SSD leaves");
            };
            assert_eq!(block + logical - entry.logical_block(), 100 + logical);
        }
        drop(backend);

        let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
        for logical in 0..8 {
            let mapping = current_page_mapping(&recovered, FS_ID, INODE, logical)
                .unwrap()
                .unwrap();
            assert_eq!(mapping.ssd_block(), 100 + logical, "stage={stage}");
        }
    }
}

#[ktest]
fn fair_sibling_relink_accepts_existing_slot_plus_one() {
    const FS_ID: u64 = 0xf406;
    const INODE: u64 = 22;
    let (_image, _config, pool) = restartable_pool(12);
    let mutation = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();
    let entry = [ExtentRecordEntry::leaf(0, 1, 100, false).unwrap()];
    let persist_node = |logical_index, right_sibling| {
        let record = ExtentNodeRecord::new(
            FS_ID,
            INODE,
            1,
            logical_index,
            0,
            PersistentObjectRef::NONE,
            right_sibling,
            &entry,
        )
        .unwrap();
        mutation
            .persist_initialized_child(PageKey::new(FS_ID, INODE, logical_index), &record, None)
            .unwrap()
            .reference()
    };
    let old_right = persist_node(0x8200, PersistentObjectRef::NONE);
    let left = persist_node(0x8201, old_right);
    let new_right = persist_node(0x8202, old_right);
    let guard = pool.lock_metadata_mutation(left).unwrap();

    pool.persist_extent_sibling_with_guard(
        &guard,
        left,
        Some(old_right.slot_plus_one()),
        new_right,
        None,
    )
    .expect("a FAIR split must relink a node that already has a right sibling");

    let left = pool
        .read_authoritative_metadata::<ExtentNodeRecord>(left)
        .unwrap();
    assert_eq!(left.right_sibling_slot(), Some(new_right.slot_plus_one()));
}

#[ktest]
fn rnvpce_random_overwrites_leave_baseline_leaf_unchanged() {
    const FS_ID: u64 = 0xf404;
    const INODE: u64 = 20;
    const FILE_PAGES: u64 = 512;
    const SSD_START: u64 = 50_000;
    let (_image, _config, pool) = restartable_pool(512);
    let initial = [ExtentRecordEntry::leaf(0, FILE_PAGES as u32, SSD_START, false).unwrap()];
    let snapshot = AuthoritySnapshot::new(
        1,
        &initial,
        FILE_PAGES * PAGE_SIZE as u64,
        FILE_PAGES * (PAGE_SIZE / 512) as u64,
        0o100644,
        0,
    )
    .unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let inode_before = pool.fixed_inode_ref(FS_ID, INODE).unwrap().unwrap();
    let record_before = pool
        .read_authoritative_metadata::<InodeRecord>(inode_before)
        .unwrap();
    let root_before = pool
        .read_authoritative_metadata_boxed::<ExtentNodeRecord>(record_before.extent_root())
        .unwrap();

    for iteration in 0..300u64 {
        let logical = iteration * 197 % FILE_PAGES;
        let pages =
            [RangeWritePage::new(logical, SSD_START + logical, 0, PAGE_SIZE, false).unwrap()];
        let request = RangeWriteBatch::new(
            iteration + 2,
            logical * PAGE_SIZE as u64,
            PAGE_SIZE,
            &[],
            &pages,
            FILE_PAGES * PAGE_SIZE as u64,
            FILE_PAGES * (PAGE_SIZE / 512) as u64,
            (PAGE_SIZE / 512) as u64,
        )
        .unwrap();
        let input = [iteration as u8; PAGE_SIZE];
        commit_range_write(
            pool.clone(),
            FS_ID,
            INODE,
            &request,
            &mut VmReader::from(input.as_slice()).to_fallible(),
            |_block, _staging| panic!("full-page overwrite must not read SSD"),
            None,
        )
        .unwrap_or_else(|error| panic!("random overwrite {iteration} failed: {error:?}"));
        assert!(
            mapped_nv_page_ref(&pool, FS_ID, INODE, logical)
                .unwrap()
                .is_some(),
            "random overwrite {iteration} lost logical page {logical}"
        );
        for prior in 0..=iteration {
            let page = prior * 197 % FILE_PAGES;
            assert_eq!(
                current_page_mapping(&pool, FS_ID, INODE, page)
                    .unwrap()
                    .unwrap()
                    .ssd_block(),
                SSD_START + page,
                "random overwrite {iteration} misrouted logical page {page}",
            );
        }
    }
    let inode_after = pool.fixed_inode_ref(FS_ID, INODE).unwrap().unwrap();
    let record_after = pool
        .read_authoritative_metadata::<InodeRecord>(inode_after)
        .unwrap();
    let root_after = pool
        .read_authoritative_metadata_boxed::<ExtentNodeRecord>(record_after.extent_root())
        .unwrap();
    assert_eq!(root_before.as_bytes(), root_after.as_bytes());
}

#[ktest]
fn fair_split_normalizes_fast_slot_order() {
    const FS_ID: u64 = 0xf405;
    const INODE: u64 = 21;
    const SSD_START: u64 = 50_000;
    let (_image, _config, pool) = restartable_pool(512);
    let initial = [ExtentRecordEntry::leaf(0, 300, SSD_START, false).unwrap()];
    let snapshot = AuthoritySnapshot::new(
        1,
        &initial,
        300 * PAGE_SIZE as u64,
        300 * (PAGE_SIZE / 512) as u64,
        0o100644,
        0,
    )
    .unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();

    let mut logicals: Vec<u64> = (2..=162).step_by(2).collect();
    logicals.extend([1, 164]);
    for (iteration, logical) in logicals.iter().copied().enumerate() {
        let pages =
            [RangeWritePage::new(logical, SSD_START + logical, 0, PAGE_SIZE, false).unwrap()];
        let request = RangeWriteBatch::new(
            iteration as u64 + 2,
            logical * PAGE_SIZE as u64,
            PAGE_SIZE,
            &[],
            &pages,
            300 * PAGE_SIZE as u64,
            300 * (PAGE_SIZE / 512) as u64,
            (PAGE_SIZE / 512) as u64,
        )
        .unwrap();
        let input = [iteration as u8; PAGE_SIZE];
        commit_range_write(
            pool.clone(),
            FS_ID,
            INODE,
            &request,
            &mut VmReader::from(input.as_slice()).to_fallible(),
            |_block, _staging| panic!("full-page overwrite must not read SSD"),
            None,
        )
        .unwrap_or_else(|error| panic!("FAST overwrite {iteration} failed: {error:?}"));
    }

    for page in 0..300 {
        assert_eq!(
            current_page_mapping(&pool, FS_ID, INODE, page)
                .unwrap()
                .unwrap()
                .ssd_block(),
            SSD_START + page,
            "FAIR normalization misrouted logical page {page}",
        );
    }
    assert!(
        mapped_nv_page_ref(&pool, FS_ID, INODE, 164)
            .unwrap()
            .is_some()
    );
}

fn restartable_pool(total: usize) -> (DramNvImage, RnvpcConfig, Arc<Rnvpc>) {
    let config = RnvpcConfig::from_bytes(
        Some(total * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some((total - 1) * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let pool = Arc::new(
        Rnvpc::format(Arc::new(image.attach()), config, None).expect("fresh image should format"),
    );
    (image, config, pool)
}

fn persist_recovery_descriptor(image: &DramNvImage, slot: usize, descriptor: NvDescriptor) {
    let backend = image.attach();
    let selector = 1;
    let descriptor_offset = descriptor_offset(slot, selector).unwrap();
    backend
        .write_control(descriptor_offset, descriptor.encode())
        .unwrap();
    backend
        .persist_control(descriptor_offset, size_of::<NvDescriptor>())
        .unwrap();
    backend.fence().unwrap();
    let commit_offset = commit_offset(slot).unwrap();
    backend.store_control_u64(commit_offset, selector).unwrap();
    backend
        .persist_control(commit_offset, size_of::<u64>())
        .unwrap();
    backend.fence().unwrap();
}

fn staged_redirect_page(pool: &Arc<Rnvpc>, fill: u8) -> NvPageId {
    let page = pool.stage_data_page(&[fill; PAGE_SIZE]).unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    page
}

#[ktest]
fn rnvpce_redirect_publish_survives_every_selector_boundary() {
    const KEY: PageKey = PageKey::new(0xa101, 31, 7);
    for (fault, selected, retained) in [
        (FaultPoint::BeforeContentPersist, false, false),
        (FaultPoint::AfterContentPersist, false, false),
        (FaultPoint::AfterCommitStore, false, true),
        (FaultPoint::AfterCommitPersist, true, true),
        (FaultPoint::AfterCommitFence, true, true),
    ] {
        let (image, config, pool) = restartable_pool(8);
        let page = staged_redirect_page(&pool, 0x51);
        assert!(
            pool.publish_redirect_batch(&[(page, KEY, 9007)], Some(fault))
                .is_err()
        );
        assert_eq!(
            pool.data_page_state(page).unwrap() != DataPageState::Free,
            retained,
            "a possibly selected slot must not be reused after {fault:?}",
        );
        drop(pool);
        image.simulate_crash();
        let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
        assert_eq!(recovered.redirect_lookup(KEY).is_some(), selected);
    }
}

#[ktest]
fn rnvpce_concurrent_publishers_get_distinct_logical_versions() {
    const KEY: PageKey = PageKey::new(0xa102, 32, 8);
    let pool = test_pool(16, 1, 15, None);
    let first = staged_redirect_page(&pool, 0x61);
    let second = staged_redirect_page(&pool, 0x62);
    let versions = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let ready = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for (worker, page) in [first, second].into_iter().enumerate() {
        let pool = pool.clone();
        let versions = versions.clone();
        let ready = ready.clone();
        let go = go.clone();
        threads.push(
            crate::thread::kernel_thread::ThreadOptions::new(move || {
                ready.fetch_add(1, Ordering::AcqRel);
                while go.load(Ordering::Acquire) == 0 {
                    crate::thread::Thread::yield_now();
                }
                let handle = pool
                    .publish_redirect_batch(&[(page, KEY, 9008)], None)
                    .unwrap()[0];
                versions[worker].store(handle.logical_version, Ordering::Release);
            })
            .spawn(),
        );
    }
    while ready.load(Ordering::Acquire) != 2 {
        crate::thread::Thread::yield_now();
    }
    go.store(1, Ordering::Release);
    for thread in threads {
        thread.join();
    }
    let mut versions = versions
        .iter()
        .map(|version| version.load(Ordering::Acquire))
        .collect::<Vec<_>>();
    versions.sort_unstable();
    assert_eq!(versions, vec![1, 2]);
    assert_eq!(pool.redirect_lookup(KEY).unwrap().logical_version, 2);
}

#[ktest]
fn rnvpce_logical_version_overflow_rejects_publication() {
    const KEY: PageKey = PageKey::new(0xa103, 33, 9);
    let pool = test_pool(8, 1, 7, None);
    let existing = staged_redirect_page(&pool, 0x71);
    let reference = PersistentObjectRef::new(existing.index() as u64, 1).unwrap();
    pool.install_redirect_for_test(
        KEY,
        DescriptorHandle {
            reference,
            logical_version: u64::MAX,
            state: NvObjectState::NvDirty,
            ssd_target: 9009,
            migration_target: false,
        },
    )
    .unwrap();
    let page = staged_redirect_page(&pool, 0x72);
    assert_eq!(
        pool.publish_redirect_batch(&[(page, KEY, 9009)], None)
            .unwrap_err()
            .error(),
        Errno::EOVERFLOW
    );
}

#[ktest]
fn rnvpce_slot_reuse_rejects_old_allocation_generation() {
    const KEY: PageKey = PageKey::new(0xa104, 34, 10);
    let pool = test_pool(8, 1, 7, None);
    let page = staged_redirect_page(&pool, 0x81);
    let old = pool
        .publish_redirect_batch(&[(page, KEY, 9010)], None)
        .unwrap()[0];
    let reclaimed = pool.clear_redirects_exact(&[(KEY, old)], None).unwrap();
    assert_eq!(reclaimed, vec![page]);
    pool.free_data_page(page).unwrap();
    let reused = staged_redirect_page(&pool, 0x82);
    assert_eq!(reused, page);
    match pool.pin_redirect(KEY, old) {
        Err(error) => assert_eq!(error.error(), Errno::ESTALE),
        Ok(_) => panic!("stale allocation generation was pinned"),
    }
}

#[ktest]
fn rnvpce_failed_publish_aborts_only_its_pending_token() {
    const KEY: PageKey = PageKey::new(0xa105, 35, 11);
    let pool = test_pool(8, 1, 7, None);
    let failed = staged_redirect_page(&pool, 0x91);
    assert!(
        pool.publish_redirect_batch(
            &[(failed, KEY, 9011)],
            Some(FaultPoint::BeforeContentPersist),
        )
        .is_err()
    );
    let committed = staged_redirect_page(&pool, 0x92);
    let handle = pool
        .publish_redirect_batch(&[(committed, KEY, 9011)], None)
        .unwrap()[0];
    assert_eq!(handle.logical_version, 1);
}

#[ktest]
fn rnvpce_uncertain_selector_latches_recovery_required() {
    let pool = test_pool(8, 1, 7, None);
    let first = staged_redirect_page(&pool, 0xa1);
    assert!(
        pool.publish_redirect_batch(
            &[(first, PageKey::new(0xa106, 36, 12), 9012)],
            Some(FaultPoint::AfterCommitStore),
        )
        .is_err()
    );
    let second = staged_redirect_page(&pool, 0xa2);
    assert_eq!(
        pool.publish_redirect_batch(&[(second, PageKey::new(0xa106, 36, 13), 9013)], None,)
            .unwrap_err()
            .error(),
        Errno::EUCLEAN
    );
}

#[ktest]
fn rnvpce_clearing_old_descriptor_keeps_new_redirect() {
    const KEY: PageKey = PageKey::new(0xa107, 37, 14);
    let pool = test_pool(8, 1, 7, None);
    let old_page = staged_redirect_page(&pool, 0xb1);
    let old = pool
        .publish_redirect_batch(&[(old_page, KEY, 9014)], None)
        .unwrap()[0];
    let new_page = staged_redirect_page(&pool, 0xb2);
    let new = pool
        .publish_redirect_batch(&[(new_page, KEY, 9014)], None)
        .unwrap()[0];

    assert_eq!(
        pool.clear_redirects_exact(&[(KEY, old)], None).unwrap(),
        vec![old_page]
    );
    assert_eq!(pool.redirect_lookup(KEY), Some(new));
    let pin = pool.pin_redirect(KEY, new).unwrap();
    let mut bytes = [0; PAGE_SIZE];
    pin.read(&mut bytes).unwrap();
    assert_eq!(bytes, [0xb2; PAGE_SIZE]);
}

#[ktest]
fn rnvpce_redirect_io_middle_page_overwrite_keeps_extent_tree_bytes_unchanged() {
    const FS_ID: u64 = 0xa201;
    const INODE: u64 = 41;
    let pool = test_pool(32, 8, 24, None);
    let baseline = ExtentRecordEntry::leaf(0, 8, 20_000, false).unwrap();
    let baselines = [baseline];
    let snapshot = AuthoritySnapshot::new(
        1,
        &baselines,
        8 * PAGE_SIZE as u64,
        (8 * PAGE_SIZE / 512) as u64,
        0o100644,
        0,
    )
    .unwrap();
    let authority = bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let before = pool
        .read_metadata_record::<ExtentNodeRecord>(
            NvFrameId::try_from(authority.extent_root().slot().unwrap() as usize).unwrap(),
        )
        .unwrap();
    let spans = [RangeWritePage::new(4, 20_004, 0, PAGE_SIZE, false).unwrap()];
    let request = RangeWriteBatch::new(
        2,
        4 * PAGE_SIZE as u64,
        PAGE_SIZE,
        &[],
        &spans,
        8 * PAGE_SIZE as u64,
        (8 * PAGE_SIZE / 512) as u64,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = [0xc1; PAGE_SIZE];
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(input.as_slice()).to_fallible(),
        |_, _| panic!("full overwrite must not load SSD"),
        None,
    )
    .unwrap();
    let after = pool
        .read_metadata_record::<ExtentNodeRecord>(
            NvFrameId::try_from(authority.extent_root().slot().unwrap() as usize).unwrap(),
        )
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        pool.redirect_lookup(PageKey::new(FS_ID, INODE, 4))
            .unwrap()
            .ssd_target,
        20_004
    );
}

#[ktest]
fn rnvpce_redirect_io_reads_mixed_ssd_and_redirect_pages() {
    const KEY: PageKey = PageKey::new(0xa202, 42, 3);
    let pool = test_pool(8, 1, 7, None);
    let page = staged_redirect_page(&pool, 0xd1);
    let handle = pool
        .publish_redirect_batch(&[(page, KEY, 30_003)], None)
        .unwrap()[0];
    assert_eq!(
        mapped_nv_page_ref(&pool, KEY.fs_id, KEY.inode, 2).unwrap(),
        None
    );
    assert_eq!(
        mapped_nv_page_ref(&pool, KEY.fs_id, KEY.inode, 3).unwrap(),
        Some(handle.reference)
    );
    let pin = pool.pin_redirect(KEY, handle).unwrap();
    let mut bytes = [0; PAGE_SIZE];
    pin.read(&mut bytes).unwrap();
    assert_eq!(bytes, [0xd1; PAGE_SIZE]);
}

#[allow(dead_code)]
fn rnvpce_redirect_clear_waits_for_exact_reader_pin() {
    const KEY: PageKey = PageKey::new(0xa202, 42, 4);
    let pool = test_pool(8, 1, 7, None);
    let page = staged_redirect_page(&pool, 0xd2);
    let dirty = pool
        .publish_redirect_batch(&[(page, KEY, 30_004)], None)
        .unwrap()[0];
    let writeback = pool
        .transition_redirects(&[(KEY, dirty)], NvObjectState::NvWriteback, None)
        .unwrap()[0];
    let reader = pool.pin_redirect(KEY, writeback).unwrap();
    assert_eq!(
        pool.clear_redirects_exact(&[(KEY, writeback)], None)
            .unwrap_err()
            .error(),
        Errno::EBUSY
    );
    assert_eq!(pool.redirect_lookup(KEY), Some(writeback));
    drop(reader);
    assert_eq!(
        pool.clear_redirects_exact(&[(KEY, writeback)], None)
            .unwrap(),
        vec![page]
    );
}

#[ktest]
fn rnvpce_redirect_io_128k_overwrite_publishes_one_redirect_range() {
    const FS_ID: u64 = 0xa203;
    const INODE: u64 = 43;
    let pool = test_pool(64, 8, 56, None);
    let baseline = ExtentRecordEntry::leaf(0, 32, 40_000, false).unwrap();
    let baselines = [baseline];
    let snapshot = AuthoritySnapshot::new(
        1,
        &baselines,
        32 * PAGE_SIZE as u64,
        (32 * PAGE_SIZE / 512) as u64,
        0o100644,
        0,
    )
    .unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let spans = (0..32)
        .map(|page| RangeWritePage::new(page, 40_000 + page, 0, PAGE_SIZE, false).unwrap())
        .collect::<Vec<_>>();
    let request = RangeWriteBatch::new(
        2,
        0,
        32 * PAGE_SIZE,
        &[],
        &spans,
        32 * PAGE_SIZE as u64,
        (32 * PAGE_SIZE / 512) as u64,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = [0xe1; 32 * PAGE_SIZE];
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(input.as_slice()).to_fallible(),
        |_, _| panic!("full overwrite must not load SSD"),
        None,
    )
    .unwrap();
    let ranges = pool.redirect_dirty_ranges_for_test(64);
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].pages.len(), 32);
}

#[ktest]
fn rnvpce_initial_range_capacity_count_does_not_reread_extent_tree() {
    const FS_ID: u64 = 0xa20a;
    const INODE: u64 = 49;
    let pool = test_pool(64, 8, 56, None);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let spans = (0..32)
        .map(|page| RangeWritePage::new(page, 0, 0, PAGE_SIZE, true).unwrap())
        .collect::<Vec<_>>();
    let request = RangeWriteBatch::new(
        2,
        0,
        32 * PAGE_SIZE,
        &[],
        &spans,
        0,
        0,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let reads = pool.authoritative_metadata_reads();

    assert_eq!(
        range_write_new_pages(&pool, FS_ID, INODE, &request).unwrap(),
        32
    );
    assert_eq!(pool.authoritative_metadata_reads() - reads, 0);
}

#[ktest]
fn rnvpce_initial_range_commit_does_not_revalidate_each_page() {
    const FS_ID: u64 = 0xa20b;
    const INODE: u64 = 50;
    let pool = test_pool(64, 8, 56, None);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let spans = (0..32)
        .map(|page| RangeWritePage::new(page, 50_000 + page, 0, PAGE_SIZE, true).unwrap())
        .collect::<Vec<_>>();
    let request = RangeWriteBatch::new(
        2,
        0,
        32 * PAGE_SIZE,
        &[],
        &spans,
        0,
        0,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let lookups = pool.extent_tree_lookups();
    let input = [0xe2; 32 * PAGE_SIZE];

    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(input.as_slice()).to_fallible(),
        |_, _| panic!("full initial write must not load SSD"),
        None,
    )
    .unwrap();

    assert_eq!(
        pool.extent_tree_lookups() - lookups,
        0,
        "initial range commit reread the extent tree once per page"
    );
}

fn publish_test_redirect_range(pool: &Arc<Rnvpc>, fs_id: u64, inode: u64, pages: usize) {
    let staged = (0..pages)
        .map(|page| {
            (
                staged_redirect_page(pool, page as u8),
                PageKey::new(fs_id, inode, page as u64),
                60_000 + page as u64,
            )
        })
        .collect::<Vec<_>>();
    pool.publish_redirect_batch(&staged, None).unwrap();
}

fn dirty_nv_entry_foreground_publishes_one_triple_contiguous_entry() {
    const FS_ID: u64 = 0xb201;
    const INODE: u64 = 71;
    let pool = test_pool(48, 8, 40, None);
    publish_test_redirect_range(&pool, FS_ID, INODE, 32);

    let claimed = pool.claim_ready_nv_entries(FS_ID, 8).unwrap();
    assert_eq!(claimed.len(), 1);
    let entry = claimed[0].entry();
    assert_eq!(entry.logical_range(), 0..32);
    let nv_range = entry.nv_slot_range();
    assert_eq!(nv_range.end - nv_range.start, 32);
    assert_eq!(entry.ssd_block_range(), 60_000..60_032);
    let persisted = pool
        .persisted_redirect_extent_for_test(entry.nv_slot_range().start as usize)
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted.authority().unwrap(),
        NvRedirectAuthority::NvAuthoritative
    );
    assert_eq!(persisted.logical_range().unwrap(), 0..32);
}

fn dirty_nv_entry_foreground_splits_at_nv_and_ssd_discontinuities() {
    const FS_ID: u64 = 0xb202;
    const INODE: u64 = 72;
    let pool = test_pool(24, 4, 20, None);
    let first = staged_redirect_page(&pool, 1);
    let gap = staged_redirect_page(&pool, 2);
    let third = staged_redirect_page(&pool, 3);
    pool.publish_redirect_batch(
        &[
            (first, PageKey::new(FS_ID, INODE, 0), 70_000),
            (third, PageKey::new(FS_ID, INODE, 1), 70_001),
        ],
        None,
    )
    .unwrap();
    pool.abort_data_page(gap).unwrap();

    let nv_fragmented = pool.claim_ready_nv_entries(FS_ID, 8).unwrap();
    assert_eq!(nv_fragmented.len(), 2);
    drop(nv_fragmented);

    let fourth = staged_redirect_page(&pool, 4);
    let fifth = staged_redirect_page(&pool, 5);
    pool.publish_redirect_batch(
        &[
            (fourth, PageKey::new(FS_ID, INODE, 2), 80_000),
            (fifth, PageKey::new(FS_ID, INODE, 3), 90_000),
        ],
        None,
    )
    .unwrap();
    let all = pool.claim_ready_nv_entries(FS_ID, 16).unwrap();
    assert_eq!(all.len(), 4);
}

fn dirty_nv_entry_foreground_preserves_publication_boundaries() {
    const FS_ID: u64 = 0xb203;
    const INODE: u64 = 73;
    let pool = test_pool(16, 2, 14, None);
    publish_test_redirect_range(&pool, FS_ID, INODE, 2);
    let second = [
        (
            staged_redirect_page(&pool, 3),
            PageKey::new(FS_ID, INODE, 2),
            60_002,
        ),
        (
            staged_redirect_page(&pool, 4),
            PageKey::new(FS_ID, INODE, 3),
            60_003,
        ),
    ];
    pool.publish_redirect_batch(&second, None).unwrap();

    let claimed = pool.claim_ready_nv_entries(FS_ID, 4).unwrap();
    assert_eq!(claimed.len(), 2);
    assert_eq!(claimed[0].entry().logical_range(), 0..2);
    assert_eq!(claimed[1].entry().logical_range(), 2..4);
}

fn dirty_nv_entry_foreground_failure_exposes_no_entry() {
    const FS_ID: u64 = 0xb204;
    let pool = test_pool(8, 2, 6, None);
    let page = staged_redirect_page(&pool, 1);
    assert!(
        pool.publish_redirect_batch(
            &[(page, PageKey::new(FS_ID, 74, 0), 91_000)],
            Some(FaultPoint::BeforeContentPersist),
        )
        .is_err()
    );
    assert!(pool.claim_ready_nv_entries(FS_ID, 1).unwrap().is_empty());
}

fn partial_overwrite_redirects_whole_pages_without_splitting_ssd_extent() {
    const FS_ID: u64 = 0xb205;
    const INODE: u64 = 75;
    const SSD_START: u64 = 100_000;
    let pool = test_pool(24, 4, 20, None);
    let baseline = [ExtentRecordEntry::leaf(0, 4, SSD_START, false).unwrap()];
    let snapshot = AuthoritySnapshot::new(
        1,
        &baseline,
        (4 * PAGE_SIZE) as u64,
        (4 * PAGE_SIZE / 512) as u64,
        0o100644,
        0,
    )
    .unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let pages = [
        RangeWritePage::new(1, SSD_START + 1, 0, PAGE_SIZE, false).unwrap(),
        RangeWritePage::new(2, SSD_START + 2, 0, PAGE_SIZE / 2, false).unwrap(),
    ];
    let request = RangeWriteBatch::new(
        2,
        PAGE_SIZE as u64,
        PAGE_SIZE + PAGE_SIZE / 2,
        &[],
        &pages,
        (4 * PAGE_SIZE) as u64,
        (4 * PAGE_SIZE / 512) as u64,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = vec![0xa1; PAGE_SIZE + PAGE_SIZE / 2];
    let ssd_reads = AtomicUsize::new(0);
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(input.as_slice()).to_fallible(),
        |block, staging| {
            assert_eq!(block, SSD_START + 2);
            ssd_reads.fetch_add(1, Ordering::Relaxed);
            staging.write_all(&[0x22; PAGE_SIZE])
        },
        None,
    )
    .unwrap();
    assert_eq!(ssd_reads.load(Ordering::Relaxed), 1);

    let claimed = pool.claim_ready_nv_entries(FS_ID, 1).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].entry().logical_range(), 1..3);
    assert_eq!(
        claimed[0].entry().ssd_block_range(),
        SSD_START + 1..SSD_START + 3
    );
    drop(claimed);

    for (logical, expected_tail) in [(1, 0xa1), (2, 0x22)] {
        let reference = mapped_nv_page_ref(&pool, FS_ID, INODE, logical)
            .unwrap()
            .unwrap();
        let page = NvPageId::try_from(reference.slot().unwrap() as usize).unwrap();
        let pin = pool.pin_data_page(page).unwrap();
        let mut bytes = [0; PAGE_SIZE];
        pin.read(&mut bytes).unwrap();
        assert!(bytes[..PAGE_SIZE / 2].iter().all(|byte| *byte == 0xa1));
        assert!(
            bytes[PAGE_SIZE / 2..]
                .iter()
                .all(|byte| *byte == expected_tail)
        );
    }

    let mut extents = Vec::new();
    visit_ssd_extents(&pool, FS_ID, INODE, |entry| {
        extents.push((entry.logical_block(), entry.length(), entry.ssd_block()?));
        Ok(())
    })
    .unwrap();
    assert_eq!(extents, vec![(0, 4, SSD_START)]);
}

#[ktest]
fn rnvpce_existing_nv_entry_overwrite_reuses_owned_pages() {
    const FS_ID: u64 = 0xb206;
    const INODE: u64 = 76;
    const SSD_START: u64 = 110_000;
    let pool = test_pool(16, 2, 14, None);
    let baseline = [ExtentRecordEntry::leaf(0, 2, SSD_START, false).unwrap()];
    let snapshot = AuthoritySnapshot::new(
        1,
        &baseline,
        (2 * PAGE_SIZE) as u64,
        (2 * PAGE_SIZE / 512) as u64,
        0o100644,
        0,
    )
    .unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let pages = [
        RangeWritePage::new(0, SSD_START, 0, PAGE_SIZE, false).unwrap(),
        RangeWritePage::new(1, SSD_START + 1, 0, PAGE_SIZE, false).unwrap(),
    ];
    let first = RangeWriteBatch::new(
        2,
        0,
        2 * PAGE_SIZE,
        &[],
        &pages,
        (2 * PAGE_SIZE) as u64,
        (2 * PAGE_SIZE / 512) as u64,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &first,
        &mut VmReader::from([0x31; 2 * PAGE_SIZE].as_slice()).to_fallible(),
        |_, _| panic!("full overwrite must not load SSD"),
        None,
    )
    .unwrap();
    let before = pages.map(|page| {
        pool.redirect_lookup(PageKey::new(FS_ID, INODE, page.logical_block()))
            .unwrap()
            .reference
    });

    let second = RangeWriteBatch::new(
        3,
        0,
        2 * PAGE_SIZE,
        &[],
        &pages,
        (2 * PAGE_SIZE) as u64,
        (2 * PAGE_SIZE / 512) as u64,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    assert_eq!(
        range_write_new_pages(&pool, FS_ID, INODE, &second).unwrap(),
        0
    );
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &second,
        &mut VmReader::from([0x72; 2 * PAGE_SIZE].as_slice()).to_fallible(),
        |_, _| panic!("NV overwrite must not load SSD"),
        None,
    )
    .unwrap();

    for (page, expected_reference) in pages.iter().zip(before) {
        let key = PageKey::new(FS_ID, INODE, page.logical_block());
        let handle = pool.redirect_lookup(key).unwrap();
        assert_eq!(handle.reference, expected_reference);
        let pin = pool.pin_redirect(key, handle).unwrap();
        let mut bytes = [0; PAGE_SIZE];
        pin.read(&mut bytes).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0x72));
    }
    let claimed = pool.claim_ready_nv_entries(FS_ID, 2).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].entry().logical_range(), 0..2);
}

#[ktest]
fn rnvpce_dirty_nv_entry_foreground() {
    dirty_nv_entry_foreground_publishes_one_triple_contiguous_entry();
    dirty_nv_entry_foreground_splits_at_nv_and_ssd_discontinuities();
    dirty_nv_entry_foreground_preserves_publication_boundaries();
    dirty_nv_entry_foreground_failure_exposes_no_entry();
    partial_overwrite_redirects_whole_pages_without_splitting_ssd_extent();
}

#[ktest]
fn rnvpce_nv_entry_claim() {
    const FS_ID: u64 = 0xb207;
    const OTHER_FS_ID: u64 = 0xb208;
    const INODE: u64 = 77;
    let pool = test_pool(16, 2, 14, None);
    publish_test_redirect_range(&pool, FS_ID, INODE, 2);
    publish_test_redirect_range(&pool, OTHER_FS_ID, INODE + 1, 1);

    let entry = pool
        .lookup_dirty_nv_entry(PageKey::new(FS_ID, INODE, 0))
        .unwrap();
    let lease = pool.claim_ready_nv_entries(FS_ID, 2).unwrap().remove(0);
    assert_eq!(entry.state(), NvExtentEntryState::Migrating);
    assert_eq!(entry.try_write().err().unwrap().error(), Errno::EBUSY);

    let reader = entry.try_read().unwrap();
    assert_eq!(reader.nv_slot(0).unwrap(), entry.nv_slot_range().start);
    pool.data_segment_from_entry(&reader, 1).unwrap();
    let other = pool
        .lookup_dirty_nv_entry(PageKey::new(OTHER_FS_ID, INODE + 1, 0))
        .unwrap();
    let disjoint_writer = other.try_write().unwrap();

    pool.complete_nv_entry(&lease).unwrap();
    assert_eq!(entry.state(), NvExtentEntryState::IoComplete);
    let state = Arc::new(AtomicUsize::new(0));
    let thread_state = state.clone();
    let reclaim = crate::thread::kernel_thread::ThreadOptions::new(move || {
        thread_state.store(1, Ordering::Release);
        lease.begin_reclaim_and_wait_for_readers().unwrap();
        thread_state.store(2, Ordering::Release);
        lease.finish_reclaim().unwrap();
        thread_state.store(3, Ordering::Release);
    })
    .spawn();
    while entry.state() != NvExtentEntryState::Reclaiming {
        crate::thread::Thread::yield_now();
    }
    assert_eq!(state.load(Ordering::Acquire), 1);
    assert_eq!(entry.try_read().err().unwrap().error(), Errno::ESTALE);
    drop(disjoint_writer);
    drop(reader);
    reclaim.join();
    assert_eq!(state.load(Ordering::Acquire), 3);
    assert_eq!(entry.state(), NvExtentEntryState::Reclaimed);
}

#[ktest]
fn rnvpce_nv_entry_work_item() {
    const FS_ID: u64 = 0xb209;
    const INODE: u64 = 78;
    let pool = test_pool(16, 2, 14, None);
    publish_test_redirect_range(&pool, FS_ID, INODE, 4);
    assert_eq!(pool.stats().data_pages, 4);

    let lease = pool.claim_ready_nv_entries(FS_ID, 4).unwrap().remove(0);
    let entry = lease.entry().clone();
    let head = entry.nv_slot_range().start as usize;
    pool.complete_nv_entry(&lease).unwrap();
    assert_eq!(pool.reclaim_nv_entry(&lease).unwrap(), 4);

    assert_eq!(entry.state(), NvExtentEntryState::Reclaimed);
    assert_eq!(pool.stats().data_pages, 0);
    for logical in 0..4 {
        let key = PageKey::new(FS_ID, INODE, logical);
        assert!(pool.lookup_dirty_nv_entry(key).is_none());
        assert!(pool.redirect_lookup(key).is_none());
    }
    assert!(
        pool.persisted_redirect_extent_for_test(head)
            .unwrap()
            .is_none()
    );
}

#[ktest]
fn rnvpce_data_slot_recovery() {
    const FS_ID: u64 = 0xb20a;
    const INODE: u64 = 79;
    let (image, config, pool) = restartable_pool(16);
    publish_test_redirect_range(&pool, FS_ID, INODE, 4);
    let published = pool
        .lookup_dirty_nv_entry(PageKey::new(FS_ID, INODE, 0))
        .unwrap();
    let head = published.nv_slot_range().start as usize;
    assert!(pool.persistent_slot_is_selected_for_test(head).unwrap());
    for tail in head + 1..head + 4 {
        assert!(
            !pool.persistent_slot_is_selected_for_test(tail).unwrap(),
            "tail NVPage must not carry a per-page persistent descriptor"
        );
    }
    drop(published);
    drop(pool);
    image.simulate_crash();
    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let entry = recovered
        .lookup_dirty_nv_entry(PageKey::new(FS_ID, INODE, 2))
        .unwrap();
    assert_eq!(entry.logical_range(), 0..4);
    assert_eq!(recovered.claim_ready_nv_entries(FS_ID, 1).unwrap().len(), 1);
    drop(recovered);

    let (pending_image, pending_config, pending_pool) = restartable_pool(16);
    publish_test_redirect_range(&pending_pool, FS_ID + 1, INODE + 1, 2);
    let pending_entry = pending_pool
        .lookup_dirty_nv_entry(PageKey::new(FS_ID + 1, INODE + 1, 0))
        .unwrap();
    pending_pool
        .mark_redirect_extent_pending_for_test(&pending_entry)
        .unwrap();
    drop(pending_entry);
    drop(pending_pool);
    pending_image.simulate_crash();
    let pending_recovered =
        Arc::new(Rnvpc::recover(Arc::new(pending_image.attach()), pending_config, None).unwrap());
    assert_eq!(pending_recovered.stats().data_pages, 0);
    assert!(
        pending_recovered
            .lookup_dirty_nv_entry(PageKey::new(FS_ID + 1, INODE + 1, 0))
            .is_none()
    );
    assert!(
        pending_recovered
            .claim_ready_nv_entries(FS_ID + 1, 1)
            .unwrap()
            .is_empty()
    );
}

#[allow(dead_code)]
fn rnvpce_dirty_extent_claim_is_dram_only_and_requeues_on_drop() {
    const FS_ID: u64 = 0xa204;
    const INODE: u64 = 44;
    let pool = test_pool(16, 2, 14, None);
    publish_test_redirect_range(&pool, FS_ID, INODE, 4);

    let lease = pool.claim_ready_dirty_extents(FS_ID, 4).unwrap().remove(0);
    let claimed = pool.claim_dirty_extent(lease).unwrap();
    assert_eq!(claimed.pages().len(), 4);
    assert!(pool.migration_overlap(FS_ID, INODE, 0, 4));
    assert!(!pool.migration_overlap(FS_ID, INODE, 4, 4));
    for page in 0..4 {
        let handle = pool
            .redirect_lookup(PageKey::new(FS_ID, INODE, page))
            .unwrap();
        assert_eq!(handle.state, NvObjectState::NvDirty);
        assert_eq!(
            pool.persisted_redirect_state_for_test(handle).unwrap(),
            NvObjectState::NvDirty
        );
    }
    let first_key = PageKey::new(FS_ID, INODE, 0);
    let first_handle = pool.redirect_lookup(first_key).unwrap();
    assert_eq!(
        pool.clear_redirects_exact(&[(first_key, first_handle)], None)
            .unwrap_err()
            .error(),
        Errno::EBUSY
    );
    drop(claimed);

    assert!(!pool.migration_overlap(FS_ID, INODE, 0, 4));
    assert_eq!(
        pool.claim_ready_dirty_extents(FS_ID, 4)
            .unwrap()
            .remove(0)
            .extent()
            .len(),
        4
    );
}

#[allow(dead_code)]
fn rnvpce_redirect_claim_claims_32_pages_with_one_range_snapshot() {
    let pool = test_pool(48, 8, 40, None);
    publish_test_redirect_range(&pool, 0xa205, 45, 32);
    let snapshot = pool.dirty_redirect_ranges(0xa205, 32).remove(0);
    let claimed = pool.claim_redirect_range(&snapshot, 32).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].pages.len(), 32);
    assert_eq!(claimed[0].baseline_token.logical_start, 0);
    assert_eq!(claimed[0].baseline_token.generations, vec![0; 32]);
}

#[allow(dead_code)]
fn rnvpce_redirect_claim_splits_around_one_stale_page() {
    const FS_ID: u64 = 0xa206;
    const INODE: u64 = 46;
    let pool = test_pool(16, 2, 14, None);
    publish_test_redirect_range(&pool, FS_ID, INODE, 4);
    let snapshot = pool.dirty_redirect_ranges(FS_ID, 4).remove(0);
    let replacement = staged_redirect_page(&pool, 0xee);
    pool.publish_redirect_batch(
        &[(replacement, PageKey::new(FS_ID, INODE, 2), 60_002)],
        None,
    )
    .unwrap();

    let claimed = pool.claim_redirect_range(&snapshot, 4).unwrap();
    assert_eq!(
        claimed
            .iter()
            .map(|range| range.pages.len())
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(
        pool.redirect_lookup(PageKey::new(FS_ID, INODE, 2))
            .unwrap()
            .state,
        NvObjectState::NvDirty
    );
}

#[allow(dead_code)]
fn rnvpce_redirect_claim_obeys_budget_without_losing_tail() {
    const FS_ID: u64 = 0xa207;
    let pool = test_pool(12, 2, 10, None);
    publish_test_redirect_range(&pool, FS_ID, 47, 4);
    let snapshot = pool.dirty_redirect_ranges(FS_ID, 4).remove(0);
    let claimed = pool.claim_redirect_range(&snapshot, 2).unwrap();
    assert_eq!(
        claimed.iter().map(|range| range.pages.len()).sum::<usize>(),
        2
    );
    assert_eq!(
        pool.dirty_redirect_ranges(FS_ID, 4)
            .iter()
            .map(|range| range.pages.len())
            .sum::<usize>(),
        2
    );
}

#[allow(dead_code)]
fn rnvpce_redirect_claim_reads_no_extent_leaf() {
    const FS_ID: u64 = 0xa208;
    let pool = test_pool(12, 2, 10, None);
    publish_test_redirect_range(&pool, FS_ID, 48, 4);
    let routes = pool.extent_tree_routes();
    let leaf_reads = pool.migration_claim_leaf_reads();
    let snapshot = pool.dirty_redirect_ranges(FS_ID, 4).remove(0);
    let claimed = pool.claim_redirect_range(&snapshot, 4).unwrap();
    assert_eq!(claimed[0].pages.len(), 4);
    assert_eq!(pool.extent_tree_routes(), routes);
    assert_eq!(pool.migration_claim_leaf_reads(), leaf_reads);
}

#[allow(dead_code)]
fn rnvpce_redirect_completion_older_bio_owns_page_key_until_release() {
    const KEY: PageKey = PageKey::new(0xa209, 49, 0);
    let pool = test_pool(8, 1, 7, None);
    publish_test_redirect_range(&pool, KEY.fs_id, KEY.inode, 1);
    let old_snapshot = pool.dirty_redirect_ranges(KEY.fs_id, 1).remove(0);
    let old_claim = pool.claim_redirect_range(&old_snapshot, 1).unwrap();
    assert_eq!(old_claim[0].pages.len(), 1);

    let replacement = staged_redirect_page(&pool, 0xf1);
    pool.publish_redirect_batch(&[(replacement, KEY, 60_000)], None)
        .unwrap();
    let new_snapshot = pool.dirty_redirect_ranges(KEY.fs_id, 1).remove(0);
    assert!(
        pool.claim_redirect_range(&new_snapshot, 1)
            .unwrap()
            .is_empty(),
        "new generation must not submit while the old SSD BIO owns PageKey"
    );
    pool.release_baseline_migration(KEY);
    assert_eq!(
        pool.claim_redirect_range(&new_snapshot, 1)
            .unwrap()
            .iter()
            .map(|range| range.pages.len())
            .sum::<usize>(),
        1
    );
}

#[allow(dead_code)]
fn rnvpce_redirect_completion_detects_baseline_generation_change() {
    const FS_ID: u64 = 0xa20a;
    let pool = test_pool(8, 1, 7, None);
    publish_test_redirect_range(&pool, FS_ID, 50, 1);
    let snapshot = pool.dirty_redirect_ranges(FS_ID, 1).remove(0);
    let claimed = pool.claim_redirect_range(&snapshot, 1).unwrap();
    assert!(pool.validate_baseline_token(&claimed[0].baseline_token));
    let key = claimed[0].pages[0].key;
    assert!(pool.begin_baseline_mutation(&[key]).is_err());
    pool.release_baseline_migration(key);
    pool.advance_baseline_generation(FS_ID, 50, 0, 1).unwrap();
    assert!(!pool.validate_baseline_token(&claimed[0].baseline_token));
}

#[allow(dead_code)]
fn rnvpce_redirect_io_write_during_old_writeback_publishes_cow_generation() {
    const KEY: PageKey = PageKey::new(0xa204, 44, 5);
    let pool = test_pool(12, 1, 11, None);
    let old_page = staged_redirect_page(&pool, 0xf1);
    let old = pool
        .publish_redirect_batch(&[(old_page, KEY, 50_005)], None)
        .unwrap()[0];
    let writeback = pool
        .transition_redirects(&[(KEY, old)], NvObjectState::NvWriteback, None)
        .unwrap()[0];
    let new_page = staged_redirect_page(&pool, 0xf2);
    let new = pool
        .publish_redirect_batch(&[(new_page, KEY, 50_005)], None)
        .unwrap()[0];
    assert!(new.logical_version > writeback.logical_version);
    assert_eq!(pool.redirect_lookup(KEY), Some(new));
    let old_pin = pool.pin_persistent_data_page(writeback.reference).unwrap();
    let mut old_bytes = [0; PAGE_SIZE];
    old_pin.read(&mut old_bytes).unwrap();
    assert_eq!(old_bytes, [0xf1; PAGE_SIZE]);
}

#[allow(dead_code)]
fn rnvpce_old_migration_target_cannot_mutate_new_redirect() {
    const FS_ID: u64 = 0xa20b;
    const KEY: PageKey = PageKey::new(FS_ID, 51, 6);
    let pool = test_pool(12, 1, 11, None);
    let old_page = staged_redirect_page(&pool, 0xc1);
    pool.publish_redirect_batch(&[(old_page, KEY, 60_006)], None)
        .unwrap();
    let snapshot = pool.dirty_redirect_ranges(FS_ID, 1).remove(0);
    let mut claimed = pool.claim_redirect_range(&snapshot, 1).unwrap();
    let old = claimed[0].pages[0].handle;

    let new_page = staged_redirect_page(&pool, 0xc2);
    let new = pool
        .publish_redirect_batch(&[(new_page, KEY, 60_006)], None)
        .unwrap()[0];
    pool.assign_migration_targets(&[(old.reference, 70_006)], None)
        .unwrap();
    assert_eq!(pool.redirect_lookup(KEY), Some(new));
    assert!(!new.migration_target);
    drop(claimed.remove(0));
    pool.release_baseline_migration(KEY);
}

#[ktest]
fn rnvpce_range_transaction_rejects_more_than_128_pages() {
    let spans = (0..129)
        .map(|page| RangeWritePage::new(page, 80_000 + page, 0, PAGE_SIZE, false).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        RangeWriteBatch::new(
            1,
            0,
            129 * PAGE_SIZE,
            &[],
            &spans,
            129 * PAGE_SIZE as u64,
            (129 * PAGE_SIZE / 512) as u64,
            (PAGE_SIZE / 512) as u64,
        )
        .unwrap_err()
        .error(),
        Errno::EINVAL
    );
}

fn recovery_descriptor(
    key: PageKey,
    ssd_block: u64,
    logical_version: u64,
    allocation_seq: u64,
    state: NvObjectState,
) -> NvDescriptor {
    let mut descriptor = NvDescriptor::for_nv_page(
        key.fs_id,
        key.inode,
        key.page_index,
        ssd_block,
        logical_version,
    )
    .with_allocation_seq(allocation_seq)
    .unwrap();
    if state != NvObjectState::NvDirty {
        descriptor
            .transition_to(NvObjectState::NvWriteback)
            .unwrap();
    }
    if state == NvObjectState::NvReclaimable {
        descriptor
            .transition_to(NvObjectState::NvReclaimable)
            .unwrap();
    }
    descriptor
}

#[ktest]
fn rnvpce_recovery_selects_highest_data_version_independent_of_slot_order() {
    let (image, config, pool) = restartable_pool(8);
    drop(pool);
    let key = PageKey::new(0xa701, 41, 7);
    persist_recovery_descriptor(
        &image,
        0,
        recovery_descriptor(key, 900, 9, 19, NvObjectState::NvDirty),
    );
    persist_recovery_descriptor(
        &image,
        1,
        recovery_descriptor(key, 800, 8, 18, NvObjectState::NvDirty),
    );
    image.simulate_crash();

    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let entry = recovered.indexed_data_entry(key).unwrap();
    assert_eq!(entry.reference.slot(), Some(0));
    assert_eq!(entry.reference.allocation_seq(), 19);
    assert_eq!(entry.logical_version, 9);
    assert_eq!(entry.ssd_backing_block, 900);
    assert_eq!(
        recovered.redirect_lookup(key),
        Some(DescriptorHandle {
            reference: entry.reference,
            logical_version: 9,
            state: NvObjectState::NvDirty,
            ssd_target: 900,
            migration_target: false,
        })
    );
}

#[ktest]
fn rnvpce_recovery_rejects_equal_data_versions() {
    let (image, config, pool) = restartable_pool(8);
    drop(pool);
    let key = PageKey::new(0xa702, 42, 8);
    persist_recovery_descriptor(
        &image,
        0,
        recovery_descriptor(key, 800, 11, 21, NvObjectState::NvDirty),
    );
    persist_recovery_descriptor(
        &image,
        1,
        recovery_descriptor(key, 801, 11, 22, NvObjectState::NvDirty),
    );
    image.simulate_crash();

    let error = match Rnvpc::recover(Arc::new(image.attach()), config, None) {
        Ok(_) => panic!("equal logical versions must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.error(), Errno::EUCLEAN);
}

#[ktest]
fn rnvpce_recovery_keeps_all_redirect_states_authoritative() {
    let (image, config, pool) = restartable_pool(8);
    drop(pool);
    for (slot, state) in [
        NvObjectState::NvDirty,
        NvObjectState::NvWriteback,
        NvObjectState::NvReclaimable,
    ]
    .into_iter()
    .enumerate()
    {
        persist_recovery_descriptor(
            &image,
            slot,
            recovery_descriptor(
                PageKey::new(0xa703, 43, slot as u64),
                900 + slot as u64,
                1,
                30 + slot as u64,
                state,
            ),
        );
    }
    image.simulate_crash();

    let recovered = Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap();
    for slot in 0..3 {
        let entry = recovered
            .indexed_data_entry(PageKey::new(0xa703, 43, slot as u64))
            .unwrap();
        assert_eq!(entry.state, NvObjectState::NvDirty);
        assert_eq!(
            recovered
                .data_page_nv_state(NvPageId::try_from(slot).unwrap())
                .unwrap(),
            Some(NvObjectState::NvDirty)
        );
    }
    assert_eq!(
        recovered
            .dirty_redirect_ranges(0xa703, 8)
            .iter()
            .map(|range| range.pages.len())
            .sum::<usize>(),
        3
    );
}

#[ktest]
fn rnvpce_recovery_requeues_the_persisted_migration_target() {
    let (image, config, pool) = restartable_pool(8);
    drop(pool);
    let key = PageKey::new(0xa704, 44, 9);
    let mut descriptor = recovery_descriptor(key, 0, 3, 33, NvObjectState::NvDirty);
    descriptor.assign_migration_target(12_345).unwrap();
    descriptor.mark_migration_credit_released().unwrap();
    persist_recovery_descriptor(&image, 0, descriptor);
    image.simulate_crash();

    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let handle = recovered.redirect_lookup(key).unwrap();
    assert_eq!(handle.state, NvObjectState::NvDirty);
    assert_eq!(handle.ssd_target, 12_345);
    assert!(handle.migration_target);
    assert!(
        recovered
            .persistent_data_migration_credit_released(handle.reference)
            .unwrap()
    );
    let snapshot = recovered.dirty_redirect_ranges(key.fs_id, 8).remove(0);
    assert_eq!(snapshot.pages[0], handle);
    assert_eq!(
        recovered.claim_redirect_range(&snapshot, 1).unwrap()[0]
            .pages
            .len(),
        1
    );
}

#[ktest]
fn rnvpce_recovery_cleanup_is_idempotent() {
    let (image, config, pool) = restartable_pool(8);
    drop(pool);
    let key = PageKey::new(0xa704, 44, 9);
    persist_recovery_descriptor(
        &image,
        0,
        recovery_descriptor(key, 800, 1, 40, NvObjectState::NvDirty),
    );
    persist_recovery_descriptor(
        &image,
        1,
        recovery_descriptor(key, 801, 2, 41, NvObjectState::NvDirty),
    );
    image.simulate_crash();

    let recovered = Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap();
    assert_eq!(recovered.stats().data_pages, 1);
    assert_eq!(recovered.stats().free_pages, 7);
    assert_eq!(
        recovered.indexed_data_entry(key).unwrap().reference.slot(),
        Some(1)
    );
    drop(recovered);
    image.simulate_crash();

    let recovered = Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap();
    assert_eq!(recovered.stats().data_pages, 1);
    assert_eq!(recovered.stats().free_pages, 7);
    assert_eq!(
        recovered.indexed_data_entry(key).unwrap().reference.slot(),
        Some(1)
    );
}

struct ClaimedExtentFixture {
    pool: Arc<Rnvpc>,
    identity: Arc<MigrationExtentIdentity>,
    updates: Vec<SsdMigrationPage>,
    evictions: Vec<EvictingDataPage>,
    leaf_ref: PersistentObjectRef,
}

fn claimed_extent_fixture(fs_id: u64, inode: u64, target: u64) -> ClaimedExtentFixture {
    let (_image, _config, pool) = restartable_pool(16);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), fs_id, inode, &snapshot, None).unwrap();
    let pages = [RangeWritePage::new(0, 0, 0, PAGE_SIZE, true).unwrap()];
    let request =
        RangeWriteBatch::new(2, 0, PAGE_SIZE, &[], &pages, 0, 0, (PAGE_SIZE / 512) as u64).unwrap();
    commit_range_write(
        pool.clone(),
        fs_id,
        inode,
        &request,
        &mut VmReader::from(&[0x5au8; PAGE_SIZE][..]).to_fallible(),
        |_block, _staging| panic!("full-page write must not load SSD"),
        None,
    )
    .unwrap();
    let reference = mapped_nv_page_ref(&pool, fs_id, inode, 0).unwrap().unwrap();
    let locator = pool.reverse_lookup(reference).unwrap().unwrap();
    let leaf_ref = pool
        .persistent_metadata_ref(NvFrameId::try_from(locator.leaf_slot() as usize).unwrap())
        .unwrap();
    let (identity, claimed_pages) = pool
        .claim_migration_extent(reference, locator)
        .unwrap()
        .into_parts();
    let mut updates = Vec::new();
    let mut evictions = Vec::new();
    for claimed in claimed_pages {
        claimed
            .eviction
            .assign_migration_target(claimed.reference, target + claimed.key.page_index)
            .unwrap();
        updates.push(SsdMigrationPage {
            logical_block: claimed.key.page_index,
            reference: claimed.reference,
            ssd_block: target + claimed.key.page_index,
        });
        evictions.push(claimed.eviction);
    }
    ClaimedExtentFixture {
        pool,
        identity,
        updates,
        evictions,
        leaf_ref,
    }
}

#[cfg(any())]
fn rnvpce_extent_completion_does_not_wait_for_inode_publication() {
    const FS_ID: u64 = 0x9931;
    const INODE: u64 = 41;
    let mut fixture = claimed_extent_fixture(FS_ID, INODE, 51_000);
    let publication = fixture.pool.begin_publication_for(FS_ID, INODE).unwrap();
    let state = Arc::new(AtomicUsize::new(0));
    let thread_pool = fixture.pool.clone();
    let thread_identity = fixture.identity.clone();
    let thread_updates = fixture.updates.clone();
    let thread_state = state.clone();
    let completion = crate::thread::kernel_thread::ThreadOptions::new(move || {
        thread_state.store(1, Ordering::Release);
        migrate_claimed_nv_extent_to_ssd(&thread_pool, &thread_identity, &thread_updates, None)
            .unwrap();
        thread_state.store(2, Ordering::Release);
    })
    .spawn();
    while state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..128 {
        crate::thread::Thread::yield_now();
    }
    let completed_while_publication_was_held = state.load(Ordering::Acquire) == 2;
    drop(publication);
    completion.join();
    assert!(
        completed_while_publication_was_held,
        "completion must not acquire the foreground inode publication guard"
    );
    let update = fixture.updates[0];
    fixture
        .evictions
        .pop()
        .unwrap()
        .reclaim_after_ssd_publish(update.reference, None)
        .unwrap();
}

#[cfg(any())]
fn rnvpce_extent_completion_waits_for_its_leaf_guard() {
    const FS_ID: u64 = 0x9932;
    const INODE: u64 = 42;
    let mut fixture = claimed_extent_fixture(FS_ID, INODE, 52_000);
    let leaf_guard = fixture
        .pool
        .lock_metadata_mutation(fixture.leaf_ref)
        .unwrap();
    let state = Arc::new(AtomicUsize::new(0));
    let thread_pool = fixture.pool.clone();
    let thread_identity = fixture.identity.clone();
    let thread_updates = fixture.updates.clone();
    let thread_state = state.clone();
    let completion = crate::thread::kernel_thread::ThreadOptions::new(move || {
        thread_state.store(1, Ordering::Release);
        migrate_claimed_nv_extent_to_ssd(&thread_pool, &thread_identity, &thread_updates, None)
            .unwrap();
        thread_state.store(2, Ordering::Release);
    })
    .spawn();
    while state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..128 {
        crate::thread::Thread::yield_now();
    }
    assert_eq!(state.load(Ordering::Acquire), 1);
    drop(leaf_guard);
    completion.join();
    assert_eq!(state.load(Ordering::Acquire), 2);
    let update = fixture.updates[0];
    fixture
        .evictions
        .pop()
        .unwrap()
        .reclaim_after_ssd_publish(update.reference, None)
        .unwrap();
}

#[ktest]
fn rnvpce_space_reservation_serializes_concurrent_reserve_and_consume() {
    const FS_ID: u64 = 0x9914;
    const INODE: u64 = 26;
    const ITERATIONS: u64 = 64;
    let pool = test_pool(32, 16, 16, None);

    let initial_mutation = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();
    reserve_ssd_capacity(&pool, &initial_mutation, FS_ID, INODE, 100).unwrap();
    drop(initial_mutation);

    let ready = Arc::new(AtomicUsize::new(0));
    let reserve_pool = pool.clone();
    let reserve_ready = ready.clone();
    let reserver = crate::thread::kernel_thread::ThreadOptions::new(move || {
        let foreground_mutation = ExtentMutation::new(reserve_pool.clone(), FS_ID, INODE).unwrap();
        reserve_ready.fetch_add(1, Ordering::Release);
        while reserve_ready.load(Ordering::Acquire) != 2 {
            crate::thread::Thread::yield_now();
        }
        for _ in 0..ITERATIONS {
            reserve_ssd_capacity(&reserve_pool, &foreground_mutation, FS_ID, INODE, 1).unwrap();
            crate::thread::Thread::yield_now();
        }
    })
    .spawn();

    let consume_pool = pool.clone();
    let consume_ready = ready.clone();
    let consumer = crate::thread::kernel_thread::ThreadOptions::new(move || {
        consume_ready.fetch_add(1, Ordering::Release);
        while consume_ready.load(Ordering::Acquire) != 2 {
            crate::thread::Thread::yield_now();
        }
        for _ in 0..ITERATIONS {
            consume_ssd_capacity_locked(&consume_pool, FS_ID, INODE, 1).unwrap();
            crate::thread::Thread::yield_now();
        }
    })
    .spawn();

    reserver.join();
    consumer.join();

    let reservation = space_reservation(&pool, FS_ID, INODE).unwrap().unwrap();
    assert_eq!(reservation.reserved_blocks(), 100 + ITERATIONS);
    assert_eq!(reservation.consumed_blocks(), ITERATIONS);
    assert_eq!(reservation.outstanding_blocks(), 100);
}

#[ktest]
fn rnvpce_pending_mark_update_and_reclaim_wait_for_completion_reader() {
    const FS_ID: u64 = 0x9924;
    const INODE: u64 = 34;
    for reclaim in [false, true] {
        let pool = test_pool(16, 16, 0, None);
        let mark = PendingExtentMutationRecord::prepared_split(
            FS_ID,
            INODE,
            1,
            100,
            101,
            PersistentObjectRef::new(100, 1).unwrap(),
            PersistentObjectRef::new(101, 1).unwrap(),
            PersistentObjectRef::new(102, 1).unwrap(),
            1,
        )
        .unwrap();
        let key = PageKey::new(FS_ID, INODE, PENDING_EXTENT_MUTATION_LOGICAL_INDEX);
        let reference = pool.persist_repair_record(key, &mark, true, None).unwrap();
        let frame = NvFrameId::try_from(reference.slot().unwrap() as usize).unwrap();
        let reader = pool.pin_frame(frame).unwrap();
        let state = Arc::new(AtomicUsize::new(0));
        let updater_pool = pool.clone();
        let updater_state = state.clone();
        let updater = crate::thread::kernel_thread::ThreadOptions::new(move || {
            let publication = updater_pool.begin_publication_for(FS_ID, INODE).unwrap();
            updater_state.store(1, Ordering::Release);
            let result = if reclaim {
                updater_pool.reclaim_repair_record(reference, true, None)
            } else {
                updater_pool
                    .mark_pending_extent_mutation_phase(
                        &publication,
                        reference,
                        ExtentMutationPhase::Prepared,
                        ExtentMutationPhase::ParentPublished,
                    )
                    .map(|_| ())
            };
            updater_state.store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
        })
        .spawn();
        while state.load(Ordering::Acquire) == 0 {
            crate::thread::Thread::yield_now();
        }
        for _ in 0..32 {
            crate::thread::Thread::yield_now();
        }
        assert_eq!(state.load(Ordering::Acquire), 1);
        // Wait until the updater has closed reader admission, then start a
        // reader that must revalidate the generation after the clear finishes.
        loop {
            match pool.try_lock_metadata_mutation(reference) {
                Err(error) if error.error() == Errno::EBUSY => break,
                Ok(guard) => drop(guard),
                Err(error) => panic!("unexpected mark identity: {error:?}"),
            }
            crate::thread::Thread::yield_now();
        }
        let late_pool = pool.clone();
        let late_state = Arc::new(AtomicUsize::new(0));
        let late_state_thread = late_state.clone();
        let late_reader = crate::thread::kernel_thread::ThreadOptions::new(move || {
            late_state_thread.store(1, Ordering::Release);
            let result =
                late_pool.read_authoritative_metadata::<PendingExtentMutationRecord>(reference);
            if reclaim {
                assert_eq!(result.unwrap_err().error(), Errno::ESTALE);
            } else {
                assert_eq!(
                    result.unwrap().phase().unwrap(),
                    ExtentMutationPhase::ParentPublished
                );
            }
            late_state_thread.store(2, Ordering::Release);
        })
        .spawn();
        while late_state.load(Ordering::Acquire) == 0 {
            crate::thread::Thread::yield_now();
        }
        drop(reader);
        updater.join();
        late_reader.join();
        assert_eq!(late_state.load(Ordering::Acquire), 2);
        assert_eq!(state.load(Ordering::Acquire), 2);
        if reclaim {
            assert!(pool.current_metadata_ref(key).is_none());
        } else {
            assert_eq!(
                pool.read_authoritative_metadata::<PendingExtentMutationRecord>(reference)
                    .unwrap()
                    .phase()
                    .unwrap(),
                ExtentMutationPhase::ParentPublished
            );
        }
    }
}

#[ktest]
fn rnvpce_migration_credit_release_survives_persistent_ledger_failure() {
    const FS_ID: u64 = 0x9914;
    const INODE: u64 = 26;
    const SSD_BLOCK: u64 = 44_001;
    let (_image, _config, pool) = restartable_pool(16);
    let page = pool.stage_data_page(&[0x65; PAGE_SIZE]).unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.commit_data_page_in_owner(page, PageKey::new(FS_ID, INODE, 0), None)
        .unwrap();
    let reference = pool.persistent_data_ref(page).unwrap();
    let eviction = pool
        .pin_for_writeback_seq(page, reference.allocation_seq())
        .unwrap();
    eviction
        .assign_migration_target(reference, SSD_BLOCK)
        .unwrap();
    drop(eviction);

    assert!(
        test_release_migration_credit_with(&pool, reference, true, || {
            return_errno_with_message!(Errno::EIO, "injected reservation ledger failure")
        })
        .unwrap(),
        "the live EXT4 reservation credit must still be released once"
    );
    assert!(
        pool.persistent_data_migration_credit_released(reference)
            .unwrap()
    );
    assert!(
        !test_release_migration_credit_with(&pool, reference, true, || {
            panic!("a completed credit phase must not refresh the ledger twice")
        })
        .unwrap(),
        "retry must not release the live credit twice"
    );
}

#[cfg(any())]
fn rnvpce_foreground_batch_updates_each_leaf_once() {
    const FS_ID: u64 = 0x9920;
    const INODE: u64 = 30;
    let (_image, _config, pool) = restartable_pool(32);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let pages = [
        RangeWritePage::new(0, 0, 0, PAGE_SIZE, true).unwrap(),
        RangeWritePage::new(1, 0, 0, PAGE_SIZE, true).unwrap(),
    ];
    let request = RangeWriteBatch::new(
        2,
        0,
        2 * PAGE_SIZE,
        &[],
        &pages,
        0,
        0,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = [0x64u8; 2 * PAGE_SIZE];
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(&input[..]).to_fallible(),
        |_block, _staging| panic!("full-page write must not load SSD"),
        None,
    )
    .unwrap();

    let first = mapped_nv_page_ref(&pool, FS_ID, INODE, 0).unwrap().unwrap();
    let second = mapped_nv_page_ref(&pool, FS_ID, INODE, 1).unwrap().unwrap();
    let first_reverse = pool.reverse_lookup(first).unwrap().unwrap();
    let second_reverse = pool.reverse_lookup(second).unwrap().unwrap();
    assert_eq!(first_reverse.leaf_slot(), second_reverse.leaf_slot());
    let leaf_ref = pool
        .persistent_metadata_ref(NvFrameId::try_from(first_reverse.leaf_slot() as usize).unwrap())
        .unwrap();
    let leaf = pool
        .read_authoritative_metadata::<ExtentNodeRecord>(leaf_ref)
        .unwrap();
    let entries = leaf.entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].logical_block(), 0);
    assert_eq!(entries[0].length(), 2);
    assert_eq!(first_reverse.entry_index(), second_reverse.entry_index());
    assert_eq!(
        leaf.header().generation(),
        1,
        "FAST range append must not rewrite the leaf"
    );
}

#[ktest]
fn rnvpce_existing_baseline_overwrite_uses_no_extent_update_session() {
    const FS_ID: u64 = 0x9925;
    const INODE: u64 = 35;
    const FILE_PAGES: u64 = 8;
    const SSD_START: u64 = 70_000;
    let (_image, _config, pool) = restartable_pool(32);
    let initial = [ExtentRecordEntry::leaf(0, FILE_PAGES as u32, SSD_START, false).unwrap()];
    let snapshot = AuthoritySnapshot::new(
        1,
        &initial,
        FILE_PAGES * PAGE_SIZE as u64,
        FILE_PAGES * (PAGE_SIZE / 512) as u64,
        0o100644,
        0,
    )
    .unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();

    let pages = [
        RangeWritePage::new(2, SSD_START + 2, 0, PAGE_SIZE, false).unwrap(),
        RangeWritePage::new(3, SSD_START + 3, 0, PAGE_SIZE, false).unwrap(),
    ];
    let request = RangeWriteBatch::new(
        2,
        2 * PAGE_SIZE as u64,
        2 * PAGE_SIZE,
        &[],
        &pages,
        FILE_PAGES * PAGE_SIZE as u64,
        FILE_PAGES * (PAGE_SIZE / 512) as u64,
        2 * (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = [0x66u8; 2 * PAGE_SIZE];
    let before = pool.extent_field_update_sessions();
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(input.as_slice()).to_fallible(),
        |_block, _staging| panic!("full-page overwrite must not read SSD"),
        None,
    )
    .unwrap();

    assert_eq!(pool.extent_field_update_sessions() - before, 0);
    assert!(
        mapped_nv_page_ref(&pool, FS_ID, INODE, 2)
            .unwrap()
            .is_some()
    );
    assert!(
        mapped_nv_page_ref(&pool, FS_ID, INODE, 3)
            .unwrap()
            .is_some()
    );
    for logical in [0, 1, 4, 7] {
        assert_eq!(
            current_page_mapping(&pool, FS_ID, INODE, logical)
                .unwrap()
                .unwrap()
                .ssd_block(),
            SSD_START + logical,
        );
    }
}

#[ktest]
fn rnvpce_new_ssd_baseline_is_published_as_one_dirty_extent() {
    const FS_ID: u64 = 0x9929;
    const INODE: u64 = 39;
    const LOGICAL_START: u64 = 64;
    const EXTENT_PAGES: u32 = 32;
    const SSD_START: u64 = 90_000;
    let (_image, _config, pool) = restartable_pool(128);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let field_updates = pool.extent_field_update_sessions();

    publish_ssd_baseline_extent(&pool, FS_ID, INODE, LOGICAL_START, EXTENT_PAGES, SSD_START)
        .unwrap();

    let mut extents = Vec::new();
    visit_ssd_extents(&pool, FS_ID, INODE, |entry| {
        extents.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(extents.len(), 1);
    assert_eq!(extents[0].logical_block(), LOGICAL_START);
    assert_eq!(extents[0].length(), EXTENT_PAGES);
    assert_eq!(
        extents[0].data_ref().unwrap(),
        ExtentDataRef::Ssd(SSD_START)
    );
    publish_ssd_baseline_extent(
        &pool,
        FS_ID,
        INODE,
        LOGICAL_START + u64::from(EXTENT_PAGES),
        EXTENT_PAGES,
        SSD_START + u64::from(EXTENT_PAGES),
    )
    .unwrap();
    extents.clear();
    visit_ssd_extents(&pool, FS_ID, INODE, |entry| {
        extents.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(extents.len(), 1);
    assert_eq!(extents[0].length(), EXTENT_PAGES * 2);
    assert_eq!(pool.extent_field_update_sessions() - field_updates, 2);
    assert_eq!(
        authoritative_inode_state(&pool, FS_ID, INODE)
            .unwrap()
            .unwrap()
            .generation(),
        1,
        "an in-place baseline append must not republish an unchanged inode root"
    );
}

#[ktest]
fn rnvpce_ready_dirty_extents_share_one_baseline_session() {
    const FS_ID: u64 = 0x992a;
    const INODE: u64 = 40;
    let (_image, _config, pool) = restartable_pool(128);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let field_updates = pool.extent_field_update_sessions();
    let extents = [
        SsdBaselineExtent::new(0, 4, 100_000).unwrap(),
        SsdBaselineExtent::new(8, 4, 200_000).unwrap(),
    ];

    publish_ssd_baseline_extents(&pool, FS_ID, INODE, &extents).unwrap();

    let mut mappings = Vec::new();
    visit_ssd_extents(&pool, FS_ID, INODE, |entry| {
        mappings.push((entry.logical_block(), entry.length(), entry.ssd_block()?));
        Ok(())
    })
    .unwrap();
    mappings.sort_unstable_by_key(|mapping| mapping.0);
    assert_eq!(mappings, vec![(0, 4, 100_000), (8, 4, 200_000)]);
    assert_eq!(pool.extent_field_update_sessions() - field_updates, 2);
    assert_eq!(
        authoritative_inode_state(&pool, FS_ID, INODE)
            .unwrap()
            .unwrap()
            .generation(),
        1
    );
}

#[ktest]
fn rnvpce_checkpoint_visits_each_split_baseline_once() {
    const FS_ID: u64 = 0x9928;
    const INODE: u64 = 38;
    const EXTENTS: u64 = 512;
    let (_image, _config, pool) = restartable_pool(4096);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();

    for first in (0..EXTENTS).step_by(32) {
        let mappings = (first..(first + 32).min(EXTENTS))
            .map(|index| (index * 2, 80_000 + index * 3))
            .collect::<Vec<_>>();
        publish_ssd_baseline_batch(&pool, FS_ID, INODE, &mappings).unwrap();
    }
    assert_eq!(
        publish_ssd_baseline_batch(&pool, FS_ID, INODE, &[(2_001, 0)])
            .unwrap_err()
            .error(),
        Errno::EINVAL
    );

    let mut visited = BTreeMap::new();
    visit_ssd_extents(&pool, FS_ID, INODE, |entry| {
        let ExtentDataRef::Ssd(start) = entry.data_ref()? else {
            panic!("Native checkpoint reached an NV extent");
        };
        for delta in 0..u64::from(entry.length()) {
            let logical = entry.logical_block() + delta;
            assert!(
                visited.insert(logical, start + delta).is_none(),
                "checkpoint emitted logical block {logical} twice"
            );
        }
        Ok(())
    })
    .unwrap();

    assert_eq!(visited.len(), EXTENTS as usize);
    for index in 0..EXTENTS {
        let logical = index * 2;
        assert_eq!(visited.get(&logical), Some(&(80_000 + index * 3)));
        assert_eq!(
            current_page_mapping(&pool, FS_ID, INODE, logical)
                .unwrap()
                .unwrap()
                .ssd_block(),
            visited[&logical]
        );
    }
}

#[cfg(any())]
fn rnvpce_batch_migration_updates_leaf_targets_in_place() {
    const FS_ID: u64 = 0x9921;
    const INODE: u64 = 31;
    let (_image, _config, pool) = restartable_pool(32);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let pages = [
        RangeWritePage::new(0, 0, 0, PAGE_SIZE, true).unwrap(),
        RangeWritePage::new(1, 0, 0, PAGE_SIZE, true).unwrap(),
    ];
    let request = RangeWriteBatch::new(
        2,
        0,
        2 * PAGE_SIZE,
        &[],
        &pages,
        0,
        0,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = [0x65u8; 2 * PAGE_SIZE];
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(&input[..]).to_fallible(),
        |_block, _staging| panic!("full-page write must not load SSD"),
        None,
    )
    .unwrap();
    let references: Vec<_> = (0..2)
        .map(|logical| {
            mapped_nv_page_ref(&pool, FS_ID, INODE, logical)
                .unwrap()
                .unwrap()
        })
        .collect();
    let reverse = pool.reverse_lookup(references[0]).unwrap().unwrap();
    assert_eq!(
        reverse.leaf_slot(),
        pool.reverse_lookup(references[1])
            .unwrap()
            .unwrap()
            .leaf_slot()
    );
    let claimed = pool.claim_migration_extent(references[0], reverse).unwrap();
    assert!(
        current_page_mapping(&pool, FS_ID, INODE, 0)
            .unwrap()
            .unwrap()
            .is_in_flight()
    );
    let (identity, claimed_pages) = claimed.into_parts();
    let mut updates = Vec::new();
    let mut evictions = Vec::new();
    for claimed in claimed_pages {
        let target = 45_000 + claimed.key.page_index;
        claimed
            .eviction
            .assign_migration_target(claimed.reference, target)
            .unwrap();
        updates.push(SsdMigrationPage {
            logical_block: claimed.key.page_index,
            reference: claimed.reference,
            ssd_block: target,
        });
        evictions.push(claimed.eviction);
    }
    let leaf_ref = pool
        .persistent_metadata_ref(NvFrameId::try_from(reverse.leaf_slot() as usize).unwrap())
        .unwrap();
    let before = pool
        .read_authoritative_metadata::<ExtentNodeRecord>(leaf_ref)
        .unwrap()
        .header()
        .generation();
    assert_eq!(
        migrate_claimed_nv_extent_to_ssd(&pool, &identity, &updates[..1], None)
            .unwrap_err()
            .error(),
        Errno::EINVAL
    );
    assert!(
        mapped_nv_page_ref(&pool, FS_ID, INODE, 0)
            .unwrap()
            .is_some()
    );
    assert!(
        mapped_nv_page_ref(&pool, FS_ID, INODE, 1)
            .unwrap()
            .is_some()
    );
    let mut stale = updates.clone();
    stale[1].reference = PersistentObjectRef::new(
        stale[1].reference.slot().unwrap(),
        stale[1].reference.allocation_seq() + 1,
    )
    .unwrap();
    assert_eq!(
        migrate_claimed_nv_extent_to_ssd(&pool, &identity, &stale, None)
            .unwrap_err()
            .error(),
        Errno::ESTALE
    );
    let routes_before_completion = pool.extent_tree_routes();
    let outcomes = migrate_claimed_nv_extent_to_ssd(&pool, &identity, &updates, None).unwrap();
    assert!(outcomes.into_iter().all(|result| result.unwrap()));
    assert_eq!(
        pool.extent_tree_routes(),
        routes_before_completion,
        "claimed extent completion must use its reverse locator instead of routing from root"
    );
    let after = pool
        .read_authoritative_metadata::<ExtentNodeRecord>(leaf_ref)
        .unwrap()
        .header()
        .generation();
    assert_eq!(
        after, before,
        "target-word updates must not rewrite the leaf"
    );
    let migrated = pool
        .read_authoritative_metadata::<ExtentNodeRecord>(leaf_ref)
        .unwrap();
    let entries = migrated.entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].length(), 2);
    assert_eq!(entries[0].data_ref().unwrap(), ExtentDataRef::Ssd(45_000));
    assert_eq!(
        space_reservation(&pool, FS_ID, INODE)
            .unwrap()
            .unwrap()
            .outstanding_blocks(),
        0
    );
    let retry = migrate_claimed_nv_extent_to_ssd(&pool, &identity, &updates, None).unwrap();
    assert!(retry.into_iter().all(|result| !result.unwrap()));
    for (update, eviction) in updates.iter().zip(evictions) {
        eviction
            .reclaim_after_ssd_publish(update.reference, None)
            .unwrap();
        let page = NvPageId::try_from(update.reference.slot().unwrap() as usize).unwrap();
        assert!(
            !pool
                .free_data_page_if_seq(page, update.reference.allocation_seq())
                .unwrap()
        );
        assert_eq!(
            current_page_mapping(&pool, FS_ID, INODE, update.logical_block)
                .unwrap()
                .unwrap()
                .ssd_block(),
            update.ssd_block
        );
    }

    let overwrite = [RangeWritePage::new(0, 45_000, 0, PAGE_SIZE, false).unwrap()];
    let request = RangeWriteBatch::new(
        3,
        0,
        PAGE_SIZE,
        &[],
        &overwrite,
        2 * PAGE_SIZE as u64,
        2 * (PAGE_SIZE / 512) as u64,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = [0x91u8; PAGE_SIZE];
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(&input[..]).to_fallible(),
        |_block, _staging| panic!("full-page overwrite must not load SSD"),
        None,
    )
    .expect("a migrated and reclaimed logical page must remain overwritable");
    assert!(
        mapped_nv_page_ref(&pool, FS_ID, INODE, 0)
            .unwrap()
            .is_some()
    );
}

#[cfg(any())]
fn rnvpce_extent_completion_rejects_fragmented_targets_as_one_unit() {
    const FS_ID: u64 = 0x9927;
    const INODE: u64 = 37;
    let (_image, _config, pool) = restartable_pool(32);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let pages = [
        RangeWritePage::new(0, 0, 0, PAGE_SIZE, true).unwrap(),
        RangeWritePage::new(1, 0, 0, PAGE_SIZE, true).unwrap(),
    ];
    let request = RangeWriteBatch::new(
        2,
        0,
        2 * PAGE_SIZE,
        &[],
        &pages,
        0,
        0,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    let input = [0x39u8; 2 * PAGE_SIZE];
    commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(&input[..]).to_fallible(),
        |_block, _staging| panic!("full-page write must not load SSD"),
        None,
    )
    .unwrap();
    let first = mapped_nv_page_ref(&pool, FS_ID, INODE, 0).unwrap().unwrap();
    let locator = pool.reverse_lookup(first).unwrap().unwrap();
    let (identity, claimed_pages) = pool
        .claim_migration_extent(first, locator)
        .unwrap()
        .into_parts();
    let mut updates = Vec::new();
    let mut evictions = Vec::new();
    for claimed in claimed_pages {
        let target = 47_000 + claimed.key.page_index * 2;
        claimed
            .eviction
            .assign_migration_target(claimed.reference, target)
            .unwrap();
        updates.push(SsdMigrationPage {
            logical_block: claimed.key.page_index,
            reference: claimed.reference,
            ssd_block: target,
        });
        evictions.push(claimed.eviction);
    }
    assert_eq!(
        migrate_claimed_nv_extent_to_ssd(&pool, &identity, &updates, None)
            .unwrap_err()
            .error(),
        Errno::EAGAIN
    );
    for logical in 0..2 {
        assert!(
            mapped_nv_page_ref(&pool, FS_ID, INODE, logical)
                .unwrap()
                .is_some(),
            "a fragmented target run must leave the complete NV extent authoritative"
        );
    }
    drop(evictions);
}

#[cfg(any())]
fn rnvpce_completion_batches_credit_and_ledger_across_extents() {
    const FS_ID: u64 = 0x9933;
    const INODE: u64 = 43;
    let (_image, _config, pool) = restartable_pool(32);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();

    for (generation, logical) in [(2, 0), (3, 2)] {
        let pages = [RangeWritePage::new(logical, 0, 0, PAGE_SIZE, true).unwrap()];
        let request = RangeWriteBatch::new(
            generation,
            logical * PAGE_SIZE as u64,
            PAGE_SIZE,
            &[],
            &pages,
            logical * PAGE_SIZE as u64,
            logical * (PAGE_SIZE / 512) as u64,
            (PAGE_SIZE / 512) as u64,
        )
        .unwrap();
        commit_range_write(
            pool.clone(),
            FS_ID,
            INODE,
            &request,
            &mut VmReader::from(&[generation as u8; PAGE_SIZE][..]).to_fallible(),
            |_block, _staging| panic!("full-page write must not load SSD"),
            None,
        )
        .unwrap();
    }

    let mut batch = Vec::new();
    let mut evictions = Vec::new();
    for logical in [0, 2] {
        let reference = mapped_nv_page_ref(&pool, FS_ID, INODE, logical)
            .unwrap()
            .unwrap();
        let locator = pool.reverse_lookup(reference).unwrap().unwrap();
        let (identity, claimed_pages) = pool
            .claim_migration_extent(reference, locator)
            .unwrap()
            .into_parts();
        let mut updates = Vec::new();
        for claimed in claimed_pages {
            let target = 53_000 + claimed.key.page_index;
            claimed
                .eviction
                .assign_migration_target(claimed.reference, target)
                .unwrap();
            updates.push(SsdMigrationPage {
                logical_block: claimed.key.page_index,
                reference: claimed.reference,
                ssd_block: target,
            });
            evictions.push(claimed.eviction);
        }
        batch.push(ClaimedSsdMigrationExtent { identity, updates });
    }
    let before = space_reservation(&pool, FS_ID, INODE).unwrap().unwrap();

    let outcomes = migrate_claimed_nv_extents_to_ssd(&pool, FS_ID, INODE, &batch, None).unwrap();

    assert!(outcomes.into_iter().flatten().all(|result| result.unwrap()));
    let after = space_reservation(&pool, FS_ID, INODE).unwrap().unwrap();
    assert_eq!(
        after.header().generation(),
        before.header().generation() + 1
    );
    assert_eq!(after.outstanding_blocks(), 0);
    for (extent, eviction) in batch.iter().zip(evictions) {
        eviction
            .reclaim_after_ssd_publish(extent.updates[0].reference, None)
            .unwrap();
    }
}

#[cfg(any())]
fn rnvpce_candidate_claim_reads_shared_leaf_once() {
    const FS_ID: u64 = 0x9934;
    const INODE: u64 = 44;
    let (_image, _config, pool) = restartable_pool(32);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();

    for (generation, logical) in [(2, 0), (3, 2)] {
        let pages = [RangeWritePage::new(logical, 0, 0, PAGE_SIZE, true).unwrap()];
        let request = RangeWriteBatch::new(
            generation,
            logical * PAGE_SIZE as u64,
            PAGE_SIZE,
            &[],
            &pages,
            logical * PAGE_SIZE as u64,
            logical * (PAGE_SIZE / 512) as u64,
            (PAGE_SIZE / 512) as u64,
        )
        .unwrap();
        commit_range_write(
            pool.clone(),
            FS_ID,
            INODE,
            &request,
            &mut VmReader::from(&[generation as u8; PAGE_SIZE][..]).to_fallible(),
            |_block, _staging| panic!("full-page write must not load SSD"),
            None,
        )
        .unwrap();
    }
    let seeds: Vec<_> = [0, 2]
        .into_iter()
        .map(|logical| {
            let reference = mapped_nv_page_ref(&pool, FS_ID, INODE, logical)
                .unwrap()
                .unwrap();
            let locator = pool.reverse_lookup(reference).unwrap().unwrap();
            (reference, locator)
        })
        .collect();
    assert_eq!(seeds[0].1.leaf_slot(), seeds[1].1.leaf_slot());
    let before = pool.migration_claim_leaf_reads();

    let claimed = pool.claim_migration_extents(&seeds, 128).unwrap();

    assert_eq!(claimed.len(), 2);
    assert_eq!(pool.migration_claim_leaf_reads() - before, 1);
}

#[allow(dead_code)]
fn rnvpce_recovery_rebuilds_redirects_without_reverse_edges() {
    const FS_ID: u64 = 0x9925;
    const INODE: u64 = 35;
    let (image, config, pool) = restartable_pool(32);
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    let authority = bootstrap_inode_authority(pool.clone(), FS_ID, INODE, &snapshot, None).unwrap();
    let fixed = authority.inode();
    let pages = [
        RangeWritePage::new(0, 0, 0, PAGE_SIZE, true).unwrap(),
        RangeWritePage::new(1, 0, 0, PAGE_SIZE, true).unwrap(),
        RangeWritePage::new(2, 0, 0, PAGE_SIZE, true).unwrap(),
    ];
    let request = RangeWriteBatch::new(
        2,
        0,
        3 * PAGE_SIZE,
        &[],
        &pages,
        0,
        0,
        (PAGE_SIZE / 512) as u64,
    )
    .unwrap();
    // Exhausting a bounded reader covers the short-copy protocol; this does not
    // model a CPU fault while traversing user page tables.
    let input = vec![0x73; PAGE_SIZE + 37];
    let written = commit_range_write(
        pool.clone(),
        FS_ID,
        INODE,
        &request,
        &mut VmReader::from(input.as_slice()).to_fallible(),
        |_, _| panic!("new pages must not read SSD"),
        None,
    )
    .unwrap();
    assert_eq!(written.copied(), input.len());
    assert_eq!(written.reserved_blocks(), 2);
    assert_eq!(written.inode(), fixed);
    assert!(
        mapped_nv_page_ref(&pool, FS_ID, INODE, 2)
            .unwrap()
            .is_none()
    );
    drop(pool);
    image.simulate_crash();
    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    assert_eq!(
        recovered.fixed_inode_ref(FS_ID, INODE).unwrap(),
        Some(fixed)
    );
    let state = authoritative_inode_state(&recovered, FS_ID, INODE)
        .unwrap()
        .unwrap();
    assert_eq!(state.file_size(), input.len() as u64);
    assert_eq!(state.block_count(), (2 * PAGE_SIZE / 512) as u64);
    assert_eq!(
        space_reservation(&recovered, FS_ID, INODE)
            .unwrap()
            .unwrap()
            .outstanding_blocks(),
        2
    );
    for logical in 0..2 {
        let reference = mapped_nv_page_ref(&recovered, FS_ID, INODE, logical)
            .unwrap()
            .unwrap();
        assert!(recovered.reverse_lookup(reference).unwrap().is_none());
        let page = NvPageId::try_from(reference.slot().unwrap() as usize).unwrap();
        let pin = recovered.pin_data_page(page).unwrap();
        let mut contents = vec![0; PAGE_SIZE];
        pin.read(&mut contents).unwrap();
        let copied = if logical == 0 { PAGE_SIZE } else { 37 };
        assert!(contents[..copied].iter().all(|byte| *byte == 0x73));
        assert!(contents[copied..].iter().all(|byte| *byte == 0));
    }
    assert!(
        mapped_nv_page_ref(&recovered, FS_ID, INODE, 2)
            .unwrap()
            .is_none()
    );
    let free = recovered.stats().free_pages;
    drop(recovered);
    image.simulate_crash();
    let twice = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    assert_eq!(twice.stats().free_pages, free);
    assert_eq!(
        space_reservation(&twice, FS_ID, INODE)
            .unwrap()
            .unwrap()
            .outstanding_blocks(),
        2
    );
}

#[cfg(any())]
fn rnvpce_batch_migration_crash_recovers_authoritative_forward_graph() {
    use super::fault::FaultPoint;
    for phase in 0..5 {
        ostd::early_println!("NVPAGE_CRASH_PHASE {}", phase);
        let (image, config, pool) = restartable_pool(48);
        let fs_id = 0x9930 + phase;
        let inode = 51;
        let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
        bootstrap_inode_authority(pool.clone(), fs_id, inode, &snapshot, None).unwrap();
        let pages = [
            RangeWritePage::new(0, 0, 0, PAGE_SIZE, true).unwrap(),
            RangeWritePage::new(1, 0, 0, PAGE_SIZE, true).unwrap(),
        ];
        let request = RangeWriteBatch::new(
            2,
            0,
            2 * PAGE_SIZE,
            &[],
            &pages,
            0,
            0,
            (PAGE_SIZE / 512) as u64,
        )
        .unwrap();
        let input = [0x6a; 2 * PAGE_SIZE];
        commit_range_write(
            pool.clone(),
            fs_id,
            inode,
            &request,
            &mut VmReader::from(input.as_slice()).to_fallible(),
            |_, _| panic!("new pages"),
            None,
        )
        .unwrap();
        let references: Vec<_> = (0..2)
            .map(|logical| {
                mapped_nv_page_ref(&pool, fs_id, inode, logical)
                    .unwrap()
                    .expect("initial NV mapping")
            })
            .collect();
        let leases: Vec<_> = references
            .iter()
            .map(|r| {
                pool.pin_for_writeback_seq(
                    NvPageId::try_from(r.slot().unwrap() as usize).unwrap(),
                    r.allocation_seq(),
                )
                .unwrap()
            })
            .collect();
        let targets: Vec<_> = references
            .iter()
            .enumerate()
            .map(|(i, r)| (*r, 46000 + i as u64))
            .collect();
        let target_fault = match phase {
            0 => Some(FaultPoint::AfterContentPersist),
            1 => Some(FaultPoint::AfterCommitPersist),
            _ => None,
        };
        let result = pool.assign_migration_targets(&targets, target_fault);
        assert_eq!(result.is_ok(), phase >= 2);
        if phase >= 2 {
            // Test backend has no physical SSD: simulate its successful data+flush
            // prerequisite and verify only the NVM publication/recovery protocol.
            let updates: Vec<_> = references
                .iter()
                .enumerate()
                .map(|(i, r)| SsdMigrationPage {
                    logical_block: i as u64,
                    reference: *r,
                    ssd_block: 46000 + i as u64,
                })
                .collect();
            assert!(
                migrate_mapped_nv_pages_to_ssd_by_reverse(&pool, fs_id, inode, &updates, None)
                    .unwrap()
                    .into_iter()
                    .all(|r| r.unwrap())
            );
            let reclaim: Vec<_> = leases.into_iter().zip(references.iter().copied()).collect();
            match phase {
                2 => drop(reclaim), // Published SSD extents, descriptors still live.
                3 => assert!(
                    pool.reclaim_migrated_pages(reclaim, Some(FaultPoint::AfterClearPersist))
                        .is_err()
                ),
                _ => pool.reclaim_migrated_pages(reclaim, None).unwrap(),
            }
        } else {
            drop(leases);
        }
        drop(pool);
        for _ in 0..2 {
            image.simulate_crash();
            let recovered =
                Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
            for logical in 0..2 {
                if phase < 2 {
                    let reference = mapped_nv_page_ref(&recovered, fs_id, inode, logical)
                        .unwrap()
                        .expect("recovered NV mapping");
                    let pin = recovered
                        .pin_data_page(
                            NvPageId::try_from(reference.slot().unwrap() as usize).unwrap(),
                        )
                        .unwrap();
                    let mut data = [0; PAGE_SIZE];
                    pin.read(&mut data).unwrap();
                    assert_eq!(data, [0x6a; PAGE_SIZE]);
                } else {
                    assert!(
                        mapped_nv_page_ref(&recovered, fs_id, inode, logical)
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(
                        current_page_mapping(&recovered, fs_id, inode, logical)
                            .unwrap()
                            .expect("recovered SSD mapping")
                            .ssd_block(),
                        46000 + logical
                    );
                }
            }
            assert_eq!(
                space_reservation(&recovered, fs_id, inode)
                    .unwrap()
                    .map_or(0, |record| record.outstanding_blocks()),
                if phase < 2 { 2 } else { 0 }
            );
            drop(recovered);
        }
    }
}
