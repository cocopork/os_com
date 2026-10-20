// SPDX-License-Identifier: MPL-2.0

//! Recovered optional latency instrumentation; disabled in throughput runs.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub(crate) const LATENCY_ENABLED: bool = false;

/// 当前单调时钟（TSC），纳秒。仅用于延迟区间测量（单调、不回退）。
/// 关闭时编译期返回 0，调用点随之成为死代码被消除。
#[inline]
pub(crate) fn read_ns() -> u64 {
    if LATENCY_ENABLED {
        aster_time::read_monotonic_time().as_nanos() as u64
    } else {
        0
    }
}

/// 单个延迟阶段的累计统计：样本数 / 纳秒和 / 最大纳秒。
pub(crate) struct LatencyStat {
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl LatencyStat {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// 记录「从 `t0_ns` 到现在」的延迟。关闭时直接返回（编译期消除）。
    #[inline]
    pub(crate) fn record_since(&self, t0_ns: u64) {
        if !LATENCY_ENABLED {
            return;
        }
        self.record_ns(read_ns().saturating_sub(t0_ns));
    }

    /// 只累计**次数**（用于事件计数而非耗时，例如池 EAGAIN 重试次数、回刷跳过次数）。
    /// `sum_ns` / `max_ns` 保持 0，宿主侧只看 `count`。
    #[inline]
    pub(crate) fn record_event(&self) {
        if !LATENCY_ENABLED {
            return;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// 一次记 `n` 个事件：合并大 BIO 时按**页数**记账，省掉逐页的原子加。
    #[inline]
    pub(crate) fn record_events(&self, n: usize) {
        if !LATENCY_ENABLED || n == 0 {
            return;
        }
        self.count.fetch_add(n as u64, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn record_ns(&self, ns: u64) {
        if !LATENCY_ENABLED {
            return;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        // CAS 更新 max（不依赖 `fetch_max`，兼容性更好）。
        let mut cur = self.max_ns.load(Ordering::Relaxed);
        while ns > cur {
            match self
                .max_ns
                .compare_exchange_weak(cur, ns, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }
}

pub(crate) struct LatencyStats {
    pub(crate) extend_commit: LatencyStat,
    pub(crate) extend_complete: LatencyStat,
    pub(crate) extend_data_wait: LatencyStat,
    pub(crate) extend_in_flight_wait: LatencyStat,
    pub(crate) extend_meta_wait: LatencyStat,
    pub(crate) extend_retire_total: LatencyStat,
    pub(crate) fg_allocate: LatencyStat,
    pub(crate) fg_body: LatencyStat,
    pub(crate) fg_copy: LatencyStat,
    pub(crate) fg_data: LatencyStat,
    pub(crate) fg_data_init: LatencyStat,
    pub(crate) fg_data_register: LatencyStat,
    pub(crate) fg_enqueue: LatencyStat,
    pub(crate) fg_extent: LatencyStat,
    pub(crate) fg_inode_publish: LatencyStat,
    pub(crate) fg_inode_read: LatencyStat,
    pub(crate) fg_lookup: LatencyStat,
    pub(crate) fg_repair_apply: LatencyStat,
    pub(crate) fg_repair_cleanup: LatencyStat,
    pub(crate) fg_repair_image: LatencyStat,
    pub(crate) fg_repair_mark: LatencyStat,
    pub(crate) fg_repair_verify: LatencyStat,
    pub(crate) fg_reserve: LatencyStat,
    pub(crate) fg_reserve_lookup: LatencyStat,
    pub(crate) fg_reserve_update: LatencyStat,
    pub(crate) fg_reverse_refresh: LatencyStat,
    pub(crate) fg_ssd_read: LatencyStat,
    pub(crate) leaf_detail_construct: LatencyStat,
    pub(crate) leaf_detail_entries: LatencyStat,
    pub(crate) leaf_detail_identity: LatencyStat,
    pub(crate) leaf_detail_pending: LatencyStat,
    pub(crate) leaf_detail_read: LatencyStat,
    pub(crate) leaf_detail_total: LatencyStat,
    pub(crate) leaf_detail_update: LatencyStat,
    pub(crate) leaf_detail_validate: LatencyStat,
    pub(crate) leaf_pending_index_hold: LatencyStat,
    pub(crate) leaf_pending_index_wait: LatencyStat,
    pub(crate) leaf_pending_read: LatencyStat,
    pub(crate) leaf_pending_retry: LatencyStat,
    pub(crate) leaf_pending_slot_wait: LatencyStat,
    pub(crate) leaf_update_buffers: LatencyStat,
    pub(crate) leaf_update_copy: LatencyStat,
    pub(crate) leaf_update_exclusive: LatencyStat,
    pub(crate) leaf_update_finalize: LatencyStat,
    pub(crate) leaf_update_notify: LatencyStat,
    pub(crate) leaf_update_old_read: LatencyStat,
    pub(crate) leaf_update_old_validate: LatencyStat,
    pub(crate) leaf_update_persist: LatencyStat,
    pub(crate) leaf_update_validate: LatencyStat,
    pub(crate) leaf_update_write: LatencyStat,
    pub(crate) migration_busy_completion: LatencyStat,
    pub(crate) migration_busy_foreground: LatencyStat,
    pub(crate) migration_credit: LatencyStat,
    pub(crate) migration_leaf: LatencyStat,
    pub(crate) migration_leaf_exclusive: LatencyStat,
    pub(crate) migration_leaf_lock_wait: LatencyStat,
    pub(crate) migration_owner_lock_wait: LatencyStat,
    pub(crate) migration_reservation: LatencyStat,
    pub(crate) migration_reservation_exclusive: LatencyStat,
    pub(crate) migration_reservation_lock_wait: LatencyStat,
    pub(crate) migration_reservation_lookup: LatencyStat,
    pub(crate) migration_reservation_update: LatencyStat,
    pub(crate) migration_slot_wait: LatencyStat,
    pub(crate) migration_validate: LatencyStat,
    pub(crate) nvpc_alloc_nvbh: LatencyStat,
    pub(crate) nvpc_dirty_mark: LatencyStat,
    pub(crate) nvpc_eagain: LatencyStat,
    pub(crate) nvpc_extend_publish: LatencyStat,
    pub(crate) nvpc_finish_clear_descriptor: LatencyStat,
    pub(crate) nvpc_finish_finalize_lock: LatencyStat,
    pub(crate) nvpc_finish_lease_wake: LatencyStat,
    pub(crate) nvpc_finish_validate_lock: LatencyStat,
    pub(crate) nvpc_index: LatencyStat,
    pub(crate) nvpc_old_read: LatencyStat,
    pub(crate) nvpc_publish: LatencyStat,
    pub(crate) nvpc_retire_lock: LatencyStat,
    pub(crate) nvpc_retire_total: LatencyStat,
    pub(crate) nvpc_stage: LatencyStat,
    pub(crate) nvpc_transition: LatencyStat,
    pub(crate) nvpc_wal: LatencyStat,
    pub(crate) nvpc_wal_append: LatencyStat,
    pub(crate) reclaim_allocation_wait: LatencyStat,
    pub(crate) reclaim_capacity: LatencyStat,
    pub(crate) reclaim_reset: LatencyStat,
    pub(crate) reclaim_slot_wait: LatencyStat,
    pub(crate) reclaim_unlock: LatencyStat,
    pub(crate) reclaim_unpin: LatencyStat,
    pub(crate) reservation_index_wait_all: LatencyStat,
    pub(crate) reservation_slot_wait_all: LatencyStat,
    pub(crate) rnvpce_completion_inner_hold: LatencyStat,
    pub(crate) rnvpce_completion_inner_wait: LatencyStat,
    pub(crate) rnvpce_completion_leaf_updates: LatencyStat,
    pub(crate) rnvpce_metadata_reader_wait: LatencyStat,
    pub(crate) rnvpce_publication_hold: LatencyStat,
    pub(crate) rnvpce_publication_wait: LatencyStat,
    pub(crate) rnvpce_reclaim_state_lock: LatencyStat,
    pub(crate) rnvpce_write_inner_wait: LatencyStat,
}
pub(crate) static LATENCY_STATS: LatencyStats = LatencyStats {
    extend_commit: LatencyStat::new(),
    extend_complete: LatencyStat::new(),
    extend_data_wait: LatencyStat::new(),
    extend_in_flight_wait: LatencyStat::new(),
    extend_meta_wait: LatencyStat::new(),
    extend_retire_total: LatencyStat::new(),
    fg_allocate: LatencyStat::new(),
    fg_body: LatencyStat::new(),
    fg_copy: LatencyStat::new(),
    fg_data: LatencyStat::new(),
    fg_data_init: LatencyStat::new(),
    fg_data_register: LatencyStat::new(),
    fg_enqueue: LatencyStat::new(),
    fg_extent: LatencyStat::new(),
    fg_inode_publish: LatencyStat::new(),
    fg_inode_read: LatencyStat::new(),
    fg_lookup: LatencyStat::new(),
    fg_repair_apply: LatencyStat::new(),
    fg_repair_cleanup: LatencyStat::new(),
    fg_repair_image: LatencyStat::new(),
    fg_repair_mark: LatencyStat::new(),
    fg_repair_verify: LatencyStat::new(),
    fg_reserve: LatencyStat::new(),
    fg_reserve_lookup: LatencyStat::new(),
    fg_reserve_update: LatencyStat::new(),
    fg_reverse_refresh: LatencyStat::new(),
    fg_ssd_read: LatencyStat::new(),
    leaf_detail_construct: LatencyStat::new(),
    leaf_detail_entries: LatencyStat::new(),
    leaf_detail_identity: LatencyStat::new(),
    leaf_detail_pending: LatencyStat::new(),
    leaf_detail_read: LatencyStat::new(),
    leaf_detail_total: LatencyStat::new(),
    leaf_detail_update: LatencyStat::new(),
    leaf_detail_validate: LatencyStat::new(),
    leaf_pending_index_hold: LatencyStat::new(),
    leaf_pending_index_wait: LatencyStat::new(),
    leaf_pending_read: LatencyStat::new(),
    leaf_pending_retry: LatencyStat::new(),
    leaf_pending_slot_wait: LatencyStat::new(),
    leaf_update_buffers: LatencyStat::new(),
    leaf_update_copy: LatencyStat::new(),
    leaf_update_exclusive: LatencyStat::new(),
    leaf_update_finalize: LatencyStat::new(),
    leaf_update_notify: LatencyStat::new(),
    leaf_update_old_read: LatencyStat::new(),
    leaf_update_old_validate: LatencyStat::new(),
    leaf_update_persist: LatencyStat::new(),
    leaf_update_validate: LatencyStat::new(),
    leaf_update_write: LatencyStat::new(),
    migration_busy_completion: LatencyStat::new(),
    migration_busy_foreground: LatencyStat::new(),
    migration_credit: LatencyStat::new(),
    migration_leaf: LatencyStat::new(),
    migration_leaf_exclusive: LatencyStat::new(),
    migration_leaf_lock_wait: LatencyStat::new(),
    migration_owner_lock_wait: LatencyStat::new(),
    migration_reservation: LatencyStat::new(),
    migration_reservation_exclusive: LatencyStat::new(),
    migration_reservation_lock_wait: LatencyStat::new(),
    migration_reservation_lookup: LatencyStat::new(),
    migration_reservation_update: LatencyStat::new(),
    migration_slot_wait: LatencyStat::new(),
    migration_validate: LatencyStat::new(),
    nvpc_alloc_nvbh: LatencyStat::new(),
    nvpc_dirty_mark: LatencyStat::new(),
    nvpc_eagain: LatencyStat::new(),
    nvpc_extend_publish: LatencyStat::new(),
    nvpc_finish_clear_descriptor: LatencyStat::new(),
    nvpc_finish_finalize_lock: LatencyStat::new(),
    nvpc_finish_lease_wake: LatencyStat::new(),
    nvpc_finish_validate_lock: LatencyStat::new(),
    nvpc_index: LatencyStat::new(),
    nvpc_old_read: LatencyStat::new(),
    nvpc_publish: LatencyStat::new(),
    nvpc_retire_lock: LatencyStat::new(),
    nvpc_retire_total: LatencyStat::new(),
    nvpc_stage: LatencyStat::new(),
    nvpc_transition: LatencyStat::new(),
    nvpc_wal: LatencyStat::new(),
    nvpc_wal_append: LatencyStat::new(),
    reclaim_allocation_wait: LatencyStat::new(),
    reclaim_capacity: LatencyStat::new(),
    reclaim_reset: LatencyStat::new(),
    reclaim_slot_wait: LatencyStat::new(),
    reclaim_unlock: LatencyStat::new(),
    reclaim_unpin: LatencyStat::new(),
    reservation_index_wait_all: LatencyStat::new(),
    reservation_slot_wait_all: LatencyStat::new(),
    rnvpce_completion_inner_hold: LatencyStat::new(),
    rnvpce_completion_inner_wait: LatencyStat::new(),
    rnvpce_completion_leaf_updates: LatencyStat::new(),
    rnvpce_metadata_reader_wait: LatencyStat::new(),
    rnvpce_publication_hold: LatencyStat::new(),
    rnvpce_publication_wait: LatencyStat::new(),
    rnvpce_reclaim_state_lock: LatencyStat::new(),
    rnvpce_write_inner_wait: LatencyStat::new(),
};
static LATENCY_DUMPED: AtomicBool = AtomicBool::new(false);
pub(crate) fn dump_latency_stats_once() {
    if LATENCY_ENABLED
        && LATENCY_STATS.fg_body.count.load(Ordering::Relaxed) != 0
        && !LATENCY_DUMPED.swap(true, Ordering::Relaxed)
    {
        dump_latency_stats();
    }
}
pub(crate) fn dump_latency_stats() {
    if !LATENCY_ENABLED {
        return;
    }
    ostd::error!(
        "[LATENCY] extend_commit count={} sum_ns={} max_ns={}",
        LATENCY_STATS.extend_commit.count.load(Ordering::Relaxed),
        LATENCY_STATS.extend_commit.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.extend_commit.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] extend_complete count={} sum_ns={} max_ns={}",
        LATENCY_STATS.extend_complete.count.load(Ordering::Relaxed),
        LATENCY_STATS.extend_complete.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.extend_complete.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] extend_data_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS.extend_data_wait.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_data_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_data_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] extend_in_flight_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .extend_in_flight_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_in_flight_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_in_flight_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] extend_meta_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS.extend_meta_wait.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_meta_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_meta_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] extend_retire_total count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .extend_retire_total
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_retire_total
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .extend_retire_total
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_allocate count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_allocate.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_allocate.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_allocate.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_body count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_body.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_body.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_body.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_copy count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_copy.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_copy.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_copy.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_data count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_data.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_data.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_data.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_data_init count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_data_init.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_data_init.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_data_init.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_data_register count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_data_register.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_data_register
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_data_register
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_enqueue count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_enqueue.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_enqueue.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_enqueue.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_extent count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_extent.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_extent.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_extent.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_inode_publish count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_inode_publish.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_inode_publish
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_inode_publish
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_inode_read count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_inode_read.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_inode_read.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_inode_read.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_lookup count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_lookup.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_lookup.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_lookup.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_repair_apply count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_repair_apply.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_repair_apply.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_repair_apply.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_repair_cleanup count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .fg_repair_cleanup
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_repair_cleanup
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_repair_cleanup
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_repair_image count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_repair_image.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_repair_image.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_repair_image.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_repair_mark count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_repair_mark.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_repair_mark.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_repair_mark.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_repair_verify count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_repair_verify.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_repair_verify
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_repair_verify
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_reserve count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_reserve.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_reserve.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_reserve.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_reserve_lookup count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .fg_reserve_lookup
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_reserve_lookup
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_reserve_lookup
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_reserve_update count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .fg_reserve_update
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_reserve_update
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_reserve_update
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_reverse_refresh count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .fg_reverse_refresh
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_reverse_refresh
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .fg_reverse_refresh
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] fg_ssd_read count={} sum_ns={} max_ns={}",
        LATENCY_STATS.fg_ssd_read.count.load(Ordering::Relaxed),
        LATENCY_STATS.fg_ssd_read.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.fg_ssd_read.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_construct count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_detail_construct
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_construct
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_construct
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_entries count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_detail_entries
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_entries
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_entries
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_identity count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_detail_identity
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_identity
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_identity
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_pending count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_detail_pending
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_pending
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_pending
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_read count={} sum_ns={} max_ns={}",
        LATENCY_STATS.leaf_detail_read.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_read
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_read
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_total count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_detail_total
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_total
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_total
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_update count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_detail_update
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_update
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_update
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_detail_validate count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_detail_validate
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_validate
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_detail_validate
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_pending_index_hold count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_pending_index_hold
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_index_hold
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_index_hold
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_pending_index_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_pending_index_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_index_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_index_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_pending_read count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_pending_read
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_read
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_read
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_pending_retry count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_pending_retry
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_retry
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_retry
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_pending_slot_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_pending_slot_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_slot_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_pending_slot_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_buffers count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_buffers
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_buffers
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_buffers
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_copy count={} sum_ns={} max_ns={}",
        LATENCY_STATS.leaf_update_copy.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_copy
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_copy
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_exclusive count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_exclusive
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_exclusive
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_exclusive
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_finalize count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_finalize
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_finalize
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_finalize
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_notify count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_notify
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_notify
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_notify
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_old_read count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_old_read
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_old_read
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_old_read
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_old_validate count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_old_validate
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_old_validate
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_old_validate
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_persist count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_persist
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_persist
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_persist
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_validate count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_validate
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_validate
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_validate
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] leaf_update_write count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .leaf_update_write
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_write
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .leaf_update_write
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_busy_completion count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_busy_completion
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_busy_completion
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_busy_completion
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_busy_foreground count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_busy_foreground
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_busy_foreground
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_busy_foreground
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_credit count={} sum_ns={} max_ns={}",
        LATENCY_STATS.migration_credit.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_credit
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_credit
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_leaf count={} sum_ns={} max_ns={}",
        LATENCY_STATS.migration_leaf.count.load(Ordering::Relaxed),
        LATENCY_STATS.migration_leaf.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.migration_leaf.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_leaf_exclusive count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_leaf_exclusive
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_leaf_exclusive
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_leaf_exclusive
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_leaf_lock_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_leaf_lock_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_leaf_lock_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_leaf_lock_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_owner_lock_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_owner_lock_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_owner_lock_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_owner_lock_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_reservation count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_reservation
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_reservation_exclusive count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_reservation_exclusive
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_exclusive
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_exclusive
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_reservation_lock_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_reservation_lock_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_lock_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_lock_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_reservation_lookup count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_reservation_lookup
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_lookup
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_lookup
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_reservation_update count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_reservation_update
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_update
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_reservation_update
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_slot_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_slot_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_slot_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_slot_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] migration_validate count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .migration_validate
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_validate
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .migration_validate
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_alloc_nvbh count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_alloc_nvbh.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_alloc_nvbh.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_alloc_nvbh.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_dirty_mark count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_dirty_mark.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_dirty_mark.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_dirty_mark.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_eagain count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_eagain.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_eagain.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_eagain.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_extend_publish count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .nvpc_extend_publish
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_extend_publish
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_extend_publish
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_finish_clear_descriptor count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .nvpc_finish_clear_descriptor
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_clear_descriptor
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_clear_descriptor
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_finish_finalize_lock count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .nvpc_finish_finalize_lock
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_finalize_lock
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_finalize_lock
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_finish_lease_wake count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .nvpc_finish_lease_wake
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_lease_wake
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_lease_wake
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_finish_validate_lock count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .nvpc_finish_validate_lock
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_validate_lock
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_finish_validate_lock
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_index count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_index.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_index.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_index.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_old_read count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_old_read.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_old_read.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_old_read.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_publish count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_publish.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_publish.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_publish.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_retire_lock count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_retire_lock.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_retire_lock
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_retire_lock
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_retire_total count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .nvpc_retire_total
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_retire_total
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .nvpc_retire_total
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_stage count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_stage.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_stage.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_stage.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_transition count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_transition.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_transition.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_transition.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_wal count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_wal.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_wal.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_wal.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] nvpc_wal_append count={} sum_ns={} max_ns={}",
        LATENCY_STATS.nvpc_wal_append.count.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_wal_append.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.nvpc_wal_append.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reclaim_allocation_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .reclaim_allocation_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reclaim_allocation_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reclaim_allocation_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reclaim_capacity count={} sum_ns={} max_ns={}",
        LATENCY_STATS.reclaim_capacity.count.load(Ordering::Relaxed),
        LATENCY_STATS
            .reclaim_capacity
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reclaim_capacity
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reclaim_reset count={} sum_ns={} max_ns={}",
        LATENCY_STATS.reclaim_reset.count.load(Ordering::Relaxed),
        LATENCY_STATS.reclaim_reset.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.reclaim_reset.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reclaim_slot_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .reclaim_slot_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reclaim_slot_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reclaim_slot_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reclaim_unlock count={} sum_ns={} max_ns={}",
        LATENCY_STATS.reclaim_unlock.count.load(Ordering::Relaxed),
        LATENCY_STATS.reclaim_unlock.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.reclaim_unlock.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reclaim_unpin count={} sum_ns={} max_ns={}",
        LATENCY_STATS.reclaim_unpin.count.load(Ordering::Relaxed),
        LATENCY_STATS.reclaim_unpin.sum_ns.load(Ordering::Relaxed),
        LATENCY_STATS.reclaim_unpin.max_ns.load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reservation_index_wait_all count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .reservation_index_wait_all
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reservation_index_wait_all
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reservation_index_wait_all
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] reservation_slot_wait_all count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .reservation_slot_wait_all
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reservation_slot_wait_all
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .reservation_slot_wait_all
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_completion_inner_hold count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_completion_inner_hold
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_completion_inner_hold
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_completion_inner_hold
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_completion_inner_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_completion_inner_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_completion_inner_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_completion_inner_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_completion_leaf_updates count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_completion_leaf_updates
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_completion_leaf_updates
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_completion_leaf_updates
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_metadata_reader_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_metadata_reader_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_metadata_reader_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_metadata_reader_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_publication_hold count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_publication_hold
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_publication_hold
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_publication_hold
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_publication_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_publication_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_publication_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_publication_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_reclaim_state_lock count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_reclaim_state_lock
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_reclaim_state_lock
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_reclaim_state_lock
            .max_ns
            .load(Ordering::Relaxed)
    );
    ostd::error!(
        "[LATENCY] rnvpce_write_inner_wait count={} sum_ns={} max_ns={}",
        LATENCY_STATS
            .rnvpce_write_inner_wait
            .count
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_write_inner_wait
            .sum_ns
            .load(Ordering::Relaxed),
        LATENCY_STATS
            .rnvpce_write_inner_wait
            .max_ns
            .load(Ordering::Relaxed)
    );
}
const COMPLETION_IDLE: u64 = 0;
const COMPLETION_QUEUE_DRAIN: u64 = 1;
const COMPLETION_MATERIALIZE: u64 = 2;
const COMPLETION_DURABILITY: u64 = 3;
const COMPLETION_MIGRATE: u64 = 4;
const COMPLETION_RELEASE_BLOCKS: u64 = 5;
const COMPLETION_FINISH_PUBLISH: u64 = 6;
const COMPLETION_FREE_PAGE: u64 = 7;
pub(crate) const COMPLETION_FINISH_REFERENCE_CHECK: u64 = 8;
pub(crate) const COMPLETION_FINISH_VALIDATE_LOCK: u64 = 9;
pub(crate) const COMPLETION_FINISH_CLEAR_DESCRIPTOR: u64 = 10;
pub(crate) const COMPLETION_FINISH_FINALIZE_LOCK: u64 = 11;
pub(crate) const COMPLETION_FINISH_LEASE_WAKE: u64 = 12;

static COMPLETION_STAGE: AtomicU64 = AtomicU64::new(COMPLETION_IDLE);
static COMPLETION_STAGE_STARTED_NS: AtomicU64 = AtomicU64::new(0);
static COMPLETION_STAGE_INODE: AtomicU64 = AtomicU64::new(0);
static COMPLETION_STAGE_PAGE: AtomicU64 = AtomicU64::new(0);
static COMPLETION_STALL_REPORTED_NS: AtomicU64 = AtomicU64::new(0);

#[inline]
pub(crate) fn set_completion_stage(stage: u64, inode: u64, page: u64) {
    if !LATENCY_ENABLED {
        return;
    }
    COMPLETION_STAGE_INODE.store(inode, Ordering::Relaxed);
    COMPLETION_STAGE_PAGE.store(page, Ordering::Relaxed);
    COMPLETION_STAGE_STARTED_NS.store(read_ns(), Ordering::Relaxed);
    COMPLETION_STALL_REPORTED_NS.store(0, Ordering::Relaxed);
    COMPLETION_STAGE.store(stage, Ordering::Release);
}

pub(crate) fn clear_completion_stage() {
    if LATENCY_ENABLED {
        COMPLETION_STAGE.store(COMPLETION_IDLE, Ordering::Release);
    }
}
