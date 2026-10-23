// SPDX-License-Identifier: MPL-2.0

//! Recovered and adapted soft-update integration regressions.
//! See docs/recovery/rnvpce-integrity.md for provenance and validation limits.

use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::prelude::ktest;

use super::{
    backend::NvMemoryBackend,
    config::{NvSimMode, RnvpcConfig},
    data_page::DataPageState,
    dram::{DramNvBackend, DramNvImage},
    extent_mutation::ExtentMutation,
    fault::FaultPoint,
    frame::Rnvpc,
    id::{NvFrameId, NvPageId},
    index::PageKey,
    metadata::{
        ExtentDataRef, ExtentMutationPhase, ExtentNodeRecord, ExtentRecordEntry, InodeRecord,
        PENDING_EXTENT_MUTATION_LOGICAL_INDEX, PendingExtentMutationRecord, PersistentObjectRef,
    },
    write_transaction::{
        AuthoritySnapshot, RangeWriteBatch, RangeWritePage, SsdMigrationPage,
        authoritative_inode_state, bootstrap_inode_authority, commit_range_write,
        consume_ssd_capacity_locked, current_page_mapping, mapped_nv_page_ref,
        migrate_claimed_nv_extent_to_ssd, migrate_mapped_nv_pages_to_ssd_by_reverse,
        reserve_ssd_capacity, space_reservation, test_release_migration_credit_with,
    },
};
use crate::prelude::*;

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
fn rnvpce_random_overwrites_split_an_inplace_leaf() {
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

    for iteration in 0..100u64 {
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

#[ktest]
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
    let outcomes = migrate_claimed_nv_extent_to_ssd(&pool, &identity, &updates, None).unwrap();
    assert!(outcomes.into_iter().all(|result| result.unwrap()));
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
}

#[ktest]
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

#[ktest]
fn rnvpce_short_reader_publishes_only_durable_prefix_and_credit() {
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
        assert!(recovered.reverse_lookup(reference).unwrap().is_some());
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

#[ktest]
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
