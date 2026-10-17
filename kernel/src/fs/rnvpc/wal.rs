// SPDX-License-Identifier: MPL-2.0

//! Persistent per-inode WAL with foreground checkpoint publication.
//!
//! `PersistentInodeWal` is the production rNVPC path. The legacy DRAM cost
//! model types below remain only for compatibility with historical tests;
//! neither SSD completion nor rTIO retires the persistent WAL.

use ostd::sync::WaitQueue;
use spin::Mutex;

use super::{
    frame::Rnvpc,
    id::NvPageId,
    pcache_layout::{
        AnchorImage, InodePcAnchor, PcObjectType, PersistentPageRef, PersistentWalRecord,
        WAL_FRAME_COUNT, WAL_RECORDS_PER_FRAME, WalFrameImage, anchor_image_range,
        anchor_selector_range, wal_record_commit_range, wal_record_range,
    },
    pcache_store::{
        PcFaultPoint, PersistentMetadataStore, typed_object_index, typed_object_local_index,
        typed_object_type,
    },
    persistent_btree::RootUpdate,
};
use crate::prelude::*;

/// 每个 inode 的 WAL 额度（op-log 条数）。见 `porting.md §3.1`。
pub(crate) const WAL_CAPACITY_PER_INODE: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalUpsert {
    pub(crate) page_index: u64,
    pub(crate) old_page: PersistentPageRef,
    pub(crate) new_page: PersistentPageRef,
    pub(crate) ssd_backing_block: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalAppendOutcome {
    pub(crate) lsn: u64,
    pub(crate) record: PersistentWalRecord,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentWalCounters {
    pub(crate) wal_appends: u64,
    pub(crate) persist_fences: u64,
    pub(crate) wal_max_used: u64,
}

#[derive(Clone, Copy)]
struct WalRuntimeState {
    head: usize,
    tail: usize,
    used: usize,
    next_lsn: u64,
    checkpointed_lsn: u64,
    anchor_image: AnchorImage,
    poisoned: bool,
    counters: PersistentWalCounters,
}

pub(crate) struct PersistentInodeWal {
    fs_id: u64,
    inode: u64,
    store: PersistentMetadataStore,
    anchor: PersistentPageRef,
    wal_frames: [PersistentPageRef; WAL_FRAME_COUNT],
    state: ostd::sync::Mutex<WalRuntimeState>,
}

impl PersistentInodeWal {
    pub(crate) fn create(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        root: PersistentPageRef,
        tree_height: u16,
    ) -> Result<Arc<Self>> {
        let store = PersistentMetadataStore::new(pool);
        let mut wal_frames = [PersistentPageRef::NONE; WAL_FRAME_COUNT];
        for (index, frame) in wal_frames.iter_mut().enumerate() {
            let result = store.allocate(
                fs_id,
                inode,
                PcObjectType::WalFrame,
                index as u64,
                &WalFrameImage::default(),
                None,
            );
            match result {
                Ok(reference) => *frame = reference,
                Err(error) => {
                    cleanup_wal_frames(&store, &wal_frames);
                    return Err(error);
                }
            }
        }
        let mut anchor_image = InodePcAnchor::empty(fs_id, inode, root);
        anchor_image.images[0].height = tree_height;
        let anchor =
            match store.allocate(fs_id, inode, PcObjectType::Anchor, 0, &anchor_image, None) {
                Ok(anchor) => anchor,
                Err(error) => {
                    // Anchor commit may have reached persistence before the
                    // backend reported failure. Retain its complete reachable
                    // graph for recovery rather than clear WAL descriptors.
                    return Err(error);
                }
            };
        Ok(Arc::new(Self {
            fs_id,
            inode,
            store,
            anchor,
            wal_frames,
            state: ostd::sync::Mutex::new(WalRuntimeState {
                head: 0,
                tail: 0,
                used: 0,
                next_lsn: 1,
                checkpointed_lsn: 0,
                anchor_image: anchor_image.images[0],
                poisoned: false,
                counters: PersistentWalCounters::default(),
            }),
        }))
    }

    pub(crate) fn recover(
        pool: Arc<Rnvpc>,
        anchor: PersistentPageRef,
        anchor_image: InodePcAnchor,
    ) -> Result<Arc<Self>> {
        let store = PersistentMetadataStore::new(pool.clone());
        let recovered_anchor = store.read::<InodePcAnchor>(anchor)?;
        if recovered_anchor != anchor_image {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent anchor identity changed during recovery"
            );
        }
        let selected = recovered_anchor.images[0];
        if selected.next_lsn == 0
            || selected.checkpointed_lsn >= selected.next_lsn
            || selected
                .next_lsn
                .checked_sub(selected.wal_used as u64)
                .is_none()
        {
            return_errno_with_message!(Errno::EIO, "invalid persistent WAL LSN state");
        }
        let metadata = pool.committed_metadata_of(recovered_anchor.fs_id, recovered_anchor.inode);
        let mut wal_frames = [PersistentPageRef::NONE; WAL_FRAME_COUNT];
        let mut found = [0usize; WAL_FRAME_COUNT];
        for (logical_index, _) in metadata {
            if typed_object_type(logical_index) != Some(PcObjectType::WalFrame) {
                continue;
            }
            let local_index = typed_object_local_index(logical_index) as usize;
            if local_index >= WAL_FRAME_COUNT {
                return_errno_with_message!(Errno::EIO, "unexpected persistent WAL frame index");
            }
            found[local_index] += 1;
        }
        for index in 0..WAL_FRAME_COUNT {
            if found[index] != 1 {
                return_errno_with_message!(Errno::EIO, "missing or duplicate persistent WAL frame");
            }
            let key = super::index::PageKey::new(
                recovered_anchor.fs_id,
                recovered_anchor.inode,
                typed_object_index(PcObjectType::WalFrame, index as u64)?,
            );
            let id = pool
                .committed_metadata_slot(key)
                .ok_or_else(|| Error::new(Errno::EIO))?;
            let generation = pool.frame_seq(id).ok_or_else(|| Error::new(Errno::EIO))?;
            let frame = PersistentPageRef::new(id.index() as u32, generation);
            store.validate_ref(frame)?;
            wal_frames[index] = frame;
        }
        Ok(Arc::new(Self {
            fs_id: recovered_anchor.fs_id,
            inode: recovered_anchor.inode,
            store,
            anchor,
            wal_frames,
            state: ostd::sync::Mutex::new(WalRuntimeState {
                head: selected.wal_head as usize,
                tail: selected.wal_tail as usize,
                used: selected.wal_used as usize,
                next_lsn: selected.next_lsn,
                checkpointed_lsn: selected.checkpointed_lsn,
                anchor_image: selected,
                poisoned: false,
                counters: PersistentWalCounters::default(),
            }),
        }))
    }

    pub(crate) fn append(
        &self,
        upsert: WalUpsert,
        fault: Option<PcFaultPoint>,
    ) -> Result<WalAppendOutcome> {
        let mut state = self.state.lock();
        if state.poisoned {
            return_errno_with_message!(Errno::EIO, "persistent WAL requires recovery");
        }
        if state.used == WAL_CAPACITY_PER_INODE {
            return_errno_with_message!(Errno::ENOSPC, "persistent inode WAL is full");
        }
        let next_lsn = state
            .next_lsn
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let record = PersistentWalRecord::upsert(
            state.next_lsn,
            upsert.page_index,
            upsert.old_page,
            upsert.new_page,
            upsert.ssd_backing_block,
        );
        self.persist_record(state.tail, record, fault)?;

        let mut next = state.anchor_image;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        next.wal_tail = ((state.tail + 1) % WAL_CAPACITY_PER_INODE) as u16;
        next.wal_used = (state.used + 1) as u16;
        next.next_lsn = next_lsn;
        let (result, ambiguous) = self.publish_anchor(next, fault);
        if let Err(error) = result {
            state.poisoned |= ambiguous;
            return Err(error);
        }
        state.tail = next.wal_tail as usize;
        state.used += 1;
        state.next_lsn = next_lsn;
        state.anchor_image = next;
        state.counters.wal_appends += 1;
        state.counters.persist_fences += 4;
        state.counters.wal_max_used = state.counters.wal_max_used.max(state.used as u64);
        Ok(WalAppendOutcome {
            lsn: record.lsn,
            record,
        })
    }

    pub(crate) fn records_after(&self, checkpointed_lsn: u64) -> Result<Vec<PersistentWalRecord>> {
        let state = *self.state.lock();
        if state.poisoned {
            return_errno_with_message!(Errno::EIO, "persistent WAL requires recovery");
        }
        let mut expected_lsn = state
            .next_lsn
            .checked_sub(state.used as u64)
            .ok_or_else(|| Error::new(Errno::EIO))?;
        if state.used != 0 && expected_lsn <= state.checkpointed_lsn {
            return_errno_with_message!(Errno::EIO, "invalid persistent WAL checkpoint boundary");
        }
        let mut records = Vec::new();
        for offset in 0..state.used {
            let slot = (state.head + offset) % WAL_CAPACITY_PER_INODE;
            let frame = self
                .store
                .read::<WalFrameImage>(self.wal_frames[slot / WAL_RECORDS_PER_FRAME])?;
            let record = frame.records[slot % WAL_RECORDS_PER_FRAME];
            if record == PersistentWalRecord::default() || record.lsn != expected_lsn {
                return_errno_with_message!(
                    Errno::EIO,
                    "missing or corrupt anchor-visible WAL record"
                );
            }
            expected_lsn = expected_lsn
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EIO))?;
            if record.lsn > checkpointed_lsn {
                records.push(record);
            }
        }
        Ok(records)
    }

    pub(crate) fn publish_checkpoint(
        &self,
        checkpointed_lsn: u64,
        fault: Option<PcFaultPoint>,
    ) -> Result<()> {
        let mut state = self.state.lock();
        inject_wal_fault(fault, PcFaultPoint::BeforeCheckpointAnchorAdvance)?;
        if state.poisoned {
            return_errno_with_message!(Errno::EIO, "persistent WAL requires recovery");
        }
        if checkpointed_lsn
            != state
                .next_lsn
                .checked_sub(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent WAL checkpoint LSN");
        }
        let mut next = state.anchor_image;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        next.checkpointed_lsn = checkpointed_lsn;
        next.wal_head = state.tail as u16;
        next.wal_used = 0;
        let (result, ambiguous) = self.publish_anchor(next, fault);
        if let Err(error) = result {
            state.poisoned |= ambiguous;
            return Err(error);
        }
        state.head = state.tail;
        state.used = 0;
        state.checkpointed_lsn = checkpointed_lsn;
        state.counters.persist_fences += 2;
        state.anchor_image = next;
        Ok(())
    }

    pub(crate) fn publish_root(
        &self,
        update: RootUpdate,
        fault: Option<PcFaultPoint>,
    ) -> Result<()> {
        let mut state = self.state.lock();
        if state.poisoned {
            return_errno_with_message!(Errno::EIO, "persistent WAL requires recovery");
        }
        let mut next = state.anchor_image;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        next.root = update.root;
        next.height = update.height;
        let mut selector_may_be_visible = false;
        let result = self.store.raw_update(self.anchor, |metadata| {
            let mut current = [0; PAGE_SIZE];
            metadata.read(&mut current)?;
            let active = InodePcAnchor::active_slot_from_page(&current)?;
            let inactive = 1 - active;
            let desired = InodePcAnchor {
                fs_id: self.fs_id,
                inode: self.inode,
                images: [
                    if inactive == 0 {
                        next
                    } else {
                        AnchorImage::default()
                    },
                    if inactive == 1 {
                        next
                    } else {
                        AnchorImage::default()
                    },
                ],
                active_slot: inactive as u64,
            };
            let mut encoded = [0; PAGE_SIZE];
            desired.encode_page(&mut encoded)?;
            let image_range = anchor_image_range(inactive);
            current[image_range.clone()].copy_from_slice(&encoded[image_range.clone()]);
            metadata.write_page(&current)?;
            metadata.persist_range(image_range.start, image_range.len())?;
            metadata.fence()?;
            inject_wal_fault(fault, PcFaultPoint::AfterRootAnchorImagePersist)?;
            let selector_range = anchor_selector_range();
            current[selector_range.clone()].copy_from_slice(&encoded[selector_range.clone()]);
            metadata.write_page(&current)?;
            selector_may_be_visible = true;
            metadata.persist_range(selector_range.start, selector_range.len())?;
            metadata.fence()?;
            inject_wal_fault(fault, PcFaultPoint::AfterRootAnchorPersist)
        });
        if let Err(error) = result {
            state.poisoned |= selector_may_be_visible;
            return Err(error);
        }
        state.counters.persist_fences += 2;
        state.anchor_image = next;
        Ok(())
    }

    pub(crate) fn anchor_ref(&self) -> PersistentPageRef {
        self.anchor
    }

    pub(crate) fn pending(&self) -> usize {
        self.state.lock().used
    }

    pub(crate) fn counters(&self) -> PersistentWalCounters {
        self.state.lock().counters
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        self.state.lock().poisoned
    }

    pub(crate) fn checkpointed_lsn(&self) -> u64 {
        self.state.lock().checkpointed_lsn
    }

    pub(crate) fn state_snapshot(&self) -> (usize, usize, usize, u64, u64) {
        let state = *self.state.lock();
        (
            state.head,
            state.tail,
            state.used,
            state.next_lsn,
            state.checkpointed_lsn,
        )
    }

    pub(crate) fn is_full(&self) -> bool {
        self.pending() == WAL_CAPACITY_PER_INODE
    }

    fn persist_record(
        &self,
        slot: usize,
        record: PersistentWalRecord,
        fault: Option<PcFaultPoint>,
    ) -> Result<()> {
        let frame_index = slot / WAL_RECORDS_PER_FRAME;
        let record_index = slot % WAL_RECORDS_PER_FRAME;
        let record_range = wal_record_range(record_index);
        let commit_range = wal_record_commit_range(record_index);
        let commit_offset = commit_range.start - record_range.start;
        let mut final_bytes = [0; super::pcache_layout::WAL_RECORD_BYTES];
        record.encode(&mut final_bytes);
        self.store
            .raw_update(self.wal_frames[frame_index], |update| {
                let mut page = [0; PAGE_SIZE];
                update.read(&mut page)?;
                page[record_range.clone()].copy_from_slice(&final_bytes);
                page[commit_range.clone()].fill(0);
                update.write_page(&page)?;
                update.persist_range(record_range.start, record_range.len())?;
                update.fence()?;
                inject_wal_fault(fault, PcFaultPoint::AfterWalRecordPersist)?;

                page[commit_range.clone()].copy_from_slice(&final_bytes[commit_offset..]);
                update.write_page(&page)?;
                update.persist_range(commit_range.start, commit_range.len())?;
                update.fence()?;
                inject_wal_fault(fault, PcFaultPoint::AfterWalCommitPersist)
            })
    }

    fn publish_anchor(
        &self,
        image: AnchorImage,
        fault: Option<PcFaultPoint>,
    ) -> (Result<()>, bool) {
        let mut selector_may_be_visible = false;
        let result = self.store.raw_update(self.anchor, |update| {
            let mut current = [0; PAGE_SIZE];
            update.read(&mut current)?;
            let active = InodePcAnchor::active_slot_from_page(&current)?;
            let inactive = 1 - active;
            let mut desired = InodePcAnchor {
                fs_id: self.fs_id,
                inode: self.inode,
                images: [AnchorImage::default(), AnchorImage::default()],
                active_slot: inactive as u64,
            };
            desired.images[inactive] = image;
            let mut encoded = [0; PAGE_SIZE];
            desired.encode_page(&mut encoded)?;
            let image_range = anchor_image_range(inactive);
            current[image_range.clone()].copy_from_slice(&encoded[image_range.clone()]);
            update.write_page(&current)?;
            update.persist_range(image_range.start, image_range.len())?;
            update.fence()?;
            inject_wal_fault(fault, PcFaultPoint::AfterWalAnchorImagePersist)?;

            let selector_range = anchor_selector_range();
            current[selector_range.clone()].copy_from_slice(&encoded[selector_range.clone()]);
            update.write_page(&current)?;
            selector_may_be_visible = true;
            update.persist_range(selector_range.start, selector_range.len())?;
            update.fence()?;
            inject_wal_fault(fault, PcFaultPoint::AfterWalAnchorSelectorPersist)
        });
        (result, selector_may_be_visible)
    }
}

fn cleanup_wal_frames(store: &PersistentMetadataStore, wal_frames: &[PersistentPageRef]) {
    for frame in wal_frames.iter().rev().copied() {
        if frame == PersistentPageRef::NONE {
            continue;
        }
        if store.remove(frame).is_err() {
            warn!("RNVPC retained an ambiguous WAL frame during failed creation");
        }
    }
}

fn inject_wal_fault(selected: Option<PcFaultPoint>, current: PcFaultPoint) -> Result<()> {
    if selected == Some(current) {
        return_errno_with_message!(Errno::EIO, "injected persistent WAL fault");
    }
    Ok(())
}

/// 是否采集纳秒级耗时。
///
/// **默认关闭**，与 `ext4/fs.rs` 的 `LATENCY_ENABLED` 同一约定：ktest 下 TSC 未标定
/// （`kernel/comps/time/src/tsc.rs` 直接 panic），所以计时必须编译期关掉；开
/// 启时需保证单调时钟已标定（真实 VM 路径）。计数类指标不受影响。
const WAL_LATENCY_ENABLED: bool = false;

#[inline]
pub(crate) fn now_ns() -> u64 {
    if WAL_LATENCY_ENABLED {
        aster_time::read_monotonic_time().as_nanos() as u64
    } else {
        0
    }
}

/// 一条 op-log 记录的生命周期。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WalRecordState {
    /// 已追加：该页还没到 SSD，额度**不可**回收。
    Appended,
    /// 该页已落 SSD（completion 到达）：额度可回收。
    Written,
}

/// 一条 op-log 记录（`porting.md §3.1` 的最小字段集）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalRecord {
    pub(crate) inode: u64,
    pub(crate) page_index: u64,
    /// CoW 前的版本；首个 update（此前无 PM 页）为 `None`。
    pub(crate) old_page: Option<NvPageId>,
    /// CoW 之后的新版本。
    pub(crate) new_page: NvPageId,
    pub(crate) state: WalRecordState,
}

/// 每个 inode 一份的成本计数器（T1.x 验收要求的那四类）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WalCounters {
    /// append 次数。
    pub(crate) appended: u64,
    /// 被 completion 退休的条数。
    pub(crate) retired: u64,
    /// append 累计耗时（纳秒）。
    pub(crate) append_ns: u64,
    /// WAL 满回压累计等待（纳秒）与次数。
    pub(crate) full_wait_ns: u64,
    pub(crate) full_wait_events: u64,
}

struct WalState {
    /// 环形缓冲的读取端（下一个可退休的记录）。
    head: usize,
    /// 环形缓冲的写入端（下一个可写槽位）。
    tail: usize,
    /// 未退休条数。
    used: usize,
    records: Vec<Option<WalRecord>>,
    counters: WalCounters,
}

/// 一个 inode 的 WAL runtime（`porting.md §3.1` 的
/// `InodeWalRuntime { inode_id, head, tail, used, records[1024], waiters }`）。
pub(crate) struct InodeWalRuntime {
    inode: u64,
    state: Mutex<WalState>,
    /// WAL 满时的回压等待队列（**每个 inode 一个**，因此互不影响）。
    waiters: WaitQueue,
}

impl InodeWalRuntime {
    pub(crate) fn new(inode: u64) -> Self {
        Self {
            inode,
            state: Mutex::new(WalState {
                head: 0,
                tail: 0,
                used: 0,
                records: (0..WAL_CAPACITY_PER_INODE).map(|_| None).collect(),
                counters: WalCounters::default(),
            }),
            waiters: WaitQueue::new(),
        }
    }

    pub(crate) fn inode(&self) -> u64 {
        self.inode
    }

    pub(crate) fn capacity(&self) -> usize {
        WAL_CAPACITY_PER_INODE
    }

    /// 未退休条数（"每 inode 未退休记录数"这一验收指标）。
    pub(crate) fn pending(&self) -> usize {
        self.state.lock().used
    }

    pub(crate) fn is_full(&self) -> bool {
        self.pending() >= WAL_CAPACITY_PER_INODE
    }

    pub(crate) fn counters(&self) -> WalCounters {
        self.state.lock().counters
    }

    /// 尝试追加一条 op-log。满则返回 `false`，调用方须先回压
    /// （[`Self::request_writeback_and_wait`]）再重试。
    pub(crate) fn try_append(&self, record: WalRecord) -> bool {
        let started = now_ns();
        let mut state = self.state.lock();
        if state.used >= WAL_CAPACITY_PER_INODE {
            return false;
        }
        let tail = state.tail;
        state.records[tail] = Some(record);
        state.tail = (tail + 1) % WAL_CAPACITY_PER_INODE;
        state.used += 1;
        state.counters.appended += 1;
        state.counters.append_ns += now_ns().saturating_sub(started);
        true
    }

    /// WAL 满回压（T1.4）：唤醒**本 inode** 的 writeback，然后等到至少一个
    /// op-log 被 completion 退休。
    ///
    /// `request_writeback` 是 P5/rTIO 的接口缝：P1 只要求"只叫醒该 inode 的
    /// writeback"，具体如何提交由后续阶段实现。
    pub(crate) fn request_writeback_and_wait(&self, request_writeback: impl FnOnce()) {
        let started = now_ns();
        request_writeback();
        let mut state = self.state.lock();
        state.counters.full_wait_events += 1;
        drop(state);
        // 等待期间允许其它线程完成页并退休记录（`complete_page` 会 wake_all）。
        //
        // **不能用 `wait_until_or_timeout`**：它每次调用都会新装一个定时器，在紧
        // 循环里形成定时器风暴、反而饿死回刷线程（见
        // `os_com_refactor_plan/002-background-writeback/README.md` §4 与
        // `ext4/fs.rs` 的 `throttle_dirty_pages` 注释）。这里改用仓库通行的做法：
        // **短自旋 + `yield_now()` 让出 CPU + 复查**。
        loop {
            if self.pending() < WAL_CAPACITY_PER_INODE {
                break;
            }
            for _ in 0..64 {
                if self.pending() < WAL_CAPACITY_PER_INODE {
                    break;
                }
                core::hint::spin_loop();
            }
            if self.pending() < WAL_CAPACITY_PER_INODE {
                break;
            }
            crate::thread::Thread::yield_now();
        }
        let mut state = self.state.lock();
        state.counters.full_wait_ns += now_ns().saturating_sub(started);
    }

    /// completion 边界推进（`design.md §3.4`）：`page_index` 这一页到达 SSD 后，
    /// 把覆盖它的记录标成 [`WalRecordState::Written`]，并把 head 推过所有连续的
    /// Written 记录（乱序完成留下的空洞会挡住 head，这与"按完成边界推进"一致）。
    ///
    /// 返回本次退休的条数。**只在 completion 路径调用**。
    pub(crate) fn complete_page(&self, page_index: u64) -> usize {
        let mut state = self.state.lock();
        let mut marked = 0;
        for slot in 0..WAL_CAPACITY_PER_INODE {
            let Some(record) = state.records[slot].as_mut() else {
                continue;
            };
            if record.page_index == page_index && record.state == WalRecordState::Appended {
                record.state = WalRecordState::Written;
                marked += 1;
            }
        }

        let mut retired = 0;
        while state.used > 0 {
            let head = state.head;
            let done = matches!(
                state.records[head],
                Some(WalRecord {
                    state: WalRecordState::Written,
                    ..
                })
            );
            if !done {
                break;
            }
            state.records[head] = None;
            state.head = (head + 1) % WAL_CAPACITY_PER_INODE;
            state.used -= 1;
            retired += 1;
        }
        state.counters.retired += retired as u64;
        let _ = marked;
        drop(state);

        if retired > 0 {
            self.waiters.wake_all();
        }
        retired
    }

    /// 读出（不退休）当前 head 记录，供 completion 路径回收它引用的旧页。
    pub(crate) fn peek_retired(&self, offset: usize) -> Option<WalRecord> {
        let state = self.state.lock();
        if offset >= state.used {
            return None;
        }
        // head 之后的记录：已被 `complete_page` 标记为 Written 的才可回收，
        // 这里返回调用方要处理的那一条（顺序与追加顺序一致）。
        let index = (state.head + offset) % WAL_CAPACITY_PER_INODE;
        state.records[index]
    }
}

/// inode → WAL runtime 的 DRAM 表（`design.md §3.2`：每个活跃 inode 一个）。
#[derive(Default)]
pub(crate) struct InodeWalTable {
    runtimes: Mutex<BTreeMap<u64, Arc<InodeWalRuntime>>>,
}

impl InodeWalTable {
    pub(crate) fn new() -> Self {
        Self {
            runtimes: Mutex::new(BTreeMap::new()),
        }
    }

    /// 取出（必要时建立）该 inode 的 WAL runtime。
    pub(crate) fn runtime(&self, inode: u64) -> Arc<InodeWalRuntime> {
        self.runtimes
            .lock()
            .entry(inode)
            .or_insert_with(|| Arc::new(InodeWalRuntime::new(inode)))
            .clone()
    }

    pub(crate) fn get(&self, inode: u64) -> Option<Arc<InodeWalRuntime>> {
        self.runtimes.lock().get(&inode).cloned()
    }

    /// inode 回收时丢掉它的 runtime（WAL 额度随之消失）。
    pub(crate) fn forget(&self, inode: u64) -> bool {
        self.runtimes.lock().remove(&inode).is_some()
    }

    pub(crate) fn active_inodes(&self) -> usize {
        self.runtimes.lock().len()
    }
}
