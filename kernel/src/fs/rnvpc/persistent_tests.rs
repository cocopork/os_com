// SPDX-License-Identifier: MPL-2.0

//! Behavioral regressions for the restored foreground persistent WAL.

use ostd::prelude::ktest;

use super::{
    backend::NvMemoryBackend,
    config::{NvSimMode, RnvpcConfig},
    cow::InodeCowContext,
    dram::DramNvBackend,
    frame::Rnvpc,
    layout::NvImageHeader,
    pcache_store::PcFaultPoint,
};
use crate::prelude::*;

fn pool() -> Arc<Rnvpc> {
    let config = RnvpcConfig::from_bytes(
        Some(1200 * PAGE_SIZE),
        Some(100 * PAGE_SIZE),
        Some(1100 * PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();
    let backend = Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
    Arc::new(Rnvpc::new(backend, config, None).unwrap())
}

#[ktest]
fn persistent_wal_1025_replays_every_repeated_key() {
    let context = InodeCowContext::create(pool(), 81, 82).unwrap();
    for sequence in 0..1024 {
        context
            .update(3, 55, |bytes| bytes[0] = sequence as u8)
            .unwrap();
    }
    assert_eq!(context.wal().pending(), 1024);
    assert_eq!(context.counters().foreground_checkpoints, 0);
    assert!(context.flush_candidates(10).is_empty());
    assert_eq!(context.complete_page(3).unwrap(), 0);
    assert_eq!(context.wal().pending(), 1024);
    context.update(4, 56, |bytes| bytes[0] = 99).unwrap();
    assert_eq!(context.wal().pending(), 1);
    assert_eq!(context.counters().records_replayed, 1024);
    assert_eq!(context.counters().foreground_checkpoints, 1);
    assert_eq!(context.flush_candidates(10).len(), 1);
    let mut bytes = [0; PAGE_SIZE];
    assert!(context.read_page(3, &mut bytes).unwrap());
    assert_eq!(bytes[0], 255);
}

#[ktest]
fn persistent_wal_failed_checkpoint_keeps_full_log() {
    let context = InodeCowContext::create(pool(), 83, 84).unwrap();
    for sequence in 0..1024 {
        context
            .update(3, 55, |bytes| bytes[0] = sequence as u8)
            .unwrap();
    }
    context.set_checkpoint_fault_for_test(Some(PcFaultPoint::BeforeCheckpointAnchorAdvance));
    assert!(context.update(4, 56, |_| {}).is_err());
    assert_eq!(context.wal().pending(), 1024);
    assert_eq!(context.wal().checkpointed_lsn(), 0);
    let mut bytes = [0; PAGE_SIZE];
    assert!(context.read_page(3, &mut bytes).unwrap());
    assert_eq!(bytes[0], 255);
    context.set_checkpoint_fault_for_test(None);
    context.update(4, 56, |_| {}).unwrap();
    assert_eq!(context.wal().pending(), 1);
}

#[ktest]
fn persistent_wal_flush_budget_counts_eligible_pages() {
    let context = InodeCowContext::create(pool(), 85, 86).unwrap();
    for _ in 0..1024 {
        context.update(10, 55, |_| {}).unwrap();
    }
    // The new low key is WAL-pinned, while key 10 was just checkpointed.
    context.update(0, 56, |_| {}).unwrap();
    let candidates = context.flush_candidates(1);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, 10);
}

#[ktest]
fn persistent_wal_finalize_reports_busy_page() {
    let pool = pool();
    let context = InodeCowContext::create(pool.clone(), 87, 88).unwrap();
    let page = pool.stage_data_page(&[0; PAGE_SIZE]).unwrap();
    let generation = pool.data_page_seq(page).unwrap();
    let pin = pool.pin_data_page(page).unwrap();
    assert!(context.finalize_flush_completion(page, generation).is_err());
    drop(pin);
    context.finalize_flush_completion(page, generation).unwrap();
}

fn restartable_pool() -> (super::dram::DramNvImage, RnvpcConfig, Arc<Rnvpc>) {
    let config = RnvpcConfig::from_bytes(
        Some(256 * PAGE_SIZE),
        Some(128 * PAGE_SIZE),
        Some(128 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = super::dram::DramNvImage::allocate_model(config).unwrap();
    let pool = Arc::new(Rnvpc::format(Arc::new(image.attach()), config, None).unwrap());
    (image, config, pool)
}

#[ktest]
fn rnvpce_old_image_version_is_rejected() {
    let (image, config, pool) = restartable_pool();
    drop(pool);
    let backend = Arc::new(image.attach());
    let mut header = NvImageHeader::default();
    backend.read_control(0, header.as_mut_bytes()).unwrap();
    header.version = 4;
    backend.write_control(0, header.as_bytes()).unwrap();
    assert!(Rnvpc::recover(backend, config, None).is_err());
}

#[ktest]
fn persistent_wal_restart_replays_repeated_keys_and_rebuilds_candidates() {
    let (image, config, pool) = restartable_pool();
    let context = InodeCowContext::create(pool.clone(), 89, 90).unwrap();
    for value in 1..=8 {
        context.update(2, 77, |bytes| bytes[0] = value).unwrap();
    }
    assert_eq!(context.wal().pending(), 8);
    drop(context);
    drop(pool);
    image.simulate_crash();
    let pool = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let context = InodeCowContext::recover_from_catalog(pool, 89, 90).unwrap();
    let mut bytes = [0; PAGE_SIZE];
    assert!(context.read_page(2, &mut bytes).unwrap());
    assert_eq!(bytes[0], 8);
    assert_eq!(context.wal().pending(), 0);
    assert_eq!(context.wal().checkpointed_lsn(), 8);
    assert_eq!(context.flush_candidates(1).len(), 1);
}

#[ktest]
fn persistent_wal_ambiguous_append_retains_data_for_restart() {
    let (image, config, pool) = restartable_pool();
    let context = InodeCowContext::create(pool.clone(), 91, 92).unwrap();
    context.update(2, 77, |bytes| bytes[0] = 1).unwrap();
    context.set_append_fault_for_test(Some(PcFaultPoint::AfterWalAnchorSelectorPersist));
    assert!(context.update(2, 77, |bytes| bytes[0] = 2).is_err());
    assert!(context.wal().is_poisoned());
    drop(context);
    drop(pool);
    image.simulate_crash();
    let pool = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let context = InodeCowContext::recover_from_catalog(pool, 91, 92).unwrap();
    let mut bytes = [0; PAGE_SIZE];
    assert!(context.read_page(2, &mut bytes).unwrap());
    assert_eq!(bytes[0], 2);
    assert_eq!(context.wal().checkpointed_lsn(), 2);
}

#[ktest]
fn persistent_wal_checkpoint_splits_and_preserves_all_keys() {
    let context = InodeCowContext::create(pool(), 93, 94).unwrap();
    for sequence in 0..1024 {
        let key = sequence % 100;
        context
            .update(key, 1000 + key, |bytes| bytes[0] = key as u8)
            .unwrap();
    }
    context.update(100, 1100, |bytes| bytes[0] = 100).unwrap();
    assert_eq!(context.counters().records_replayed, 1024);
    assert!(context.foreground_checkpoint_counters().btree_splits > 0);
    for key in 0..=100 {
        let mut bytes = [0; PAGE_SIZE];
        assert!(context.read_page(key, &mut bytes).unwrap());
        assert_eq!(bytes[0], key as u8);
    }
    assert_eq!(context.flush_candidates(1000).len(), 100);
}

#[ktest]
fn persistent_wal_root_split_faults_recover_from_committed_records() {
    use super::{
        data_page::DataPageState,
        index::PageKey,
        pcache_layout::{BTREE_NODE_ENTRY_CAPACITY, PersistentPageRef},
        persistent_btree::PersistentBtree,
        wal::{PersistentInodeWal, WalUpsert},
    };
    for fault in [
        PcFaultPoint::AfterRightNodePersist,
        PcFaultPoint::AfterLeftSiblingLinkPersist,
        PcFaultPoint::AfterNewRootPersist,
        PcFaultPoint::AfterRootAnchorImagePersist,
        PcFaultPoint::AfterRootAnchorPersist,
    ] {
        let (image, config, pool) = restartable_pool();
        let tree = PersistentBtree::create(pool.clone(), 95, 96).unwrap();
        let wal = PersistentInodeWal::create(pool.clone(), 95, 96, tree.root_ref(), 0).unwrap();
        for index in 0..=BTREE_NODE_ENTRY_CAPACITY {
            let page = pool.stage_data_page(&[index as u8; PAGE_SIZE]).unwrap();
            let generation = pool.data_page_seq(page).unwrap();
            pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
                .unwrap();
            pool.commit_data_page_in_owner(page, PageKey::new(95, 96, index as u64), None)
                .unwrap();
            let record = wal
                .append(
                    WalUpsert {
                        page_index: index as u64,
                        old_page: PersistentPageRef::NONE,
                        new_page: PersistentPageRef::new(page.index() as u32, generation),
                        ssd_backing_block: 1000 + index as u64,
                    },
                    None,
                )
                .unwrap()
                .record;
            if index < BTREE_NODE_ENTRY_CAPACITY {
                assert!(tree.replay_upsert(&record, None).unwrap().is_none());
            } else if matches!(
                fault,
                PcFaultPoint::AfterRootAnchorImagePersist | PcFaultPoint::AfterRootAnchorPersist
            ) {
                let update = tree.replay_upsert(&record, None).unwrap().unwrap();
                assert_eq!(
                    wal.publish_root(update, Some(fault)).unwrap_err().error(),
                    Errno::EIO
                );
            } else {
                assert_eq!(
                    tree.replay_upsert(&record, Some(fault))
                        .unwrap_err()
                        .error(),
                    Errno::EIO
                );
            }
        }
        drop(wal);
        drop(tree);
        drop(pool);
        image.simulate_crash();
        let pool = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
        let context = InodeCowContext::recover_from_catalog(pool, 95, 96).unwrap();
        assert_eq!(context.wal().pending(), 0);
        for index in 0..=BTREE_NODE_ENTRY_CAPACITY {
            let mut bytes = [0; PAGE_SIZE];
            assert!(context.read_page(index as u64, &mut bytes).unwrap());
            assert_eq!(bytes[0], index as u8);
        }
    }
}

#[ktest]
fn persistent_wal_and_soft_update_owners_recover_in_shared_pool() {
    use super::write_transaction::{
        AuthoritySnapshot, authoritative_inode_state, bootstrap_inode_authority,
    };
    let (image, config, pool) = restartable_pool();
    let context = InodeCowContext::create(pool.clone(), 101, 7).unwrap();
    context.update(0, 77, |bytes| bytes[0] = 0xa5).unwrap();
    let snapshot = AuthoritySnapshot::new(1, &[], 0, 0, 0o100644, 0).unwrap();
    let authority = bootstrap_inode_authority(pool.clone(), 102, 7, &snapshot, None).unwrap();
    let fixed = authority.inode();
    drop(context);
    drop(pool);
    image.simulate_crash();
    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    assert_eq!(recovered.fixed_inode_ref(102, 7).unwrap(), Some(fixed));
    assert_eq!(
        authoritative_inode_state(&recovered, 102, 7)
            .unwrap()
            .unwrap()
            .file_size(),
        0
    );
    let context = InodeCowContext::recover_from_catalog(recovered, 101, 7).unwrap();
    let mut bytes = [0; PAGE_SIZE];
    assert!(context.read_page(0, &mut bytes).unwrap());
    assert_eq!(bytes[0], 0xa5);
}
