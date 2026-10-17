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
    frame::Rnvpc,
    id::{NvFrameId, NvPageId},
    index::PageKey,
    metadata::{
        ExtentMutationPhase, ExtentNodeRecord, PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
        PendingExtentMutationRecord, PersistentObjectRef,
    },
    write_transaction::{
        AuthoritySnapshot, RangeWriteBatch, RangeWritePage, SsdMigrationPage,
        authoritative_inode_state, bootstrap_inode_authority, commit_range_write,
        consume_ssd_capacity_locked, current_page_mapping, mapped_nv_page_ref,
        migrate_mapped_nv_pages_to_ssd_by_reverse, reserve_ssd_capacity, space_reservation,
        test_release_migration_credit_with,
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
fn rnvpce_batch_migration_updates_each_leaf_once() {
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
    let mut updates = Vec::new();
    let mut evictions = Vec::new();
    for logical in 0..2 {
        let reference = mapped_nv_page_ref(&pool, FS_ID, INODE, logical)
            .unwrap()
            .unwrap();
        let page = NvPageId::try_from(reference.slot().unwrap() as usize).unwrap();
        let eviction = pool
            .pin_for_writeback_seq(page, reference.allocation_seq())
            .unwrap();
        eviction
            .assign_migration_target(reference, 45_000 + logical)
            .unwrap();
        updates.push(SsdMigrationPage {
            logical_block: logical,
            reference,
            ssd_block: 45_000 + logical,
        });
        evictions.push(eviction);
    }
    let reverse = pool.reverse_lookup(updates[0].reference).unwrap().unwrap();
    assert_eq!(
        reverse.leaf_slot(),
        pool.reverse_lookup(updates[1].reference)
            .unwrap()
            .unwrap()
            .leaf_slot()
    );
    let leaf_ref = pool
        .persistent_metadata_ref(NvFrameId::try_from(reverse.leaf_slot() as usize).unwrap())
        .unwrap();
    let before = pool
        .read_authoritative_metadata::<ExtentNodeRecord>(leaf_ref)
        .unwrap()
        .header()
        .generation();
    let foreground = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();
    let outcomes =
        migrate_mapped_nv_pages_to_ssd_by_reverse(&pool, FS_ID, INODE, &updates, None).unwrap();
    drop(foreground);
    assert!(outcomes.into_iter().all(|result| result.unwrap()));
    let after = pool
        .read_authoritative_metadata::<ExtentNodeRecord>(leaf_ref)
        .unwrap()
        .header()
        .generation();
    assert_eq!(
        after,
        before + 1,
        "one leaf must be persisted once for the whole group"
    );
    assert_eq!(
        space_reservation(&pool, FS_ID, INODE)
            .unwrap()
            .unwrap()
            .outstanding_blocks(),
        0
    );
    let retry =
        migrate_mapped_nv_pages_to_ssd_by_reverse(&pool, FS_ID, INODE, &updates, None).unwrap();
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
