// SPDX-License-Identifier: MPL-2.0

//! Route tests explicitly initialize a shared pool with default modes off.
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aster_block::bio::{BioEnqueueError, BioQueueAffinity, BioType, SubmittedBio};
use ostd::{prelude::*, sync::WaitQueue};

use super::{
    Ext4,
    migration::tests::rnvpce_sync_quiesces_all_queue_bound_workers,
    prelude::*,
    test_utils::{Ext4FixtureBuilder, make_empty_file_inode},
};
use crate::fs::{
    file::StatusFlags,
    vfs::inode::{FileOps, Inode as VfsInode},
};

fn has_native_redirect(
    pool: &Arc<crate::fs::rnvpc::frame::Rnvpc>,
    fs_id: u64,
    inode: u64,
    page_index: u64,
) -> bool {
    pool.lookup_dirty_nv_entry(crate::fs::rnvpc::index::PageKey::new(
        fs_id, inode, page_index,
    ))
    .is_some()
}

#[ktest]
fn ext4_research_routes() {
    crate::fs::fs_impls::ext4::test_utils::scenarios!(
        nv_route_rnvpce_clean_journal_is_unused,
        nv_route_rnvpce_nojournal_export_retries_metadata_errors,
        nv_route_rnvpce_rejects_unrecovered_journal,
        nv_route_rnvpce_reservation_abort_wakes_writer,
        nv_route_rnvpce_reclaim_ready_queue_contract,
        nv_route_rnvpce_completion_publishes_reclaim_work_item,
        nv_route_rnvpce_durability_epoch_coalesces_ready_batches,
        nv_route_rnvpce_durability_epoch_isolates_failed_batch,
        rnvpce_cross_queue_epoch_issues_one_flush,
        rnvpce_cross_queue_mixed_data_error_preserves_failed_entry,
        rnvpce_cross_queue_flush_error_preserves_all_entries,
        rnvpce_sync_quiesces_all_queue_bound_workers,
        nv_route_rnvpce_first_write_publishes_ssd_baseline,
        nv_route_rnvpce_foreground_allocation_is_independent_of_completion,
        nv_route_rnvpce_batch_allocation_durability,
        nv_route_rnvpce_recovers_unflushed_target_bitmap,
        nv_route_rnvpce_recovery_prefers_nv_allocation_bitmap,
        nv_route_rnvpce_sync_transfers_allocation_authority_to_ssd,
        nv_route_rnvpce_sync_exports_contiguous_extent_once,
        nv_route_rnvpce_fragmented_batch_and_partial_rollback,
        nv_route_rnvpce_same_group_entries_submit_independently,
        nv_route_rnvpce_empty_round_skips_target_preparation,
        nv_route_rnvpce_round_budget_is_independent_of_global_in_flight,
        nv_route_rnvpce_capacity_wait_releases_inode_lock,
        nv_route_rnvpce_direct_partial_write_and_deferred_export,
        nv_route_rnvpce_redirect_completion_avoids_leaf,
        nv_route_rnvpc_wal_and_native_paths_remain_distinct,
        nv_route_busy_lease_read_and_partial_write_retry,
        nv_route_rnvpc_failed_allocation_flush_does_not_append_wal,
        nv_route_mount_rejects_switching_persistent_authority,
    );
}

/// Keep real EXT4/NVM state and disk I/O; control only device submission timing.
pub(super) struct QueueRecordingDisk {
    pub(super) disk: Arc<super::test_utils::Ext4MemoryDisk>,
    pub(super) writes: Mutex<Vec<BioQueueAffinity>>,
    pub(super) pause_writes: AtomicUsize,
    pub(super) entered_writes: AtomicUsize,
    pub(super) released_writes: AtomicUsize,
    pub(super) pause_flush: AtomicBool,
    pub(super) entered_flush: AtomicBool,
    pub(super) waiters: WaitQueue,
}

impl Debug for QueueRecordingDisk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("QueueRecordingDisk")
            .field("disk", &self.disk)
            .finish_non_exhaustive()
    }
}

impl QueueRecordingDisk {
    pub(super) fn new(disk: Arc<super::test_utils::Ext4MemoryDisk>) -> Self {
        Self {
            disk,
            writes: Mutex::new(Vec::new()),
            pause_writes: AtomicUsize::new(0),
            entered_writes: AtomicUsize::new(0),
            released_writes: AtomicUsize::new(0),
            pause_flush: AtomicBool::new(false),
            entered_flush: AtomicBool::new(false),
            waiters: WaitQueue::new(),
        }
    }
}

impl BlockDevice for QueueRecordingDisk {
    fn enqueue(&self, bio: SubmittedBio) -> core::result::Result<(), BioEnqueueError> {
        if bio.type_() == BioType::Write {
            self.writes.lock().push(bio.queue_affinity());
            if let BioQueueAffinity::Queue(queue) = bio.queue_affinity() {
                let bit = 1 << queue;
                if self.pause_writes.load(Ordering::Acquire) & bit != 0 {
                    self.entered_writes.fetch_or(bit, Ordering::AcqRel);
                    self.waiters.wake_all();
                    self.waiters.wait_until(|| {
                        (self.released_writes.load(Ordering::Acquire) & bit != 0).then_some(())
                    });
                }
            }
        }
        if bio.type_() == BioType::Flush && self.pause_flush.load(Ordering::Acquire) {
            self.entered_flush.store(true, Ordering::Release);
            self.waiters.wake_all();
            self.waiters
                .wait_until(|| (!self.pause_flush.load(Ordering::Acquire)).then_some(()));
        }
        self.disk.enqueue(bio)
    }

    fn metadata(&self) -> aster_block::BlockDeviceMeta {
        aster_block::BlockDeviceMeta {
            nr_submission_queues: 4,
            max_nr_segments_per_bio: 1,
            ..self.disk.metadata()
        }
    }

    fn name(&self) -> &str {
        "rtio-queue-test"
    }

    fn id(&self) -> device_id::DeviceId {
        self.disk.id()
    }
}

pub(super) fn cross_queue_fixture(uuid: u8) -> (Arc<Ext4>, Arc<QueueRecordingDisk>) {
    // These scenarios also run inside ext4_research_routes. Distinct mounts
    // must not recover authority left by an earlier invocation in the same VM.
    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
    let mut uuid = [uuid; 16];
    let invocation = (NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed) as u64).to_le_bytes();
    uuid[1..8].copy_from_slice(&invocation[..7]);
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid(uuid)
        .build()
        .unwrap();
    for ino in 11..15 {
        fixture.write_raw_inode(ino, &make_empty_file_inode());
    }
    let disk = Arc::new(QueueRecordingDisk::new(fixture.disk.clone()));
    drop(fixture);
    let options = CString::new("rnvpce=on,rtio=off").unwrap();
    let fs = Ext4::open(disk.clone(), Some(options.as_c_str())).unwrap();
    crate::fs::rnvpc::pool()
        .unwrap()
        .configure_migration_workers(fs.rnvpc_fs_id(), 4)
        .unwrap();
    (fs, disk)
}

#[ktest]
fn rnvpce_foreground_publication_keeps_owner_and_wakes_only_its_worker() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xeb; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let before = fixture.ext4.nv_migration_worker_epochs[0].load(Ordering::Acquire);
    inode
        .write_at(
            0,
            &mut VmReader::from([0x41; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    let pool = crate::fs::rnvpc::pool().unwrap();
    let owner = pool
        .nv_entry_owner_worker(fixture.ext4.rnvpc_fs_id(), 11)
        .unwrap();
    assert_eq!(owner, 0);
    assert!(fixture.ext4.nv_migration_worker_epochs[owner].load(Ordering::Acquire) > before);

    inode
        .write_at(
            PAGE_SIZE,
            &mut VmReader::from([0x42; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    assert_eq!(
        pool.nv_entry_owner_worker(fixture.ext4.rnvpc_fs_id(), 11),
        Some(owner)
    );
}

#[ktest]
fn rnvpce_owner_wake_migrates_below_global_watermark() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xec; 16])
        .with_mount_options("rnvpce=on,rtio=on")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    fixture
        .ext4
        .read_inode(11)
        .unwrap()
        .write_at(
            0,
            &mut VmReader::from([0x51; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();

    let pool = crate::fs::rnvpc::pool().unwrap();
    let deadline = ostd::timer::Jiffies::elapsed().as_duration() + Duration::from_secs(2);
    while has_native_redirect(pool, fixture.ext4.rnvpc_fs_id(), 11, 0)
        && ostd::timer::Jiffies::elapsed().as_duration() < deadline
    {
        crate::thread::Thread::yield_now();
    }
    assert!(
        !has_native_redirect(pool, fixture.ext4.rnvpc_fs_id(), 11, 0),
        "owner notification must drive ordinary dirty work below the global watermark"
    );
}

pub(super) fn cross_queue_write(fs: &Ext4, ino: u32) {
    fs.read_inode(ino)
        .unwrap()
        .write_at(
            0,
            &mut VmReader::from([ino as u8; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
}

fn assert_cross_queue_entry(
    fs: &Ext4,
    ino: u32,
    state: crate::fs::rnvpc::redirect_index::NvExtentEntryState,
) {
    use crate::fs::rnvpc::{descriptor::NvRedirectAuthority, index::PageKey};

    let entry = crate::fs::rnvpc::pool()
        .unwrap()
        .lookup_dirty_nv_entry(PageKey::new(fs.rnvpc_fs_id(), u64::from(ino), 0))
        .expect("NV authority must survive until reclaim");
    assert_eq!(entry.state(), state, "inode {ino}");
    assert_eq!(
        entry.record().authority().unwrap(),
        NvRedirectAuthority::NvAuthoritative
    );
}

pub(super) fn assert_cross_queue_ssd_page(fs: &Ext4, disk: &QueueRecordingDisk, ino: u32) {
    use crate::fs::rnvpc::{index::PageKey, write_transaction::current_page_mapping};

    let pool = crate::fs::rnvpc::pool().unwrap();
    assert!(
        pool.lookup_dirty_nv_entry(PageKey::new(fs.rnvpc_fs_id(), u64::from(ino), 0))
            .is_none()
    );
    let target = current_page_mapping(pool, fs.rnvpc_fs_id(), u64::from(ino), 0)
        .unwrap()
        .unwrap()
        .ssd_block();
    let mut bytes = [0; PAGE_SIZE];
    disk.disk
        .segment()
        .read_bytes(target as usize * PAGE_SIZE, &mut bytes)
        .unwrap();
    assert_eq!(bytes, [ino as u8; PAGE_SIZE]);
}

// Catches per-queue coordinators/flushes, premature reclaim, and leaked credits.
#[ktest]
fn rnvpce_cross_queue_epoch_issues_one_flush() {
    use crate::fs::rnvpc::redirect_index::NvExtentEntryState::{IoComplete, Migrating};

    let (fs, disk) = cross_queue_fixture(0xe5);
    for queue in 0..4 {
        let ino = 11 + u32::from(queue);
        cross_queue_write(&fs, ino);
        assert_eq!(
            fs.migrate_nv_round_on_queue(1, BioQueueAffinity::Queue(queue))
                .unwrap(),
            1
        );
        assert_cross_queue_entry(&fs, ino, Migrating);
    }
    assert_eq!(
        *disk.writes.lock(),
        vec![
            BioQueueAffinity::Queue(0),
            BioQueueAffinity::Queue(1),
            BioQueueAffinity::Queue(2),
            BioQueueAffinity::Queue(3)
        ]
    );
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 4);
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
    assert_eq!(fs.help_nv_reclaim(4).unwrap(), 0);
    let before = disk.disk.flush_count();
    let deadline = ostd::timer::Jiffies::elapsed().as_duration() + Duration::from_secs(2);
    let mut completed = 0;
    while completed != 4 {
        completed += fs.process_nv_completions().unwrap();
        assert!(
            ostd::timer::Jiffies::elapsed().as_duration() < deadline,
            "all cross-queue epochs must complete"
        );
        crate::thread::Thread::yield_now();
    }
    // Common sync constructs Any; the concrete virtio queue-0 flush route is
    // covered by the driver test, below this filesystem recording boundary.
    assert!((1..=4).contains(&(disk.disk.flush_count() - before)));
    for ino in 11..15 {
        assert_cross_queue_entry(&fs, ino, IoComplete);
    }
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 4);
    assert_eq!(fs.help_nv_reclaim(4).unwrap(), 4);
    for ino in 11..15 {
        assert_cross_queue_ssd_page(&fs, &disk, ino);
        assert_eq!(
            crate::fs::rnvpc::pool()
                .unwrap()
                .nv_entry_owner_worker(fs.rnvpc_fs_id(), u64::from(ino)),
            None
        );
    }
    assert_eq!(fs.process_nv_completions().unwrap(), 0);
    assert!((1..=4).contains(&(disk.disk.flush_count() - before)));
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
}

// Catches restoring the whole epoch when only one queue's data failed, or
// publishing the failed entry alongside successful work after the shared flush.
#[ktest]
fn rnvpce_cross_queue_mixed_data_error_preserves_failed_entry() {
    use crate::fs::rnvpc::{
        redirect_index::NvExtentEntryState::{Dirty, IoComplete},
        write_transaction::current_page_mapping,
    };

    let (fs, disk) = cross_queue_fixture(0xe6);
    let pool = crate::fs::rnvpc::pool().unwrap();
    for queue in 0..4 {
        let ino = 11 + u32::from(queue);
        cross_queue_write(&fs, ino);
        if queue == 1 {
            let target = current_page_mapping(pool, fs.rnvpc_fs_id(), 12, 0)
                .unwrap()
                .unwrap()
                .ssd_block();
            disk.disk.set_fail_write_block(Some(target as usize));
        }
        assert_eq!(
            fs.migrate_nv_round_on_queue(1, BioQueueAffinity::Queue(queue))
                .unwrap(),
            1
        );
        disk.disk.set_fail_write_block(None);
    }
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 4);
    let before = disk.disk.flush_count();
    assert_eq!(fs.process_nv_completions().unwrap_err().error(), Errno::EIO);
    assert_eq!(disk.disk.flush_count() - before, 1);
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 3);
    for ino in 11..15 {
        assert_cross_queue_entry(&fs, ino, if ino == 12 { Dirty } else { IoComplete });
    }
    assert_eq!(fs.help_nv_reclaim(4).unwrap(), 3);
    for ino in [11, 13, 14] {
        assert_cross_queue_ssd_page(&fs, &disk, ino);
    }
    assert_cross_queue_entry(&fs, 12, Dirty);
    let mut bytes = [0; PAGE_SIZE];
    fs.read_inode(12)
        .unwrap()
        .read_at(0, &mut VmWriter::from(bytes.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(
        bytes, [12; PAGE_SIZE],
        "failed data must remain readable from NV"
    );
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
    let retry_owner = pool.nv_entry_owner_worker(fs.rnvpc_fs_id(), 12).unwrap();
    assert_eq!(
        fs.migrate_nv_round_on_queue(
            1,
            BioQueueAffinity::Queue(u16::try_from(retry_owner).unwrap()),
        )
        .unwrap(),
        1
    );
    fs.drain_nv_in_flight().unwrap();
    assert_cross_queue_ssd_page(&fs, &disk, 12);
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
}

// Catches publishing any reclaim after an unsuccessful common durability barrier.
#[ktest]
fn rnvpce_cross_queue_flush_error_preserves_all_entries() {
    use crate::fs::rnvpc::{
        redirect_index::NvExtentEntryState::Dirty, write_transaction::current_page_mapping,
    };

    let (fs, disk) = cross_queue_fixture(0xe7);
    let pool = crate::fs::rnvpc::pool().unwrap();
    let mut targets = Vec::new();
    for queue in 0..4 {
        let ino = 11 + u32::from(queue);
        cross_queue_write(&fs, ino);
        targets.push(
            current_page_mapping(pool, fs.rnvpc_fs_id(), u64::from(ino), 0)
                .unwrap()
                .unwrap()
                .ssd_block(),
        );
        assert_eq!(
            fs.migrate_nv_round_on_queue(1, BioQueueAffinity::Queue(queue))
                .unwrap(),
            1
        );
    }
    let before = disk.disk.flush_count();
    disk.disk.set_fail_flushes(true);
    assert_eq!(fs.process_nv_completions().unwrap_err().error(), Errno::EIO);
    disk.disk.set_fail_flushes(false);
    assert_eq!(disk.disk.flush_count() - before, 1);
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
    assert_eq!(fs.help_nv_reclaim(4).unwrap(), 0);
    for (index, target) in targets.into_iter().enumerate() {
        let ino = 11 + index as u32;
        assert_cross_queue_entry(&fs, ino, Dirty);
        assert_eq!(
            current_page_mapping(pool, fs.rnvpc_fs_id(), u64::from(ino), 0)
                .unwrap()
                .unwrap()
                .ssd_block(),
            target
        );
        let mut bytes = [0; PAGE_SIZE];
        fs.read_inode(ino)
            .unwrap()
            .read_at(0, &mut VmWriter::from(bytes.as_mut_slice()).to_fallible())
            .unwrap();
        assert_eq!(bytes, [ino as u8; PAGE_SIZE]);
    }
    // Candidate selection is independent of inode order. Check the entire
    // failed epoch before any retry can move an entry back to Migrating.
    for queue in [1, 2, 3, 0] {
        assert_eq!(
            fs.migrate_nv_round_on_queue(1, BioQueueAffinity::Queue(queue))
                .unwrap(),
            1
        );
    }
    fs.drain_nv_in_flight().unwrap();
    assert_eq!(disk.disk.flush_count() - before, 2);
    for ino in 11..15 {
        assert_cross_queue_ssd_page(&fs, &disk, ino);
    }
    assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
}

fn nv_route_rnvpce_durability_epoch_coalesces_ready_batches() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xd4; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    fixture.write_raw_inode(12, &make_empty_file_inode());
    let input = [0x6d; 4 * PAGE_SIZE];
    for ino in [11, 12] {
        fixture
            .ext4
            .read_inode(ino)
            .unwrap()
            .write_at(0, &mut VmReader::from(input.as_slice()).to_fallible())
            .unwrap();
        assert_eq!(fixture.ext4.migrate_nv_round(4).unwrap(), 4);
    }
    let before = fixture.disk.flush_count();
    assert_eq!(fixture.ext4.process_nv_completions().unwrap(), 8);
    assert_eq!(
        fixture.disk.flush_count() - before,
        1,
        "all ready rNVPCe batches in one drain must share one device flush"
    );
    assert_eq!(fixture.ext4.help_nv_reclaim(8).unwrap(), 8);
}

fn nv_route_rnvpce_durability_epoch_isolates_failed_batch() {
    use crate::fs::rnvpc::{index::PageKey, redirect_index::NvExtentEntryState};

    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xd5; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    fixture.write_raw_inode(12, &make_empty_file_inode());
    let pool = crate::fs::rnvpc::pool().unwrap();
    let fs_id = fixture.ext4.rnvpc_fs_id();

    fixture
        .ext4
        .read_inode(11)
        .unwrap()
        .write_at(
            0,
            &mut VmReader::from([0x71; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    let failed_target =
        crate::fs::rnvpc::write_transaction::current_page_mapping(pool, fs_id, 11, 0)
            .unwrap()
            .unwrap()
            .ssd_block() as usize;
    fixture.disk.set_fail_write_block(Some(failed_target));
    assert_eq!(fixture.ext4.migrate_nv_round(1).unwrap(), 1);
    fixture.disk.set_fail_write_block(None);

    fixture
        .ext4
        .read_inode(12)
        .unwrap()
        .write_at(
            0,
            &mut VmReader::from([0x72; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    assert_eq!(fixture.ext4.migrate_nv_round(1).unwrap(), 1);

    assert_eq!(
        fixture.ext4.process_nv_completions().unwrap_err().error(),
        Errno::EIO
    );
    assert_eq!(fixture.ext4.nv_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(
        pool.lookup_dirty_nv_entry(PageKey::new(fs_id, 11, 0))
            .unwrap()
            .state(),
        NvExtentEntryState::Dirty
    );
    assert_eq!(
        pool.lookup_dirty_nv_entry(PageKey::new(fs_id, 12, 0))
            .unwrap()
            .state(),
        NvExtentEntryState::IoComplete
    );
    assert_eq!(fixture.ext4.help_nv_reclaim(1).unwrap(), 1);
    assert!(
        pool.lookup_dirty_nv_entry(PageKey::new(fs_id, 12, 0))
            .is_none()
    );
    assert!(
        pool.lookup_dirty_nv_entry(PageKey::new(fs_id, 11, 0))
            .is_some()
    );
}

fn nv_route_rnvpce_sync_exports_contiguous_extent_once() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xce; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let input = vec![0x5a; 8 * PAGE_SIZE];
    inode
        .write_at(0, &mut VmReader::from(input.as_slice()).to_fallible())
        .unwrap();
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 8);
    fixture.ext4.drain_nv_in_flight().unwrap();

    inode.reset_preallocated_install_calls();
    fixture.ext4.sync_all().unwrap();
    assert_eq!(
        inode.preallocated_install_calls(),
        1,
        "one contiguous SSD extent must be exported as one native extent transaction"
    );
    let first = inode.data_block_of(0).unwrap();
    for logical in 0..8 {
        assert_eq!(
            inode.data_block_of(logical),
            Some(first + u64::from(logical))
        );
    }
}

fn nv_route_rnvpce_first_write_publishes_ssd_baseline() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xcc; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let pool = crate::fs::rnvpc::pool().unwrap();
    let allocation_records_before =
        crate::fs::rnvpc::write_transaction::allocation_record_count(pool);
    let input = vec![0x57; 4 * PAGE_SIZE];
    inode
        .write_at(0, &mut VmReader::from(input.as_slice()).to_fallible())
        .unwrap();
    inode
        .write_at(
            4 * PAGE_SIZE,
            &mut VmReader::from(input.as_slice()).to_fallible(),
        )
        .unwrap();

    let mut targets = Vec::new();
    for logical in 0..8 {
        let mapping = crate::fs::rnvpc::write_transaction::current_page_mapping(
            pool,
            fixture.ext4.rnvpc_fs_id(),
            11,
            logical,
        )
        .unwrap()
        .expect("foreground first write must publish its SSD baseline");
        assert!(mapping.is_written());
        targets.push(mapping.ssd_block());
        assert!(
            has_native_redirect(pool, fixture.ext4.rnvpc_fs_id(), 11, logical),
            "the NV redirect remains authoritative before migration completes"
        );
    }
    assert!(targets.windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert_eq!(
        crate::fs::rnvpc::write_transaction::allocation_record_count(pool),
        allocation_records_before,
        "foreground allocation ownership ends when the SSD baseline is published"
    );

    crate::fs::rnvpc::batch_metrics::reset_for_test();
    let routes = pool.extent_tree_routes();
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 8);
    fixture.ext4.drain_nv_in_flight().unwrap();
    assert_eq!(pool.extent_tree_routes(), routes);
    assert_eq!(
        crate::fs::rnvpc::batch_metrics::event_count(
            crate::fs::rnvpc::batch_metrics::Event::StructuralBaselineMutations,
        ),
        0
    );
}

fn nv_route_rnvpce_foreground_allocation_is_independent_of_completion() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xcd; 16])
        .with_mount_options("rnvpce=on,rtio=on")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    fixture.write_raw_inode(12, &make_empty_file_inode());
    fixture
        .ext4
        .read_inode(11)
        .unwrap()
        .write_at(
            0,
            &mut VmReader::from([0x31; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    assert_eq!(fixture.ext4.migrate_nv_round(1).unwrap(), 1);
    let completion = fixture.ext4.nv_completion_lock.lock();

    let state = Arc::new(AtomicUsize::new(0));
    let worker_state = state.clone();
    let worker_inode = fixture.ext4.read_inode(12).unwrap();
    let worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
        worker_state.store(1, Ordering::Release);
        let result = worker_inode.write_at(
            0,
            &mut VmReader::from([0x32; PAGE_SIZE].as_slice()).to_fallible(),
        );
        worker_state.store(
            match result {
                Ok(_) => 2,
                Err(error) if error.error() == Errno::EBUSY => 3,
                Err(_) => 4,
            },
            Ordering::Release,
        );
    })
    .spawn();
    while state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..256 {
        if state.load(Ordering::Acquire) != 1 {
            break;
        }
        crate::thread::Thread::yield_now();
    }
    assert_eq!(
        state.load(Ordering::Acquire),
        2,
        "entry migration must not hold an allocation-group lease needed by another writer"
    );

    drop(completion);
    fixture.ext4.drain_nv_in_flight().unwrap();
    worker.join();
    assert_eq!(state.load(Ordering::Acquire), 2);
    fixture.ext4.sync_all().unwrap();
}

fn nv_route_rnvpce_reclaim_ready_queue_contract() {
    super::migration::assert_reclaim_queue_contract();
}

fn nv_route_rnvpce_completion_publishes_reclaim_work_item() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xca; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    inode
        .write_at(
            0,
            &mut VmReader::from([0x5a; 4 * PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();

    assert_eq!(fixture.ext4.migrate_nv_round(4).unwrap(), 4);
    assert_eq!(fixture.ext4.process_nv_completions().unwrap(), 4);
    assert_eq!(fixture.ext4.nv_reclaim_pending.load(Ordering::Acquire), 4);
    let pool = crate::fs::rnvpc::pool().unwrap();
    for page_index in 0..4 {
        assert!(
            pool.lookup_dirty_nv_entry(crate::fs::rnvpc::index::PageKey::new(
                fixture.ext4.rnvpc_fs_id(),
                11,
                page_index,
            ))
            .is_some(),
            "completion must leave the durable NV redirect for a helper"
        );
    }

    pool.reset_baseline_validation_calls_for_test();
    assert_eq!(fixture.ext4.help_nv_reclaim(4).unwrap(), 4);
    assert_eq!(
        pool.baseline_validation_calls_for_test(),
        0,
        "entry reclaim must not validate a per-page or shared baseline token"
    );
    assert_eq!(fixture.ext4.nv_reclaim_pending.load(Ordering::Acquire), 0);
    for page_index in 0..4 {
        assert!(
            !has_native_redirect(pool, fixture.ext4.rnvpc_fs_id(), 11, page_index),
            "the helper must publish the SSD baseline and retire the redirect"
        );
    }
}

fn nv_route_rnvpce_same_group_entries_submit_independently() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xc9; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    fixture.write_raw_inode(12, &make_empty_file_inode());
    fixture
        .ext4
        .read_inode(11)
        .unwrap()
        .write_at(
            0,
            &mut VmReader::from([0x4a; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    fixture
        .ext4
        .read_inode(12)
        .unwrap()
        .write_at(
            0,
            &mut VmReader::from([0x4b; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();

    assert_eq!(fixture.ext4.migrate_nv_round(1).unwrap(), 1);
    let pool = crate::fs::rnvpc::pool().unwrap();
    let states = [11, 12].map(|inode| {
        pool.lookup_dirty_nv_entry(crate::fs::rnvpc::index::PageKey::new(
            fixture.ext4.rnvpc_fs_id(),
            inode,
            0,
        ))
        .unwrap()
        .state()
    });
    assert!(states.contains(&crate::fs::rnvpc::redirect_index::NvExtentEntryState::Migrating));
    assert!(states.contains(&crate::fs::rnvpc::redirect_index::NvExtentEntryState::Dirty));
    assert_eq!(fixture.ext4.migrate_nv_round(1).unwrap(), 1);
    fixture.ext4.drain_nv_in_flight().unwrap();
}

fn nv_route_rnvpce_empty_round_skips_target_preparation() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xcb; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();

    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 0);
}

fn nv_route_rnvpce_sync_transfers_allocation_authority_to_ssd() {
    use crate::fs::rnvpc::allocation_metadata::AllocationMetadataCache;

    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(256)
        .with_uuid([0xc8; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    inode
        .write_at(
            0,
            &mut VmReader::from([0x6d; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    assert_eq!(fixture.ext4.migrate_nv_round(1).unwrap(), 1);
    fixture.ext4.drain_nv_in_flight().unwrap();
    let pool = crate::fs::rnvpc::pool().unwrap();
    let target = crate::fs::rnvpc::write_transaction::current_page_mapping(
        pool,
        fixture.ext4.rnvpc_fs_id(),
        11,
        0,
    )
    .unwrap()
    .unwrap()
    .ssd_block();
    // Model an extent-export metadata allocation after rTIO published its
    // target bitmap. sync must refresh the resident NVM generation instead of
    // silently accepting the older mutation epoch.
    let late_allocation = fixture.ext4.alloc_blocks(1, target + 1, None).unwrap();
    let late_target = late_allocation.start;
    let bitmap_block = fixture.ext4.block_group(0).block_bitmap_bid_for_test();
    let bit = u16::try_from(target - fixture.ext4.block_group(0).first_block()).unwrap();
    let late_bit = u16::try_from(late_target - fixture.ext4.block_group(0).first_block()).unwrap();
    let mut raw_bitmap = [0u8; BLOCK_SIZE];
    fixture
        .disk
        .segment()
        .read_bytes(bitmap_block as usize * BLOCK_SIZE, &mut raw_bitmap)
        .unwrap();
    assert_eq!(raw_bitmap[bit as usize / 8] & (1 << (bit % 8)), 0);
    assert_eq!(raw_bitmap[late_bit as usize / 8] & (1 << (late_bit % 8)), 0);

    fixture.ext4.sync_all().unwrap();
    assert!(
        AllocationMetadataCache::new(pool.clone())
            .selected_image(fixture.ext4.rnvpc_fs_id(), 0)
            .unwrap()
            .is_none()
    );
    fixture
        .disk
        .segment()
        .read_bytes(bitmap_block as usize * BLOCK_SIZE, &mut raw_bitmap)
        .unwrap();
    assert_ne!(raw_bitmap[bit as usize / 8] & (1 << (bit % 8)), 0);
    assert_ne!(raw_bitmap[late_bit as usize / 8] & (1 << (late_bit % 8)), 0);

    assert_eq!(
        fixture.ext4.super_block().free_blocks_count(),
        u64::from(fixture.ext4.block_group(0).free_blocks_count())
    );
}

fn nv_route_rnvpce_recovery_prefers_nv_allocation_bitmap() {
    use crate::fs::rnvpc::allocation_metadata::AllocationMetadataCache;

    crate::fs::rnvpc::init_for_ext4_tests();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(256)
        .with_uuid([0xc7; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    let group = fixture.ext4.block_group(0);
    let goal = (group.first_block()..=group.last_block())
        .find(|block| {
            let bit = u16::try_from(*block - group.first_block()).unwrap();
            !group.metadata().block_bitmap.is_allocated(bit)
        })
        .expect("fixture must contain a free data block");
    let target = fixture.ext4.alloc_blocks(1, goal, None).unwrap().start;
    let image = group.allocation_image(1).unwrap();
    AllocationMetadataCache::new(crate::fs::rnvpc::pool().unwrap().clone())
        .publish_initial(fixture.ext4.rnvpc_fs_id(), 0, &image, None)
        .unwrap();

    let disk = fixture.disk.clone();
    core::mem::forget(fixture);
    let options = CString::new("rnvpce=on,rtio=off").unwrap();
    let recovered = Ext4::open(disk, Some(options.as_c_str())).unwrap();
    let bit = u16::try_from(target - recovered.block_group(0).first_block()).unwrap();
    assert!(
        recovered
            .block_group(0)
            .metadata()
            .block_bitmap
            .is_allocated(bit)
    );
    let group_free = recovered.block_group(0).free_blocks_count();
    assert_eq!(
        recovered.super_block().free_blocks_count(),
        u64::from(group_free)
    );
}

fn nv_route_rnvpce_redirect_completion_avoids_leaf() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_uuid([0xb4; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let first = vec![0x41; 4 * PAGE_SIZE];
    inode
        .write_at(0, &mut VmReader::from(first.as_slice()).to_fallible())
        .unwrap();
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 4);
    fixture.ext4.drain_nv_in_flight().unwrap();

    crate::fs::rnvpc::batch_metrics::reset_for_test();
    let second = vec![0x82; 4 * PAGE_SIZE];
    inode
        .write_at(0, &mut VmReader::from(second.as_slice()).to_fallible())
        .unwrap();
    let pool = crate::fs::rnvpc::pool().unwrap();
    let routes = pool.extent_tree_routes();
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 4);
    fixture.ext4.drain_nv_in_flight().unwrap();
    assert_eq!(pool.extent_tree_routes(), routes);
    use crate::fs::rnvpc::batch_metrics::{Event, event_count};
    assert_eq!(event_count(Event::RedirectPublishBatches), 1);
    assert_eq!(event_count(Event::RedirectPublishedPages), 4);
    assert_eq!(event_count(Event::CandidateRanges), 1);
    assert_eq!(event_count(Event::CandidatePages), 4);
    assert_eq!(event_count(Event::CompletionComparisons), 0);
    assert_eq!(event_count(Event::DescriptorClearBatches), 0);
    assert_eq!(event_count(Event::ReclaimedPages), 4);
    assert_eq!(event_count(Event::ReclaimedSlotRanges), 1);
    assert_eq!(event_count(Event::StructuralBaselineMutations), 0);
    for logical in 0..4 {
        assert!(!has_native_redirect(
            pool,
            fixture.ext4.rnvpc_fs_id(),
            11,
            logical
        ));
    }
    let mut output = vec![0; second.len()];
    inode
        .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(output, second);
}

fn nv_route_rnvpce_round_budget_is_independent_of_global_in_flight() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xb2; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let bytes = vec![0x47; 2 * PAGE_SIZE];
    inode
        .write_at(3, &mut VmReader::from(bytes.as_slice()).to_fallible())
        .unwrap();

    let prior_in_flight = usize::MAX / 2;
    fixture
        .ext4
        .nv_in_flight
        .store(prior_in_flight, Ordering::Release);
    let submitted = fixture.ext4.migrate_nv_round(64).unwrap();
    fixture
        .ext4
        .nv_in_flight
        .fetch_sub(prior_in_flight, Ordering::AcqRel);
    assert!(
        submitted > 0,
        "a migration round must not consume a global in-flight budget"
    );
    fixture.ext4.drain_nv_in_flight().unwrap();
}

fn nv_route_rnvpce_clean_journal_is_unused() {
    use super::{feature::FeatureCompatSet, test_utils::JOURNAL_START_BLOCK};

    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_journal_inode(16)
        .with_uuid([0xb1; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    assert!(
        fixture.ext4.journal().is_none(),
        "rNVPCe must not start JBD2"
    );
    assert!(fixture.ext4.begin_op(1).unwrap().get().is_none());
    assert!(!fixture.ext4.super_block().needs_recovery());
    assert_eq!(
        fixture.disk.flush_count(),
        0,
        "mount must not stamp RECOVER"
    );
    let mut journal_before = vec![0; 16 * BLOCK_SIZE];
    fixture
        .disk
        .segment()
        .read_bytes(
            JOURNAL_START_BLOCK as usize * BLOCK_SIZE,
            &mut journal_before,
        )
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let bytes = vec![0x86; 8 * PAGE_SIZE];
    inode
        .write_at(3, &mut VmReader::from(bytes.as_slice()).to_fallible())
        .unwrap();
    let before = fixture.disk.flush_count();
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 9);
    assert_eq!(
        fixture.disk.flush_count() - before,
        0,
        "orderless submission must not flush before the completion batch"
    );
    fixture.ext4.drain_nv_in_flight().unwrap();
    assert_eq!(
        fixture.disk.flush_count() - before,
        1,
        "one completed migration batch needs exactly one SSD barrier"
    );
    // Inode11 lies in inode-table block4. Export must not synchronously
    // rewrite its descriptor once for every installed data page.
    fixture.disk.count_writes_to(4);
    fixture.ext4.sync_all().unwrap();
    assert!(
        fixture.disk.counted_writes() <= 2,
        "nojournal export rewrites inode per page: {}",
        fixture.disk.counted_writes()
    );
    let mut output = vec![0; bytes.len() + 3];
    inode
        .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(&output[..3], &[0; 3]);
    assert_eq!(&output[3..], &bytes);
    assert_eq!(fixture.read_raw_inode(11).size_lo as usize, output.len());
    let mut persisted = vec![0; output.len()];
    for (index, chunk) in persisted.chunks_mut(PAGE_SIZE).enumerate() {
        let block = inode.data_block_of(index as u32).unwrap();
        fixture
            .disk
            .segment()
            .read_bytes(block as usize * BLOCK_SIZE, chunk)
            .unwrap();
    }
    assert_eq!(persisted, output);
    drop(inode);
    let disk = fixture.disk.clone();
    drop(fixture);
    let mut journal_after = vec![0; journal_before.len()];
    disk.segment()
        .read_bytes(
            JOURNAL_START_BLOCK as usize * BLOCK_SIZE,
            &mut journal_after,
        )
        .unwrap();
    assert_eq!(
        journal_before, journal_after,
        "rNVPCe must leave the unused log untouched"
    );
    let raw: super::super_block::RawSuperBlock = disk
        .segment()
        .read_val(super::super_block::SUPER_BLOCK_OFFSET)
        .unwrap();
    assert_ne!(raw.feature_compat & FeatureCompatSet::HAS_JOURNAL.bits(), 0);
    assert_eq!(
        raw.feature_incompat & super::feature::FeatureIncompatSet::RECOVER.bits(),
        0
    );
}

fn nv_route_rnvpce_rejects_unrecovered_journal() {
    use super::{
        feature::FeatureIncompatSet,
        super_block::{RawSuperBlock, SUPER_BLOCK_OFFSET},
        test_utils::JOURNAL_START_BLOCK,
    };

    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    for case in 0..5 {
        let mut builder = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_uuid([0xb2 + case; 16]);
        if case < 3 {
            builder = builder.with_journal_inode(16);
        }
        let fixture = builder.build().unwrap();
        let disk = fixture.disk.clone();
        drop(fixture);
        let mut raw: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        match case {
            0 | 3 => raw.feature_incompat |= FeatureIncompatSet::RECOVER.bits(),
            1 | 4 => raw.last_orphan = 11,
            2 => {
                // JBD2 s_start at byte28: a dirty log must not be ignored even
                // when the filesystem RECOVER bit was not set.
                disk.segment()
                    .write_bytes(
                        JOURNAL_START_BLOCK as usize * BLOCK_SIZE + 28,
                        &1u32.to_be_bytes(),
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        disk.segment().write_val(SUPER_BLOCK_OFFSET, &raw).unwrap();
        let mut before = vec![0; disk.segment().size()];
        disk.segment().read_bytes(0, &mut before).unwrap();
        let flushes = disk.flush_count();
        let options = CString::new("rnvpce=on,rtio=on").unwrap();
        match Ext4::open(disk.clone(), Some(options.as_c_str())) {
            Err(error) => assert_eq!(error.error(), Errno::EUCLEAN),
            Ok(_) => panic!("rNVPCe accepted an unrecovered filesystem, case={case}"),
        }
        let mut after = vec![0; before.len()];
        disk.segment().read_bytes(0, &mut after).unwrap();
        assert_eq!(before, after, "failed mount must not mutate disk");
        assert_eq!(disk.flush_count(), flushes);
    }
}

fn nv_route_rnvpce_nojournal_export_retries_metadata_errors() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_free_blocks(128)
        .with_journal_inode(16)
        .with_uuid([0xbc; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    // Twelve disjoint extents force an external native leaf during export.
    for index in 0..12 {
        let bytes = [0x40 + index as u8; PAGE_SIZE];
        inode
            .write_at(
                2 * index * PAGE_SIZE,
                &mut VmReader::from(bytes.as_slice()).to_fallible(),
            )
            .unwrap();
    }
    let migration_flushes = fixture.disk.flush_count();
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 12);
    fixture.ext4.drain_nv_in_flight().unwrap();
    assert_eq!(
        fixture.disk.flush_count() - migration_flushes,
        1,
        "default rNVPCe migration must use one orderless batch barrier"
    );
    fixture.disk.set_fail_writes(true);
    assert_eq!(fixture.ext4.sync_all().unwrap_err().error(), Errno::EIO);
    fixture.disk.set_fail_writes(false);
    let free_after_failure = fixture.ext4.free_blocks_count();
    fixture.ext4.sync_all().unwrap();
    assert_eq!(
        fixture.ext4.free_blocks_count() + 1,
        free_after_failure,
        "failed new-node creation rolls back; retry needs exactly one leaf"
    );
    // Now modify an existing leaf. Failure at batch writeback must retain its
    // after-image and retry without allocating a replacement metadata block.
    for index in 12..24 {
        let bytes = [0x40 + index as u8; PAGE_SIZE];
        inode
            .write_at(
                2 * index * PAGE_SIZE,
                &mut VmReader::from(bytes.as_slice()).to_fallible(),
            )
            .unwrap();
    }
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 12);
    fixture.ext4.drain_nv_in_flight().unwrap();
    let free_after_data = fixture.ext4.free_blocks_count();
    fixture.disk.set_fail_writes(true);
    assert_eq!(fixture.ext4.sync_all().unwrap_err().error(), Errno::EIO);
    fixture.disk.set_fail_writes(false);
    fixture.ext4.sync_all().unwrap();
    assert_eq!(fixture.ext4.free_blocks_count(), free_after_data);
    assert_eq!(fixture.read_raw_inode(11).size_lo as usize, 47 * PAGE_SIZE);
    for index in 0..24 {
        let block = inode.data_block_of(2 * index).unwrap();
        let mut bytes = [0; PAGE_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(block as usize * BLOCK_SIZE, &mut bytes)
            .unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0x40 + index as u8));
    }
    assert!(inode.data_block_of(1).is_none());
    // A later inode-table failure must also leave export retryable after the
    // nodes themselves have already reached SSD.
    fixture.disk.set_fail_write_block(Some(4));
    assert_eq!(fixture.ext4.sync_all().unwrap_err().error(), Errno::EIO);
    fixture.disk.set_fail_write_block(None);
    fixture.ext4.sync_all().unwrap();
    assert_eq!(fixture.ext4.free_blocks_count(), free_after_data);
}

fn nv_route_rnvpce_direct_partial_write_and_deferred_export() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x91; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let flushes = fixture.disk.flush_count();
    let payload = [0x74u8; 37];
    let mut reader = VmReader::from(payload.as_slice()).to_fallible();
    assert_eq!(
        FileOps::write_at(
            inode.as_ref(),
            3,
            &mut reader,
            StatusFlags::O_DIRECT | StatusFlags::O_SYNC
        )
        .unwrap(),
        payload.len()
    );
    assert_eq!(
        fixture.disk.flush_count(),
        flushes,
        "per-file sync must not export or flush SSD"
    );
    let mut output = [0u8; 40];
    let mut writer = VmWriter::from(output.as_mut_slice()).to_fallible();
    assert_eq!(
        FileOps::read_at(inode.as_ref(), 0, &mut writer, StatusFlags::O_DIRECT).unwrap(),
        40
    );
    assert_eq!(&output[..3], &[0; 3]);
    assert_eq!(&output[3..], &payload);
    assert!(VfsInode::page_cache(inode.as_ref()).is_none());
    assert_eq!(inode.resize(0).unwrap_err().error(), Errno::EOPNOTSUPP);
    fixture.ext4.sync_all().unwrap();
    let mut after = [0u8; 40];
    inode
        .read_at(0, &mut VmWriter::from(after.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(after, output);
    let pool = crate::fs::rnvpc::pool().unwrap();
    assert!(!has_native_redirect(
        pool,
        fixture.ext4.rnvpc_fs_id(),
        11,
        0
    ));
    assert!(
        crate::fs::rnvpc::write_transaction::current_page_mapping(
            pool,
            fixture.ext4.rnvpc_fs_id(),
            11,
            0
        )
        .unwrap()
        .unwrap()
        .is_written()
    );
}

fn nv_route_rnvpc_wal_and_native_paths_remain_distinct() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x92; 16])
        .with_mount_options("rnvpc=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let payload = [0x39u8; 73];
    assert_eq!(
        inode
            .write_at(7, &mut VmReader::from(payload.as_slice()).to_fallible())
            .unwrap(),
        73
    );
    assert_eq!(fixture.read_raw_inode(11).size_lo, 80);
    let mut output = [0u8; 80];
    assert_eq!(
        inode
            .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
            .unwrap(),
        80
    );
    assert_eq!(&output[..7], &[0; 7]);
    assert_eq!(&output[7..], &payload);
    let context = crate::fs::rnvpc::cow_context(fixture.ext4.rnvpc_fs_id(), 11).unwrap();
    assert_eq!(context.foreground_checkpoint_counters().wal_appends, 1);
    assert_eq!(
        context.flush_candidates(128).len(),
        0,
        "uncheckpointed WAL pages cannot migrate"
    );
    assert_eq!(inode.resize(0).unwrap_err().error(), Errno::EOPNOTSUPP);
    let native = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x93; 16])
        .with_mount_options("rnvpc=off,rnvpc_extend=off")
        .build()
        .unwrap();
    native.write_raw_inode(11, &make_empty_file_inode());
    let inode = native.ext4.read_inode(11).unwrap();
    assert_eq!(
        inode
            .write_at(0, &mut VmReader::from(payload.as_slice()).to_fallible())
            .unwrap(),
        73
    );
    assert!(VfsInode::page_cache(inode.as_ref()).is_some());
    assert!(crate::fs::rnvpc::existing_cow_context(native.ext4.rnvpc_fs_id(), 11).is_none());
    inode.resize(0).unwrap();
}

#[ktest]
fn rnvpc_capacity_pressure_waits_for_rtio_reclaim() {
    assert!(super::inode::is_nv_capacity_pressure(&Error::new(
        Errno::EAGAIN
    )));
    assert!(!super::inode::is_nv_capacity_pressure(&Error::new(
        Errno::EIO
    )));
}

fn nv_route_busy_lease_read_and_partial_write_retry() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x94; 16])
        .with_mount_options("rnvpc=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let original = [0x31u8; PAGE_SIZE];
    inode
        .write_at(0, &mut VmReader::from(original.as_slice()).to_fallible())
        .unwrap();
    let context = crate::fs::rnvpc::cow_context(fixture.ext4.rnvpc_fs_id(), 11).unwrap();
    let pool = crate::fs::rnvpc::pool().unwrap();
    for write in [false, true] {
        // Direct lease admission injects the transient condition deterministically;
        // production rTIO additionally requires a checkpointed WAL mapping.
        let held = pool
            .pin_for_writeback(context.resident_page(0).unwrap())
            .unwrap();
        let state = Arc::new(AtomicUsize::new(0));
        let worker_state = state.clone();
        let worker_inode = inode.clone();
        let worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
            worker_state.store(1, Ordering::Release);
            if write {
                let replacement = [0x72u8; 19];
                assert_eq!(
                    worker_inode
                        .write_at(
                            17,
                            &mut VmReader::from(replacement.as_slice()).to_fallible()
                        )
                        .unwrap(),
                    19
                );
            } else {
                let mut output = [0u8; PAGE_SIZE];
                assert_eq!(
                    worker_inode
                        .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
                        .unwrap(),
                    PAGE_SIZE
                );
                assert_eq!(output, [0x31u8; PAGE_SIZE]);
            }
            worker_state.store(2, Ordering::Release);
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
            "NV access must wait for the exact busy lease"
        );
        drop(held);
        worker.join();
        assert_eq!(state.load(Ordering::Acquire), 2);
    }
    let mut output = [0u8; PAGE_SIZE];
    inode
        .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(&output[..17], &[0x31; 17]);
    assert_eq!(&output[17..36], &[0x72; 19]);
    assert!(output[36..].iter().all(|byte| *byte == 0x31));
    assert_eq!(
        context.foreground_checkpoint_counters().wal_appends,
        2,
        "retry must not duplicate the user prefix or WAL append"
    );
}

fn nv_route_rnvpc_failed_allocation_flush_does_not_append_wal() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x95; 16])
        .with_mount_options("rnvpc=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let context = crate::fs::rnvpc::cow_context(fixture.ext4.rnvpc_fs_id(), 11).unwrap();
    let payload = [0x43u8; 31];
    fixture.disk.set_fail_flushes(true);
    let result = inode.write_at(0, &mut VmReader::from(payload.as_slice()).to_fallible());
    fixture.disk.set_fail_flushes(false);
    assert_eq!(result.unwrap_err().error(), Errno::EIO);
    assert_eq!(context.foreground_checkpoint_counters().wal_appends, 0);
    assert!(context.resident_page(0).is_none());
    // Retry must make the allocation durable even when it already exists in RAM.
    assert_eq!(
        inode
            .write_at(0, &mut VmReader::from(payload.as_slice()).to_fallible())
            .unwrap(),
        31
    );
    assert_eq!(context.foreground_checkpoint_counters().wal_appends, 1);
    assert_eq!(fixture.read_raw_inode(11).size_lo, 31);
}

fn nv_route_mount_rejects_switching_persistent_authority() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    for (uuid, mode, opposite) in [
        ([0x96; 16], "rnvpc=on,rtio=off", "rnvpce=on,rtio=off"),
        ([0x97; 16], "rnvpce=on,rtio=off", "rnvpc=on,rtio=off"),
    ] {
        let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_uuid(uuid)
            .with_mount_options(mode)
            .build()
            .unwrap();
        fixture.write_raw_inode(11, &make_empty_file_inode());
        let inode = fixture.ext4.read_inode(11).unwrap();
        inode
            .write_at(0, &mut VmReader::from([0x53u8; 8].as_slice()).to_fallible())
            .unwrap();
        for rejected in [opposite, "rnvpc=off,rnvpc_extend=off"] {
            let result = Ext4FixtureBuilder::new(2048, 256, 2048)
                .with_block_bitmap_metadata_marked()
                .with_uuid(uuid)
                .with_mount_options(rejected)
                .build();
            match result {
                Err(error) => assert_eq!(error.error(), Errno::EINVAL),
                Ok(_) => panic!("mount accepted incompatible persistent authority"),
            }
        }
    }
}

// Reproduces the 64MiB fio EAGAIN without a large filesystem: private staged
// pages consume the shared data budget, leaving one committed page to migrate.
fn nv_route_rnvpce_capacity_wait_releases_inode_lock() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xa1; 16])
        .with_mount_options("rnvpce=on,rtio=on")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let migration = fixture.ext4.nv_migration_gate.enter_exclusive();
    let payload = [0x61u8; PAGE_SIZE];
    inode
        .write_at(0, &mut VmReader::from(payload.as_slice()).to_fallible())
        .unwrap();
    let pool = crate::fs::rnvpc::pool().unwrap();
    let mut staged = Vec::new();
    loop {
        match pool.alloc_data_page() {
            Ok(page) => staged.push(page),
            Err(error) => {
                assert_eq!(error.error(), Errno::EAGAIN);
                break;
            }
        }
    }
    let state = Arc::new(AtomicUsize::new(0));
    let worker_state = state.clone();
    let worker_inode = inode.clone();
    let worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
        worker_state.store(1, Ordering::Release);
        let bytes = [0x72u8; PAGE_SIZE];
        let mut reader = VmReader::from(bytes.as_slice()).to_fallible();
        match worker_inode.write_at(PAGE_SIZE, &mut reader) {
            Ok(count) => {
                assert_eq!(count, PAGE_SIZE);
                assert_eq!(reader.remain(), 0);
                worker_state.store(2, Ordering::Release);
            }
            Err(_) => worker_state.store(3, Ordering::Release),
        }
    })
    .spawn();
    while state.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..16 {
        crate::thread::Thread::yield_now();
    }
    assert_eq!(
        state.load(Ordering::Acquire),
        1,
        "capacity pressure must block rather than escape as EAGAIN"
    );
    // A blocked writer must not retain inner: completion/export may need it.
    assert_eq!(VfsInode::size(inode.as_ref()), PAGE_SIZE);
    drop(migration);
    worker.join();
    assert_eq!(state.load(Ordering::Acquire), 2);
    for page in staged {
        pool.free_data_page(page).unwrap();
    }
    fixture.ext4.sync_all().unwrap();
    let mut bytes = vec![0u8; 2 * PAGE_SIZE];
    assert_eq!(
        inode
            .read_at(0, &mut VmWriter::from(bytes.as_mut_slice()).to_fallible())
            .unwrap(),
        bytes.len()
    );
    assert!(bytes[..PAGE_SIZE].iter().all(|byte| *byte == 0x61));
    assert!(bytes[PAGE_SIZE..].iter().all(|byte| *byte == 0x72));
    fixture
        .ext4
        .shutdown(super::fs::GoingDown::Default)
        .unwrap();
}

fn nv_route_rnvpce_batch_allocation_durability() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xa2; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let bytes = vec![0x59u8; 32 * PAGE_SIZE];
    // One contiguous first-write range must require one foreground allocator
    // call. Leave the injected second-call failure armed through migration to
    // prove rTIO does not allocate the same targets again.
    fixture.ext4.arm_alloc_blocks_enospc(1);
    inode
        .write_at(0, &mut VmReader::from(bytes.as_slice()).to_fallible())
        .unwrap();
    let before = fixture.disk.flush_count();
    let migrated = fixture.ext4.migrate_nv_round(64);
    fixture.ext4.arm_alloc_blocks_enospc(-1);
    assert_eq!(migrated.unwrap(), 32);
    assert_eq!(fixture.disk.flush_count() - before, 0);
    fixture.ext4.drain_nv_in_flight().unwrap();
    assert_eq!(
        fixture.disk.flush_count() - before,
        1,
        "all data and metadata must share the completion durability barrier"
    );
    fixture.ext4.sync_all().unwrap();
    let mut output = vec![0; bytes.len()];
    inode
        .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(output, bytes);
    assert_eq!(fixture.read_raw_inode(11).size_lo as usize, bytes.len());
    // Verify the exported native mappings against SSD bytes, not only the
    // still-live NV authority used by inode.read_at above.
    for index in 0..32 {
        let block = inode.data_block_of(index).unwrap();
        let mut page = [0u8; PAGE_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(block as usize * PAGE_SIZE, &mut page)
            .unwrap();
        assert!(page.iter().all(|byte| *byte == 0x59));
    }
    let failed = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xa3; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    failed.write_raw_inode(11, &make_empty_file_inode());
    let failed_inode = failed.ext4.read_inode(11).unwrap();
    failed_inode
        .write_at(0, &mut VmReader::from(bytes.as_slice()).to_fallible())
        .unwrap();
    let free_before = failed.ext4.free_blocks_count();
    assert_eq!(failed.ext4.migrate_nv_round(64).unwrap(), 32);
    failed.disk.set_fail_flushes(true);
    assert_eq!(
        failed.ext4.drain_nv_in_flight().unwrap_err().error(),
        Errno::EIO
    );
    failed.disk.set_fail_flushes(false);
    assert_eq!(failed.ext4.free_blocks_count(), free_before);
    let pool = crate::fs::rnvpc::pool().unwrap();
    let mut protected_targets = Vec::new();
    for index in 0..32 {
        assert!(has_native_redirect(
            pool,
            failed.ext4.rnvpc_fs_id(),
            11,
            index,
        ));
        let target = crate::fs::rnvpc::write_transaction::current_page_mapping(
            pool,
            failed.ext4.rnvpc_fs_id(),
            11,
            index,
        )
        .unwrap()
        .unwrap()
        .ssd_block();
        assert_ne!(target, 0, "failed flush must retain the foreground target");
        protected_targets.push(target);
        assert_eq!(
            has_native_redirect(pool, failed.ext4.rnvpc_fs_id(), 11, index),
            true,
            "failed completion flush must retain NV authority"
        );
        assert_eq!(
            crate::fs::rnvpc::write_transaction::current_page_mapping(
                pool,
                failed.ext4.rnvpc_fs_id(),
                11,
                index,
            )
            .unwrap()
            .unwrap()
            .ssd_block(),
            target,
            "failed device flush keeps the prepublished baseline behind NV authority"
        );
    }
    assert_eq!(failed.ext4.migrate_nv_round(64).unwrap(), 32);
    failed.ext4.drain_nv_in_flight().unwrap();
    assert_eq!(failed.ext4.free_blocks_count(), free_before);
    for (index, target) in protected_targets.into_iter().enumerate() {
        let mapping = crate::fs::rnvpc::write_transaction::current_page_mapping(
            pool,
            failed.ext4.rnvpc_fs_id(),
            11,
            index as u64,
        )
        .unwrap()
        .unwrap();
        assert!(!has_native_redirect(
            pool,
            failed.ext4.rnvpc_fs_id(),
            11,
            index as u64
        ));
        assert_eq!(mapping.ssd_block(), target);
    }
}

fn nv_route_rnvpce_recovers_unflushed_target_bitmap() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xa8; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    inode
        .write_at(
            0,
            &mut VmReader::from([0x91; PAGE_SIZE].as_slice()).to_fallible(),
        )
        .unwrap();
    let bitmap_block = fixture.ext4.block_group(0).block_bitmap_bid_for_test();
    assert_eq!(
        fixture
            .ext4
            .block_group(0)
            .validate_recovered_block(bitmap_block)
            .unwrap_err()
            .error(),
        Errno::EUCLEAN,
        "recovery must reject a persistent target in EXT4 metadata"
    );
    fixture
        .disk
        .set_fail_write_block(Some(bitmap_block as usize));
    assert_eq!(fixture.ext4.migrate_nv_round(1).unwrap(), 1);
    fixture.ext4.drain_nv_in_flight().unwrap();
    fixture.disk.set_fail_write_block(None);
    let pool = crate::fs::rnvpc::pool().unwrap();
    let target = crate::fs::rnvpc::write_transaction::current_page_mapping(
        pool,
        fixture.ext4.rnvpc_fs_id(),
        11,
        0,
    )
    .unwrap()
    .unwrap()
    .ssd_block();
    assert_ne!(target, 0);
    let bit = (target - fixture.ext4.block_group(0).first_block()) as u16;
    let mut raw_bitmap = [0u8; BLOCK_SIZE];
    fixture
        .disk
        .segment()
        .read_bytes(bitmap_block as usize * BLOCK_SIZE, &mut raw_bitmap)
        .unwrap();
    assert_eq!(raw_bitmap[bit as usize / 8] & (1 << (bit % 8)), 0);

    drop(inode);
    let disk = fixture.disk.clone();
    core::mem::forget(fixture);
    let before = disk.flush_count();
    let options = CString::new("rnvpce=on,rtio=off").unwrap();
    let recovered = Ext4::open(disk.clone(), Some(options.as_c_str())).unwrap();
    assert_eq!(disk.flush_count() - before, 0);
    assert!(
        recovered
            .block_group(0)
            .metadata()
            .block_bitmap
            .is_allocated(bit)
    );
    let group_free = u32::from(
        recovered
            .block_group(0)
            .metadata()
            .block_bitmap
            .free_count(),
    );
    assert_eq!(recovered.block_group(0).free_blocks_count(), group_free);
    assert_eq!(
        recovered.super_block().free_blocks_count(),
        u64::from(group_free)
    );
    disk.segment()
        .read_bytes(bitmap_block as usize * BLOCK_SIZE, &mut raw_bitmap)
        .unwrap();
    assert_eq!(raw_bitmap[bit as usize / 8] & (1 << (bit % 8)), 0);
}

fn nv_route_rnvpce_reservation_abort_wakes_writer() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0xa4; 16])
        .with_mount_options("rnvpce=on,rtio=on")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let migration = fixture.ext4.nv_migration_gate.enter_exclusive();
    let pool = crate::fs::rnvpc::pool().unwrap();
    let mut staged = Vec::new();
    while let Ok(page) = pool.alloc_data_page() {
        staged.push(page);
    }
    let state = Arc::new(AtomicUsize::new(0));
    let worker_state = state.clone();
    let worker_inode = inode.clone();
    let worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
        let bytes = [0x38u8; PAGE_SIZE];
        let result = worker_inode.write_at(0, &mut VmReader::from(bytes.as_slice()).to_fallible());
        worker_state.store(if result.is_ok() { 1 } else { 2 }, Ordering::Release);
    })
    .spawn();
    while fixture.ext4.nv_capacity_waiters.load(Ordering::Acquire) == 0 {
        crate::thread::Thread::yield_now();
    }
    for _ in 0..32 {
        crate::thread::Thread::yield_now();
    }
    // No data in this filesystem is migratable: only reservation rollback
    // can release the capacity and wake the writer.
    for page in staged {
        pool.free_data_page(page).unwrap();
    }
    drop(migration);
    for _ in 0..128 {
        crate::thread::kernel_thread::ThreadOptions::new(|| {})
            .spawn()
            .join();
    }
    assert_eq!(
        state.load(Ordering::Acquire),
        1,
        "aborted reservations must wake capacity waiters"
    );
    worker.join();
    fixture
        .ext4
        .shutdown(super::fs::GoingDown::Default)
        .unwrap();
}

fn nv_route_rnvpce_fragmented_batch_and_partial_rollback() {
    use super::{fs::AllocIntent, journal::BlockFreeAuth};
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_free_blocks(128)
        .with_uuid([0xc4; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    let held = fixture
        .ext4
        .alloc_blocks_with_intent(128, 0, None, AllocIntent::RnvpceForeground)
        .unwrap();
    assert_eq!(held.end - held.start, 128);
    for block in held.clone().step_by(2) {
        fixture
            .ext4
            .free_blocks(BlockFreeAuth::without_revoke_duty(block, 1), None)
            .unwrap();
    }
    assert_eq!(fixture.ext4.free_blocks_count(), 64);
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let mut bytes = vec![0; 8 * PAGE_SIZE];
    for (index, page) in bytes.chunks_mut(PAGE_SIZE).enumerate() {
        page.fill(0x30 + index as u8);
    }
    // No run can exceed one block. Fail after the first partial return and
    // require foreground rollback of every private block before retrying.
    fixture.ext4.arm_alloc_blocks_enospc(1);
    assert_eq!(
        inode
            .write_at(0, &mut VmReader::from(bytes.as_slice()).to_fallible())
            .unwrap_err()
            .error(),
        Errno::ENOSPC
    );
    fixture.ext4.arm_alloc_blocks_enospc(-1);
    assert_eq!(fixture.ext4.free_blocks_count(), 64);
    inode
        .write_at(0, &mut VmReader::from(bytes.as_slice()).to_fallible())
        .unwrap();
    assert_eq!(fixture.ext4.free_blocks_count(), 56);
    let pool = crate::fs::rnvpc::pool().unwrap();
    for logical in 0..8 {
        assert!(has_native_redirect(
            pool,
            fixture.ext4.rnvpc_fs_id(),
            11,
            logical,
        ));
        assert!(
            crate::fs::rnvpc::write_transaction::current_page_mapping(
                pool,
                fixture.ext4.rnvpc_fs_id(),
                11,
                logical,
            )
            .unwrap()
            .is_some()
        );
    }
    assert_eq!(fixture.ext4.migrate_nv_round(64).unwrap(), 8);
    fixture.ext4.drain_nv_in_flight().unwrap();
    for logical in 0..8 {
        assert!(
            !has_native_redirect(pool, fixture.ext4.rnvpc_fs_id(), 11, logical as u64),
            "fragmented per-page completion must reclaim NV authority"
        );
        let mapping = crate::fs::rnvpc::write_transaction::current_page_mapping(
            pool,
            fixture.ext4.rnvpc_fs_id(),
            11,
            logical as u64,
        )
        .unwrap()
        .unwrap();
        assert_ne!(mapping.ssd_block(), 0);
    }
}
