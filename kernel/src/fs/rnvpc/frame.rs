// SPDX-License-Identifier: MPL-2.0

//! Allocation, pinning, and object state management for RNVPC pages.

use core::{
    fmt,
    marker::PhantomData,
    ops::{Deref, Range},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use aster_block::BLOCK_SIZE;
use ostd::{mm::USegment, sync::WaitQueue};

use super::{
    backend::NvMemoryBackend,
    capacity::{CapacityState, PoolClass, RnvpcStats},
    config::RnvpcConfig,
    data_page::DataPageState,
    descriptor::{NvDescriptor, NvObjectKind, NvObjectState},
    fault::{FaultPoint, inject},
    id::{NvFrameId, NvPageId},
    index::{
        DirtyNvPage, IndexCounters, PageEntry, PageIndex, PageKey, Residency, ReverseExtentRef,
        RuntimeHandle,
    },
    metadata::{
        AuthoritativeMetadataRecord, ExtentDataRef, ExtentMutationPhase, ExtentNodeRecord,
        INODE_BLOCK_COUNT_OFFSET, INODE_EXTENT_ROOT_OFFSET, INODE_FILE_SIZE_OFFSET,
        INODE_GENERATION_OFFSET, InodeRecord, PENDING_EXTENT_MUTATION_STATE_OFFSET,
        PendingExtentMutationRecord, PersistentMetadataHeader, PersistentMetadataKind,
        PersistentMetadataRecord, PersistentObjectRef,
    },
    persist::PersistEngine,
    recover,
};
use crate::{
    fs::fs_impls::ext4::{
        COMPLETION_FINISH_CLEAR_DESCRIPTOR, COMPLETION_FINISH_FINALIZE_LOCK,
        COMPLETION_FINISH_LEASE_WAKE, COMPLETION_FINISH_VALIDATE_LOCK, LATENCY_ENABLED,
        LATENCY_STATS, clear_completion_stage, read_ns, set_completion_stage,
    },
    prelude::*,
};

static DATA_PAGE_LEASE_EPOCH: AtomicU64 = AtomicU64::new(0);
static DATA_PAGE_LEASE_WAITERS: WaitQueue = WaitQueue::new();
static METADATA_PIN_EPOCH: AtomicU64 = AtomicU64::new(0);
static METADATA_PIN_WAITERS: WaitQueue = WaitQueue::new();

mod object_table;
use object_table::ObjectTable;
mod publication;
pub(crate) use publication::InodePublication;
mod repair_workspace;
use repair_workspace::{REPAIR_LOGICAL_INDEX_TAG, RepairWorkspace};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublicationRole {
    Foreground,
    Completion,
}

#[derive(Clone, Copy, Debug)]
struct PublicationOwner {
    role: PublicationRole,
    acquired_at_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectKind {
    Free,
    Metadata,
    Data(DataPageState),
}

#[derive(Clone, Copy, Debug)]
struct ObjectRecord {
    kind: ObjectKind,
    pins: u32,
    persisting: bool,
    committed: bool,
    key: Option<PageKey>,
    version: u64,
    /// 本槽位**当前这一版分配的序号**（分配时由 `AllocationState::next_alloc_seq` 递增赋予）。
    ///
    /// 槽位会被回收复用（页 id / frame id 只是索引），所以"索引相等"不等于"同一
    /// 对象"。I5 的 rNVPCe 要求"归还/撤销的必须确实是我分配的那一版"，因此它走
    /// [`Rnvpc::free_data_page_seq`] / [`Rnvpc::free_frame_seq`] 这类带序号的接口
    /// （与 rNVPC 给 CoW 页加 `generation` 是同一类 ABA 防护）。
    alloc_seq: u64,
    profile_write_ns: u64,
    profile_commit_ns: u64,
}

impl ObjectRecord {
    fn free() -> Self {
        Self {
            kind: ObjectKind::Free,
            pins: 0,
            persisting: false,
            committed: false,
            key: None,
            version: 0,
            alloc_seq: 0,
            profile_write_ns: 0,
            profile_commit_ns: 0,
        }
    }
}

/// T4.1：一个 NVBH 对某个 NVPage 的引用。
///
/// `published == false` 表示"引用已在 NVBH 上登记但尚未发布"——这种引用只存在
/// DRAM 侧，崩溃后不可见（`design.md §5.1` 步骤 4）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReferenceEntry {
    pub(crate) key: PageKey,
    pub(crate) page: NvPageId,
    pub(crate) published: bool,
}

/// T4.2：一次容量预留。
///
/// "等容量"必须发生在取任何低层 page/NVBH 锁**之前**，所以预留的做法是：先把池
/// 对象（数据页）拿到手，之后再进低层临界区消费它们。预留失败（额度不足）由调用方
/// 在锁外重试；未消费的页在守卫析构时归还。
pub(crate) struct CapacityReservation {
    pool: Arc<Rnvpc>,
    pages: Vec<NvPageId>,
}

impl Debug for CapacityReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapacityReservation")
            .field("remaining", &self.pages.len())
            .finish()
    }
}

impl Drop for CapacityReservation {
    fn drop(&mut self) {
        for page in self.pages.drain(..) {
            // 归还失败只可能是调用方已把该页消费/释放——留一条错误日志即可，
            // 绝不能在析构里 panic。
            if self.pool.free_data_page(page).is_err() {
                error!(
                    "RNVPC capacity reservation could not return page {:?}",
                    page
                );
            }
        }
    }
}

/// T4.2：某个 inode 的发布权。活着时该 inode 的 publish 序列独占；析构即释放。
pub(crate) struct PublicationGuard {
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    acquired_at_ns: u64,
    context: Arc<InodePublication>,
    role: PublicationRole,
}

impl PublicationGuard {
    pub(super) fn matches(&self, pool: &Arc<Rnvpc>, fs_id: u64, inode: u64) -> bool {
        Arc::ptr_eq(&self.pool, pool) && self.fs_id == fs_id && self.inode == inode
    }
}

impl Debug for PublicationGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicationGuard")
            .field("fs_id", &self.fs_id)
            .field("inode", &self.inode)
            .finish()
    }
}

impl Drop for PublicationGuard {
    fn drop(&mut self) {
        LATENCY_STATS
            .rnvpce_publication_hold
            .record_since(self.acquired_at_ns);
        self.context.release();
    }
}

pub(crate) const FIXED_INODE_LOGICAL_INDEX: u64 = (1 << 61) | 3;

struct AllocationState {
    free_ids: Vec<usize>,
    /// 下一个分配序号（只增不减）。见 [`ObjectRecord::alloc_seq`]。
    next_alloc_seq: u64,
    capacity: CapacityState,
}

struct PoolState {
    /// DRAM 控制面索引（T3.2）：key 为 `(fs_id, inode, page_index)`，**只索引数据页**
    /// （NVBH 不进页索引）；叶子记录驻留位置、运行时句柄、SSD 落点与状态。
    index: PageIndex,
    /// Current committed metadata version by logical identity. This volatile
    /// index is rebuilt from self-describing descriptors after a crash.
    metadata_index: BTreeMap<PageKey, usize>,
    /// T4.1：NVBH 槽位 → 引用登记（`NVBH -> NVPage`），DRAM-only。
    references: BTreeMap<usize, ReferenceEntry>,
    /// Duplicate-suppressed physical-view feed for rTIO. Stale generations may
    /// remain in the FIFO and are discarded when collected.
    dirty_queue: VecDeque<DirtyNvPage>,
    dirty_members: BTreeSet<(usize, u64)>,
    migration_freezes: BTreeMap<(u64, u64), Vec<Range<u64>>>,
}

pub(crate) struct Rnvpc {
    backend: Arc<dyn NvMemoryBackend>,
    persist: PersistEngine,
    config: RnvpcConfig,
    state: SpinLock<PoolState>,
    // Nested acquisition order: pool state -> allocation -> one slot.
    // Persist/I/O and wakeups must happen outside these locks.
    allocation: SpinLock<AllocationState>,
    // Identity lookup for inode initialization, recovery and standalone pool users.
    // Live EXT4 inodes cache the Arc; publication never holds this directory lock.
    publication_contexts: Mutex<BTreeMap<(u64, u64), Weak<InodePublication>>>,
    // Cleared repair images remain allocated here to avoid hot-path frame churn.
    // This mutex is independent from the global pool control state.
    repair_workspace: Mutex<RepairWorkspace>,
    objects: ObjectTable,
    // Diagnostic total only. Admission/freeing uses the slot-protected pins.
    pinned_frames: AtomicUsize,
    extent_mutation_epoch: AtomicU64,
    high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) struct ExtentRangeFreeze {
    pool: Arc<Rnvpc>,
    owner: (u64, u64),
    range: Range<u64>,
    active: bool,
}

impl Drop for ExtentRangeFreeze {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.pool.state.lock();
        if let Some(ranges) = state.migration_freezes.get_mut(&self.owner) {
            if let Some(index) = ranges
                .iter()
                .position(|range| range.start == self.range.start && range.end == self.range.end)
            {
                ranges.swap_remove(index);
            }
            if ranges.is_empty() {
                state.migration_freezes.remove(&self.owner);
            }
        }
    }
}

/// Owns the bytes of a persistent metadata record on the heap.
///
/// Large records such as a 4 KiB extent node must not be returned by value on
/// the kernel stack. The bytes remain validated and are exposed as `T` through
/// zerocopy's checked reference conversion.
pub(crate) struct BoxedMetadataRecord<T: PersistentMetadataRecord> {
    bytes: Box<[u8]>,
    _record: PhantomData<T>,
}

impl<T: PersistentMetadataRecord> Deref for BoxedMetadataRecord<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        T::ref_from_bytes(&self.bytes[..size_of::<T>()])
            .expect("validated persistent metadata layout")
    }
}

impl Rnvpc {
    pub(crate) fn data_page_lease_epoch(&self) -> u64 {
        DATA_PAGE_LEASE_EPOCH.load(Ordering::Acquire)
    }

    pub(crate) fn wait_for_data_page_lease_change(&self, observed: u64) {
        let _ = DATA_PAGE_LEASE_WAITERS.wait_until(|| {
            (DATA_PAGE_LEASE_EPOCH.load(Ordering::Acquire) != observed).then_some(())
        });
    }

    pub(crate) fn foreground_write_must_throttle(&self) -> bool {
        let stats = self.stats();
        stats.free_pages <= stats.total_pages.saturating_sub(stats.high_water_pages)
    }

    pub(crate) fn new(
        backend: Arc<dyn NvMemoryBackend>,
        config: RnvpcConfig,
        high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        if backend.capacity_pages() != config.total_pages() {
            return_errno_with_message!(
                Errno::EINVAL,
                "RNVPC backend and configured capacities differ"
            );
        }

        let total_pages = config.total_pages();
        let objects = ObjectTable::new(total_pages)?;
        let mut free_ids = Vec::new();
        free_ids
            .try_reserve_exact(total_pages)
            .map_err(|_| Error::with_message(Errno::ENOMEM, "RNVPC free-list allocation failed"))?;
        free_ids.extend((0..total_pages).rev());
        let persist = PersistEngine::new(backend.clone(), total_pages)?;
        Ok(Self {
            backend,
            persist,
            config,
            objects,
            pinned_frames: AtomicUsize::new(0),
            publication_contexts: Mutex::new(BTreeMap::new()),
            repair_workspace: Mutex::new(RepairWorkspace::new(config.meta_reserve_pages())),
            allocation: SpinLock::new(AllocationState {
                free_ids,
                next_alloc_seq: 1,
                capacity: CapacityState::new(config),
            }),
            state: SpinLock::new(PoolState {
                index: PageIndex::new(),
                metadata_index: BTreeMap::new(),
                references: BTreeMap::new(),
                dirty_queue: VecDeque::new(),
                dirty_members: BTreeSet::new(),
                migration_freezes: BTreeMap::new(),
            }),
            extent_mutation_epoch: AtomicU64::new(0),
            high_water_wake,
        })
    }

    pub(crate) fn extent_mutation_epoch(&self) -> u64 {
        self.extent_mutation_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn note_extent_mutation(&self) {
        self.extent_mutation_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn format(
        backend: Arc<dyn NvMemoryBackend>,
        config: RnvpcConfig,
        high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        let pool = Self::new(backend, config, high_water_wake)?;
        pool.persist.format()?;
        Ok(pool)
    }

    pub(crate) fn recover(
        backend: Arc<dyn NvMemoryBackend>,
        config: RnvpcConfig,
        high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        Self::recover_internal(backend, config, high_water_wake, false).map(|(pool, _)| pool)
    }

    fn recover_internal(
        backend: Arc<dyn NvMemoryBackend>,
        config: RnvpcConfig,
        high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
        profile: bool,
    ) -> Result<(Self, recover::RecoveryProfile)> {
        let total_start = recovery_now_ns(profile);
        let persist = PersistEngine::new(backend.clone(), config.total_pages())?;
        let scan_start = recovery_now_ns(profile);
        let entries = recover::scan(&persist, config.total_pages())?;
        let mut recovery_profile = recover::RecoveryProfile {
            descriptor_scan_ns: recovery_elapsed_ns(profile, scan_start),
            ..Default::default()
        };
        let rebuild_start = recovery_now_ns(profile);
        let mut pool = Self::new(backend, config, high_water_wake)?;
        {
            let state = pool.state.get_mut();
            let allocation = pool.allocation.get_mut();
            let mut max_alloc_seq = 0;
            for entry in entries {
                pool.objects.get_mut(entry.index).kind = match entry.class {
                    PoolClass::Metadata => ObjectKind::Metadata,
                    PoolClass::Data => ObjectKind::Data(DataPageState::NvDirty),
                    // WAL 是 DRAM-only（`porting.md §3.1`），扫描不应当产出它。
                    PoolClass::Wal => {
                        return_errno_with_message!(
                            Errno::EIO,
                            "WAL pages are DRAM-only and must not be recovered"
                        )
                    }
                };
                pool.objects.get_mut(entry.index).committed = true;
                pool.objects.get_mut(entry.index).key = Some(entry.key);
                pool.objects.get_mut(entry.index).version = entry.version;
                pool.objects.get_mut(entry.index).alloc_seq = entry.allocation_seq;
                max_alloc_seq = max_alloc_seq.max(entry.allocation_seq);
                allocation.capacity.account_recovered(entry.class)?;
                // T3.3：只从自描述描述符重建 `Nv` 条目；`Dram` 条目掉电即失效，
                // 没有任何东西可以让它们复活（也不该复活）。
                if entry.class == PoolClass::Data {
                    let id = NvPageId::try_from(entry.index)
                        .expect("pool slot index must fit a NvPageId");
                    state.index.insert(
                        entry.key,
                        PageEntry {
                            residency: Residency::Nv,
                            handle: Some(RuntimeHandle::Nv(id)),
                            ssd_backing_block: entry.ssd_backing_block,
                            state: NvObjectState::NvDirty,
                        },
                    );
                } else {
                    let replace = state
                        .metadata_index
                        .get(&entry.key)
                        .and_then(|current| pool.objects.get(*current))
                        .is_none_or(|current| entry.version > current.version);
                    if replace {
                        state.metadata_index.insert(entry.key, entry.index);
                    }
                }
            }
            allocation
                .free_ids
                .retain(|index| pool.objects.get_mut(*index).kind == ObjectKind::Free);
            if max_alloc_seq != 0 {
                allocation.next_alloc_seq = max_alloc_seq
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            }
        }
        recovery_profile.runtime_rebuild_ns = recovery_elapsed_ns(profile, rebuild_start);
        pool.persist = persist;
        let pool = Arc::new(pool);
        recover::reconcile_profiled(&pool, profile.then_some(&mut recovery_profile))?;
        recovery_profile.total_ns = recovery_elapsed_ns(profile, total_start);
        let pool = Arc::try_unwrap(pool)
            .map_err(|_| Error::with_message(Errno::EBUSY, "recovery retained an RNVPC handle"))?;
        Ok((pool, recovery_profile))
    }

    /// Rebuilds all volatile recovery state from the same persistent bytes into
    /// an independent temporary pool. The live global pool is not consulted,
    /// which models loss of Rust handles and DRAM indexes within one VM.
    pub(crate) fn profile_recovery(&self) -> Result<recover::RecoveryProfile> {
        let (recovered, profile) =
            Self::recover_internal(self.backend.clone(), self.config, None, true)?;
        drop(recovered);
        Ok(profile)
    }

    pub(crate) fn alloc_frame(&self, class: PoolClass) -> Result<NvFrameId> {
        if class != PoolClass::Metadata {
            return_errno_with_message!(Errno::EINVAL, "data pages require alloc_data_page");
        }
        let (index, _seq, wake) = self.allocate_index_with_repair_drain(PoolClass::Metadata)?;
        if wake {
            self.wake_high_water();
        }
        NvFrameId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))
    }

    /// 分配一个控制面 frame，并返回它的**分配序号**（ABA 防护，见 [`ObjectRecord::alloc_seq`]）。
    pub(crate) fn alloc_frame_seq(&self) -> Result<(NvFrameId, u64)> {
        let (index, seq, wake) = self.allocate_index_with_repair_drain(PoolClass::Metadata)?;
        if wake {
            self.wake_high_water();
        }
        let id = NvFrameId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        Ok((id, seq))
    }

    pub(crate) fn alloc_data_page(&self) -> Result<NvPageId> {
        let (index, _seq, wake) = self.allocate_index_with_repair_drain(PoolClass::Data)?;
        if wake {
            self.wake_high_water();
        }
        NvPageId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))
    }

    /// 分配一个数据页，并返回它的**分配序号**（ABA 防护）。
    pub(crate) fn alloc_data_page_seq(&self) -> Result<(NvPageId, u64)> {
        let (index, seq, wake) = self.allocate_index_with_repair_drain(PoolClass::Data)?;
        if wake {
            self.wake_high_water();
        }
        let id = NvPageId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        Ok((id, seq))
    }

    /// 该数据页当前的分配序号（`None` = 槽位不是数据页或未分配）。
    pub(crate) fn data_page_seq(&self, id: NvPageId) -> Option<u64> {
        let state = self.objects.lock_checked(id.index()).ok()?;
        let record = state.records.get(id.index())?;
        matches!(record.kind, ObjectKind::Data(_)).then_some(record.alloc_seq)
    }

    /// 该 frame 当前的分配序号（`None` = 槽位不是元数据 frame）。
    pub(crate) fn frame_seq(&self, id: NvFrameId) -> Option<u64> {
        let state = self.objects.lock_checked(id.index()).ok()?;
        let record = state.records.get(id.index())?;
        (record.kind == ObjectKind::Metadata).then_some(record.alloc_seq)
    }

    /// rTIO v0：开始一页的 SSD 回刷（`NvDirty -> InFlight`，独占）。
    ///
    /// 拿到句柄后：成功调 `finish(None)`（→ `Synced`，可回收），失败直接 drop
    /// （自动回 `NvDirty`，保留数据供重试）。
    pub(crate) fn pin_for_writeback(self: &Arc<Self>, id: NvPageId) -> Result<EvictingDataPage> {
        self.begin_data_page_eviction(id)
    }

    /// 同 [`pin_for_writeback`](Self::pin_for_writeback)，但要求槽位**仍是候选采集时
    /// 的那一版**（`seq` 相符）。
    ///
    /// 必须用它：回刷候选是一个**快照**，而从采集到 pin 之间页 id 可能已被归还并复用。
    /// 不加校验就会把**别人**的页内容写到旧候选记下的目标块上（真实的数据损坏路径）。
    /// 校验与独占必须**同一把锁内**完成，否则校验通过后槽位仍可能被换掉。
    pub(crate) fn pin_for_writeback_seq(
        self: &Arc<Self>,
        id: NvPageId,
        seq: u64,
    ) -> Result<EvictingDataPage> {
        self.begin_data_page_eviction_inner(id, Some(seq))
    }

    /// 为 per-inode WAL 记入 `pages` 页 PM 容量（`porting.md §3.1`：WAL 与 CoW page
    /// 都计入 PM 容量）。半途失败时把已记入的部分退回，不留账。
    pub(crate) fn charge_wal_pages(&self, pages: usize) -> Result<()> {
        let mut wake = false;
        {
            let mut state = self.allocation.lock();
            for charged in 0..pages {
                if let Err(error) = state.capacity.before_allocate(PoolClass::Wal) {
                    for _ in 0..charged {
                        state.capacity.account_free(PoolClass::Wal);
                    }
                    return Err(error);
                }
                wake |= state.capacity.account_allocate(PoolClass::Wal);
            }
        }
        if wake {
            self.wake_high_water();
        }
        Ok(())
    }

    /// 退回 WAL 占用的 PM 容量（completion 退休记录后调用）。
    pub(crate) fn release_wal_pages(&self, pages: usize) {
        let mut state = self.allocation.lock();
        for _ in 0..pages {
            state.capacity.account_free(PoolClass::Wal);
        }
    }

    /// Allocates a staged data page initialized with one full page of content.
    pub(crate) fn stage_data_page(&self, input: &[u8]) -> Result<NvPageId> {
        if input.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page content is not page-sized");
        }
        let id = self.alloc_data_page()?;
        if let Err(error) = self.backend.write_page(id, input) {
            self.free_data_page(id)
                .expect("freshly allocated data page must be freeable");
            return Err(error);
        }
        Ok(id)
    }

    /// 同 [`stage_data_page`](Self::stage_data_page)，另返回**分配序号**（ABA 防护）。
    pub(crate) fn stage_data_page_seq(&self, input: &[u8]) -> Result<(NvPageId, u64)> {
        if input.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page content is not page-sized");
        }
        let (id, seq) = self.alloc_data_page_seq()?;
        if let Err(error) = self.backend.write_page(id, input) {
            // 刚分配的页不可能已被别人复用，直接按无序号路径归还即可。
            let _ = self.free_data_page(id);
            return Err(error);
        }
        Ok((id, seq))
    }

    pub(crate) fn pin_frame(self: &Arc<Self>, id: NvFrameId) -> Result<PinnedFrame> {
        loop {
            let observed = METADATA_PIN_EPOCH.load(Ordering::Acquire);
            {
                let mut state = self.objects.lock_checked(id.index())?;
                let record = state
                    .records
                    .get(id.index())
                    .ok_or_else(|| Error::new(Errno::EINVAL))?;
                if record.kind != ObjectKind::Metadata {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "RNVPC frame is not allocated metadata"
                    );
                }
                if !record.persisting {
                    let pins = record
                        .pins
                        .checked_add(1)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    self.account_pin()?;
                    state.records[id.index()].pins = pins;
                    return Ok(PinnedFrame {
                        pool: self.clone(),
                        id,
                    });
                }
            }
            let _ = METADATA_PIN_WAITERS.wait_until(|| {
                (METADATA_PIN_EPOCH.load(Ordering::Acquire) != observed).then_some(())
            });
        }
    }

    /// Pins an allocated data page so its bytes remain stable until dropped.
    pub(crate) fn pin_data_page(self: &Arc<Self>, id: NvPageId) -> Result<PinnedDataPage> {
        self.pin_data_page_in_owner(id, None)
    }

    /// 按 **所有者** `(fs_id, inode)` 固定一个已提交数据页（取代旧的 namespace 隔离）。
    pub(crate) fn pin_owned_data_page(
        self: &Arc<Self>,
        id: NvPageId,
        owner: (u64, u64),
    ) -> Result<PinnedDataPage> {
        self.pin_data_page_in_owner(id, Some(owner))
    }

    fn pin_data_page_in_owner(
        self: &Arc<Self>,
        id: NvPageId,
        owner: Option<(u64, u64)>,
    ) -> Result<PinnedDataPage> {
        let mut state = self.objects.lock_checked(id.index())?;
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !matches!(record.kind, ObjectKind::Data(_)) {
            return_errno_with_message!(Errno::EINVAL, "RNVPC page is not allocated data");
        }
        if owner.is_some_and(|(fs_id, inode)| {
            record
                .key
                .is_none_or(|key| (key.fs_id, key.inode) != (fs_id, inode))
        }) {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page owner mismatch");
        }
        if record.persisting {
            return_errno_with_message!(Errno::EBUSY, "RNVPC data page persistence is in progress");
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.account_pin()?;
        state.records[id.index()].pins = pins;
        Ok(PinnedDataPage {
            pool: self.clone(),
            id,
        })
    }

    pub(crate) fn free_frame(&self, id: NvFrameId) -> Result<()> {
        // 对象没了，它持有的引用边也就没了（不留悬空登记）。
        let result = self.free_index(id.index(), PoolClass::Metadata);
        if result.is_ok() {
            self.state.lock().references.remove(&id.index());
        }
        result
    }

    pub(crate) fn free_data_page(&self, id: NvPageId) -> Result<()> {
        let state = self.state.lock();
        let mut allocation = self.allocation.lock();
        let mut objects = self.objects.lock(id.index());
        if objects.records[id.index()].committed {
            return_errno_with_message!(Errno::EBUSY, "RNVPC data page is still committed");
        }
        // T4.1 不变量 2：仍被**已发布**引用持有的页不得回收（必须先撤销引用）。
        if state
            .references
            .values()
            .any(|entry| entry.published && entry.page == id)
        {
            return_errno_with_message!(Errno::EBUSY, "RNVPC data page is still referenced");
        }
        let record = objects
            .records
            .get_mut(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let ObjectKind::Data(data_state) = record.kind else {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page is not allocated");
        };
        if record.pins != 0 || !data_state.can_free() {
            return_errno_with_message!(Errno::EBUSY, "RNVPC data page cannot be freed");
        }
        record.kind = ObjectKind::Free;
        allocation.free_ids.push(id.index());
        allocation.capacity.account_free(PoolClass::Data);
        Ok(())
    }

    /// **原子版**的页归还：**同一把锁内**完成“确定槽位仍是这一版”与“归还”。
    ///
    /// 必须先校验再归还，两步又要不可分割：否则检查通过后槽位可能马上被别的路径
    /// 归还并被重新分配，归还就会落在**新主人**身上（首轮实测：`complete_flush`
    /// 因此删掉了并发 retire 刚 `stage_reference` 的未发布引用）。
    ///
    /// `Ok(true)` = 已归还；`Ok(false)` = 现在还不该归（已不是这一版 / 已提交 /
    /// 被已发布引用持有 / 被 pin 住），调用方保留待重试。
    pub(crate) fn free_data_page_if_seq(&self, id: NvPageId, seq: u64) -> Result<bool> {
        let state = self.state.lock();
        let mut allocation = self.allocation.lock();
        let mut objects = self.objects.lock_checked(id.index())?;
        let record = objects
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        // 槽位已经不是“那一版”（已归还或已被复用）⇒ 什么都不做。
        let ObjectKind::Data(data_state) = record.kind else {
            return Ok(false);
        };
        if record.alloc_seq != seq || record.committed || record.pins != 0 || !data_state.can_free()
        {
            return Ok(false);
        }
        // T4.1 不变量 2：仍被**已发布**引用持有的页不得回收（必须先撤销引用）。
        if state
            .references
            .values()
            .any(|entry| entry.published && entry.page == id)
        {
            return Ok(false);
        }
        let record = &mut objects.records[id.index()];
        record.kind = ObjectKind::Free;
        allocation.free_ids.push(id.index());
        allocation.capacity.account_free(PoolClass::Data);
        Ok(true)
    }

    pub(crate) fn abort_data_page(&self, id: NvPageId) -> Result<()> {
        let mut state = self.allocation.lock();
        let mut objects = self.objects.lock_checked(id.index())?;
        let record = objects
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.committed || record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "RNVPC data page cannot be aborted");
        }
        if !matches!(
            record.kind,
            ObjectKind::Data(DataPageState::Staged | DataPageState::NvDirty)
        ) {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page is not abortable");
        }
        objects.records[id.index()] = ObjectRecord::free();
        state.free_ids.push(id.index());
        state.capacity.account_free(PoolClass::Data);
        Ok(())
    }

    pub(crate) fn data_page_state(&self, id: NvPageId) -> Result<DataPageState> {
        let state = self.objects.lock_checked(id.index())?;
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        match record.kind {
            ObjectKind::Free => Ok(DataPageState::Free),
            ObjectKind::Data(data_state) => Ok(data_state),
            ObjectKind::Metadata => {
                return_errno_with_message!(Errno::EINVAL, "RNVPC object is metadata")
            }
        }
    }

    /// 数据页在持久侧的 NV 对象状态（T2.3）。
    ///
    /// 返回 `None` 表示该页尚未发布为 NV 对象（`Staged`）或已被释放；返回
    /// `NvReclaimable` 表示 writeback 已完成、只能回收。
    pub(crate) fn data_page_nv_state(&self, id: NvPageId) -> Result<Option<NvObjectState>> {
        let state = self.objects.lock_checked(id.index())?;
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        match record.kind {
            ObjectKind::Free => Ok(None),
            ObjectKind::Data(data_state) => Ok(data_state.nv_object_state()),
            ObjectKind::Metadata => {
                return_errno_with_message!(Errno::EINVAL, "RNVPC object is metadata")
            }
        }
    }

    pub(crate) fn transition_data_page(
        &self,
        id: NvPageId,
        expected: DataPageState,
        next: DataPageState,
    ) -> Result<()> {
        if !expected.can_transition_to(next) {
            return_errno_with_message!(Errno::EINVAL, "invalid RNVPC data page transition");
        }
        // 池内迁移不得与持久侧的状态机冲突：两端都有 NV 状态时必须合法。
        if let (Some(from), Some(to)) = (expected.nv_object_state(), next.nv_object_state())
            && !from.can_transition_to(to)
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "RNVPC data page transition violates the NV object state machine"
            );
        }

        let mut state = self.state.lock();
        let mut objects = self.objects.lock(id.index());
        let key = {
            let record = objects
                .records
                .get_mut(id.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if record.kind != ObjectKind::Data(expected) {
                return_errno_with_message!(Errno::EBUSY, "RNVPC data page state changed");
            }
            record.kind = ObjectKind::Data(next);
            record.key
        };
        // DRAM 页索引的叶子状态必须跟着页状态走：写回候选只认 `NvDirty` 条目，
        // 不然已提交（`InFlight`）的页会被反复当成候选重新收集。
        if let Some(key) = key
            && let Some(entry) = state.index.lookup_mut(key)
            && let Some(nv_state) = next.nv_object_state()
        {
            entry.state = nv_state;
        }
        Ok(())
    }

    /// T4.2：预留 `pages` 个数据页的容量。
    ///
    /// 额度不足 ⇒ `EAGAIN`（调用方在**锁外**重试，而不是持着低层锁等容量）。
    /// 成功后这些页都处于 `Staged`，由调用方在低层临界区内消费。
    pub(crate) fn reserve_data_pages(
        self: &Arc<Self>,
        pages: usize,
    ) -> Result<CapacityReservation> {
        let mut reservation = CapacityReservation {
            pool: self.clone(),
            pages: Vec::new(),
        };
        reservation
            .pages
            .try_reserve_exact(pages)
            .map_err(|_| Error::with_message(Errno::ENOMEM, "RNVPC reservation allocation"))?;
        for _ in 0..pages {
            match self.alloc_data_page() {
                Ok(page) => reservation.pages.push(page),
                Err(error) => {
                    // 守卫析构会归还已经拿到的页；这里原样把错误抛给调用方重试。
                    return Err(error);
                }
            }
        }
        Ok(reservation)
    }

    /// 从预留里取一个页；预留耗尽 ⇒ `EAGAIN`（调用方应重新预留）。
    pub(crate) fn take_reserved_page(
        &self,
        reservation: &mut CapacityReservation,
    ) -> Result<NvPageId> {
        reservation
            .pages
            .pop()
            .ok_or_else(|| Error::new(Errno::EAGAIN))
    }

    pub(crate) fn stage_reserved_data_page(
        &self,
        reservation: &mut CapacityReservation,
        input: &[u8],
    ) -> Result<NvPageId> {
        if input.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page content is not page-sized");
        }
        let id = self.take_reserved_page(reservation)?;
        if let Err(error) = self.backend.write_page(id, input) {
            let _ = self.free_data_page(id);
            return Err(error);
        }
        Ok(id)
    }

    pub(crate) fn begin_reserved_data_page(
        self: &Arc<Self>,
        reservation: &mut CapacityReservation,
    ) -> Result<StagingDataPage> {
        let id = self.take_reserved_page(reservation)?;
        match self.pin_data_page(id) {
            Ok(pin) => Ok(StagingDataPage {
                pin: Some(pin),
                initialized: false,
            }),
            Err(error) => {
                let _ = self.free_data_page(id);
                Err(error)
            }
        }
    }

    /// 预留中尚未消费的页数（ktest）。
    pub(crate) fn reserved_page_count(&self, reservation: &CapacityReservation) -> usize {
        reservation.pages.len()
    }

    /// T4.3：发布失败时的回滚。
    ///
    /// 撤销 `bh` 上的引用登记（无论是否已发布），**保留被引用页的数据与原状态**，
    /// 使调用方可以修好问题后重试（`design.md §6` completion 规则 2 的同构要求：
    /// 失败保留数据、允许重试）。
    /// **原子版**的 NVBH 释放：**同一把锁内**完成“确定槽位仍是这一版”+“撤销引用”+“归还”。
    ///
    /// 三步都必须在同一把锁里：分开做的话，`frame_seq(id) == seq` 通过之后槽位可能
    /// 被归还并立刻重新分配，随后的 `revoke_reference` / `free_frame` 就会落到**新主人**
    /// 身上。首轮实测 63 次 `publish_reference` 报 `reference was never staged`
    /// 全部来自这个窗口（被删的正是并发 retire 刚登记的未发布引用）。
    ///
    /// `Ok(true)` = 已撤销并归还；`Ok(false)` = 槽位已不是这一版（调用方只能记一笔）。
    pub(crate) fn revoke_and_free_frame_if_seq(&self, id: NvFrameId, seq: u64) -> Result<bool> {
        let mut state = self.state.lock();
        let mut allocation = self.allocation.lock();
        let mut objects = self.objects.lock_checked(id.index())?;
        {
            let record = objects
                .records
                .get(id.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if record.kind != ObjectKind::Metadata || record.alloc_seq != seq {
                return Ok(false);
            }
            if record.committed || record.pins != 0 || record.persisting {
                return_errno_with_message!(Errno::EBUSY, "RNVPC frame is pinned");
            }
        }
        // 对象没了，它持有的引用边也就没了（不留悬空登记）。
        state.references.remove(&id.index());
        let record = &mut objects.records[id.index()];
        record.kind = ObjectKind::Free;
        allocation.free_ids.push(id.index());
        allocation.capacity.account_free(PoolClass::Metadata);
        Ok(true)
    }

    pub(crate) fn rollback_publication(&self, bh: NvFrameId) -> Result<()> {
        self.state.lock().references.remove(&bh.index());
        Ok(())
    }

    /// T4.2：取得某个 inode 的**发布权**。
    ///
    /// 同一 inode 的 publish 必须串行（`design.md §5.1` 不变量 4），不同 inode 可以
    /// 并行。竞争者在不持有 pool spin lock 的情况下睡眠，直到当前发布序列结束。
    pub(crate) fn begin_publication(self: &Arc<Self>, inode: u64) -> Result<PublicationGuard> {
        self.begin_publication_for(0, inode)
    }

    pub(crate) fn begin_publication_for(
        self: &Arc<Self>,
        fs_id: u64,
        inode: u64,
    ) -> Result<PublicationGuard> {
        self.begin_publication_for_role(fs_id, inode, PublicationRole::Foreground)
    }

    pub(crate) fn begin_publication_for_role(
        self: &Arc<Self>,
        fs_id: u64,
        inode: u64,
        role: PublicationRole,
    ) -> Result<PublicationGuard> {
        self.inode_publication(fs_id, inode).begin(self, role)
    }

    pub(crate) fn inode_publication(
        self: &Arc<Self>,
        fs_id: u64,
        inode: u64,
    ) -> Arc<InodePublication> {
        let mut contexts = self.publication_contexts.lock();
        if let Some(context) = contexts.get(&(fs_id, inode)).and_then(Weak::upgrade) {
            return context;
        }
        let context = Arc::new(InodePublication::new(self, fs_id, inode));
        contexts.insert((fs_id, inode), Arc::downgrade(&context));
        context
    }

    /// T4.1 步骤 4：在 `bh` 上登记一个**未发布**的引用。
    ///
    /// 只写 DRAM 侧登记表，不触碰持久面——崩溃后这条边不存在。
    pub(crate) fn stage_reference(
        &self,
        bh: NvFrameId,
        key: PageKey,
        page: NvPageId,
    ) -> Result<()> {
        let mut state = self.state.lock();
        if self.objects.get(bh.index()).map(|record| record.kind) != Some(ObjectKind::Metadata) {
            return_errno_with_message!(Errno::EINVAL, "RNVPC reference owner is not an NVBH");
        }
        if !matches!(
            self.objects.get(page.index()).map(|record| record.kind),
            Some(ObjectKind::Data(_))
        ) {
            return_errno_with_message!(Errno::EINVAL, "RNVPC referenced object is not a data page");
        }
        state.references.insert(
            bh.index(),
            ReferenceEntry {
                key,
                page,
                published: false,
            },
        );
        Ok(())
    }

    /// T4.1 步骤 5：发布引用。
    ///
    /// **只有已就绪（非 `Staged`）的 NVPage 才允许被引用**——这是"数据先于引用"
    /// 落到 API 上的形式（`design.md §5.1` 不变量 1）。
    pub(crate) fn publish_reference(&self, bh: NvFrameId) -> Result<()> {
        let mut state = self.state.lock();
        let Some(entry) = state.references.get(&bh.index()).copied() else {
            return_errno_with_message!(Errno::EINVAL, "RNVPC reference was never staged");
        };
        let page_state = match self
            .objects
            .get(entry.page.index())
            .map(|record| record.kind)
        {
            Some(ObjectKind::Data(data_state)) => data_state,
            _ => return_errno_with_message!(Errno::EINVAL, "RNVPC referenced page is gone"),
        };
        if page_state == DataPageState::Staged {
            return_errno_with_message!(
                Errno::EINVAL,
                "RNVPC reference would publish a page that is not ready"
            );
        }
        state
            .references
            .get_mut(&bh.index())
            .expect("entry was just looked up")
            .published = true;
        Ok(())
    }

    /// T4.1 步骤 7：撤销引用，返回被撤销的页（此后它才可回收）。
    pub(crate) fn revoke_reference(&self, bh: NvFrameId) -> Result<Option<NvPageId>> {
        let mut state = self.state.lock();
        Ok(state.references.remove(&bh.index()).map(|entry| entry.page))
    }

    /// 查 `bh` 的引用登记（ktest）。
    pub(crate) fn reference_of(&self, bh: NvFrameId) -> Option<ReferenceEntry> {
        self.state.lock().references.get(&bh.index()).copied()
    }

    /// 被**已发布**引用持有的页数（ktest）。
    pub(crate) fn published_reference_count(&self) -> usize {
        self.state
            .lock()
            .references
            .values()
            .filter(|entry| entry.published)
            .count()
    }

    /// DRAM 页索引的计数器（T3.1/T3.2 验收用）。
    pub(crate) fn index_counters(&self) -> IndexCounters {
        self.state.lock().index.counters()
    }

    /// 按逻辑身份查 DRAM 页索引。
    pub(crate) fn index_lookup(&self, key: PageKey) -> Option<PageEntry> {
        self.state.lock().index.lookup(key).copied()
    }

    /// Resolves and pins the current committed NV version of `key` atomically.
    ///
    /// Keeping the index lookup, record validation, and pin acquisition under
    /// one state lock prevents a concurrent replacement from turning the
    /// resolved slot into a stale or reusable object before it is pinned.
    pub(crate) fn pin_current_data_page(
        self: &Arc<Self>,
        key: PageKey,
    ) -> Result<Option<PinnedDataPage>> {
        let state = self.state.lock();
        let Some(entry) = state.index.lookup(key).copied() else {
            return Ok(None);
        };
        if entry.residency == Residency::Dram {
            return Ok(None);
        }
        let Some(RuntimeHandle::Nv(id)) = entry.handle else {
            return_errno_with_message!(Errno::EUCLEAN, "NV index entry has no NV handle");
        };
        let mut objects = self.objects.lock_checked(id.index())?;
        let record = objects
            .records
            .get(id.index())
            .copied()
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "NV index slot is invalid"))?;
        if !record.committed
            || !matches!(record.kind, ObjectKind::Data(_))
            || record.key != Some(key)
        {
            return_errno_with_message!(Errno::EUCLEAN, "NV index points to stale data");
        }
        if record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NV data page persistence is in progress");
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.account_pin()?;
        objects.records[id.index()].pins = pins;
        Ok(Some(PinnedDataPage {
            pool: self.clone(),
            id,
        }))
    }

    /// Pins the exact committed data-page generation named by a persistent reference.
    pub(crate) fn pin_persistent_data_page(
        self: &Arc<Self>,
        reference: PersistentObjectRef,
    ) -> Result<PinnedDataPage> {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let id = NvPageId::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let mut state = self.objects.lock_checked(slot)?;
        let record = state
            .records
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !record.committed
            || !matches!(record.kind, ObjectKind::Data(_))
            || record.alloc_seq != reference.allocation_seq()
        {
            return_errno_with_message!(Errno::ESTALE, "stale persistent data reference");
        }
        if record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NV data page persistence is in progress");
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.account_pin()?;
        state.records[slot].pins = pins;
        Ok(PinnedDataPage {
            pool: self.clone(),
            id,
        })
    }

    /// Exclusively claims a committed NVPage for a foreground in-place update.
    /// The same `persisting` bit is used by rTIO, so migration and mutation
    /// cannot observe or modify the page concurrently.
    pub(crate) fn begin_persistent_data_mutation(
        self: &Arc<Self>,
        reference: PersistentObjectRef,
    ) -> Result<MutatingDataPage> {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let id = NvPageId::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let mut state = self.objects.lock_checked(slot)?;
        let record = state
            .records
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !record.committed
            || record.kind != ObjectKind::Data(DataPageState::NvDirty)
            || record.alloc_seq != reference.allocation_seq()
        {
            return_errno_with_message!(Errno::ESTALE, "NV data mutation targets a stale page");
        }
        if record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NV data page is busy");
        }
        self.account_pin()?;
        state.records[slot].pins = 1;
        state.records[slot].persisting = true;
        Ok(MutatingDataPage {
            pool: self.clone(),
            id,
            active: true,
        })
    }

    /// 该页是否被某个**已发布**引用持有（T4.1）。completion 回收前必须问这一句。
    pub(crate) fn is_page_referenced(&self, id: NvPageId) -> bool {
        self.state
            .lock()
            .references
            .values()
            .any(|entry| entry.published && entry.page == id)
    }

    /// 当前处于 `NvDirty` 的页（DRAM 页索引口径）——rTIO 的写回候选来源。
    pub(crate) fn dirty_candidates(&self) -> Vec<PageKey> {
        self.state.lock().index.dirty_candidates()
    }

    pub(crate) fn enqueue_dirty_nvpage(&self, page: NvPageId, allocation_seq: u64) -> Result<bool> {
        let mut state = self.state.lock();
        let mut objects = self.objects.lock_checked(page.index())?;
        let record = objects
            .records
            .get(page.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !record.committed
            || record.alloc_seq != allocation_seq
            || record.kind != ObjectKind::Data(DataPageState::NvDirty)
        {
            return_errno_with_message!(Errno::ESTALE, "dirty NVPage generation is stale");
        }
        let identity = (page.index(), allocation_seq);
        if LATENCY_ENABLED {
            objects.records[page.index()].profile_commit_ns = read_ns();
        }
        if !state.dirty_members.insert(identity) {
            return Ok(false);
        }
        state.dirty_queue.push_back(DirtyNvPage {
            page,
            allocation_seq,
        });
        Ok(true)
    }

    pub(crate) fn collect_dirty_nvpages(&self, budget: usize) -> Vec<DirtyNvPage> {
        let mut state = self.state.lock();
        let mut out = Vec::new();
        while out.len() < budget {
            let Some(candidate) = state.dirty_queue.pop_front() else {
                break;
            };
            state
                .dirty_members
                .remove(&(candidate.page.index(), candidate.allocation_seq));
            let objects = self.objects.lock(candidate.page.index());
            let Some(record) = objects.records.get(candidate.page.index()) else {
                continue;
            };
            if record.committed
                && record.alloc_seq == candidate.allocation_seq
                && record.kind == ObjectKind::Data(DataPageState::NvDirty)
                && objects.reverse.is_some()
            {
                out.push(candidate);
            }
        }
        out
    }

    pub(crate) fn install_reverse(
        &self,
        data: PersistentObjectRef,
        locator: ReverseExtentRef,
    ) -> Result<()> {
        let slot = usize::try_from(data.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let mut state = self.objects.lock_checked(slot)?;
        let record = state
            .records
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !record.committed
            || record.alloc_seq != data.allocation_seq()
            || !matches!(record.kind, ObjectKind::Data(_))
        {
            return_errno_with_message!(Errno::ESTALE, "reverse target generation is stale");
        }
        if state.reverse.is_some() {
            return_errno_with_message!(Errno::EEXIST, "NVPage already has a reverse owner");
        }
        state.reverse = Some(locator);
        Ok(())
    }

    pub(crate) fn replace_reverse(
        &self,
        data: PersistentObjectRef,
        locator: ReverseExtentRef,
    ) -> Result<()> {
        let slot = usize::try_from(data.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let mut state = self.objects.lock_checked(slot)?;
        let record = state
            .records
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !record.committed || record.alloc_seq != data.allocation_seq() {
            return_errno_with_message!(Errno::ESTALE, "reverse target generation is stale");
        }
        state.reverse = Some(locator);
        Ok(())
    }

    pub(crate) fn reverse_lookup(
        &self,
        data: PersistentObjectRef,
    ) -> Result<Option<ReverseExtentRef>> {
        let slot = usize::try_from(data.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let state = self.objects.lock_checked(slot)?;
        Ok(if state.records[slot].alloc_seq == data.allocation_seq() {
            state.reverse
        } else {
            None
        })
    }

    pub(crate) fn clear_reverse(&self, data: PersistentObjectRef) -> Result<bool> {
        let slot = usize::try_from(data.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let mut state = self.objects.lock_checked(slot)?;
        Ok(
            state.records[slot].alloc_seq == data.allocation_seq()
                && state.reverse.take().is_some(),
        )
    }

    pub(crate) fn freeze_extent_range(
        self: &Arc<Self>,
        fs_id: u64,
        inode: u64,
        range: Range<u64>,
    ) -> Result<ExtentRangeFreeze> {
        if range.start >= range.end {
            return_errno_with_message!(Errno::EINVAL, "empty rTIO extent freeze");
        }
        self.state
            .lock()
            .migration_freezes
            .entry((fs_id, inode))
            .or_default()
            .push(range.clone());
        Ok(ExtentRangeFreeze {
            pool: self.clone(),
            owner: (fs_id, inode),
            range,
            active: true,
        })
    }

    pub(crate) fn migration_is_frozen(&self, key: PageKey) -> bool {
        self.state
            .lock()
            .migration_freezes
            .get(&(key.fs_id, key.inode))
            .is_some_and(|ranges| ranges.iter().any(|range| range.contains(&key.page_index)))
    }

    /// 页索引中的条目数。
    pub(crate) fn index_len(&self) -> usize {
        self.state.lock().index.len()
    }

    pub(crate) fn stats(&self) -> RnvpcStats {
        let mut stats = self.allocation.lock().capacity.stats();
        stats.pinned_frames = self.pinned_frames.load(Ordering::Relaxed);
        stats
    }

    fn account_pin(&self) -> Result<()> {
        self.pinned_frames
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pins| {
                pins.checked_add(1)
            })
            .map(|_| ())
            .map_err(|_| Error::new(Errno::EOVERFLOW))
    }

    fn account_unpin(&self) {
        let previous = self.pinned_frames.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0);
    }

    /// 按逻辑身份找一个**已提交的元数据对象**的存储槽位。
    ///
    /// T2.2b：这里不再查全局 resident 索引（已删除），而是按描述符身份 `PageKey`
    /// 在对象表上线性扫描；数据页的真实查找路径由 DRAM B-tree（`index::PageIndex`）
    /// 承担，本函数只服务于元数据对象与管理接口。
    pub(crate) fn committed_metadata_slot(&self, key: PageKey) -> Option<NvFrameId> {
        let state = self.state.lock();
        let index = *state.metadata_index.get(&key)?;
        let objects = self.objects.lock(index);
        let record = objects.records.get(index)?;
        (record.committed && record.kind == ObjectKind::Metadata && record.key == Some(key))
            .then(|| NvFrameId::try_from(index).ok())
            .flatten()
    }

    #[cfg(ktest)]
    pub(crate) fn metadata_lookup_index_len(&self) -> usize {
        self.state.lock().metadata_index.len()
    }

    /// 某 `(fs_id, inode)` 下所有已提交的元数据对象：`(logical_index, slot)`。
    pub(crate) fn committed_metadata_of(&self, fs_id: u64, inode: u64) -> Vec<(u64, NvFrameId)> {
        self.objects.collect(|index, record| {
            let key = record.key?;
            (record.committed
                && record.kind == ObjectKind::Metadata
                && key.fs_id == fs_id
                && key.inode == inode)
                .then(|| {
                    (
                        key.page_index,
                        NvFrameId::try_from(index)
                            .expect("committed metadata index must fit NvFrameId"),
                    )
                })
        })
    }

    /// 按逻辑身份找一个已提交数据页。
    pub(crate) fn committed_data_page(&self, key: PageKey) -> Option<NvPageId> {
        self.objects
            .collect(|index, record| {
                (record.committed
                    && matches!(record.kind, ObjectKind::Data(_))
                    && record.key == Some(key))
                .then(|| NvPageId::try_from(index).ok())
                .flatten()
            })
            .into_iter()
            .next()
    }

    /// 所有已提交数据页：`(PageKey, slot)`（恢复/统计用）。
    pub(crate) fn committed_data_pages(&self) -> Vec<(PageKey, NvPageId)> {
        self.objects.collect(|index, record| {
            let key = record.key?;
            (record.committed && matches!(record.kind, ObjectKind::Data(_))).then(|| {
                (
                    key,
                    NvPageId::try_from(index).expect("data page index must fit NvPageId"),
                )
            })
        })
    }

    /// 某 `(fs_id, inode)` 下所有已提交数据页：`(page_index, slot)`。
    #[cfg(ktest)]
    pub(crate) fn committed_data_pages_of(&self, fs_id: u64, inode: u64) -> Vec<(u64, NvPageId)> {
        self.objects.collect(|index, record| {
            let key = record.key?;
            (record.committed
                && matches!(record.kind, ObjectKind::Data(_))
                && key.fs_id == fs_id
                && key.inode == inode)
                .then(|| {
                    (
                        key.page_index,
                        NvPageId::try_from(index).expect("data page index must fit NvPageId"),
                    )
                })
        })
    }

    /// 数据页的 `page_index`。
    pub(crate) fn data_page_index(&self, id: NvPageId) -> Result<u64> {
        Ok(self.data_page_key(id)?.page_index)
    }

    /// 数据页的逻辑身份。
    pub(crate) fn data_page_key(&self, id: NvPageId) -> Result<PageKey> {
        let state = self.objects.lock_checked(id.index())?;
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !matches!(record.kind, ObjectKind::Data(_)) || !record.committed {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page is not committed");
        }
        record.key.ok_or_else(|| Error::new(Errno::EIO))
    }

    pub(crate) fn persist_metadata_record<T>(
        self: &Arc<Self>,
        key: PageKey,
        record: &T,
        fault: Option<FaultPoint>,
    ) -> Result<(NvFrameId, Option<NvFrameId>)>
    where
        T: PersistentMetadataRecord,
    {
        self.persist_metadata_record_inner(key, record, fault, false, None)
    }

    /// Persists a repair record without putting repair images in the global
    /// metadata index. Pending mutation marks pass `indexed = true` because
    /// runtime completion and recovery locate them by logical key.
    pub(crate) fn persist_repair_record<T>(
        self: &Arc<Self>,
        key: PageKey,
        record: &T,
        indexed: bool,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef>
    where
        T: PersistentMetadataRecord,
    {
        record.validate_record()?;
        Self::validate_metadata_identity(key, record)?;
        if record.metadata_header().kind()? == PersistentMetadataKind::PendingExtentMutation
            && !indexed
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "pending extent mutation records must remain indexed"
            );
        }
        if size_of::<T>() > PAGE_SIZE {
            return_errno_with_message!(Errno::E2BIG, "persistent metadata record exceeds one NVBH");
        }

        let frame = self.acquire_repair_frame()?;
        let key = if indexed {
            key
        } else {
            let allocation_seq = self
                .frame_seq(frame)
                .ok_or_else(|| Error::new(Errno::ESTALE))?;
            if allocation_seq >= REPAIR_LOGICAL_INDEX_TAG {
                self.recycle_repair_frame(frame)?;
                return_errno_with_message!(
                    Errno::EOVERFLOW,
                    "repair allocation sequence exceeds key space"
                );
            }
            PageKey::new(
                key.fs_id,
                key.inode,
                REPAIR_LOGICAL_INDEX_TAG | allocation_seq,
            )
        };
        let write_result = (|| {
            let pin = self.pin_frame(frame)?;
            let mut page = [0u8; PAGE_SIZE];
            page[..size_of::<T>()].copy_from_slice(record.as_bytes());
            pin.write(&page)
        })();
        if let Err(error) = write_result {
            self.recycle_repair_frame(frame)?;
            return Err(error);
        }

        // A failed commit may already have reached persistent media. It must
        // remain allocated for recovery instead of being returned to the cache.
        self.commit_repair_metadata_frame(frame, key, indexed, fault)?;
        self.persistent_metadata_ref(frame)
    }

    /// Clears one exact repair record and returns its raw frame to the bounded
    /// workspace cache.
    ///
    /// The caller holds the inode publication right. When reclaiming an
    /// unindexed image, it must have durably reclaimed every indexed pending
    /// mark that names the image first.
    pub(crate) fn reclaim_repair_record(
        &self,
        reference: PersistentObjectRef,
        indexed: bool,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let (frame, _) = self.metadata_identity(reference)?;

        self.begin_clear(frame.index(), ObjectKind::Metadata)?;
        let result = self.persist.clear_frame(frame.index(), fault);
        let mut objects = self.objects.lock(frame.index());
        objects.records[frame.index()].pins -= 1;
        objects.records[frame.index()].persisting = false;
        self.account_unpin();
        if result.is_err() {
            return result;
        }

        let key = objects.records[frame.index()].key;
        objects.records[frame.index()].committed = false;
        objects.records[frame.index()].key = None;
        objects.records[frame.index()].version = 0;
        drop(objects);

        if indexed {
            if let Some(key) = key {
                let mut state = self.state.lock();
                if state.metadata_index.get(&key) == Some(&frame.index()) {
                    state.metadata_index.remove(&key);
                }
            }
        }
        self.recycle_repair_frame(frame)
    }

    fn persist_metadata_record_inner<T>(
        self: &Arc<Self>,
        key: PageKey,
        record: &T,
        fault: Option<FaultPoint>,
        replace: bool,
        expected_old: Option<PersistentObjectRef>,
    ) -> Result<(NvFrameId, Option<NvFrameId>)>
    where
        T: PersistentMetadataRecord,
    {
        record.validate_record()?;
        Self::validate_metadata_identity(key, record)?;
        if size_of::<T>() > PAGE_SIZE {
            return_errno_with_message!(Errno::E2BIG, "persistent metadata record exceeds one NVBH");
        }

        let frame = self.alloc_frame(PoolClass::Metadata)?;
        let write_result = (|| {
            let pin = self.pin_frame(frame)?;
            let mut page = [0u8; PAGE_SIZE];
            page[..size_of::<T>()].copy_from_slice(record.as_bytes());
            pin.write(&page)
        })();
        if let Err(error) = write_result {
            let _ = self.free_frame(frame);
            return Err(error);
        }

        match self.commit_metadata_frame(frame, key, u64::MAX, fault, replace, expected_old) {
            Ok(old) => Ok((frame, old)),
            Err(error) => {
                let _ = self.free_frame(frame);
                Err(error)
            }
        }
    }

    pub(crate) fn read_metadata_record<T>(self: &Arc<Self>, frame: NvFrameId) -> Result<T>
    where
        T: PersistentMetadataRecord,
    {
        if size_of::<T>() > PAGE_SIZE {
            return_errno_with_message!(Errno::E2BIG, "persistent metadata record exceeds one NVBH");
        }

        let pin = self.pin_frame(frame)?;
        let is_committed = {
            let state = self.objects.lock_checked(frame.index())?;
            state
                .records
                .get(frame.index())
                .is_some_and(|record| record.kind == ObjectKind::Metadata && record.committed)
        };
        if !is_committed {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent metadata record is not committed"
            );
        }

        let mut page = [0u8; PAGE_SIZE];
        pin.read(&mut page)?;
        T::decode_record(&page[..size_of::<T>()])
    }

    pub(crate) fn read_metadata_record_boxed<T>(
        self: &Arc<Self>,
        frame: NvFrameId,
    ) -> Result<BoxedMetadataRecord<T>>
    where
        T: PersistentMetadataRecord,
    {
        if size_of::<T>() > PAGE_SIZE {
            return_errno_with_message!(Errno::E2BIG, "persistent metadata record exceeds one NVBH");
        }

        let pin = self.pin_frame(frame)?;
        let is_committed = {
            let state = self.objects.lock_checked(frame.index())?;
            state
                .records
                .get(frame.index())
                .is_some_and(|record| record.kind == ObjectKind::Metadata && record.committed)
        };
        if !is_committed {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent metadata record is not committed"
            );
        }

        let mut bytes = vec![0u8; PAGE_SIZE].into_boxed_slice();
        pin.read(&mut bytes)?;
        let record =
            T::ref_from_bytes(&bytes[..size_of::<T>()]).map_err(|_| Error::new(Errno::EUCLEAN))?;
        record.validate_record()?;
        Ok(BoxedMetadataRecord {
            bytes,
            _record: PhantomData,
        })
    }

    pub(crate) fn persistent_metadata_ref(&self, frame: NvFrameId) -> Result<PersistentObjectRef> {
        let state = self.objects.lock_checked(frame.index())?;
        let record = state
            .records
            .get(frame.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Metadata || !record.committed {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent metadata reference targets an uncommitted frame"
            );
        }
        PersistentObjectRef::new(frame.index() as u64, record.alloc_seq)
    }

    pub(crate) fn persistent_data_ref(&self, page: NvPageId) -> Result<PersistentObjectRef> {
        let state = self.objects.lock_checked(page.index())?;
        let record = state
            .records
            .get(page.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !matches!(record.kind, ObjectKind::Data(_)) || !record.committed {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent data reference targets an uncommitted page"
            );
        }
        PersistentObjectRef::new(page.index() as u64, record.alloc_seq)
    }

    pub(crate) fn validate_persistent_data_ref(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<PageKey> {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let state = self.objects.lock_checked(slot)?;
        let record = state
            .records
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !matches!(record.kind, ObjectKind::Data(_))
            || !record.committed
            || record.alloc_seq != reference.allocation_seq()
        {
            return_errno_with_message!(Errno::ESTALE, "stale persistent data reference");
        }
        record
            .key
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "committed data page has no key"))
    }

    pub(crate) fn persistent_data_ssd_backing(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<u64> {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        self.validate_persistent_data_ref(reference)?;
        self.persist
            .read_entry(slot)?
            .filter(|descriptor| descriptor.allocation_seq() == reference.allocation_seq())
            .map(|descriptor| descriptor.ssd_backing_block)
            .ok_or_else(|| Error::with_message(Errno::ESTALE, "stale persistent data descriptor"))
    }

    /// The caller holds inode publication and the page's migration lease.
    /// Snapshot identity and reverse together; read descriptor bytes outside the spinlock.
    pub(crate) fn validate_migration_and_reverse(
        &self,
        reference: PersistentObjectRef,
        expected_key: PageKey,
        ssd_block: u64,
    ) -> Result<ReverseExtentRef> {
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let locator = {
            let started = read_ns();
            let state = self.objects.lock_checked(slot)?;
            LATENCY_STATS.migration_slot_wait.record_since(started);
            let record = &state.records[slot];
            if !matches!(record.kind, ObjectKind::Data(_))
                || !record.committed
                || record.alloc_seq != reference.allocation_seq()
                || record.key != Some(expected_key)
            {
                return_errno_with_message!(Errno::ESTALE, "rTIO migration identity changed");
            }
            state.reverse.ok_or_else(|| Error::new(Errno::ESTALE))?
        };
        let descriptor = self
            .persist
            .read_entry(slot)?
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        if ssd_block == 0
            || descriptor.allocation_seq() != reference.allocation_seq()
            || descriptor.ssd_backing_block != ssd_block
        {
            return_errno_with_message!(Errno::ESTALE, "rTIO migration target changed");
        }
        Ok(locator)
    }

    pub(crate) fn persistent_data_has_migration_target(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<bool> {
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EINVAL))?;
        self.validate_persistent_data_ref(reference)?;
        self.persist
            .read_entry(slot)?
            .filter(|descriptor| descriptor.allocation_seq() == reference.allocation_seq())
            .map(|descriptor| descriptor.has_migration_target())
            .ok_or_else(|| Error::with_message(Errno::ESTALE, "stale persistent data descriptor"))
    }

    pub(crate) fn persistent_data_migration_credit_released(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<bool> {
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EINVAL))?;
        self.validate_persistent_data_ref(reference)?;
        self.persist
            .read_entry(slot)?
            .filter(|descriptor| descriptor.allocation_seq() == reference.allocation_seq())
            .map(|descriptor| descriptor.migration_credit_released())
            .ok_or_else(|| Error::with_message(Errno::ESTALE, "stale persistent data descriptor"))
    }

    pub(crate) fn mark_migration_credit_released(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<()> {
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EINVAL))?;
        let mut descriptor = self
            .persist
            .read_entry(slot)?
            .filter(|descriptor| descriptor.allocation_seq() == reference.allocation_seq())
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        descriptor.mark_migration_credit_released()?;
        self.persist.commit_frame(slot, &descriptor, None)
    }

    fn persist_migration_target(&self, reference: PersistentObjectRef, target: u64) -> Result<()> {
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EINVAL))?;
        {
            let state = self.objects.lock_checked(slot)?;
            let record = state
                .records
                .get(slot)
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if record.kind != ObjectKind::Data(DataPageState::InFlight)
                || !record.committed
                || !record.persisting
                || record.alloc_seq != reference.allocation_seq()
            {
                return_errno_with_message!(Errno::ESTALE, "migration lease is stale");
            }
        }
        let mut descriptor = self
            .persist
            .read_entry(slot)?
            .filter(|descriptor| descriptor.allocation_seq() == reference.allocation_seq())
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        descriptor.assign_migration_target(target)?;
        self.persist.commit_frame(slot, &descriptor, None)?;
        let key = self.validate_persistent_data_ref(reference)?;
        let mut state = self.state.lock();
        if let Some(entry) = state.index.lookup_mut(key) {
            entry.ssd_backing_block = target;
            entry.state = NvObjectState::NvWriteback;
        }
        Ok(())
    }

    fn clear_persistent_migration_target(&self, reference: PersistentObjectRef) -> Result<()> {
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EINVAL))?;
        let key = self.validate_persistent_data_ref(reference)?;
        let mut descriptor = self
            .persist
            .read_entry(slot)?
            .filter(|descriptor| descriptor.allocation_seq() == reference.allocation_seq())
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        descriptor.clear_migration_target()?;
        self.persist.commit_frame(slot, &descriptor, None)?;
        let mut state = self.state.lock();
        if let Some(entry) = state.index.lookup_mut(key) {
            entry.ssd_backing_block = 0;
            entry.state = NvObjectState::NvDirty;
        }
        Ok(())
    }

    pub(super) fn recover_data_index_entry(&self, reference: PersistentObjectRef) -> Result<()> {
        let key = self.validate_persistent_data_ref(reference)?;
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let page = NvPageId::try_from(slot).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let descriptor = self
            .persist
            .read_entry(slot)?
            .filter(|descriptor| descriptor.allocation_seq() == reference.allocation_seq())
            .ok_or_else(|| {
                Error::with_message(Errno::ESTALE, "stale persistent data descriptor")
            })?;
        self.state.lock().index.insert(
            key,
            PageEntry {
                residency: Residency::Nv,
                handle: Some(RuntimeHandle::Nv(page)),
                ssd_backing_block: descriptor.ssd_backing_block,
                state: NvObjectState::NvDirty,
            },
        );
        Ok(())
    }

    pub(super) fn committed_metadata_refs(&self) -> Vec<PersistentObjectRef> {
        self.objects.collect(|index, record| {
            (record.kind == ObjectKind::Metadata && record.committed)
                .then(|| PersistentObjectRef::new(index as u64, record.alloc_seq).ok())
                .flatten()
        })
    }

    pub(super) fn persistent_metadata_kind(
        self: &Arc<Self>,
        reference: PersistentObjectRef,
    ) -> Result<PersistentMetadataKind> {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let frame = NvFrameId::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        if self.persistent_metadata_ref(frame)? != reference {
            return_errno_with_message!(Errno::ESTALE, "stale persistent metadata reference");
        }
        let pin = self.pin_frame(frame)?;
        let mut page = [0u8; PAGE_SIZE];
        pin.read(&mut page)?;
        PersistentMetadataHeader::decode_kind(&page[..size_of::<PersistentMetadataHeader>()])
    }

    fn extent_references_data(self: &Arc<Self>, target: PersistentObjectRef) -> Result<bool> {
        for reference in self.committed_metadata_refs() {
            if self.persistent_metadata_kind(reference)? != PersistentMetadataKind::ExtentNode {
                continue;
            }
            let node = self.read_authoritative_metadata::<ExtentNodeRecord>(reference)?;
            if node.level() == 0
                && node.entries()?.iter().any(|entry| {
                    matches!(entry.data_ref(), Ok(ExtentDataRef::Nv(reference)) if reference == target)
                })
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn persistent_metadata_key(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<PageKey> {
        self.metadata_identity(reference).map(|(_, key)| key)
    }

    /// Clears and reclaims the exact committed metadata generation.
    ///
    /// The caller must hold the object's publication right so the validated
    /// slot cannot be concurrently reclaimed and reused between validation and
    /// the durable clear.
    pub(crate) fn reclaim_committed_metadata(&self, reference: PersistentObjectRef) -> Result<()> {
        let (frame, _) = self.metadata_identity(reference)?;
        self.clear_frame(frame, None)?;
        self.free_frame(frame)
    }

    pub(crate) fn current_metadata_ref(&self, key: PageKey) -> Option<PersistentObjectRef> {
        let profiling = LATENCY_ENABLED
            && key.page_index == super::write_transaction::SPACE_RESERVATION_LOGICAL_INDEX;
        let started = read_ns();
        let state = self.state.lock();
        if profiling {
            LATENCY_STATS
                .reservation_index_wait_all
                .record_since(started);
        }
        let index = *state.metadata_index.get(&key)?;
        let started = read_ns();
        let objects = self.objects.lock(index);
        if profiling {
            LATENCY_STATS
                .reservation_slot_wait_all
                .record_since(started);
        }
        let record = objects.records.get(index)?;
        (record.kind == ObjectKind::Metadata && record.committed && record.key == Some(key))
            .then(|| PersistentObjectRef::new(index as u64, record.alloc_seq).ok())
            .flatten()
    }

    /// Returns the stable NV identity of an inode's fixed persistent root.
    ///
    /// A fixed inode is allocated once and subsequently updated in place. More
    /// than one committed descriptor for the fixed key is therefore corruption,
    /// rather than a version-selection problem.
    pub(crate) fn fixed_inode_ref(
        self: &Arc<Self>,
        fs_id: u64,
        inode: u64,
    ) -> Result<Option<PersistentObjectRef>> {
        let key = PageKey::new(fs_id, inode, FIXED_INODE_LOGICAL_INDEX);
        let found = {
            let state = self.state.lock();
            state
                .metadata_index
                .get(&key)
                .copied()
                .map(|index| {
                    let record = self
                        .objects
                        .get(index)
                        .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                    PersistentObjectRef::new(index as u64, record.alloc_seq)
                })
                .transpose()?
        };
        let Some(reference) = found else {
            return Ok(None);
        };
        let record = self
            .read_authoritative_metadata::<InodeRecord>(reference)
            .map_err(|_| Error::new(Errno::EUCLEAN))?;
        if record.header().fs_id() != fs_id || record.header().inode() != inode {
            return_errno_with_message!(Errno::EUCLEAN, "fixed inode root identity mismatch");
        }
        Ok(Some(reference))
    }

    /// Creates the only persistent inode root for `(fs_id, inode)`.
    pub(crate) fn create_fixed_inode(
        self: &Arc<Self>,
        record: &InodeRecord,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        let header = record.header();
        let key = PageKey::new(header.fs_id(), header.inode(), FIXED_INODE_LOGICAL_INDEX);
        if self
            .fixed_inode_ref(header.fs_id(), header.inode())?
            .is_some()
        {
            return_errno_with_message!(Errno::EEXIST, "fixed inode root already exists");
        }
        self.persist_authoritative_metadata(key, record, fault)
    }

    /// Persists a new image into an inode's existing NVBH without changing its
    /// generation-checked object identity.
    pub(crate) fn update_fixed_inode(
        self: &Arc<Self>,
        publication: &PublicationGuard,
        current: PersistentObjectRef,
        record: &InodeRecord,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        let header = record.header();
        if !Arc::ptr_eq(&publication.pool, self)
            || publication.fs_id != header.fs_id()
            || publication.inode != header.inode()
        {
            return_errno_with_message!(Errno::EPERM, "fixed inode update lacks publication right");
        }
        if self.fixed_inode_ref(header.fs_id(), header.inode())? != Some(current) {
            return_errno_with_message!(Errno::ESTALE, "fixed inode identity mismatch");
        }
        let (frame, key) = self.metadata_identity(current)?;
        Self::validate_metadata_identity(key, record)?;
        self.begin_exclusive_metadata_update(frame, current.allocation_seq())?;

        let mut old_page = vec![0u8; PAGE_SIZE].into_boxed_slice();
        let mut new_page = vec![0u8; PAGE_SIZE].into_boxed_slice();
        let mut old_page_loaded = false;
        let mut persisted_stage = 0u8;
        let result = (|| {
            self.backend.read_page(self.page_id(frame), &mut old_page)?;
            old_page_loaded = true;
            let old = InodeRecord::decode(&old_page[..size_of::<InodeRecord>()])?;
            if record.header().generation() <= old.header().generation() {
                return_errno_with_message!(Errno::ESTALE, "inode generation did not advance");
            }
            if record.extent_root() != old.extent_root()
                || record.mode() != old.mode()
                || record.flags() != old.flags()
            {
                return_errno_with_message!(
                    Errno::EOPNOTSUPP,
                    "inode topology update requires a durable mutation protocol"
                );
            }

            new_page.copy_from_slice(&old_page);
            new_page[..size_of::<InodeRecord>()].copy_from_slice(record.as_bytes());
            inject(fault, FaultPoint::BeforeContentPersist)?;
            self.persist_page_u64(frame, INODE_BLOCK_COUNT_OFFSET, record.block_count())?;
            persisted_stage = 1;
            inject(fault, FaultPoint::AfterDependencyPersist)?;
            self.persist_page_u64(frame, INODE_FILE_SIZE_OFFSET, record.file_size())?;
            persisted_stage = 2;
            inject(fault, FaultPoint::AfterVisibilityPersist)?;
            self.persist_page_u64(frame, INODE_GENERATION_OFFSET, record.header().generation())?;
            persisted_stage = 3;
            inject(fault, FaultPoint::AfterContentPersist)
        })();

        if result.is_err() && old_page_loaded {
            for (stage, offset) in [
                (1, INODE_BLOCK_COUNT_OFFSET),
                (2, INODE_FILE_SIZE_OFFSET),
                (3, INODE_GENERATION_OFFSET),
            ] {
                if persisted_stage >= stage {
                    old_page[offset..offset + size_of::<u64>()]
                        .copy_from_slice(&new_page[offset..offset + size_of::<u64>()]);
                }
            }
            let _ = self.backend.write_page(self.page_id(frame), &old_page);
        }
        let mut state = self.objects.lock(frame.index());
        state.records[frame.index()].pins -= 1;
        state.records[frame.index()].persisting = false;
        if persisted_stage == 3 {
            state.records[frame.index()].version = record.header().generation();
        }
        self.account_unpin();
        drop(state);
        note_metadata_pin_change();
        result.map(|_| current)
    }

    /// Switches only the fixed inode's extent-root reference while a durable
    /// root-split mark owns recovery of every intermediate reference image.
    pub(crate) fn publish_fixed_inode_root(
        self: &Arc<Self>,
        publication: &PublicationGuard,
        current: PersistentObjectRef,
        expected_root: PersistentObjectRef,
        new_root: PersistentObjectRef,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        if !Arc::ptr_eq(&publication.pool, self) {
            return_errno_with_message!(Errno::EPERM, "inode root update lacks publication right");
        }
        let (frame, key) = self.metadata_identity(current)?;
        if publication.fs_id != key.fs_id || publication.inode != key.inode {
            return_errno_with_message!(Errno::EPERM, "inode root update lacks publication right");
        }
        self.read_authoritative_metadata_boxed::<ExtentNodeRecord>(new_root)?;
        {
            let mut state = self.objects.lock_checked(frame.index())?;
            let object = state
                .records
                .get(frame.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if object.kind != ObjectKind::Metadata
                || !object.committed
                || object.alloc_seq != current.allocation_seq()
            {
                return_errno_with_message!(Errno::ESTALE, "fixed inode object was replaced");
            }
            if object.pins != 0 || object.persisting {
                return_errno_with_message!(Errno::EBUSY, "fixed inode object is busy");
            }
            self.account_pin()?;
            state.records[frame.index()].pins = 1;
            state.records[frame.index()].persisting = true;
        }

        let result = (|| {
            let inode = self.read_metadata_page_unchecked::<InodeRecord>(frame)?;
            if inode.header().fs_id() != key.fs_id
                || inode.header().inode() != key.inode
                || inode.extent_root() != expected_root
            {
                return_errno_with_message!(Errno::ESTALE, "fixed inode root was replaced");
            }
            let slot_plus_one = new_root
                .slot()
                .and_then(|slot| slot.checked_add(1))
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            inject(fault, FaultPoint::BeforeContentPersist)?;
            self.persist_page_u64(
                frame,
                INODE_EXTENT_ROOT_OFFSET + size_of::<u64>(),
                new_root.allocation_seq(),
            )?;
            inject(fault, FaultPoint::AfterDependencyPersist)?;
            self.persist_page_u64(frame, INODE_EXTENT_ROOT_OFFSET, slot_plus_one)?;
            inject(fault, FaultPoint::AfterVisibilityPersist)?;
            inject(fault, FaultPoint::AfterContentPersist)
        })();

        let mut state = self.objects.lock(frame.index());
        state.records[frame.index()].pins = 0;
        state.records[frame.index()].persisting = false;
        self.account_unpin();
        result.map(|_| current)
    }

    /// Rolls a marked root split forward after a crash, including the
    /// dependency-persisted image where the new allocation sequence and old
    /// slot coexist temporarily in the fixed inode.
    pub(crate) fn repair_fixed_inode_root(
        self: &Arc<Self>,
        publication: &PublicationGuard,
        current: PersistentObjectRef,
        old_root: PersistentObjectRef,
        new_root: PersistentObjectRef,
    ) -> Result<PersistentObjectRef> {
        if !Arc::ptr_eq(&publication.pool, self) {
            return_errno_with_message!(Errno::EPERM, "inode root repair lacks publication right");
        }
        let (frame, key) = self.metadata_identity(current)?;
        if publication.fs_id != key.fs_id || publication.inode != key.inode {
            return_errno_with_message!(Errno::EPERM, "inode root repair lacks publication right");
        }
        self.read_authoritative_metadata_boxed::<ExtentNodeRecord>(old_root)?;
        self.read_authoritative_metadata_boxed::<ExtentNodeRecord>(new_root)?;

        {
            let mut state = self.objects.lock_checked(frame.index())?;
            let object = state
                .records
                .get(frame.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if object.kind != ObjectKind::Metadata
                || !object.committed
                || object.alloc_seq != current.allocation_seq()
            {
                return_errno_with_message!(Errno::ESTALE, "fixed inode object was replaced");
            }
            if object.pins != 0 || object.persisting {
                return_errno_with_message!(Errno::EBUSY, "fixed inode object is busy");
            }
            self.account_pin()?;
            state.records[frame.index()].pins = 1;
            state.records[frame.index()].persisting = true;
        }

        let result = (|| {
            let mut page = vec![0u8; PAGE_SIZE].into_boxed_slice();
            self.backend.read_page(self.page_id(frame), &mut page)?;
            let header = PersistentMetadataHeader::ref_from_bytes(
                &page[..size_of::<PersistentMetadataHeader>()],
            )
            .map_err(|_| Error::new(Errno::EUCLEAN))?;
            if header.kind()? != PersistentMetadataKind::Inode
                || header.fs_id() != key.fs_id
                || header.inode() != key.inode
            {
                return_errno_with_message!(Errno::EUCLEAN, "root-split inode identity is corrupt");
            }

            let read_u64 = |offset: usize| {
                u64::from_ne_bytes(
                    page[offset..offset + size_of::<u64>()]
                        .try_into()
                        .expect("u64 field has a fixed width"),
                )
            };
            let slot = read_u64(INODE_EXTENT_ROOT_OFFSET);
            let allocation_seq = read_u64(INODE_EXTENT_ROOT_OFFSET + size_of::<u64>());
            let old_slot = old_root
                .slot()
                .and_then(|slot| slot.checked_add(1))
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            let new_slot = new_root
                .slot()
                .and_then(|slot| slot.checked_add(1))
                .ok_or_else(|| Error::new(Errno::EINVAL))?;

            match (slot, allocation_seq) {
                pair if pair == (new_slot, new_root.allocation_seq()) => {}
                pair if pair == (old_slot, old_root.allocation_seq()) => {
                    self.persist_page_u64(
                        frame,
                        INODE_EXTENT_ROOT_OFFSET + size_of::<u64>(),
                        new_root.allocation_seq(),
                    )?;
                    self.persist_page_u64(frame, INODE_EXTENT_ROOT_OFFSET, new_slot)?;
                }
                pair if pair == (old_slot, new_root.allocation_seq()) => {
                    self.persist_page_u64(frame, INODE_EXTENT_ROOT_OFFSET, new_slot)?;
                }
                _ => {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "fixed inode root is outside the marked split transition"
                    );
                }
            }

            let repaired = self.read_metadata_page_unchecked::<InodeRecord>(frame)?;
            if repaired.extent_root() != new_root {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "fixed inode root repair did not commit"
                );
            }
            Ok(())
        })();

        let mut state = self.objects.lock(frame.index());
        state.records[frame.index()].pins = 0;
        state.records[frame.index()].persisting = false;
        self.account_unpin();
        result.map(|_| current)
    }

    fn read_metadata_page_unchecked<T>(&self, frame: NvFrameId) -> Result<T>
    where
        T: PersistentMetadataRecord,
    {
        let mut page = vec![0u8; PAGE_SIZE].into_boxed_slice();
        self.backend.read_page(self.page_id(frame), &mut page)?;
        T::decode_record(&page[..size_of::<T>()])
    }

    fn persist_page_u64(&self, frame: NvFrameId, offset: usize, value: u64) -> Result<()> {
        self.backend
            .store_page_u64(self.page_id(frame), offset, value)?;
        let absolute = frame
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.backend.persist(absolute, size_of::<u64>())?;
        self.backend.fence()
    }

    /// Atomically advances a pending extent mutation mark by persisting only
    /// its aligned state word. The publication right serializes this update
    /// with every other mutation of the owning inode.
    pub(crate) fn mark_pending_extent_mutation_phase(
        self: &Arc<Self>,
        publication: &PublicationGuard,
        current: PersistentObjectRef,
        expected: ExtentMutationPhase,
        next: ExtentMutationPhase,
    ) -> Result<PersistentObjectRef> {
        let (frame, key) = self.metadata_identity(current)?;
        if !Arc::ptr_eq(&publication.pool, self)
            || publication.fs_id != key.fs_id
            || publication.inode != key.inode
        {
            return_errno_with_message!(
                Errno::EPERM,
                "extent mutation mark lacks the inode publication right"
            );
        }

        {
            let mut state = self.objects.lock_checked(frame.index())?;
            let object = state
                .records
                .get(frame.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if object.kind != ObjectKind::Metadata
                || !object.committed
                || object.alloc_seq != current.allocation_seq()
            {
                return_errno_with_message!(Errno::ESTALE, "extent mutation mark was replaced");
            }
            if object.pins != 0 || object.persisting {
                return_errno_with_message!(Errno::EBUSY, "extent mutation mark is busy");
            }
            self.account_pin()?;
            state.records[frame.index()].pins = 1;
            state.records[frame.index()].persisting = true;
        }

        let result = (|| {
            let mut page = [0u8; PAGE_SIZE];
            self.backend.read_page(self.page_id(frame), &mut page)?;
            let mark = PendingExtentMutationRecord::decode(
                &page[..size_of::<PendingExtentMutationRecord>()],
            )?;
            if mark.header().fs_id() != key.fs_id
                || mark.header().inode() != key.inode
                || mark.phase()? != expected
            {
                return_errno_with_message!(Errno::ESTALE, "extent mutation mark changed");
            }
            let state_word = (mark.state_word() & !0xff) | next as u64;
            self.persist_page_u64(frame, PENDING_EXTENT_MUTATION_STATE_OFFSET, state_word)
        })();

        let mut state = self.objects.lock(frame.index());
        state.records[frame.index()].pins = 0;
        state.records[frame.index()].persisting = false;
        self.account_unpin();
        result.map(|_| current)
    }

    /// Restores a stable metadata target from a committed typed repair image.
    ///
    /// The target's descriptor identity is checked before its bytes are
    /// overwritten, but its current payload is intentionally never decoded:
    /// recovery must also repair a torn or otherwise undecodable target image.
    pub(crate) fn repair_metadata_from_image<T>(
        self: &Arc<Self>,
        publication: &PublicationGuard,
        repair: PersistentObjectRef,
        target: PersistentObjectRef,
    ) -> Result<PersistentObjectRef>
    where
        T: PersistentMetadataRecord,
    {
        let (repair_frame, _) = self.metadata_identity(repair)?;
        let repair_image = self.read_metadata_record_boxed::<T>(repair_frame)?;
        let (target_frame, target_key) = self.metadata_identity(target)?;
        if !Arc::ptr_eq(&publication.pool, self)
            || publication.fs_id != target_key.fs_id
            || publication.inode != target_key.inode
        {
            return_errno_with_message!(
                Errno::EPERM,
                "metadata repair lacks the inode publication right"
            );
        }
        if repair_image.metadata_header().fs_id() != target_key.fs_id
            || repair_image.metadata_header().inode() != target_key.inode
        {
            return_errno_with_message!(Errno::EINVAL, "repair image and target identities differ");
        }
        let descriptor = self
            .persist
            .read_entry(target_frame.index())?
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        if descriptor.kind()? != NvObjectKind::Nvbh
            || descriptor.fs_id != target_key.fs_id
            || descriptor.inode != target_key.inode
            || descriptor.logical_index != target_key.page_index
            || descriptor.ssd_backing_block != target_frame.index() as u64
            || descriptor.allocation_seq() != target.allocation_seq()
        {
            return_errno_with_message!(Errno::ESTALE, "repair target descriptor changed");
        }

        self.begin_exclusive_metadata_update(target_frame, target.allocation_seq())?;

        let result = (|| {
            self.backend
                .write_page(self.page_id(target_frame), &repair_image.bytes)?;
            let absolute = target_frame
                .index()
                .checked_mul(PAGE_SIZE)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            self.backend.persist(absolute, PAGE_SIZE)?;
            self.backend.fence()
        })();

        let mut state = self.objects.lock(target_frame.index());
        state.records[target_frame.index()].pins -= 1;
        state.records[target_frame.index()].persisting = false;
        if result.is_ok() {
            state.records[target_frame.index()].version =
                repair_image.metadata_header().generation();
        }
        self.account_unpin();
        drop(state);
        note_metadata_pin_change();
        result.map(|_| target)
    }

    /// Updates a typed metadata object in its existing NVBH.
    ///
    /// The descriptor, slot, and allocation sequence remain unchanged. The
    /// caller must serialize semantic field ordering; this primitive only
    /// provides exclusive access and the content-persist/fence boundary.
    pub(crate) fn update_authoritative_metadata_in_place<T>(
        self: &Arc<Self>,
        publication: &PublicationGuard,
        current: PersistentObjectRef,
        record: &T,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef>
    where
        T: AuthoritativeMetadataRecord,
    {
        record.validate_record()?;
        let (frame, key) = self.metadata_identity(current)?;
        Self::validate_metadata_identity(key, record)?;
        if !Arc::ptr_eq(&publication.pool, self)
            || publication.fs_id != key.fs_id
            || publication.inode != key.inode
        {
            return_errno_with_message!(
                Errno::EPERM,
                "metadata update lacks the inode publication right"
            );
        }
        if size_of::<T>() > PAGE_SIZE {
            return_errno_with_message!(Errno::E2BIG, "persistent metadata record exceeds one NVBH");
        }

        let started = read_ns();
        self.begin_exclusive_metadata_update(frame, current.allocation_seq())?;
        if LATENCY_ENABLED && publication.role == PublicationRole::Completion {
            if matches!(
                record.metadata_header().kind(),
                Ok(PersistentMetadataKind::SpaceReservation)
            ) {
                LATENCY_STATS
                    .migration_reservation_exclusive
                    .record_since(started);
            } else if matches!(
                record.metadata_header().kind(),
                Ok(PersistentMetadataKind::ExtentNode)
            ) {
                LATENCY_STATS.migration_leaf_exclusive.record_since(started);
            }
        }

        let mut old_page = vec![0u8; PAGE_SIZE].into_boxed_slice();
        let mut new_page = vec![0u8; PAGE_SIZE].into_boxed_slice();
        let mut old_page_loaded = false;
        let mut content_persisted = false;
        let result = (|| {
            self.backend.read_page(self.page_id(frame), &mut old_page)?;
            old_page_loaded = true;
            let old_record = T::ref_from_bytes(&old_page[..size_of::<T>()])
                .map_err(|_| Error::new(Errno::EUCLEAN))?;
            old_record.validate_record()?;
            if record.metadata_header().generation() <= old_record.metadata_header().generation() {
                return_errno_with_message!(Errno::ESTALE, "metadata generation did not advance");
            }
            new_page[..size_of::<T>()].copy_from_slice(record.as_bytes());
            inject(fault, FaultPoint::BeforeContentPersist)?;
            self.backend.write_page(self.page_id(frame), &new_page)?;
            self.backend.persist(frame.index() * PAGE_SIZE, PAGE_SIZE)?;
            self.backend.fence()?;
            content_persisted = true;
            inject(fault, FaultPoint::AfterContentPersist)
        })();

        if result.is_err() && old_page_loaded && !content_persisted {
            let _ = self.backend.write_page(self.page_id(frame), &old_page);
        }
        let mut state = self.objects.lock(frame.index());
        state.records[frame.index()].pins -= 1;
        state.records[frame.index()].persisting = false;
        if content_persisted {
            state.records[frame.index()].version = record.metadata_header().generation();
        }
        self.account_unpin();
        drop(state);
        note_metadata_pin_change();
        result.map(|_| current)
    }

    pub(crate) fn read_metadata_by_ref<T>(
        self: &Arc<Self>,
        reference: PersistentObjectRef,
    ) -> Result<T>
    where
        T: PersistentMetadataRecord,
    {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let frame = NvFrameId::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        if self.persistent_metadata_ref(frame)? != reference {
            return_errno_with_message!(Errno::EINVAL, "stale persistent metadata reference");
        }
        self.read_metadata_record(frame)
    }

    /// Persists the first version of an authoritative EXT4 metadata object and
    /// returns its generation-checked NV identity.
    pub(crate) fn persist_authoritative_metadata<T>(
        self: &Arc<Self>,
        key: PageKey,
        record: &T,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef>
    where
        T: AuthoritativeMetadataRecord,
    {
        let (frame, _) = self.persist_metadata_record_inner(key, record, fault, false, None)?;
        self.persistent_metadata_ref(frame)
    }

    /// CoW-replaces exactly `current`; a reference to any older generation is
    /// rejected with `ESTALE` instead of silently replacing the latest object.
    pub(crate) fn replace_authoritative_metadata<T>(
        self: &Arc<Self>,
        current: PersistentObjectRef,
        record: &T,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef>
    where
        T: AuthoritativeMetadataRecord,
    {
        let (_, key) = self.metadata_identity(current)?;
        let _ = self.read_authoritative_metadata::<T>(current)?;
        let (frame, _) =
            self.persist_metadata_record_inner(key, record, fault, true, Some(current))?;
        self.persistent_metadata_ref(frame)
    }

    /// Reads an authoritative object by stable NV identity, never by logical
    /// key or a cached `NvFrameId`.
    pub(crate) fn read_authoritative_metadata<T>(
        self: &Arc<Self>,
        reference: PersistentObjectRef,
    ) -> Result<T>
    where
        T: AuthoritativeMetadataRecord,
    {
        let (frame, _) = self.metadata_identity(reference)?;
        self.read_metadata_record(frame)
    }

    pub(crate) fn read_authoritative_metadata_boxed<T>(
        self: &Arc<Self>,
        reference: PersistentObjectRef,
    ) -> Result<BoxedMetadataRecord<T>>
    where
        T: AuthoritativeMetadataRecord,
    {
        let (frame, _) = self.metadata_identity(reference)?;
        self.read_metadata_record_boxed(frame)
    }

    fn validate_metadata_identity<T>(key: PageKey, record: &T) -> Result<()>
    where
        T: PersistentMetadataRecord,
    {
        let header = record.metadata_header();
        if header.fs_id() != key.fs_id || header.inode() != key.inode {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent metadata identity does not match its object key"
            );
        }
        Ok(())
    }

    fn metadata_identity(&self, reference: PersistentObjectRef) -> Result<(NvFrameId, PageKey)> {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let slot = usize::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let frame = NvFrameId::try_from(slot).map_err(|_| Error::new(Errno::EINVAL))?;
        let state = self.objects.lock_checked(slot)?;
        let record = state
            .records
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Metadata
            || !record.committed
            || record.alloc_seq != reference.allocation_seq()
        {
            return_errno_with_message!(Errno::ESTALE, "stale persistent metadata reference");
        }
        let key = record.key.ok_or_else(|| Error::new(Errno::EIO))?;
        Ok((frame, key))
    }

    pub(crate) fn commit_frame(
        &self,
        id: NvFrameId,
        home_block: u64,
        slot_bitmap: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.commit_metadata_frame(
            id,
            PageKey::new(0, 0, home_block),
            slot_bitmap,
            fault,
            false,
            None,
        )
        .map(|_| ())
    }

    pub(crate) fn replace_metadata_frame(
        &self,
        id: NvFrameId,
        key: PageKey,
        slot_bitmap: u64,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvFrameId>> {
        self.commit_metadata_frame(id, key, slot_bitmap, fault, true, None)
    }

    /// 把旧的 `(key, version)` 折成**自描述描述符**（T2.2a）。
    ///
    /// 映射是双射、不丢信息：`namespace[16]` ↔ `(fs_id, inode)`（两个 LE u64）、
    /// `home_block` ↔ `logical_index`、`version` ↔ `recovery_id`；而 `ssd_backing_block`
    /// 直接取对象所在的槽位号——对象自此自带物理地址，不再依赖外部全局索引。
    fn descriptor_for(
        &self,
        class: PoolClass,
        key: PageKey,
        version: u64,
        slot: usize,
        ssd_backing_block: Option<u64>,
    ) -> Result<NvDescriptor> {
        let allocation_seq = self
            .objects
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?
            .alloc_seq;
        let (fs_id, inode) = (key.fs_id, key.inode);
        let descriptor = match class {
            PoolClass::Metadata => NvDescriptor::for_nvbh(
                fs_id,
                inode,
                key.page_index,
                BLOCK_SIZE as u32,
                slot as u64,
                version,
            ),
            PoolClass::Data => Ok(NvDescriptor::for_nv_page(
                fs_id,
                inode,
                key.page_index,
                ssd_backing_block.unwrap_or(slot as u64),
                version,
            )),
            PoolClass::Wal => {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "WAL pages are DRAM-only and are never persisted"
                )
            }
        }?;
        descriptor.with_allocation_seq(allocation_seq)
    }

    fn commit_metadata_frame(
        &self,
        id: NvFrameId,
        key: PageKey,
        _slot_bitmap: u64,
        fault: Option<FaultPoint>,
        replace: bool,
        expected_old: Option<PersistentObjectRef>,
    ) -> Result<Option<NvFrameId>> {
        let (version, old) =
            self.begin_commit(id.index(), ObjectKind::Metadata, key, replace, expected_old)?;
        let descriptor =
            self.descriptor_for(PoolClass::Metadata, key, version, id.index(), None)?;
        let result = self.persist.commit_frame(id.index(), &descriptor, fault);
        let mut state = self.state.lock();
        let mut objects = self.objects.lock(id.index());
        objects.records[id.index()].pins -= 1;
        objects.records[id.index()].persisting = false;
        self.account_unpin();
        if result.is_ok() {
            objects.records[id.index()].committed = true;
            objects.records[id.index()].key = Some(key);
            objects.records[id.index()].version = version;
            state.metadata_index.insert(key, id.index());
        }
        result.map(|_| old.and_then(|index| NvFrameId::try_from(index).ok()))
    }

    fn commit_repair_metadata_frame(
        &self,
        frame: NvFrameId,
        key: PageKey,
        indexed: bool,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        if indexed {
            return self
                .commit_metadata_frame(frame, key, u64::MAX, fault, false, None)
                .map(|_| ());
        }

        {
            let mut objects = self.objects.lock_checked(frame.index())?;
            if objects.records[frame.index()].committed {
                return_errno_with_message!(Errno::EBUSY, "RNVPC repair frame is already committed");
            }
            self.pin_persistent_update(&mut objects, frame.index(), ObjectKind::Metadata)?;
        }
        let result = self
            .descriptor_for(PoolClass::Metadata, key, 1, frame.index(), None)
            .and_then(|descriptor| self.persist.commit_frame(frame.index(), &descriptor, fault));
        let mut objects = self.objects.lock(frame.index());
        objects.records[frame.index()].pins -= 1;
        objects.records[frame.index()].persisting = false;
        self.account_unpin();
        if result.is_ok() {
            objects.records[frame.index()].committed = true;
            objects.records[frame.index()].key = Some(key);
            objects.records[frame.index()].version = 1;
        }
        result
    }

    pub(crate) fn commit_data_page(
        &self,
        id: NvPageId,
        home_block: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.commit_data_page_inner(id, home_block, fault, false)
            .map(|_| ())
    }

    pub(crate) fn replace_data_page(
        &self,
        id: NvPageId,
        home_block: u64,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvPageId>> {
        self.commit_data_page_inner(id, home_block, fault, true)
    }

    /// 按调用方给定的逻辑身份提交一个数据页；索引条目随之指向新版本。
    pub(crate) fn commit_data_page_in_owner(
        &self,
        id: NvPageId,
        key: PageKey,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvPageId>> {
        {
            let mut objects = self.objects.lock(id.index());
            self.pin_persistent_update(
                &mut objects,
                id.index(),
                ObjectKind::Data(DataPageState::NvDirty),
            )?;
        }
        let descriptor = self.descriptor_for(PoolClass::Data, key, 1, id.index(), None)?;
        let result = self.persist.commit_frame(id.index(), &descriptor, fault);
        let mut state = self.state.lock();
        let mut objects = self.objects.lock(id.index());
        objects.records[id.index()].pins -= 1;
        objects.records[id.index()].persisting = false;
        self.account_unpin();
        if result.is_ok() {
            objects.records[id.index()].committed = true;
            objects.records[id.index()].key = Some(key);
            objects.records[id.index()].version = 1;
            state.index.insert(
                key,
                PageEntry {
                    residency: Residency::Nv,
                    handle: Some(RuntimeHandle::Nv(id)),
                    ssd_backing_block: id.index() as u64,
                    state: NvObjectState::NvDirty,
                },
            );
        }
        result.map(|_| None)
    }

    /// 按调用方给定的逻辑身份**替换**该 key 的已提交旧版本。
    ///
    /// 与 [`commit_data_page_in_owner`](Self::commit_data_page_in_owner) 的唯一差别是
    /// 这里走 `replace` 语义：同 key 的已提交旧对象被找出来并作为返回值交给调用方，
    /// 由调用方按池的淘汰协议回收（否则同 key 会同时存在两个已提交对象，
    /// 既让 `committed_data_page(key)` 产生歧义，也会永久占用池容量）。
    /// I5 的 rNVPCe soft-update 写路径需要它（rNVPC 走的是 `replace_data_page` 的
    /// `home_block` 编址形式）。
    pub(crate) fn replace_data_page_in_owner(
        &self,
        id: NvPageId,
        key: PageKey,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvPageId>> {
        self.commit_data_page_with_key(id, key, fault, true, None)
    }

    pub(crate) fn replace_data_page_in_owner_at(
        &self,
        id: NvPageId,
        key: PageKey,
        ssd_backing_block: u64,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvPageId>> {
        self.commit_data_page_with_key(id, key, fault, true, Some(ssd_backing_block))
    }

    fn commit_data_page_inner(
        &self,
        id: NvPageId,
        home_block: u64,
        fault: Option<FaultPoint>,
        replace: bool,
    ) -> Result<Option<NvPageId>> {
        self.commit_data_page_with_key(id, PageKey::new(0, 0, home_block), fault, replace, None)
    }

    fn commit_data_page_with_key(
        &self,
        id: NvPageId,
        key: PageKey,
        fault: Option<FaultPoint>,
        replace: bool,
        ssd_backing_block: Option<u64>,
    ) -> Result<Option<NvPageId>> {
        let (version, old) = self.begin_commit(
            id.index(),
            ObjectKind::Data(DataPageState::NvDirty),
            key,
            replace,
            None,
        )?;
        let descriptor =
            self.descriptor_for(PoolClass::Data, key, version, id.index(), ssd_backing_block)?;
        let result = self.persist.commit_frame(id.index(), &descriptor, fault);
        let mut state = self.state.lock();
        let mut objects = self.objects.lock(id.index());
        objects.records[id.index()].pins -= 1;
        objects.records[id.index()].persisting = false;
        self.account_unpin();
        if result.is_ok() {
            objects.records[id.index()].committed = true;
            objects.records[id.index()].key = Some(key);
            objects.records[id.index()].version = version;
            // 同一 key 的新版本直接覆盖索引条目：索引永远指向当前版本。
            state.index.insert(
                key,
                PageEntry {
                    residency: Residency::Nv,
                    handle: Some(RuntimeHandle::Nv(id)),
                    ssd_backing_block: ssd_backing_block.unwrap_or(id.index() as u64),
                    state: NvObjectState::NvDirty,
                },
            );
        }
        result.map(|_| old.and_then(|index| NvPageId::try_from(index).ok()))
    }

    pub(crate) fn clear_frame(&self, id: NvFrameId, fault: Option<FaultPoint>) -> Result<()> {
        self.clear_committed(id.index(), ObjectKind::Metadata, None, fault)
    }

    /// Acquires the only pin on a committed metadata frame before SSD eviction.
    ///
    /// The returned guard keeps new readers out until the caller either clears
    /// the durable commit word with [`EvictingFrame::finish`] or drops the guard
    /// to abort the eviction.
    pub(crate) fn begin_frame_eviction(self: &Arc<Self>, id: NvFrameId) -> Result<EvictingFrame> {
        let mut state = self.objects.lock_checked(id.index())?;
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Metadata {
            return_errno_with_message!(Errno::EINVAL, "RNVPC frame is not allocated metadata");
        }
        if !record.committed {
            return_errno_with_message!(Errno::EINVAL, "RNVPC frame is not committed");
        }
        if record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "RNVPC frame is pinned");
        }
        self.account_pin()?;
        state.records[id.index()].pins = 1;
        state.records[id.index()].persisting = true;
        Ok(EvictingFrame {
            pool: self.clone(),
            id,
            active: true,
        })
    }

    /// Acquires the only pin on a committed dirty data page before SSD drain.
    ///
    /// The caller may read the page while the guard is active, then clear its
    /// durable residency with [`EvictingDataPage::finish`]. Dropping the guard
    /// leaves the committed data page resident and dirty.
    pub(crate) fn begin_data_page_eviction(
        self: &Arc<Self>,
        id: NvPageId,
    ) -> Result<EvictingDataPage> {
        self.begin_data_page_eviction_inner(id, None)
    }

    /// 仅当槽位仍是 `seq` 那一版时才独占它（面向"候选 + 回收账本"这类快照）。
    pub(crate) fn begin_data_page_eviction_if_seq(
        self: &Arc<Self>,
        id: NvPageId,
        seq: u64,
    ) -> Result<EvictingDataPage> {
        self.begin_data_page_eviction_inner(id, Some(seq))
    }

    /// `expect_seq` = 候选采集时记下的分配序号（`None` = 不做校验）。
    fn begin_data_page_eviction_inner(
        self: &Arc<Self>,
        id: NvPageId,
        expect_seq: Option<u64>,
    ) -> Result<EvictingDataPage> {
        let mut state = self.objects.lock_checked(id.index())?;
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Data(DataPageState::NvDirty) {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page is not dirty");
        }
        // 槽位已被归还并复用（ABA）⇒ 这不是候选记下的那一版，绝不能独占它。
        if let Some(expected) = expect_seq
            && record.alloc_seq != expected
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "RNVPC data page is not the expected version"
            );
        }
        if !record.committed {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data page is not committed");
        }
        if record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "RNVPC data page is pinned");
        }
        self.account_pin()?;
        let record = &mut state.records[id.index()];
        record.kind = ObjectKind::Data(DataPageState::InFlight);
        record.pins = 1;
        record.persisting = true;
        Ok(EvictingDataPage {
            pool: self.clone(),
            id,
            active: true,
            profile_times: [record.profile_write_ns, record.profile_commit_ns, read_ns()],
        })
    }

    pub(crate) fn clear_data_page(
        self: &Arc<Self>,
        id: NvPageId,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let reference = self.persistent_data_ref(id)?;
        let key = self.validate_persistent_data_ref(reference)?;
        let _publication = self.begin_publication_for(key.fs_id, key.inode)?;
        if self.extent_references_data(reference)? {
            return_errno_with_message!(Errno::EBUSY, "RNVPC data page is still extent-referenced");
        }
        // 撤销持久可达性时，页可能还是 `NvDirty`（尚未写回）或已经是 `Synced`
        // （写回完成、等回收）——按**当前**状态撤销，成功后统一落到 `Synced`。
        let current = self.data_page_state(id)?;
        self.clear_committed(
            id.index(),
            ObjectKind::Data(current),
            Some(ObjectKind::Data(DataPageState::Synced)),
            fault,
        )
    }

    fn clear_committed(
        &self,
        index: usize,
        expected: ObjectKind,
        next: Option<ObjectKind>,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.begin_clear(index, expected)?;
        let result = self.persist.clear_frame(index, fault);
        let mut state = self.state.lock();
        let mut objects = self.objects.lock(index);
        objects.records[index].pins -= 1;
        objects.records[index].persisting = false;
        self.account_unpin();
        if result.is_ok() {
            if let Some(key) = objects.records[index].key {
                if matches!(objects.records[index].kind, ObjectKind::Data(_)) {
                    state.index.remove(key);
                } else if state.metadata_index.get(&key) == Some(&index) {
                    state.metadata_index.remove(&key);
                }
            }
            objects.records[index].committed = false;
            objects.records[index].key = None;
            objects.records[index].version = 0;
            if let Some(next) = next {
                objects.records[index].kind = next;
            }
        }
        result
    }

    fn allocate_index(&self, class: PoolClass) -> Result<(usize, u64, bool)> {
        let mut state = self.allocation.lock();
        let next_seq = state
            .next_alloc_seq
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        state.capacity.before_allocate(class)?;
        let index = state
            .free_ids
            .pop()
            .ok_or_else(|| Error::new(Errno::ENOSPC))?;
        let mut objects = self.objects.lock(index);
        let record = &mut objects.records[index];
        debug_assert_eq!(record.kind, ObjectKind::Free);
        record.kind = match class {
            PoolClass::Metadata => ObjectKind::Metadata,
            PoolClass::Data => ObjectKind::Data(DataPageState::Staged),
            // WAL 页不走 free-list：它们由 `charge_wal_pages` 直接记账。
            PoolClass::Wal => {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "WAL pages are accounted by charge_wal_pages, not allocate_index"
                )
            }
        };
        record.persisting = false;
        record.committed = false;
        record.key = None;
        record.version = 0;
        let seq = state.next_alloc_seq;
        state.next_alloc_seq = next_seq;
        objects.records[index].alloc_seq = seq;
        objects.reverse = None;
        Ok((index, seq, state.capacity.account_allocate(class)))
    }

    fn allocate_index_with_repair_drain(&self, class: PoolClass) -> Result<(usize, u64, bool)> {
        match self.allocate_index(class) {
            Ok(allocation) => Ok(allocation),
            Err(error) if error.error() == Errno::ENOSPC => {
                let Some(frame) = self.repair_workspace.lock().drain_one() else {
                    return Err(error);
                };
                self.free_frame(frame)?;
                self.allocate_index(class)
            }
            Err(error) => Err(error),
        }
    }

    fn acquire_repair_frame(&self) -> Result<NvFrameId> {
        let cached = { self.repair_workspace.lock().take() };
        if let Some(frame) = cached {
            self.refresh_repair_frame_generation(frame)?;
            return Ok(frame);
        }
        self.alloc_frame(PoolClass::Metadata)
    }

    fn refresh_repair_frame_generation(&self, frame: NvFrameId) -> Result<()> {
        let mut allocation = self.allocation.lock();
        let next_seq = allocation
            .next_alloc_seq
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let mut objects = self.objects.lock_checked(frame.index())?;
        let record = objects
            .records
            .get(frame.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Metadata
            || record.committed
            || record.pins != 0
            || record.persisting
        {
            return_errno_with_message!(Errno::EBUSY, "cached repair frame is not reusable");
        }
        let record = &mut objects.records[frame.index()];
        record.key = None;
        record.version = 0;
        record.alloc_seq = allocation.next_alloc_seq;
        allocation.next_alloc_seq = next_seq;
        objects.reverse = None;
        Ok(())
    }

    fn recycle_repair_frame(&self, frame: NvFrameId) -> Result<()> {
        {
            let objects = self.objects.lock_checked(frame.index())?;
            let record = objects
                .records
                .get(frame.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if record.kind != ObjectKind::Metadata
                || record.committed
                || record.pins != 0
                || record.persisting
            {
                return_errno_with_message!(Errno::EBUSY, "repair frame is not clear for reuse");
            }
        }
        if self.repair_workspace.lock().put(frame) {
            return Ok(());
        }
        self.free_frame(frame)
    }

    fn free_index(&self, index: usize, class: PoolClass) -> Result<()> {
        let mut state = self.allocation.lock();
        let mut objects = self.objects.lock(index);
        if objects.records[index].committed {
            return_errno_with_message!(Errno::EBUSY, "RNVPC frame is still committed");
        }
        let record = objects
            .records
            .get_mut(index)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let expected = match class {
            PoolClass::Metadata => ObjectKind::Metadata,
            PoolClass::Data => unreachable!(),
            PoolClass::Wal => {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "WAL pages are released by release_wal_pages, not free_index"
                )
            }
        };
        if record.kind != expected {
            return_errno_with_message!(Errno::EINVAL, "RNVPC frame is not allocated");
        }
        if record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "RNVPC frame is pinned");
        }
        record.kind = ObjectKind::Free;
        state.free_ids.push(index);
        state.capacity.account_free(class);
        Ok(())
    }

    fn begin_commit(
        &self,
        index: usize,
        expected: ObjectKind,
        key: PageKey,
        replace: bool,
        expected_old: Option<PersistentObjectRef>,
    ) -> Result<(u64, Option<usize>)> {
        let state = self.state.lock();
        if self
            .objects
            .get(index)
            .is_some_and(|record| record.committed)
        {
            return_errno_with_message!(Errno::EBUSY, "RNVPC object is already committed");
        }
        let old = if matches!(expected, ObjectKind::Data(_)) {
            self.committed_version_of_key(&state, index, key)
        } else {
            self.committed_metadata_version_of_key(&state, index, key)
        };
        if old.is_some() && !replace {
            return_errno_with_message!(Errno::EBUSY, "RNVPC home already has a committed version");
        }
        if let Some(expected_old) = expected_old {
            let expected_index = expected_old
                .slot()
                .and_then(|slot| usize::try_from(slot).ok())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            let matches = old.is_some_and(|(old_index, _)| {
                old_index == expected_index
                    && self
                        .objects
                        .get(old_index)
                        .is_some_and(|record| record.alloc_seq == expected_old.allocation_seq())
            });
            if !matches {
                return_errno_with_message!(Errno::ESTALE, "metadata object was replaced");
            }
        }
        let version = old
            .map(|(_, previous_version)| {
                previous_version
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))
            })
            .transpose()?
            .unwrap_or(1);
        let mut objects = self.objects.lock(index);
        self.pin_persistent_update(&mut objects, index, expected)?;
        Ok((version, old.map(|(previous, _)| previous)))
    }

    /// 找“**同一 key 的已提交旧版本**”（`begin_commit` 的 `replace` / `EBUSY` 判据）。
    ///
    /// Data-page equivalent of the metadata lookup index, backed by the DRAM
    /// page index.
    /// **只适用于数据页**（它们才是 B-tree 的内容）；元数据对象请用扫描版。
    fn committed_version_of_key(
        &self,
        state: &PoolState,
        index: usize,
        key: PageKey,
    ) -> Option<(usize, u64)> {
        let old = match state.index.lookup(key)?.handle {
            Some(RuntimeHandle::Nv(page)) => page.index(),
            _ => return None,
        };
        if old == index {
            return None;
        }
        let record = self.objects.get(old)?;
        (record.committed && record.key == Some(key)).then_some((old, record.version))
    }

    fn committed_metadata_version_of_key(
        &self,
        state: &PoolState,
        index: usize,
        key: PageKey,
    ) -> Option<(usize, u64)> {
        let old = *state.metadata_index.get(&key)?;
        if old == index {
            return None;
        }
        let record = self.objects.get(old)?;
        (record.committed && record.kind == ObjectKind::Metadata && record.key == Some(key))
            .then_some((old, record.version))
    }

    fn begin_clear(&self, index: usize, expected: ObjectKind) -> Result<()> {
        let mut state = self.objects.lock(index);
        if !state.records[index].committed {
            return_errno_with_message!(Errno::EINVAL, "RNVPC object is not committed");
        }
        self.pin_persistent_update(&mut state, index, expected)
    }

    fn begin_exclusive_metadata_update(&self, frame: NvFrameId, allocation_seq: u64) -> Result<()> {
        let wait_started = read_ns();
        {
            let mut state = self.objects.lock_checked(frame.index())?;
            let object = state
                .records
                .get(frame.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if object.kind != ObjectKind::Metadata
                || !object.committed
                || object.alloc_seq != allocation_seq
            {
                return_errno_with_message!(Errno::ESTALE, "metadata object was replaced");
            }
            if object.persisting {
                return_errno_with_message!(Errno::EBUSY, "metadata persistence is in progress");
            }
            state.records[frame.index()].persisting = true;
        }

        loop {
            let observed = METADATA_PIN_EPOCH.load(Ordering::Acquire);
            {
                let mut state = self.objects.lock(frame.index());
                let object = &state.records[frame.index()];
                debug_assert_eq!(object.kind, ObjectKind::Metadata);
                debug_assert!(object.committed);
                debug_assert_eq!(object.alloc_seq, allocation_seq);
                debug_assert!(object.persisting);
                if object.pins == 0 {
                    if let Err(error) = self.account_pin() {
                        state.records[frame.index()].persisting = false;
                        return Err(error);
                    }
                    state.records[frame.index()].pins = 1;
                    LATENCY_STATS
                        .rnvpce_metadata_reader_wait
                        .record_since(wait_started);
                    return Ok(());
                }
            }
            let _ = METADATA_PIN_WAITERS.wait_until(|| {
                (METADATA_PIN_EPOCH.load(Ordering::Acquire) != observed).then_some(())
            });
        }
    }

    fn pin_persistent_update(
        &self,
        state: &mut object_table::ObjectSlot,
        index: usize,
        expected: ObjectKind,
    ) -> Result<()> {
        let record = state
            .records
            .get(index)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != expected {
            return_errno_with_message!(Errno::EINVAL, "RNVPC object has the wrong class");
        }
        if record.persisting {
            return_errno_with_message!(
                Errno::EBUSY,
                "RNVPC object persistence is already in progress"
            );
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.account_pin()?;
        state.records[index].pins = pins;
        state.records[index].persisting = true;
        Ok(())
    }

    fn write_pinned_frame(&self, id: NvFrameId, input: &[u8]) -> Result<()> {
        {
            let state = self.objects.lock_checked(id.index())?;
            let record = state
                .records
                .get(id.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if record.kind != ObjectKind::Metadata
                || record.pins == 0
                || record.persisting
                || record.committed
            {
                return_errno_with_message!(Errno::EBUSY, "RNVPC frame is not writable");
            }
        }
        self.backend.write_page(self.page_id(id), input)
    }

    fn write_pinned_data_page(&self, id: NvPageId, input: &[u8]) -> Result<()> {
        {
            let state = self.objects.lock_checked(id.index())?;
            let record = state
                .records
                .get(id.index())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if record.kind != ObjectKind::Data(DataPageState::Staged)
                || record.pins == 0
                || record.persisting
                || record.committed
            {
                return_errno_with_message!(Errno::EBUSY, "RNVPC data page is not writable");
            }
        }
        self.backend.write_page(id, input)
    }

    fn page_id(&self, id: NvFrameId) -> NvPageId {
        NvPageId::try_from(id.index()).unwrap()
    }

    fn unpin_frame(&self, id: NvFrameId) {
        let mut state = self.objects.lock(id.index());
        let record = &mut state.records[id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Metadata);
        debug_assert!(record.pins > 0);
        record.pins -= 1;
        self.account_unpin();
        drop(state);
        note_metadata_pin_change();
    }

    fn unpin_data_page(&self, id: NvPageId) {
        let mut state = self.objects.lock(id.index());
        let record = &mut state.records[id.index()];
        debug_assert!(matches!(record.kind, ObjectKind::Data(_)));
        debug_assert!(record.pins > 0);
        record.pins -= 1;
        self.account_unpin();
    }

    fn abort_frame_eviction(&self, id: NvFrameId) {
        let mut state = self.objects.lock(id.index());
        let record = &mut state.records[id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Metadata);
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.pins = 0;
        record.persisting = false;
        self.account_unpin();
    }

    fn abort_data_page_eviction(&self, id: NvPageId) {
        let mut state = self.state.lock();
        let mut objects = self.objects.lock(id.index());
        let record = &mut objects.records[id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Data(DataPageState::InFlight));
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.kind = ObjectKind::Data(DataPageState::NvDirty);
        record.pins = 0;
        record.persisting = false;
        let allocation_seq = record.alloc_seq;
        self.account_unpin();
        let identity = (id.index(), allocation_seq);
        if state.dirty_members.insert(identity) {
            state.dirty_queue.push_back(DirtyNvPage {
                page: id,
                allocation_seq,
            });
        }
        drop(objects);
        drop(state);
        note_data_page_lease_change();
    }

    fn release_data_page_mutation(&self, id: NvPageId) {
        let mut state = self.objects.lock(id.index());
        let record = &mut state.records[id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Data(DataPageState::NvDirty));
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.pins = 0;
        record.persisting = false;
        self.account_unpin();
        drop(state);
        note_data_page_lease_change();
    }

    fn wake_high_water(&self) {
        if let Some(callback) = &self.high_water_wake {
            callback();
        }
    }
}

fn recovery_now_ns(enabled: bool) -> u64 {
    if enabled {
        aster_time::read_monotonic_time().as_nanos() as u64
    } else {
        0
    }
}

fn recovery_elapsed_ns(enabled: bool, start: u64) -> u64 {
    recovery_now_ns(enabled).saturating_sub(start)
}

fn note_data_page_lease_change() {
    DATA_PAGE_LEASE_EPOCH.fetch_add(1, Ordering::AcqRel);
    DATA_PAGE_LEASE_WAITERS.wake_all();
}

fn note_metadata_pin_change() {
    METADATA_PIN_EPOCH.fetch_add(1, Ordering::AcqRel);
    METADATA_PIN_WAITERS.wake_all();
}

pub(crate) struct PinnedFrame {
    pool: Arc<Rnvpc>,
    id: NvFrameId,
}

impl PinnedFrame {
    pub(crate) fn id(&self) -> NvFrameId {
        self.id
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool.backend.read_page(self.page_id(), output)
    }

    pub(crate) fn write(&self, input: &[u8]) -> Result<()> {
        self.pool.write_pinned_frame(self.id, input)
    }

    fn page_id(&self) -> NvPageId {
        NvPageId::try_from(self.id.index()).unwrap()
    }
}

impl Drop for PinnedFrame {
    fn drop(&mut self) {
        self.pool.unpin_frame(self.id);
    }
}

/// A pinned data page whose bytes remain stable for the lifetime of the
/// handle.
pub(crate) struct PinnedDataPage {
    pool: Arc<Rnvpc>,
    id: NvPageId,
}

impl PinnedDataPage {
    pub(crate) fn id(&self) -> NvPageId {
        self.id
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool.backend.read_page(self.id, output)
    }

    pub(crate) fn copy_to_writer(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        len: usize,
    ) -> Result<usize> {
        self.pool
            .backend
            .copy_to_writer(self.id, offset, writer, len)
    }

    /// Writes a staged page before it is made NV-dirty or committed.
    pub(crate) fn write(&self, input: &[u8]) -> Result<()> {
        self.pool.write_pinned_data_page(self.id, input)
    }
}

impl Drop for PinnedDataPage {
    fn drop(&mut self) {
        self.pool.unpin_data_page(self.id);
    }
}

pub(crate) struct StagingDataPage {
    pin: Option<PinnedDataPage>,
    initialized: bool,
}

impl StagingDataPage {
    pub(crate) fn id(&self) -> NvPageId {
        self.pin.as_ref().expect("staging page is active").id
    }

    pub(crate) fn zero_all(&mut self) -> Result<()> {
        let pin = self.pin.as_ref().expect("staging page is active");
        pin.pool.backend.zero_page_range(pin.id, 0, PAGE_SIZE)?;
        self.initialized = true;
        Ok(())
    }

    pub(crate) fn copy_from_reader(
        &mut self,
        offset: usize,
        reader: &mut VmReader,
        len: usize,
    ) -> Result<usize> {
        let pin = self.pin.as_ref().expect("staging page is active");
        if LATENCY_ENABLED {
            pin.pool.objects.lock(pin.id.index()).records[pin.id.index()].profile_write_ns =
                read_ns();
        }
        pin.pool
            .backend
            .copy_from_reader(pin.id, offset, reader, len)
    }

    pub(crate) fn dma_segment(&self) -> Result<USegment> {
        let pin = self.pin.as_ref().expect("staging page is active");
        pin.pool.backend.data_page_segment(pin.id)
    }

    pub(crate) fn finish(mut self) -> Result<NvPageId> {
        if !self.initialized {
            return_errno_with_message!(Errno::EINVAL, "staged NVPage is not fully initialized");
        }
        let pin = self.pin.take().expect("staging page is active");
        let id = pin.id;
        drop(pin);
        Ok(id)
    }
}

impl Drop for StagingDataPage {
    fn drop(&mut self) {
        if let Some(pin) = self.pin.take() {
            let pool = pin.pool.clone();
            let id = pin.id;
            drop(pin);
            let _ = pool.free_data_page(id);
        }
    }
}

pub(crate) struct MutatingDataPage {
    pool: Arc<Rnvpc>,
    id: NvPageId,
    active: bool,
}

impl MutatingDataPage {
    pub(crate) fn copy_from_reader(
        &self,
        offset: usize,
        reader: &mut VmReader,
        len: usize,
        fault: Option<FaultPoint>,
    ) -> Result<usize> {
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if end == offset {
            return Ok(0);
        }
        inject(fault, FaultPoint::BeforeContentPersist)?;
        if LATENCY_ENABLED {
            self.pool.objects.lock(self.id.index()).records[self.id.index()].profile_write_ns =
                read_ns();
        }
        let copied = self
            .pool
            .backend
            .copy_from_reader(self.id, offset, reader, len)?;
        if copied != 0 {
            let absolute = self
                .id
                .index()
                .checked_mul(PAGE_SIZE)
                .and_then(|base| base.checked_add(offset))
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            self.pool.backend.persist(absolute, copied)?;
            self.pool.backend.fence()?;
            inject(fault, FaultPoint::AfterContentPersist)?;
        }
        Ok(copied)
    }

    pub(crate) fn write_range(
        &self,
        offset: usize,
        input: &[u8],
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let end = offset
            .checked_add(input.len())
            .filter(|end| *end <= PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if end == offset {
            return Ok(());
        }
        inject(fault, FaultPoint::BeforeContentPersist)?;
        self.pool.backend.write_page_range(self.id, offset, input)?;
        let absolute = self
            .id
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.pool.backend.persist(absolute, input.len())?;
        self.pool.backend.fence()?;
        inject(fault, FaultPoint::AfterContentPersist)
    }

    pub(crate) fn finish(mut self) {
        self.pool.release_data_page_mutation(self.id);
        self.active = false;
    }
}

impl Drop for MutatingDataPage {
    fn drop(&mut self) {
        if self.active {
            self.pool.release_data_page_mutation(self.id);
        }
    }
}

pub(crate) struct EvictingFrame {
    pool: Arc<Rnvpc>,
    id: NvFrameId,
    active: bool,
}

impl EvictingFrame {
    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool
            .backend
            .read_page(self.pool.page_id(self.id), output)
    }

    pub(crate) fn finish(mut self, fault: Option<FaultPoint>) -> Result<()> {
        self.pool.persist.clear_frame(self.id.index(), fault)?;
        let mut state = self.pool.state.lock();
        let mut objects = self.pool.objects.lock(self.id.index());
        let record = &mut objects.records[self.id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Metadata);
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.pins = 0;
        record.persisting = false;
        self.pool.account_unpin();
        if let Some(key) = objects.records[self.id.index()].key
            && state.metadata_index.get(&key) == Some(&self.id.index())
        {
            state.metadata_index.remove(&key);
        }
        objects.records[self.id.index()].committed = false;
        objects.records[self.id.index()].key = None;
        objects.records[self.id.index()].version = 0;
        self.active = false;
        Ok(())
    }
}

impl Debug for EvictingFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvictingFrame")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for EvictingFrame {
    fn drop(&mut self) {
        if self.active {
            self.pool.abort_frame_eviction(self.id);
        }
    }
}

pub(crate) struct EvictingDataPage {
    pool: Arc<Rnvpc>,
    id: NvPageId,
    active: bool,
    profile_times: [u64; 3],
}

impl EvictingDataPage {
    pub(crate) fn profile_times(&self) -> [u64; 3] {
        self.profile_times
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool.backend.read_page(self.id, output)
    }

    pub(crate) fn assign_migration_target(
        &self,
        reference: PersistentObjectRef,
        target: u64,
    ) -> Result<()> {
        if reference.slot() != Some(self.id.index() as u64) {
            return_errno_with_message!(Errno::ESTALE, "migration lease reference mismatch");
        }
        self.pool.persist_migration_target(reference, target)
    }

    pub(crate) fn retry_migration(mut self, reference: PersistentObjectRef) -> Result<()> {
        if self.pool.persistent_data_has_migration_target(reference)? {
            self.pool.clear_persistent_migration_target(reference)?;
        }
        self.pool.abort_data_page_eviction(self.id);
        self.active = false;
        Ok(())
    }

    /// rNVPCe reclamation gate: the mapped extent must already point
    /// at SSD before the persistent NV descriptor can be cleared.
    #[cfg(ktest)]
    pub(crate) fn finish_unreferenced(self, fault: Option<FaultPoint>) -> Result<()> {
        let reference = self.pool.persistent_data_ref(self.id)?;
        if self.pool.is_page_referenced(self.id) || self.pool.extent_references_data(reference)? {
            return_errno_with_message!(
                Errno::EBUSY,
                "rNVPCe data page is still persistently referenced"
            );
        }
        self.finish(fault)
    }

    /// Reclaims a new-path rNVPCe page after its SSD reference is durable.
    /// The caller must have replaced the forward reference while this lease
    /// remained active. The retained reverse locator is cleared below.
    pub(crate) fn reclaim_after_ssd_publish(
        mut self,
        reference: PersistentObjectRef,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let page = self.id.index() as u64;
        let result = (|| {
            set_completion_stage(COMPLETION_FINISH_VALIDATE_LOCK, 0, page);
            let started = read_ns();
            {
                // Validate the pinned generation before destroying its persistent
                // recovery evidence. The eviction lease excludes mutation, but an
                // explicit pre-check keeps a stale completion from clearing a
                // descriptor that belongs to a later allocation.
                let t_lock = read_ns();
                let state = self.pool.objects.lock(self.id.index());
                LATENCY_STATS.rnvpce_reclaim_state_lock.record_since(t_lock);
                let record = &state.records[self.id.index()];
                if record.kind != ObjectKind::Data(DataPageState::InFlight)
                    || record.pins != 1
                    || !record.persisting
                    || record.alloc_seq != reference.allocation_seq()
                    || state.reverse.is_none()
                {
                    return_errno_with_message!(Errno::ESTALE, "rNVPCe migration lease changed");
                }
            }
            LATENCY_STATS
                .nvpc_finish_validate_lock
                .record_since(started);

            set_completion_stage(COMPLETION_FINISH_CLEAR_DESCRIPTOR, 0, page);
            let started = read_ns();
            // zn: clean NVPage description on NVDIMM
            self.pool.persist.clear_frame(self.id.index(), fault)?;
            LATENCY_STATS
                .nvpc_finish_clear_descriptor
                .record_since(started);

            set_completion_stage(COMPLETION_FINISH_FINALIZE_LOCK, 0, page);
            let started = read_ns();
            {
                let mut allocation = self.pool.allocation.lock();
                let t_lock = read_ns();
                let mut state = self.pool.objects.lock(self.id.index());
                LATENCY_STATS.rnvpce_reclaim_state_lock.record_since(t_lock);
                let record = &state.records[self.id.index()];
                // The lease stayed active across descriptor clearing; the
                // identity validated above cannot change before this reclaim.
                debug_assert_eq!(record.kind, ObjectKind::Data(DataPageState::InFlight));
                debug_assert_eq!(record.pins, 1);
                debug_assert!(record.persisting);
                debug_assert_eq!(record.alloc_seq, reference.allocation_seq());
                debug_assert!(state.reverse.is_some());
                state.reverse = None;
                // zn: reset in-mem runtime state of NVPage(ObjectRecord)
                let record = &mut state.records[self.id.index()];
                record.kind = ObjectKind::Free;
                record.pins = 0;
                record.persisting = false;
                record.committed = false;
                record.key = None;
                record.version = 0;
                self.pool.account_unpin();
                allocation.free_ids.push(self.id.index());
                allocation.capacity.account_free(PoolClass::Data);
                self.active = false;
            }
            LATENCY_STATS
                .nvpc_finish_finalize_lock
                .record_since(started);

            set_completion_stage(COMPLETION_FINISH_LEASE_WAKE, 0, page);
            let started = read_ns();
            note_data_page_lease_change();
            LATENCY_STATS.nvpc_finish_lease_wake.record_since(started);
            Ok(())
        })();
        clear_completion_stage();
        result
    }

    pub(crate) fn finish(mut self, fault: Option<FaultPoint>) -> Result<()> {
        self.pool.persist.clear_frame(self.id.index(), fault)?;
        let mut state = self.pool.objects.lock(self.id.index());
        let record = &mut state.records[self.id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Data(DataPageState::InFlight));
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.kind = ObjectKind::Data(DataPageState::Synced);
        record.pins = 0;
        record.persisting = false;
        self.pool.account_unpin();
        state.records[self.id.index()].committed = false;
        state.records[self.id.index()].key = None;
        state.records[self.id.index()].version = 0;
        self.active = false;
        drop(state);
        note_data_page_lease_change();
        Ok(())
    }
}

impl Debug for EvictingDataPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvictingDataPage")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for EvictingDataPage {
    fn drop(&mut self) {
        if self.active {
            self.pool.abort_data_page_eviction(self.id);
        }
    }
}

#[cfg(ktest)]
mod allocation_tests {
    use super::*;
    use crate::fs::rnvpc::{config::NvSimMode, dram::DramNvBackend};

    #[ostd::prelude::ktest]
    fn rnvpce_allocation_reclaim_does_not_take_pool_control() {
        let config = RnvpcConfig::from_bytes(
            Some(8 * PAGE_SIZE),
            Some(2 * PAGE_SIZE),
            Some(6 * PAGE_SIZE),
            Some(NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate(config).unwrap());
        let pool = Arc::new(Rnvpc::new(backend, config, None).unwrap());
        let (page, seq) = pool.alloc_data_page_seq().unwrap();
        pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
            .unwrap();
        pool.replace_data_page_in_owner_at(page, PageKey::new(77, 1, 0), 0, None)
            .unwrap();
        let reference = pool.persistent_data_ref(page).unwrap();
        pool.install_reverse(reference, ReverseExtentRef::new(7, 0).unwrap())
            .unwrap();
        let lease = pool.pin_for_writeback_seq(page, seq).unwrap();
        // Model a successfully published SSD edge. Holding control throughout
        // proves the allocator and successful reclaim never reacquire it.
        let control = pool.state.lock();
        lease.reclaim_after_ssd_publish(reference, None).unwrap();
        assert_eq!(pool.stats().free_pages, 8);
        assert_eq!(pool.stats().pinned_frames, 0);
        assert!(pool.persist.read_entry(page.index()).unwrap().is_none());
        let (reused, next_seq) = pool.alloc_data_page_seq().unwrap();
        assert_eq!(reused, page);
        assert!(next_seq > seq);
        pool.abort_data_page(reused).unwrap();
        pool.charge_wal_pages(1).unwrap();
        pool.release_wal_pages(1);
        assert_eq!(pool.stats().free_pages, 8);
        drop(control);
    }
}
