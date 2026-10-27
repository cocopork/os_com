// SPDX-License-Identifier: MPL-2.0

//! Capacity-bounded memory backend for persistent filesystem state.

#![expect(
    dead_code,
    reason = "Task 7-8 interfaces are consumed by later RNVPC tasks"
)]

use spin::Once;

pub(crate) mod backend;
pub(crate) mod batch_metrics;
pub(crate) mod capacity;
pub(crate) mod config;
pub(crate) mod cow;
pub(crate) mod data_page;
pub(crate) mod descriptor;
pub(crate) mod dram;
pub(crate) mod extend_cow;
pub(crate) mod extent_index;
pub(crate) mod extent_mutation;
pub(crate) mod extent_state;
pub(crate) mod extent_target;
pub(crate) mod fault;
pub(crate) mod frame;
pub(crate) mod id;
pub(crate) mod index;
pub(crate) mod layout;
pub(crate) mod metadata;
pub(crate) mod mode;
pub(crate) mod pcache_layout;
pub(crate) mod pcache_store;
pub(crate) mod persist;
pub(crate) mod persistent_btree;
pub(crate) mod reclaim;
pub(crate) mod recover;
pub(crate) mod redirect_index;
pub(crate) mod wal;
pub(crate) mod write_transaction;

use self::{
    config::RnvpcConfig, cow::InodeCowContext, dram::DramNvBackend, extend_cow::ExtendCowContext,
    frame::Rnvpc, id::NvPageId,
};
use crate::prelude::*;

static RNVPC: Once<Arc<Rnvpc>> = Once::new();

pub(crate) fn pool() -> Option<&'static Arc<Rnvpc>> {
    RNVPC.get()
}

#[cfg(ktest)]
pub(crate) fn init_for_ext4_tests() {
    RNVPC.call_once(|| {
        let config = RnvpcConfig::from_bytes(
            Some(64 * 1024 * 1024),
            Some(32 * 1024 * 1024),
            Some(32 * 1024 * 1024),
            Some(config::NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
        Arc::new(Rnvpc::format(backend, config, None).unwrap())
    });
}

pub(crate) fn recovery_benchmark_enabled() -> bool {
    config::recovery_benchmark_enabled()
}

pub(crate) fn profile_recovery() -> Result<recover::RecoveryProfile> {
    pool()
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "rNVPC pool is not configured"))?
        .profile_recovery()
}

pub(crate) fn read_mapped_page_from(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    page_index: u64,
    output: &mut [u8],
) -> Result<bool> {
    if output.len() != PAGE_SIZE {
        return_errno_with_message!(Errno::EINVAL, "rNVPCe reads must cover one page");
    }
    let Some(reference) = write_transaction::mapped_nv_page_ref(pool, fs_id, inode, page_index)?
    else {
        return Ok(false);
    };
    let page = pool.pin_persistent_data_page(reference)?;
    page.read(output)?;
    Ok(true)
}

pub(crate) fn read_mapped_page(
    fs_id: u64,
    inode: u64,
    page_index: u64,
    output: &mut [u8],
) -> Result<bool> {
    let pool =
        pool().ok_or_else(|| Error::with_message(Errno::EINVAL, "rNVPC pool is not configured"))?;
    read_mapped_page_from(pool, fs_id, inode, page_index, output)
}

pub(super) fn init() {
    let Some(config) = RnvpcConfig::from_command_line()
        .unwrap_or_else(|error| panic!("invalid RNVPC configuration: {:?}", error))
    else {
        return;
    };

    let total_pages = config.total_pages();
    let mode = config.mode();
    RNVPC.call_once(|| {
        let backend: Arc<dyn backend::NvMemoryBackend> = Arc::new(
            DramNvBackend::allocate(config)
                .unwrap_or_else(|error| panic!("failed to allocate RNVPC: {:?}", error)),
        );
        Arc::new(
            Rnvpc::format(backend, config, None)
                .unwrap_or_else(|error| panic!("failed to initialize RNVPC: {:?}", error)),
        )
    });
    ostd::info!(
        "initialized DRAM-backed RNVPC with {} pages ({} MiB), mode {:?}",
        total_pages,
        total_pages * PAGE_SIZE / (1024 * 1024),
        mode
    );
}

#[cfg(ktest)]
mod rnvpce_integrity_tests;
#[cfg(ktest)]
mod tests;

static COW_CONTEXTS: Once<Mutex<BTreeMap<(u64, u64), Arc<InodeCowContext>>>> = Once::new();

/// 取得（必要时创建）某个 inode 的 CoW/WAL 上下文（I2 的前提）。
///
/// ext4 的写路径需要按 inode 复用上下文：WAL 额度、待 completion 回收的旧页列表都
/// 在上下文里。池未配置（挂载选项开了 `rnvpc` 却没用 cmdline 给容量）⇒ `EINVAL`，
/// 绝不静默降级。
///
/// Context identity includes the filesystem and inode; creation and recovery may sleep.
pub(crate) fn cow_context(fs_id: u64, inode: u64) -> Result<Arc<InodeCowContext>> {
    let pool =
        pool().ok_or_else(|| Error::with_message(Errno::EINVAL, "rNVPC pool is not configured"))?;
    let table = COW_CONTEXTS.call_once(|| Mutex::new(BTreeMap::new()));
    let mut table = table.lock();
    if let Some(context) = table.get(&(fs_id, inode)) {
        return Ok(context.clone());
    }
    let context = match InodeCowContext::recover_from_catalog(pool.clone(), fs_id, inode) {
        Ok(context) => context,
        Err(error) if error.error() == Errno::ENOENT => {
            InodeCowContext::create(pool.clone(), fs_id, inode)?
        }
        Err(error) => return Err(error),
    };
    table.insert((fs_id, inode), context.clone());
    Ok(context)
}

/// 已经存在的 CoW/WAL 上下文（读路径用；不创建）。
pub(crate) fn existing_cow_context(fs_id: u64, inode: u64) -> Option<Arc<InodeCowContext>> {
    COW_CONTEXTS.get()?.lock().get(&(fs_id, inode)).cloned()
}

/// rNVPCe（`rnvpc_extend`）的上下文表。
///
/// **与 `COW_CONTEXTS` 并列、不共用**：rNVPCe 的一致性由 soft update 提供，不经过
/// per-inode WAL（`ext4-io-integration.md §1.1`）。两条路线各自一份表，任何一侧都
/// 看不到另一侧的状态。
static EXTEND_CONTEXTS: Once<SpinLock<BTreeMap<u64, Arc<ExtendCowContext>>>> = Once::new();

/// 取得（必要时创建）某个 inode 的 rNVPCe soft-update 上下文。
///
/// `fs_id` 是 `PageKey` 的文件系统标识（superblock UUID 低 64 位）。池未配置
/// （挂载选项开了 `rnvpc_extend` 却没用 cmdline 给容量）⇒ `EINVAL`，绝不静默降级。
///
/// 已知限制与 [`cow_context`] 相同：以 inode 号为键，不含 `fs_id`——当前实验只用
/// 单个文件系统实例；多文件系统共存时需要把键换成 `(fs_id, inode)`。
pub(crate) fn extend_context(inode: u64, fs_id: u64) -> Result<Arc<ExtendCowContext>> {
    let pool =
        pool().ok_or_else(|| Error::with_message(Errno::EINVAL, "rNVPC pool is not configured"))?;
    let table = EXTEND_CONTEXTS.call_once(|| SpinLock::new(BTreeMap::new()));
    let mut table = table.lock();
    if let Some(context) = table.get(&inode) {
        debug_assert_eq!(context.fs_id(), fs_id);
        return Ok(context.clone());
    }
    let context = ExtendCowContext::new(pool.clone(), inode, fs_id);
    table.insert(inode, context.clone());
    Ok(context)
}

/// 已经存在的 rNVPCe 上下文（读路径用；不创建）。
pub(crate) fn existing_extend_context(inode: u64) -> Option<Arc<ExtendCowContext>> {
    let table = EXTEND_CONTEXTS.get()?;
    table.lock().get(&inode).cloned()
}

/// I4：一轮的 rNVPC 统计快照（供 ext4 卸载时打印 `[RNVPC]` 行）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RnvpcSnapshot {
    pub(crate) checkpoint: cow::ForegroundCheckpointCounters,
    /// 本轮建过 CoW/WAL 上下文的 inode 数。
    pub(crate) contexts: usize,
    /// 聚合的 CoW 计数。
    pub(crate) cow: cow::CowCounters,
    /// 聚合的 WAL 计数。
    pub(crate) wal: wal::WalCounters,
    /// 聚合的未退休 WAL 条数。
    pub(crate) wal_pending: usize,
    /// 建过 rNVPCe（soft-update，无 WAL）上下文的 inode 数。
    pub(crate) extend_contexts: usize,
    /// 聚合的 rNVPCe 计数（无 WAL；验收口径：它涨而 `wal*` 保持 0）。
    pub(crate) extend: extend_cow::ExtendCounters,
    /// 聚合的 rNVPCe 驻留 NV 版本数。
    pub(crate) extend_resident: usize,
    /// 聚合的 rNVPCe in-flight（未落 SSD）页数。
    pub(crate) extend_in_flight: usize,
}

/// rTIO v0：前台在"容量吃紧"时通知回刷线程的请求计数。
///
/// 回刷线程无法被前台直接唤醒（它属于 ext4 侧、持 `Weak<Ext4>`），所以用计数 +
/// 回刷线程的周期轮询来传递"该干活了"；计数本身也是可观测口径。
static FLUSH_REQUESTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// rTIO v0：前台请求回刷（WAL 满 / 池容量吃紧时调用）。
pub(crate) fn notify_flush() {
    FLUSH_REQUESTS.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
}

/// rTIO v0：累计的回刷请求数。
pub(crate) fn flush_requests() -> u64 {
    FLUSH_REQUESTS.load(core::sync::atomic::Ordering::Acquire)
}

/// rTIO v0：是否有回刷压力（水线越线 / WAL 满 / 前台请求 / rNVPCe 有可回刷版本）。
pub(crate) fn has_flush_pressure() -> bool {
    needs_flush_watermark() || extend_has_flushable_page()
}

/// rNVPCe：是否有任何 inode 存在**可回刷**（目标块已定）的 NV 版本。
///
/// 只认可回刷的页：目标块未知的版本无处可写，把它算作压力会让回刷线程空转。
fn extend_has_flushable_page() -> bool {
    let Some(table) = EXTEND_CONTEXTS.get() else {
        return false;
    };
    table
        .lock()
        .values()
        .any(|context| context.has_flushable_page())
}

/// rTIO v0：池是否已越过回刷水线（已用 ≥ `high_water_pages`）。
///
/// 只要越过就该开始 NV→SSD 回刷，把容量还给池；未越过时**不写 SSD**（数据只驻
/// NVDIMM，这正是"小数据集 ⇒ 仅 NVDIMM"的场景）。
pub(crate) fn needs_flush_watermark() -> bool {
    let Some(pool) = pool() else {
        return false;
    };
    let stats = pool.stats();
    stats.free_pages <= stats.total_pages.saturating_sub(stats.high_water_pages)
}

/// rTIO v0：WAL 已满的 inode。
///
/// Diagnostic only: foreground checkpoint retires the persistent WAL; migration does not.
pub(crate) fn wal_full_inodes() -> Vec<u64> {
    COW_CONTEXTS
        .get()
        .map(|table| {
            table
                .lock()
                .iter()
                .filter(|(_, context)| context.wal().is_full())
                .map(|((_, inode), _)| *inode)
                .collect()
        })
        .unwrap_or_default()
}

/// rTIO v0：聚合所有 inode 的回刷候选：
/// `(inode, page_index, NV 页, SSD 目标块, 代次, 页分配序号)`。
///
/// 两条路线各取各的上下文（rNVPC = `COW_CONTEXTS` 的 CoW/WAL；rNVPCe =
/// `EXTEND_CONTEXTS` 的 soft-update），**不互相回刷**。
pub(crate) fn flush_candidates(
    fs_id: u64,
    budget: usize,
) -> Vec<(u64, u64, NvPageId, u64, u64, u64)> {
    let contexts: Vec<_> = COW_CONTEXTS
        .get()
        .map(|table| {
            table
                .lock()
                .iter()
                .filter(|((owner, _), _)| *owner == fs_id)
                .map(|((_, inode), context)| (*inode, context.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for (inode, context) in contexts {
        for (index, page, target, generation, seq) in
            context.flush_candidates(budget.saturating_sub(out.len()))
        {
            out.push((inode, index, page, target, generation, seq));
        }
        if out.len() >= budget {
            break;
        }
    }
    out
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RnvpceFlushCandidate {
    pub(crate) inode: u64,
    pub(crate) page_index: u64,
    pub(crate) page: NvPageId,
    pub(crate) reference: metadata::PersistentObjectRef,
    pub(crate) reverse: index::ReverseExtentRef,
}

/// rNVPCe candidates come from the duplicate-suppressed physical dirty queue.
/// The reverse locator is validated again when migration is claimed and when
/// completion publishes the SSD mapping; candidate discovery never walks an
/// inode extent tree.
pub(crate) fn rnvpce_flush_candidates(fs_id: u64, budget: usize) -> Vec<RnvpceFlushCandidate> {
    let Some(pool) = pool() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for candidate in pool.collect_dirty_nvpages(budget) {
        let Ok(reference) = pool.persistent_data_ref(candidate.page) else {
            continue;
        };
        if reference.allocation_seq() != candidate.allocation_seq {
            continue;
        }
        let Ok(key) = pool.validate_persistent_data_ref(reference) else {
            continue;
        };
        if key.fs_id != fs_id {
            let _ = pool.enqueue_dirty_nvpage(candidate.page, candidate.allocation_seq);
            continue;
        }
        if pool.migration_is_frozen(key) {
            let _ = pool.enqueue_dirty_nvpage(candidate.page, candidate.allocation_seq);
            continue;
        }
        let Ok(Some(reverse)) = pool.reverse_lookup(reference) else {
            // A local extent mutation may be retargeting this locator. Keep
            // the exact generation discoverable and let the next pass retry
            // after the per-inode publication gate is released.
            let _ = pool.enqueue_dirty_nvpage(candidate.page, candidate.allocation_seq);
            continue;
        };
        if !seen.insert((key.inode, reverse.leaf_slot(), reverse.entry_index())) {
            continue;
        }
        out.push(RnvpceFlushCandidate {
            inode: key.inode,
            page_index: key.page_index,
            page: candidate.page,
            reference,
            reverse,
        });
    }
    out
}

/// rTIO v0：一页的 SSD 写已完成，推进 completion（两条路线各自退休/回收）。
///
/// `flushed_page`/`flushed_generation` 一起标识刚写到 SSD 的那一版（页 id 会被复用，
/// 所以必须带代次）；回收只发生在各自的 completion 里。
pub(crate) fn prepare_flush_completion(
    fs_id: u64,
    inode: u64,
    page_index: u64,
    page: NvPageId,
    generation: u64,
) -> Result<bool> {
    let context = existing_cow_context(fs_id, inode).ok_or_else(|| Error::new(Errno::ENOENT))?;
    context.prepare_flush_completion(page_index, page, generation)
}

pub(crate) fn finalize_flush_completion(
    fs_id: u64,
    inode: u64,
    page: NvPageId,
    generation: u64,
) -> Result<()> {
    let context = existing_cow_context(fs_id, inode).ok_or_else(|| Error::new(Errno::ENOENT))?;
    context.finalize_flush_completion(page, generation)
}

/// I4：聚合所有 inode 上下文的计数。池未配置（`Native` 挂载）时全 0。
pub(crate) fn snapshot() -> RnvpcSnapshot {
    let mut snap = RnvpcSnapshot::default();
    if let Some(table) = COW_CONTEXTS.get() {
        let table = table.lock();
        snap.contexts = table.len();
        for context in table.values() {
            let cow = context.counters();
            snap.cow.updates += cow.updates;
            snap.cow.cow_alloc_copy_ns += cow.cow_alloc_copy_ns;
            snap.cow.wal_backpressure_waits += cow.wal_backpressure_waits;
            snap.cow.old_pages_freed += cow.old_pages_freed;

            let wal = context.wal_counters();
            snap.wal.appended += wal.appended;
            snap.wal.retired += wal.retired;
            snap.wal.append_ns += wal.append_ns;
            snap.wal.full_wait_ns += wal.full_wait_ns;
            snap.wal.full_wait_events += wal.full_wait_events;
            snap.wal_pending += context.wal().pending();
            let checkpoint = context.foreground_checkpoint_counters();
            snap.checkpoint.wal_appends += checkpoint.wal_appends;
            snap.checkpoint.checkpoint_count += checkpoint.checkpoint_count;
            snap.checkpoint.checkpoint_records += checkpoint.checkpoint_records;
            snap.checkpoint.btree_updates += checkpoint.btree_updates;
            snap.checkpoint.btree_splits += checkpoint.btree_splits;
            snap.checkpoint.persist_fences += checkpoint.persist_fences;
            snap.checkpoint.checkpoint_blocked_writes += checkpoint.checkpoint_blocked_writes;
            snap.checkpoint.wal_max_used =
                snap.checkpoint.wal_max_used.max(checkpoint.wal_max_used);
        }
    }
    if let Some(table) = EXTEND_CONTEXTS.get() {
        let table = table.lock();
        snap.extend_contexts = table.len();
        for context in table.values() {
            let extend = context.counters();
            snap.extend.retires += extend.retires;
            snap.extend.stage_ns += extend.stage_ns;
            snap.extend.commit_ns += extend.commit_ns;
            snap.extend.publish_ns += extend.publish_ns;
            snap.extend.in_flight_waits += extend.in_flight_waits;
            snap.extend.data_waits += extend.data_waits;
            snap.extend.meta_waits += extend.meta_waits;
            snap.extend.nvbh_alloc_ns += extend.nvbh_alloc_ns;
            snap.extend.pages_reclaimed += extend.pages_reclaimed;
            snap.extend.stage_retries += extend.stage_retries;
            snap.extend.stale_skips += extend.stale_skips;
            snap.extend_resident += context.resident_pages();
            snap.extend_in_flight += context.in_flight();
        }
    }
    snap
}

#[cfg(ktest)]
mod persistent_tests;

#[cfg(ktest)]
mod extent_inplace_tests;
