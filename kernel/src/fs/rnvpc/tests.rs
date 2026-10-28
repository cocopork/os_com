// SPDX-License-Identifier: MPL-2.0

//! Recovered subset of the original tests; see docs/recovery/entry-tests-provenance.md.

use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::prelude::ktest;

use super::{
    backend::NvMemoryBackend,
    capacity::PoolClass,
    config::{NvSimMode, RnvpcConfig},
    data_page::DataPageState,
    dram::DramNvBackend,
    extent_mutation::ExtentMutation,
    fault::FaultPoint,
    frame::Rnvpc,
    id::{NvFrameId, NvPageId},
    index::{PageKey, ReverseExtentRef},
    metadata::{ExtentNodeRecord, ExtentRecordEntry, PersistentObjectRef},
    write_transaction::{consume_ssd_capacity, reserve_ssd_capacity, space_reservation},
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
fn config_is_disabled_without_pool_size() {
    assert_eq!(
        RnvpcConfig::from_bytes(None, None, None, None).unwrap(),
        None
    );
}

#[ktest]
fn config_accepts_page_aligned_capacity_split() {
    let config = RnvpcConfig::from_bytes(
        Some(16 * PAGE_SIZE),
        Some(4 * PAGE_SIZE),
        Some(12 * PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();

    assert_eq!(config.total_pages(), 16);
    assert_eq!(config.meta_reserve_pages(), 4);
    assert_eq!(config.data_limit_pages(), 12);
    assert_eq!(config.mode(), NvSimMode::Fast);
}

#[ktest]
fn config_accepts_model_restart_mode() {
    let config = RnvpcConfig::from_params(Some("16K"), Some("4K"), Some("12K"), Some("model"))
        .unwrap()
        .unwrap();
    assert_eq!(config.mode(), NvSimMode::Model);
}

#[ktest]
fn config_rejects_unaligned_and_inconsistent_capacities() {
    assert!(RnvpcConfig::from_bytes(Some(PAGE_SIZE + 1), None, None, None).is_err());
    assert!(
        RnvpcConfig::from_bytes(
            Some(8 * PAGE_SIZE),
            Some(9 * PAGE_SIZE),
            Some(4 * PAGE_SIZE),
            None,
        )
        .is_err()
    );
    assert!(
        RnvpcConfig::from_bytes(
            Some(8 * PAGE_SIZE),
            Some(4 * PAGE_SIZE),
            Some(5 * PAGE_SIZE),
            None,
        )
        .is_err()
    );
    assert!(
        RnvpcConfig::from_bytes(
            Some((u32::MAX as usize + 2) * PAGE_SIZE),
            Some(PAGE_SIZE),
            Some(0),
            None,
        )
        .is_err()
    );
}

#[ktest]
fn config_parses_kernel_parameter_values() {
    let config = RnvpcConfig::from_params(Some("16K"), Some("4K"), Some("12K"), Some("fast"))
        .unwrap()
        .unwrap();

    assert_eq!(config.total_pages(), 4);
    assert_eq!(config.meta_reserve_pages(), 1);
    assert_eq!(config.data_limit_pages(), 3);
    assert_eq!(config.mode(), NvSimMode::Fast);
}

#[ktest]
fn config_rejects_invalid_kernel_parameter_values() {
    assert!(RnvpcConfig::from_params(Some("4097"), Some("4K"), Some("0"), None).is_err());
    assert!(RnvpcConfig::from_params(Some("8K"), Some("4K"), Some("4K"), Some("slow")).is_err());
    assert!(RnvpcConfig::from_params(None, None, None, Some("fast")).is_err());
}

#[ktest]
fn pool_ids_use_checked_conversions() {
    assert_eq!(NvFrameId::try_from(7usize).unwrap().index(), 7);
    assert_eq!(NvPageId::try_from(11usize).unwrap().index(), 11);

    if usize::BITS > u32::BITS {
        let too_large = u32::MAX as usize + 1;
        assert!(NvFrameId::try_from(too_large).is_err());
        assert!(NvPageId::try_from(too_large).is_err());
    }
}

#[ktest]
fn dram_backend_locates_pages_across_chunks() {
    let config = RnvpcConfig::from_bytes(
        Some(5 * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(4 * PAGE_SIZE),
        None,
    )
    .unwrap()
    .unwrap();
    let backend = DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap();
    let pattern = [0x5a; PAGE_SIZE];

    backend
        .write_page(NvPageId::try_from(2usize).unwrap(), &pattern)
        .unwrap();
    let mut actual = [0; PAGE_SIZE];
    backend
        .read_page(NvPageId::try_from(2usize).unwrap(), &mut actual)
        .unwrap();

    assert_eq!(backend.capacity_pages(), 5);
    assert_eq!(actual, pattern);
}

#[ktest]
fn dram_backend_rejects_invalid_page_access() {
    let config =
        RnvpcConfig::from_bytes(Some(2 * PAGE_SIZE), Some(PAGE_SIZE), Some(PAGE_SIZE), None)
            .unwrap()
            .unwrap();
    let backend = DramNvBackend::allocate_with_chunk_pages(config, 1).unwrap();

    assert!(
        backend
            .write_page(NvPageId::try_from(2usize).unwrap(), &[0; PAGE_SIZE])
            .is_err()
    );
    assert!(
        backend
            .write_page(NvPageId::try_from(0usize).unwrap(), &[0; 1])
            .is_err()
    );
}

#[ktest]
fn fast_mode_counts_persist_and_fence_boundaries() {
    let config = RnvpcConfig::from_bytes(
        Some(2 * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();
    let backend = DramNvBackend::allocate_with_chunk_pages(config, 1).unwrap();

    backend.persist(0, PAGE_SIZE).unwrap();
    backend.fence().unwrap();
    backend.persist(PAGE_SIZE, PAGE_SIZE).unwrap();

    let stats = backend.stats();
    assert_eq!(stats.persist_calls, 2);
    assert_eq!(stats.fence_calls, 1);
}

#[ktest]
fn capacity_policy_preserves_metadata_progress() {
    for (total, reserve, data_limit) in [(8, 2, 6), (7, 3, 4), (6, 2, 3)] {
        let pool = test_pool(total, reserve, data_limit, None);
        let mut data_pages = Vec::new();
        for _ in 0..data_limit {
            data_pages.push(pool.alloc_data_page().unwrap());
        }
        assert_eq!(pool.alloc_data_page().unwrap_err().error(), Errno::EAGAIN);

        let mut metadata_frames = Vec::new();
        for _ in 0..total - data_limit {
            metadata_frames.push(pool.alloc_frame(PoolClass::Metadata).unwrap());
        }
        assert_eq!(
            pool.alloc_frame(PoolClass::Metadata).unwrap_err().error(),
            Errno::ENOSPC
        );

        for frame in metadata_frames {
            pool.free_frame(frame).unwrap();
        }
        for page in data_pages {
            pool.free_data_page(page).unwrap();
        }
    }
}

#[ktest]
fn metadata_can_borrow_unused_data_capacity() {
    let pool = test_pool(8, 2, 6, None);
    let mut frames = Vec::new();
    for _ in 0..7 {
        frames.push(pool.alloc_frame(PoolClass::Metadata).unwrap());
    }
    let page = pool.alloc_data_page().unwrap();

    assert_eq!(pool.stats().metadata_pages, 7);
    assert_eq!(pool.stats().data_pages, 1);

    pool.free_data_page(page).unwrap();
    for frame in frames {
        pool.free_frame(frame).unwrap();
    }
}

#[ktest]
fn high_watermark_wakes_and_hard_limit_backpressures() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let callback_wakes = wakes.clone();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        callback_wakes.fetch_add(1, Ordering::Relaxed);
    });
    let pool = test_pool(8, 2, 6, Some(wake));

    let mut pages = Vec::new();
    for _ in 0..6 {
        pages.push(pool.alloc_data_page().unwrap());
    }
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert_eq!(pool.alloc_data_page().unwrap_err().error(), Errno::EAGAIN);
    let stats = pool.stats();
    assert_eq!(stats.high_water_events, 1);
    assert_eq!(stats.backpressure_events, 1);

    pool.free_data_page(pages.pop().unwrap()).unwrap();
    pages.push(pool.alloc_data_page().unwrap());
    assert_eq!(
        wakes.load(Ordering::Relaxed),
        1,
        "migration pressure must remain active until the low watermark"
    );
    for page in pages {
        pool.free_data_page(page).unwrap();
    }

    let low_limit_wakes = Arc::new(AtomicUsize::new(0));
    let callback_wakes = low_limit_wakes.clone();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        callback_wakes.fetch_add(1, Ordering::Relaxed);
    });
    let low_limit_pool = test_pool(8, 6, 2, Some(wake));
    let _first = low_limit_pool.alloc_data_page().unwrap();
    let _second = low_limit_pool.alloc_data_page().unwrap();
    assert_eq!(low_limit_wakes.load(Ordering::Relaxed), 1);
    assert_eq!(
        low_limit_pool.alloc_data_page().unwrap_err().error(),
        Errno::EAGAIN
    );
}

#[ktest]
fn pinned_and_repeated_free_are_rejected() {
    let pool = test_pool(4, 1, 3, None);
    let frame = pool.alloc_frame(PoolClass::Metadata).unwrap();
    let pin = pool.pin_frame(frame).unwrap();

    assert_eq!(pool.free_frame(frame).unwrap_err().error(), Errno::EBUSY);
    drop(pin);
    pool.free_frame(frame).unwrap();
    assert_eq!(pool.free_frame(frame).unwrap_err().error(), Errno::EINVAL);
}

#[ktest]
fn data_page_transitions_are_checked() {
    let pool = test_pool(4, 1, 3, None);
    let page = pool.alloc_data_page().unwrap();
    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::Staged);

    assert!(
        pool.transition_data_page(page, DataPageState::Staged, DataPageState::InFlight)
            .is_err()
    );
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.transition_data_page(page, DataPageState::NvDirty, DataPageState::InFlight)
        .unwrap();
    assert_eq!(pool.free_data_page(page).unwrap_err().error(), Errno::EBUSY);
    pool.transition_data_page(page, DataPageState::InFlight, DataPageState::NvDirty)
        .unwrap();
    pool.transition_data_page(page, DataPageState::NvDirty, DataPageState::InFlight)
        .unwrap();
    pool.transition_data_page(page, DataPageState::InFlight, DataPageState::Synced)
        .unwrap();
    pool.free_data_page(page).unwrap();
    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::Free);
}

#[ktest]
fn staging_data_page_copies_a_full_page_and_returns_a_staged_pin() {
    let pool = test_pool(4, 1, 3, None);
    let content = [0x4c; PAGE_SIZE];
    let page = pool.stage_data_page(&content).unwrap();

    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::Staged);
    let pin = pool.pin_data_page(page).unwrap();
    let mut read_back = [0; PAGE_SIZE];
    pin.read(&mut read_back).unwrap();
    assert_eq!(read_back, content);

    pin.write(&[0x9e; PAGE_SIZE]).unwrap();
    pin.read(&mut read_back).unwrap();
    assert_eq!(read_back, [0x9e; PAGE_SIZE]);
    drop(pin);
    pool.free_data_page(page).unwrap();
}

#[ktest]
fn staging_data_page_rejects_partial_content() {
    let pool = test_pool(4, 1, 3, None);
    let partial = [0; PAGE_SIZE - 1];

    assert_eq!(
        pool.stage_data_page(&partial).unwrap_err().error(),
        Errno::EINVAL
    );
    assert_eq!(pool.stats().free_pages, 4);
}

#[ktest]
fn rnvpce_space_reservation_grows_and_consumes_without_ssd_location() {
    const FS_ID: u64 = 0x9911;
    const INODE: u64 = 23;
    let pool = test_pool(16, 8, 8, None);
    let mutation = ExtentMutation::new(pool.clone(), FS_ID, INODE).unwrap();

    reserve_ssd_capacity(&pool, &mutation, FS_ID, INODE, 5).unwrap();
    reserve_ssd_capacity(&pool, &mutation, FS_ID, INODE, 3).unwrap();
    let reservation = space_reservation(&pool, FS_ID, INODE).unwrap().unwrap();
    assert_eq!(reservation.reserved_blocks(), 8);
    assert_eq!(reservation.consumed_blocks(), 0);
    assert_eq!(reservation.outstanding_blocks(), 8);

    consume_ssd_capacity(&pool, &mutation, FS_ID, INODE, 3).unwrap();
    let reservation = space_reservation(&pool, FS_ID, INODE).unwrap().unwrap();
    assert_eq!(reservation.reserved_blocks(), 8);
    assert_eq!(reservation.consumed_blocks(), 3);
    assert_eq!(reservation.outstanding_blocks(), 5);
    assert_eq!(
        pool.stats().metadata_pages,
        1,
        "one inode must keep one in-place space reservation record"
    );
    assert!(consume_ssd_capacity(&pool, &mutation, FS_ID, INODE, 6).is_err());
}

#[ktest]
fn rnvpce_dirty_queue_deduplicates_and_returns_reverse_locator() {
    const FS_ID: u64 = 0x9912;
    const INODE: u64 = 24;
    let pool = test_pool(16, 8, 8, None);
    let mut capacity = pool.reserve_data_pages(1).unwrap();
    let page = pool
        .stage_reserved_data_page(&mut capacity, &[0x5a; PAGE_SIZE])
        .unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.replace_data_page_in_owner_at(page, PageKey::new(FS_ID, INODE, 7), 0, None)
        .unwrap();
    let reference = pool.persistent_data_ref(page).unwrap();
    let reverse = ReverseExtentRef::new(0x1234, 91).unwrap();
    assert_eq!(reverse.leaf_slot(), 0x1234);
    assert_eq!(reverse.entry_index(), 91);
    pool.install_reverse(reference, reverse).unwrap();

    assert!(
        pool.enqueue_dirty_nvpage(page, reference.allocation_seq())
            .unwrap()
    );
    assert!(
        !pool
            .enqueue_dirty_nvpage(page, reference.allocation_seq())
            .unwrap()
    );
    assert_eq!(pool.collect_dirty_nvpages(8).len(), 1);
    assert!(pool.collect_dirty_nvpages(8).is_empty());
    assert_eq!(pool.reverse_lookup(reference).unwrap(), Some(reverse));

    let key = PageKey::new(FS_ID, INODE, 7);
    let freeze = pool.freeze_extent_range(FS_ID, INODE, 7..12).unwrap();
    assert!(pool.migration_is_frozen(key));
    assert!(!pool.migration_is_frozen(PageKey::new(FS_ID, INODE, 6)));
    drop(freeze);
    assert!(!pool.migration_is_frozen(key));
}

fn install_claimable_nv_extent(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    logical_start: u64,
    pages: usize,
) -> (Vec<PersistentObjectRef>, ReverseExtentRef) {
    let mut reservation = pool.reserve_data_pages(pages).unwrap();
    let mut references = Vec::new();
    for offset in 0..pages {
        let page = pool
            .stage_reserved_data_page(&mut reservation, &[offset as u8; PAGE_SIZE])
            .unwrap();
        pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
            .unwrap();
        pool.replace_data_page_in_owner_at(
            page,
            PageKey::new(fs_id, inode, logical_start + offset as u64),
            0,
            None,
        )
        .unwrap();
        references.push(pool.persistent_data_ref(page).unwrap());
    }
    references.sort_by_key(|reference| reference.slot());
    let entry = ExtentRecordEntry::nv_leaf_range(
        logical_start,
        pages as u32,
        references[0].slot_plus_one(),
    )
    .unwrap();
    let key = PageKey::new(fs_id, inode, logical_start);
    let leaf = ExtentNodeRecord::new(
        fs_id,
        inode,
        1,
        logical_start,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &[entry],
    )
    .unwrap();
    let leaf_ref = pool
        .persist_authoritative_metadata(key, &leaf, None)
        .unwrap();
    let locator = ReverseExtentRef::new(leaf_ref.slot().unwrap(), 0).unwrap();
    for reference in &references {
        pool.install_reverse(*reference, locator).unwrap();
    }
    (references, locator)
}

#[ktest]
fn rnvpce_optimistic_extent_claim_releases_metadata_coordination_before_io() {
    const FS_ID: u64 = 0x9913;
    const INODE: u64 = 25;
    const LOGICAL_START: u64 = 64;
    let pool = test_pool(48, 16, 32, None);
    let (references, locator) = install_claimable_nv_extent(&pool, FS_ID, INODE, LOGICAL_START, 4);

    let claimed = pool.claim_migration_extent(references[0], locator).unwrap();
    assert_eq!(claimed.logical_range(), LOGICAL_START..LOGICAL_START + 4);
    assert_eq!(claimed.len(), 4);
    assert!(!pool.migration_is_frozen(PageKey::new(FS_ID, INODE, LOGICAL_START)));
    let foreground = pool.begin_publication_for(FS_ID, INODE).unwrap();
    drop(foreground);
    for reference in &references {
        let page = NvPageId::try_from(reference.slot().unwrap() as usize).unwrap();
        assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::InFlight);
    }

    drop(claimed);
    for reference in references {
        let page = NvPageId::try_from(reference.slot().unwrap() as usize).unwrap();
        assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::NvDirty);
    }
}

#[ktest]
fn rnvpce_optimistic_extent_claim_rolls_back_every_member_on_stale_identity() {
    const FS_ID: u64 = 0x9914;
    const INODE: u64 = 26;
    const LOGICAL_START: u64 = 96;
    let pool = test_pool(48, 16, 32, None);
    let (references, locator) = install_claimable_nv_extent(&pool, FS_ID, INODE, LOGICAL_START, 4);
    pool.replace_reverse(
        references[2],
        ReverseExtentRef::new(locator.leaf_slot(), 1).unwrap(),
    )
    .unwrap();

    assert!(pool.claim_migration_extent(references[0], locator).is_err());
    for reference in references {
        let page = NvPageId::try_from(reference.slot().unwrap() as usize).unwrap();
        assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::NvDirty);
    }
}

#[ktest]
fn rnvpce_extent_node_update_waits_for_a_transient_reader() {
    const FS_ID: u64 = 0x55aa;
    const INODE: u64 = 24;

    let pool = test_pool(16, 16, 0, None);
    let key = PageKey::new(FS_ID, INODE, 0x3200);
    let first = ExtentNodeRecord::new(
        FS_ID,
        INODE,
        1,
        key.page_index,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &[ExtentRecordEntry::leaf(0, 1, 10_000, false).unwrap()],
    )
    .unwrap();
    let node_ref = pool
        .persist_authoritative_metadata(key, &first, None)
        .unwrap();
    let frame = NvFrameId::try_from(node_ref.slot().unwrap() as usize).unwrap();
    let reader = pool.pin_frame(frame).unwrap();
    let second = ExtentNodeRecord::new(
        FS_ID,
        INODE,
        2,
        key.page_index,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &[ExtentRecordEntry::leaf(0, 1, 10_001, false).unwrap()],
    )
    .unwrap();
    let state = Arc::new(AtomicUsize::new(0));
    let updater_pool = pool.clone();
    let updater_state = state.clone();
    let updater = crate::thread::kernel_thread::ThreadOptions::new(move || {
        let publication = updater_pool.begin_publication_for(FS_ID, INODE).unwrap();
        updater_state.store(1, Ordering::Release);
        let result = updater_pool.update_authoritative_metadata_in_place(
            &publication,
            node_ref,
            &second,
            None,
        );
        updater_state.store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
    })
    .spawn();

    while state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..32 {
        crate::thread::Thread::yield_now();
    }
    assert_eq!(
        state.load(Ordering::Acquire),
        1,
        "the extent updater must wait instead of returning EBUSY"
    );
    drop(reader);
    updater.join();
    assert_eq!(state.load(Ordering::Acquire), 2);
    assert_eq!(
        pool.read_authoritative_metadata::<ExtentNodeRecord>(node_ref)
            .unwrap(),
        second
    );
}

#[ktest]
fn rnvpce_extent_node_writer_intent_blocks_new_readers() {
    const FS_ID: u64 = 0x55aa;
    const INODE: u64 = 25;

    let pool = test_pool(16, 16, 0, None);
    let key = PageKey::new(FS_ID, INODE, 0x3201);
    let first = ExtentNodeRecord::new(
        FS_ID,
        INODE,
        1,
        key.page_index,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &[ExtentRecordEntry::leaf(0, 1, 10_000, false).unwrap()],
    )
    .unwrap();
    let node_ref = pool
        .persist_authoritative_metadata(key, &first, None)
        .unwrap();
    let frame = NvFrameId::try_from(node_ref.slot().unwrap() as usize).unwrap();
    let existing_reader = pool.pin_frame(frame).unwrap();
    let second = ExtentNodeRecord::new(
        FS_ID,
        INODE,
        2,
        key.page_index,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &[ExtentRecordEntry::leaf(0, 1, 10_001, false).unwrap()],
    )
    .unwrap();
    let writer_state = Arc::new(AtomicUsize::new(0));
    let writer_pool = pool.clone();
    let observed_writer_state = writer_state.clone();
    let writer = crate::thread::kernel_thread::ThreadOptions::new(move || {
        let publication = writer_pool.begin_publication_for(FS_ID, INODE).unwrap();
        observed_writer_state.store(1, Ordering::Release);
        let result = writer_pool.update_authoritative_metadata_in_place(
            &publication,
            node_ref,
            &second,
            None,
        );
        observed_writer_state.store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
    })
    .spawn();

    while writer_state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..32 {
        crate::thread::Thread::yield_now();
    }

    let reader_state = Arc::new(AtomicUsize::new(0));
    let reader_pool = pool.clone();
    let observed_reader_state = reader_state.clone();
    let late_reader = crate::thread::kernel_thread::ThreadOptions::new(move || {
        observed_reader_state.store(1, Ordering::Release);
        let result = reader_pool.pin_frame(frame);
        observed_reader_state.store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
    })
    .spawn();
    while reader_state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..32 {
        crate::thread::Thread::yield_now();
    }
    assert_eq!(writer_state.load(Ordering::Acquire), 1);
    assert_eq!(
        reader_state.load(Ordering::Acquire),
        1,
        "new extent readers must wait behind writer intent"
    );

    drop(existing_reader);
    writer.join();
    late_reader.join();
    assert_eq!(writer_state.load(Ordering::Acquire), 2);
    assert_eq!(reader_state.load(Ordering::Acquire), 2);
}

#[ktest]
fn rnvpce_publication_context_lifetime_and_isolation() {
    use super::frame::PublicationRole;
    let pool = test_pool(16, 16, 0, None);
    let context = pool.inode_publication(7, 9);
    let weak = Arc::downgrade(&context);
    let guard = context.begin(&pool, PublicationRole::Foreground).unwrap();
    drop(context);
    let cached = pool.inode_publication(7, 9);
    assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), &cached));
    // Another inode, or the same number on another filesystem, remains independent.
    let other = pool.begin_publication_for(7, 10).unwrap();
    let other_fs = pool.begin_publication_for(8, 9).unwrap();
    let foreign_pool = test_pool(16, 16, 0, None);
    assert!(
        cached
            .begin(&foreign_pool, PublicationRole::Completion)
            .is_err()
    );
    drop(other);
    drop(other_fs);
    drop(guard);
    let next = cached.begin(&pool, PublicationRole::Completion).unwrap();
    drop(next);
    drop(cached);
    assert!(weak.upgrade().is_none());
    let recreated = pool.inode_publication(7, 9);
    let _guard = recreated.begin(&pool, PublicationRole::Foreground).unwrap();
}

#[ktest]
fn rnvpc_runtime_backend_rejects_crash_model() {
    let config = RnvpcConfig::from_bytes(
        Some(4 * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(3 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    assert!(DramNvBackend::allocate(config).is_err());
}

#[ktest]
fn rnvpce_allocation_reclaim_failure_preserves_capacity() {
    let pool = test_pool(8, 2, 6, None);
    let (page, seq) = pool.alloc_data_page_seq().unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.replace_data_page_in_owner_at(page, PageKey::new(78, 1, 0), 0, None)
        .unwrap();
    let reference = pool.persistent_data_ref(page).unwrap();
    let reverse = ReverseExtentRef::new(7, 0).unwrap();
    pool.install_reverse(reference, reverse).unwrap();
    let wrong = PersistentObjectRef::new(page.index() as u64, seq + 1).unwrap();
    let lease = pool.pin_for_writeback_seq(page, seq).unwrap();
    assert_eq!(
        lease
            .reclaim_after_ssd_publish(wrong, None)
            .unwrap_err()
            .error(),
        Errno::ESTALE
    );
    assert_eq!(pool.stats().free_pages, 7);
    assert_eq!(pool.persistent_data_ref(page).unwrap(), reference);
    let lease = pool.pin_for_writeback_seq(page, seq).unwrap();
    assert!(
        lease
            .reclaim_after_ssd_publish(reference, Some(FaultPoint::BeforeClear))
            .is_err()
    );
    assert_eq!(pool.stats().free_pages, 7);
    assert_eq!(pool.stats().pinned_frames, 0);
    assert_eq!(pool.reverse_lookup(reference).unwrap(), Some(reverse));
    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::NvDirty);
    pool.pin_for_writeback_seq(page, seq)
        .unwrap()
        .reclaim_after_ssd_publish(reference, None)
        .unwrap();
    assert_eq!(pool.stats().free_pages, 8);
    assert!(!pool.free_data_page_if_seq(page, seq).unwrap());
}

#[ktest]
fn rnvpce_slot_reference_pins_owner_and_blocks_reuse() {
    let pool = test_pool(2, 1, 1, None);
    let old_key = PageKey::new(91, 7, 3);
    let (page, seq) = pool.alloc_data_page_seq().unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.replace_data_page_in_owner_at(page, old_key, 0, None)
        .unwrap();
    let reference = pool.persistent_data_ref(page).unwrap();
    let slot_plus_one = reference.slot().unwrap() + 1;

    let reader = pool
        .pin_persistent_data_slot(slot_plus_one, old_key)
        .unwrap();
    assert_eq!(reader.id(), page);
    assert_eq!(
        pool.pin_for_writeback_seq(page, seq).unwrap_err().error(),
        Errno::EBUSY
    );
    drop(reader);

    pool.pin_for_writeback_seq(page, seq)
        .unwrap()
        .finish(None)
        .unwrap();
    assert!(pool.free_data_page_if_seq(page, seq).unwrap());

    let new_key = PageKey::new(92, 8, 4);
    let (reused, _) = pool.alloc_data_page_seq().unwrap();
    assert_eq!(reused, page);
    pool.transition_data_page(reused, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.replace_data_page_in_owner_at(reused, new_key, 0, None)
        .unwrap();
    assert!(
        pool.pin_persistent_data_slot(slot_plus_one, old_key)
            .is_err()
    );
    assert_eq!(
        pool.pin_persistent_data_slot(slot_plus_one, new_key)
            .unwrap()
            .id(),
        reused
    );
}

#[ktest]
fn rnvpce_allocation_parallel_reclaim_preserves_accounting() {
    let pool = test_pool(32, 8, 24, None);
    let mut threads = Vec::new();
    for inode in 1..=4u64 {
        let pool = pool.clone();
        threads.push(
            crate::thread::kernel_thread::ThreadOptions::new(move || {
                for step in 0..32u64 {
                    let (page, seq) = pool.alloc_data_page_seq().unwrap();
                    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
                        .unwrap();
                    pool.replace_data_page_in_owner_at(
                        page,
                        PageKey::new(79, inode, step),
                        0,
                        None,
                    )
                    .unwrap();
                    let reference = pool.persistent_data_ref(page).unwrap();
                    pool.install_reverse(reference, ReverseExtentRef::new(inode, 0).unwrap())
                        .unwrap();
                    let lease = pool.pin_for_writeback_seq(page, seq).unwrap();
                    crate::thread::Thread::yield_now();
                    lease.reclaim_after_ssd_publish(reference, None).unwrap();
                    assert!(!pool.free_data_page_if_seq(page, seq).unwrap());
                }
            })
            .spawn(),
        );
    }
    for thread in threads {
        thread.join();
    }
    assert_eq!(pool.stats().free_pages, 32);
    assert_eq!(pool.stats().data_pages, 0);
    assert_eq!(pool.stats().pinned_frames, 0);
    let pages: Vec<_> = (0..24)
        .map(|_| pool.alloc_data_page_seq().unwrap())
        .collect();
    let unique: BTreeSet<_> = pages.iter().map(|(page, _)| page.index()).collect();
    assert_eq!(unique.len(), 24);
    assert!(pool.alloc_data_page_seq().is_err());
    for (page, _) in pages {
        pool.abort_data_page(page).unwrap();
    }
    assert_eq!(pool.stats().free_pages, 32);
}

#[ktest]
fn rnvpce_metadata_mutation_guard_wakes_a_waiting_leaf_mutator() {
    const FS_ID: u64 = 0x9923;
    const INODE: u64 = 33;
    let pool = test_pool(8, 8, 0, None);
    let key = PageKey::new(FS_ID, INODE, 0x9b00);
    let leaf = ExtentNodeRecord::new(
        FS_ID,
        INODE,
        1,
        key.page_index,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &[],
    )
    .unwrap();
    let reference = pool
        .persist_authoritative_metadata(key, &leaf, None)
        .unwrap();
    let held = pool.lock_metadata_mutation(reference).unwrap();

    let state = Arc::new(AtomicUsize::new(0));
    let waiter_pool = pool.clone();
    let waiter_state = state.clone();
    let waiter = crate::thread::kernel_thread::ThreadOptions::new(move || {
        waiter_state.store(1, Ordering::Release);
        let guard = waiter_pool.lock_metadata_mutation(reference).unwrap();
        drop(guard);
        waiter_state.store(2, Ordering::Release);
    })
    .spawn();

    while state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..4 {
        crate::thread::Thread::yield_now();
    }
    assert_eq!(
        state.load(Ordering::Acquire),
        1,
        "a same-leaf mutator must wait for the held guard"
    );

    drop(held);
    waiter.join();
    assert_eq!(state.load(Ordering::Acquire), 2);
}

#[ktest]
fn descriptor_update_keeps_old_committed_copy_on_crash() {
    use super::{descriptor::NvDescriptor, dram::DramNvImage, persist::PersistEngine};
    let config = RnvpcConfig::from_bytes(
        Some(8 * PAGE_SIZE),
        Some(2 * PAGE_SIZE),
        Some(6 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let backend = Arc::new(image.attach());
    let engine = PersistEngine::new(backend, 8).unwrap();
    engine.format().unwrap();
    let original = NvDescriptor::for_nv_page(71, 72, 0, 0, 1)
        .with_allocation_seq(1)
        .unwrap();
    engine.commit_frame(0, &original, None).unwrap();
    let mut changed = original;
    changed.assign_migration_target(900).unwrap();
    assert!(
        engine
            .commit_frame(0, &changed, Some(FaultPoint::AfterContentPersist))
            .is_err()
    );
    image.simulate_crash();
    let recovered = PersistEngine::new(Arc::new(image.attach()), 8).unwrap();
    assert_eq!(
        recovered.read_entry(0).unwrap(),
        Some(original),
        "unfinished descriptor update must retain its selected old copy"
    );
}

#[ktest]
fn data_frame_batch_commit_uses_two_fences_and_recovers() {
    use super::{descriptor::NvDescriptor, dram::DramNvImage, persist::PersistEngine};
    let config = RnvpcConfig::from_bytes(
        Some(8 * PAGE_SIZE),
        Some(2 * PAGE_SIZE),
        Some(6 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let backend = Arc::new(image.attach());
    let engine = PersistEngine::new(backend.clone(), 8).unwrap();
    engine.format().unwrap();
    let updates: Vec<_> = (0..3)
        .map(|slot| {
            backend
                .write_page(
                    NvPageId::try_from(slot).unwrap(),
                    &[slot as u8 + 1; PAGE_SIZE],
                )
                .unwrap();
            (
                slot,
                NvDescriptor::for_nv_page(81, 82, slot as u64, 0, 1)
                    .with_allocation_seq(slot as u64 + 1)
                    .unwrap(),
            )
        })
        .collect();
    let before = backend.stats();
    engine.commit_frames(&updates, None).unwrap();
    let after = backend.stats();
    assert_eq!(after.fence_calls - before.fence_calls, 2);
    assert_eq!(after.persist_calls - before.persist_calls, 9);

    image.simulate_crash();
    let backend = Arc::new(image.attach());
    let recovered = PersistEngine::new(backend.clone(), 8).unwrap();
    for (slot, descriptor) in updates {
        assert_eq!(recovered.read_entry(slot).unwrap(), Some(descriptor));
        let mut data = [0; PAGE_SIZE];
        backend
            .read_page(NvPageId::try_from(slot).unwrap(), &mut data)
            .unwrap();
        assert_eq!(data, [slot as u8 + 1; PAGE_SIZE]);
    }
}

#[ktest]
fn descriptor_batch_publication_and_clear_survive_partial_crash() {
    use super::{descriptor::NvDescriptor, dram::DramNvImage, persist::PersistEngine};
    for fault in [
        None,
        Some(FaultPoint::BeforeContentPersist),
        Some(FaultPoint::AfterContentPersist),
        Some(FaultPoint::AfterCommitStore),
        Some(FaultPoint::AfterCommitPersist),
        Some(FaultPoint::AfterCommitFence),
    ] {
        let config = RnvpcConfig::from_bytes(
            Some(8 * PAGE_SIZE),
            Some(2 * PAGE_SIZE),
            Some(6 * PAGE_SIZE),
            Some(NvSimMode::Model),
        )
        .unwrap()
        .unwrap();
        let image = DramNvImage::allocate_model(config).unwrap();
        let backend = Arc::new(image.attach());
        let engine = PersistEngine::new(backend.clone(), 8).unwrap();
        engine.format().unwrap();
        let old: Vec<_> = (0..3)
            .map(|i| {
                (
                    i,
                    NvDescriptor::for_nv_page(71, 72, i as u64, 0, 1)
                        .with_allocation_seq(i as u64 + 1)
                        .unwrap(),
                )
            })
            .collect();
        for (slot, d) in &old {
            engine.commit_frame(*slot, d, None).unwrap();
        }
        let new: Vec<_> = old
            .iter()
            .map(|(i, d)| {
                let mut d = *d;
                d.assign_migration_target(900 + *i as u64).unwrap();
                (*i, d)
            })
            .collect();
        // The working data is deliberately changed without persist: descriptor-only
        // updates must not accidentally make these bytes durable.
        backend
            .write_page(NvPageId::try_from(0usize).unwrap(), &[0x66; PAGE_SIZE])
            .unwrap();
        let before = backend.stats();
        let result = engine.update_descriptors(&new, fault);
        assert_eq!(result.is_ok(), fault.is_none());
        if fault.is_none() {
            assert_eq!(backend.stats().fence_calls - before.fence_calls, 2);
            assert_eq!(backend.stats().persist_calls - before.persist_calls, 6);
        }
        if matches!(
            fault,
            Some(FaultPoint::AfterCommitStore | FaultPoint::AfterCommitPersist)
        ) {
            assert!(
                engine.update_descriptors(&new, None).is_err(),
                "uncertain selectors must block copy reuse until recovery"
            );
        }
        image.simulate_crash();
        let backend = Arc::new(image.attach());
        let recovered = PersistEngine::new(backend.clone(), 8).unwrap();
        for ((slot, old), (_, new)) in old.iter().zip(&new) {
            let actual = recovered.read_entry(*slot).unwrap().unwrap();
            assert!(actual == *old || actual == *new);
            if fault == Some(FaultPoint::AfterContentPersist) {
                assert_eq!(actual, *old);
            }
            if fault.is_none() || fault == Some(FaultPoint::AfterCommitFence) {
                assert_eq!(actual, *new);
            }
            if fault == Some(FaultPoint::AfterCommitPersist) {
                assert_eq!(actual, if *slot == 0 { *new } else { *old });
            }
        }
        let mut data = [0; PAGE_SIZE];
        backend
            .read_page(NvPageId::try_from(0usize).unwrap(), &mut data)
            .unwrap();
        assert_eq!(
            data, [0; PAGE_SIZE],
            "descriptor updates must not repersist data"
        );
        // Recovery can be repeated without changing the selected state.
        image.simulate_crash();
        let backend = Arc::new(image.attach());
        let recovered = PersistEngine::new(backend.clone(), 8).unwrap();
        let before = backend.stats();
        recovered.clear_frames(&[0, 1, 2], None).unwrap();
        assert_eq!(backend.stats().fence_calls - before.fence_calls, 1);
        image.simulate_crash();
        let recovered = PersistEngine::new(Arc::new(image.attach()), 8).unwrap();
        for slot in 0..3 {
            assert!(recovered.read_entry(slot).unwrap().is_none());
        }
    }
}

#[ktest]
fn rnvpce_batch_reclaim_uses_one_durability_barrier() {
    let config = RnvpcConfig::from_bytes(
        Some(16 * PAGE_SIZE),
        Some(4 * PAGE_SIZE),
        Some(12 * PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();
    let backend = Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
    let pool = Arc::new(Rnvpc::new(backend.clone(), config, None).unwrap());
    let mut pages = Vec::new();
    for i in 0..4 {
        let page = pool.stage_data_page(&[0x77; PAGE_SIZE]).unwrap();
        pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
            .unwrap();
        pool.commit_data_page_in_owner(page, PageKey::new(77, 78, i), None)
            .unwrap();
        let reference = pool.persistent_data_ref(page).unwrap();
        pool.install_reverse(reference, ReverseExtentRef::new(7, i as usize).unwrap())
            .unwrap();
        let lease = pool
            .pin_for_writeback_seq(page, reference.allocation_seq())
            .unwrap();
        pages.push((lease, reference));
    }
    let before = backend.stats();
    pool.reclaim_migrated_pages(pages, None).unwrap();
    assert_eq!(
        backend.stats().fence_calls - before.fence_calls,
        1,
        "one batch must clear all descriptors before a shared fence"
    );
    assert_eq!(pool.stats().free_pages, 16);
    assert_eq!(pool.stats().pinned_frames, 0);
}

#[ktest]
fn descriptor_batch_rejects_stale_duplicate_and_legacy_layout() {
    use super::{descriptor::NvDescriptor, dram::DramNvImage, persist::PersistEngine};
    let config = RnvpcConfig::from_bytes(
        Some(8 * PAGE_SIZE),
        Some(2 * PAGE_SIZE),
        Some(6 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let backend = Arc::new(image.attach());
    let engine = PersistEngine::new(backend.clone(), 8).unwrap();
    engine.format().unwrap();
    let d = NvDescriptor::for_nv_page(7, 8, 0, 0, 1)
        .with_allocation_seq(1)
        .unwrap();
    engine.commit_frame(0, &d, None).unwrap();
    let mut changed = d;
    changed.assign_migration_target(900).unwrap();
    assert!(
        engine
            .update_descriptors(&[(0, changed), (0, changed)], None)
            .is_err()
    );
    assert!(
        engine
            .update_descriptors(&[(0, changed.with_allocation_seq(2).unwrap())], None)
            .is_err()
    );
    assert_eq!(engine.read_entry(0).unwrap(), Some(d));
    // Version 3 used one 80-byte descriptor entry. Reject it without writes.
    backend.write_control(16, &3u32.to_ne_bytes()).unwrap();
    backend.persist_control(16, 4).unwrap();
    backend.fence().unwrap();
    let before = backend.stats();
    assert!(engine.read_header().is_err());
    assert_eq!(backend.stats().persist_calls, before.persist_calls);
}
