// SPDX-License-Identifier: MPL-2.0

//! I5：rNVPCe（挂载 `rnvpc_extend=on`）的 soft-update 写路径 —— **不含任何 WAL**。
//!
//! 依据 `rNVPC/design.md §4`（持久数据面 / DRAM 控制面分离）、`§5`（soft-update 写
//! 路线）、`§5.1`（七步发布顺序）与 `rNVPC-rTIO/ext4-io-integration.md §1.2`：
//!
//! ```text
//! 1 预留 NV 容量（数据页 + 引用更新）
//! 2 分配 NVPage + 必要 NVBH
//! 3 写 NVPage 数据（Staged -> NvDirty）
//! 4 更新 NVBH，登记引用但**不发布**
//! 5 NVPage 有效后**发布**引用      ← "数据先于引用"
//! 6 持久化上层元数据（= 现有 ext4 元数据路径，本模块不参与）
//! 7 **撤销旧引用之后**旧页才可回收  ← "撤销先于回收"
//! ```
//!
//! 与 [`InodeCowContext`](super::cow::InodeCowContext) 的关系：**并列、不共用**。
//! rNVPC 用 per-inode WAL + CoW 提供一致性（WAL 就是它的正确性来源），rNVPCe 用
//! soft update 提供一致性，**不需要 WAL**。因此本模块：
//!
//! - 没有 `wal`、没有 `charge_wal_delta`、没有 `WAL_CAPACITY_PER_INODE`；
//! - 数据单元是 **NVPage（数据面）+ NVBH（控制面）分离**，不是 rNVPC 的「NVPage +
//!   WAL op-log」；
//! - 回压额度是**未落 SSD 的 NV 数据页数**（论文 rTIO 的 in-flight budget），不是
//!   WAL 记录条数；
//! - 生命周期状态直接用池的 [`NvObjectState`]（`descriptor.rs`），不另造平行枚举
//!   （平行状态必然引入两套状态的同步 bug，见 `ext4_performance.md` §3 的"影子索引
//!   反模式"）。
//!
//! 在飞账本的不变式：`in_flight` = 本上下文分配、**尚未归还给池**的 NV 数据页数。
//! 页只在两种时刻归还：① 它被写回 SSD 且 completion 到达；② 它被更新版本取代
//! （旧版本内容已无意义）。两种情况都意味着"这一版不再需要落 SSD"。

use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::sync::WaitQueue;
use spin::Mutex;

use super::{
    data_page::DataPageState,
    descriptor::NvObjectState,
    frame::{PublicationGuard, Rnvpc},
    id::{NvFrameId, NvPageId},
    index::PageKey,
    wal::now_ns,
};
use crate::prelude::*;

/// rNVPCe in-flight 额度的**全局**口径说明。
///
/// 额度必须是**全池**而不是 per-inode：fio 的多 job 各有自己的 `ExtendCowContext`，
/// per-inode 额度会允许 N 倍超额，直接把池撑到 `ENOSPC`（首轮实验就是这么 abort 的）。
/// 因此准入判据取池统计里的 `data_pages`（= 池内已分配的数据页数，即"未落 SSD 的
/// NV 数据页数"）与构造时算好的 `data_budget` 比较，见
/// `ExtendCowContext::has_capacity_room`。
const DATA_BUDGET_DIVISOR: usize = 3;

/// 一个 `page_index` 的当前版本（DRAM 控制面）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExtendEntry {
    /// 数据面：当前 NV 数据页。
    page: NvPageId,
    /// `page` 的**分配序号**（ABA 防护）：槽位会被回收复用，只有序号相等才说明
    /// "这一版仍是我分配的那个对象"。
    page_seq: u64,
    /// 控制面：持有该页引用的 NVBH（Metadata 类 frame）。
    nvbh: NvFrameId,
    /// `nvbh` 的分配序号（同上）。
    nvbh_seq: u64,
    /// 该版本写回 SSD 的目标块；0 = 尚未确定（写回路径回填）。
    ssd_target: u64,
    /// 版本代次（每次退休新版本时单调递增）。页 id 会被回收复用（ABA），
    /// 完成事件必须用 `(page, generation)` 二元组判定，不能只比页 id。
    generation: u64,
    /// 该版本在持久侧的生命周期状态（`design.md §4.4` / §5.2）。
    state: NvObjectState,
    /// 是否已具备回刷条件（`ssd_target != 0`）。只有 `dirty` 的条目才是回刷候选。
    dirty: bool,
}

#[derive(Default)]
struct ExtendIndex {
    entries: BTreeMap<u64, ExtendEntry>,
    next_generation: u64,
}

/// rNVPCe 的成本计数器。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExtendCounters {
    /// 成功的退休次数（每次退休 = 一个新 NV 版本）。
    pub(crate) retires: u64,
    /// 数据页 staging（alloc + 写内容）累计耗时（纳秒）。
    pub(crate) stage_ns: u64,
    /// 数据页持久提交（`replace_data_page_in_owner`）累计耗时。
    pub(crate) commit_ns: u64,
    /// 引用发布（soft-update 第 5 步）累计耗时。
    pub(crate) publish_ns: u64,
    /// 因 in-flight 额度耗尽而等待的次数（数据类 + 元数据类的总和）。
    pub(crate) in_flight_waits: u64,
    /// 其中来自**数据页 staging** 循环的等待次数（`EAGAIN` 数据硬上限 / `ENOSPC`）。
    pub(crate) data_waits: u64,
    /// 其中来自**控制面 NVBH 分配**循环的等待次数。
    ///
    /// 每个退休版本同时占 1 个数据槽位 + 1 个 NVBH 槽位，而槽位归还只能等
    /// SSD 完成链；因此这一项才是"前台在 `write()` 里被容量卡住"的主要来源
    /// （对比 `data_waits` 即可判定卡的是数据面还是控制面）。
    pub(crate) meta_waits: u64,
    /// 归还给池的 NV 数据页数（= 写回完成 + 被取代）。
    pub(crate) pages_reclaimed: u64,
    /// 因池容量吃紧而重试 staging 的次数（可观测口径）。
    pub(crate) stage_retries: u64,
    /// NVBH（控制面）分配 + 其回压等待的累计耗时（纳秒）。
    pub(crate) nvbh_alloc_ns: u64,
    /// 因**槽位已被复用**（ABA）而跳过的陈旧条目/待回收项数。
    ///
    /// 不变式上它应当恒为 0；非 0 说明某个索引在归还之前就被重新分配了
    /// （见 `frame.rs` 的 `alloc_seq`）。
    pub(crate) stale_skips: u64,
}

/// 一次 try-release 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReleaseOutcome {
    /// 已归还给池（账已记）。
    Freed,
    /// 槽位已经不是这一版（已被别的路径归还，甚至被复用）⇒ 丢弃该待回收项。
    Stale,
    /// 暂时还不了（引用尚未撤销 / 仍被回刷线程独占）⇒ 留在队列里重试。
    Retry,
}

/// 一次 rNVPCe 退休的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtendOutcome {
    pub(crate) page_index: u64,
    /// 退休后的当前 NV 版本。
    pub(crate) new_page: NvPageId,
    /// 被取代的旧版本；`None` = 该 `page_index` 此前无 NV 版本。
    pub(crate) replaced: Option<NvPageId>,
}

/// 一个 inode 的 rNVPCe 上下文（**无 WAL**）。
pub(crate) struct ExtendCowContext {
    inode: u64,
    /// 本挂载的 `PageKey::fs_id`（superblock UUID 低 64 位）。
    fs_id: u64,
    pool: Arc<Rnvpc>,
    /// `page_index` → 当前版本。
    index: Mutex<ExtendIndex>,
    /// **在飞账本**：已退休、未落 SSD、尚未归还的 NV 数据页数（可观测口径；
    /// 验收不变式：上下文生命周期内最终回 0）。
    in_flight: AtomicUsize,
    /// 准入额度（构造时按池水线折算，只读）：池内已分配数据页数超过它即拒绝新的
    /// 前台退休。**全局口径**（见 `DATA_BUDGET_DIVISOR`）。
    data_budget: usize,
    /// in-flight 降到阈值以下的等待者。
    waiters: WaitQueue,
    /// 等 completion 回收的旧版本：`(page_index, page, page_seq)`。
    pending_old_pages: Mutex<Vec<(u64, NvPageId, u64)>>,
    counters: Mutex<ExtendCounters>,
}

impl ExtendCowContext {
    /// 建立上下文。准入额度取池水线的 1/3：每个驻留版本同时占 1 个数据页 + 1 个
    /// NVBH frame（真实占用 ≈ 2× 数据页数），留出 3× 余量才能保证"容量准入"一定
    /// 在池被撑满之前生效。
    pub(crate) fn new(pool: Arc<Rnvpc>, inode: u64, fs_id: u64) -> Arc<Self> {
        let data_budget = (pool.stats().high_water_pages / DATA_BUDGET_DIVISOR).max(64);
        Arc::new(Self {
            inode,
            fs_id,
            pool,
            index: Mutex::new(ExtendIndex::default()),
            in_flight: AtomicUsize::new(0),
            data_budget,
            waiters: WaitQueue::new(),
            pending_old_pages: Mutex::new(Vec::new()),
            counters: Mutex::new(ExtendCounters::default()),
        })
    }

    pub(crate) fn inode(&self) -> u64 {
        self.inode
    }

    pub(crate) fn fs_id(&self) -> u64 {
        self.fs_id
    }

    pub(crate) fn counters(&self) -> ExtendCounters {
        *self.counters.lock()
    }

    /// 在飞的 NV 数据页数（未落 SSD、尚未归还）。
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// 准入额度（池内已分配数据页数的上限，全局口径）。
    pub(crate) fn data_budget(&self) -> usize {
        self.data_budget
    }

    /// **前台 `write()` 的准入判据**：池里还有余量接受一个新的 NV 版本吗？
    ///
    /// 池接近满时必须**跳过** `write()` 退休（数据仍在页缓存，写回路径会用整页快照
    /// 兜底退休），而不是在容量上等待：这样"未落 SSD 的 NV 数据页数"不会把池撑满，
    /// 回刷线程始终有腾挪空间。
    ///
    /// 写回路径**不适用**本判据：它带着 SSD 落点（必然可回刷），因此用
    /// `stage_data_page_with_backpressure` 的 `EAGAIN` 自轮询等容量即可。
    pub(crate) fn has_capacity_room(&self) -> bool {
        self.pool.stats().data_pages < self.data_budget
    }

    /// 驻留（有 NV 版本）的页数。
    pub(crate) fn resident_pages(&self) -> usize {
        self.index.lock().entries.len()
    }

    /// 等 completion 回收的旧版本数。
    pub(crate) fn pending_old_pages(&self) -> usize {
        self.pending_old_pages.lock().len()
    }

    /// 是否有**可回刷**的版本（回刷线程的推进判据）。
    ///
    /// 只认 `dirty`（目标块已知）的条目：目标块未知的版本无处可写，若把它算作
    /// 压力会让回刷线程空转。
    pub(crate) fn has_flushable_page(&self) -> bool {
        self.index.lock().entries.values().any(|entry| entry.dirty)
    }

    /// 该 `page_index` 当前驻留的 NV 版本。
    pub(crate) fn resident_page(&self, page_index: u64) -> Option<NvPageId> {
        self.index
            .lock()
            .entries
            .get(&page_index)
            .map(|entry| entry.page)
    }

    /// 该 `page_index` 是否已有驻留版本（写回路径据此决定"补记落点"还是"退休"）。
    pub(crate) fn has_resident(&self, page_index: u64) -> bool {
        self.index.lock().entries.contains_key(&page_index)
    }

    /// 读路径的 NV 命中查询（soft-update 第 5 步发布的就是它可见的引用）。
    /// 命中则把整页内容拷进 `out` 并返回 `true`；未命中（从未写过，或已写回 SSD
    /// 并被回收）返回 `false`，调用方回落设备读。
    pub(crate) fn read_page(&self, page_index: u64, out: &mut [u8]) -> Result<bool> {
        let Some(page) = self.resident_page(page_index) else {
            return Ok(false);
        };
        let Ok(pinned) = self.pool.pin_data_page(page) else {
            // 索引里有、但此刻 pin 不到（并发回收/状态迁移中）：当作未命中。
            return Ok(false);
        };
        pinned.read(out)?;
        Ok(true)
    }

    /// 回刷候选：`(page_index, 当前页, SSD 目标块, 版本代次, 页分配序号)`。
    ///
    /// 形状与 [`InodeCowContext::flush_candidates`](super::cow::InodeCowContext::flush_candidates)
    /// 一致，`rnvpc::flush_candidates` 因此可以按模式分派同一套 rTIO 回刷循环。
    /// 末尾的分配序号让回刷线程在 pin 之前确认槽位仍是这一版（快照可能已过期）。
    pub(crate) fn flush_candidates(&self, budget: usize) -> Vec<(u64, NvPageId, u64, u64, u64)> {
        let index = self.index.lock();
        index
            .entries
            .iter()
            .filter(|(_, entry)| entry.dirty)
            .take(budget)
            .map(|(page_index, entry)| {
                (
                    *page_index,
                    entry.page,
                    entry.ssd_target,
                    entry.generation,
                    entry.page_seq,
                )
            })
            .collect()
    }

    /// 写回路径补记一页的 SSD 落点（`ExtentManager::submit_write_bio` 调用）。
    ///
    /// 返回 `true` 表示该页已有驻留版本（数据在 `write()` 时/上一轮就已退休），
    /// 写回只需回填目标块并把它变成回刷候选；`false` 表示这一页还没有 NV 版本
    /// （部分覆盖的写、或非 `write()` 产生的脏页），写回路径必须用整页内容兜底退休。
    pub(crate) fn mark_writeback_target(&self, page_index: u64, ssd_target: u64) -> bool {
        let mut index = self.index.lock();
        match index.entries.get_mut(&page_index) {
            Some(entry) => {
                entry.ssd_target = ssd_target;
                entry.dirty = true;
                true
            }
            None => false,
        }
    }

    /// rNVPCe 的退休：把一页的**当前内容**发布成一个新的 NV 版本（无 WAL）。
    ///
    /// `apply` 在"旧版本内容"之上做增量修改（`CoW` 语义，与 `InodeCowContext` 一致）；
    /// 调用方若提供的是整页内容（页缓存快照 / 写回时的整页 bio），`apply` 覆盖全部
    /// 字节即可，旧内容不参与结果。
    ///
    /// 七步落点见模块文档；`ssd_target == 0` 表示落点未知（`write()` 时退休），
    /// 随后由写回路径用 [`mark_writeback_target`](Self::mark_writeback_target) 回填。
    pub(crate) fn retire_page_extend(
        &self,
        page_index: u64,
        ssd_target: u64,
        apply: impl FnOnce(&mut [u8]),
    ) -> Result<ExtendOutcome> {
        use crate::fs::fs_impls::ext4::{LATENCY_STATS, read_ns};

        let t_total = read_ns();

        // 发布权（`design.md §5.1` 不变量 4：同 inode publish 串行、不同 inode 并行）。
        // 取不到时在**锁外**重试 —— 这正是 `begin_publication` 返回 `EBUSY` 的语义。
        let guard = self.acquire_publication()?;

        // 第 3 步的前半：CoW —— 读旧版本内容（无旧版本即零页），再 apply。
        let t_old = read_ns();
        let mut buffer = vec![0u8; PAGE_SIZE];
        let previous = self.resident_page(page_index);
        if let Some(old) = previous {
            // 旧页可能正被回刷线程迁往 SSD（完成即离开索引并被回收），`resident_page`
            // 给的只是句柄快照。pin 不到就是"NV 里没有这一版"，不是错误：与
            // `read_page` 保持一致（整页 apply 的调用方本就不依赖旧内容）。
            if let Ok(pinned) = self.pool.pin_data_page(old) {
                pinned.read(&mut buffer)?;
            }
        }
        LATENCY_STATS.nvpc_old_read.record_since(t_old);
        apply(&mut buffer);

        let outcome = self.commit_and_publish(page_index, ssd_target, &buffer, previous)?;
        drop(guard);
        LATENCY_STATS.nvpc_retire_total.record_since(t_total);
        LATENCY_STATS.extend_retire_total.record_since(t_total);
        Ok(outcome)
    }

    /// 七步中的第 1/2/3/4/5/7 步（发布权已持有）。
    fn commit_and_publish(
        &self,
        page_index: u64,
        ssd_target: u64,
        buffer: &[u8],
        previous: Option<NvPageId>,
    ) -> Result<ExtendOutcome> {
        use crate::fs::fs_impls::ext4::{LATENCY_STATS, read_ns};

        // 第 1/2/3 步：预留容量 → 分配 NVPage → 写数据。池容量吃紧时
        // `stage_data_page_with_backpressure` 会通知回刷线程并自轮询重试。
        let t_stage = read_ns();
        let stage_started = now_ns();
        let (new_page, page_seq) = self.stage_data_page_with_backpressure(buffer)?;
        let stage_ns = now_ns().saturating_sub(stage_started);
        // 发布前必须离开 `Staged`：池的持久提交只接受 `NvDirty`
        // （`pin_persistent_update` 校验 kind）。
        if let Err(error) =
            self.pool
                .transition_data_page(new_page, DataPageState::Staged, DataPageState::NvDirty)
        {
            let _ = self.pool.abort_data_page(new_page);
            return Err(error);
        }
        LATENCY_STATS.nvpc_stage.record_since(t_stage);

        // 控制面 NVBH 必须在**任何持久提交之前**拿到：提交之后再失败会留下
        // "已提交但无索引条目"的孤儿版本（容量永久泄漏 + 引用状态不一致），
        // 首轮实验的 `EBUSY: still referenced` 就来自这个顺序。
        let (nvbh, nvbh_seq) = match self.alloc_nvbh_with_backpressure() {
            Ok(bh) => bh,
            Err(error) => {
                let _ = self.pool.abort_data_page(new_page);
                return Err(error);
            }
        };
        let key = PageKey::new(self.fs_id, self.inode, page_index);
        // 第 4 步：登记**未发布**引用（此处的页已是 `NvDirty`，即"数据已就绪"）。
        if let Err(error) = self.pool.stage_reference(nvbh, key, new_page) {
            let _ = self.pool.abort_data_page(new_page);
            let _ = self.pool.free_frame(nvbh);
            return Err(error);
        }

        // 第 4 步的实体：把新页持久提交为 `(fs_id, inode, page_index)` 的当前版本。
        let t_commit = read_ns();
        let commit_started = now_ns();
        let replaced = match self.pool.replace_data_page_in_owner(new_page, key, None) {
            Ok(replaced) => replaced,
            Err(error) => {
                let _ = self.pool.rollback_publication(nvbh);
                let _ = self.pool.abort_data_page(new_page);
                let _ = self.pool.free_frame(nvbh);
                return Err(error);
            }
        };
        let commit_ns = now_ns().saturating_sub(commit_started);
        LATENCY_STATS.nvpc_publish.record_since(t_commit);
        LATENCY_STATS.extend_commit.record_since(t_commit);
        let replaced = replaced.or(previous);

        // 第 5 步：**数据先于引用** —— 数据已写入并已提交，才发布引用。
        // 池的 `publish_reference` 也会拒绝引用 `Staged` 页。走到这里失败是内部
        // 不变量违例（NVBH 是本函数独占的、刚 `stage_reference` 过）：只记录，
        // 数据页本身已可读且已提交，索引条目照常发布。
        let t_publish = read_ns();
        let publish_started = now_ns();
        if let Err(error) = self.pool.publish_reference(nvbh) {
            error!(
                "rnvpc_extend: publishing the NVBH reference failed: {:?} \
                 (bh={:?} seq={} page={:?} inode={} page_index={})",
                error, nvbh, nvbh_seq, new_page, self.inode, page_index
            );
        }
        let publish_ns = now_ns().saturating_sub(publish_started);
        LATENCY_STATS.nvpc_extend_publish.record_since(t_publish);

        // 发布成功 ⇒ 索引切到新版本；`insert` 返回的就是被取代的旧条目
        // （同 inode 的退休被发布权串行，因此这里不存在并发覆盖）。
        let old_entry = {
            let mut index = self.index.lock();
            let generation = index.next_generation;
            index.next_generation = generation.wrapping_add(1);
            index.entries.insert(
                page_index,
                ExtendEntry {
                    page: new_page,
                    page_seq,
                    nvbh,
                    nvbh_seq,
                    ssd_target,
                    generation,
                    state: NvObjectState::NvDirty,
                    dirty: ssd_target != 0,
                },
            )
        };

        // 第 7 步：**撤销旧引用**（否则旧页因"仍被已发布引用持有"而不可回收），
        // 再把旧页挂到待回收队列 —— 它可能正被回刷线程独占（`InFlight`），
        // 因此统一在 completion 边界回收（与 rNVPC 的 `complete_page` 同一出口）。
        if let Some(old_entry) = old_entry {
            // 第 7 步 + **ABA**：只有槽位仍是"我这一版"才会撤销引用并释放它。
            // 校验与释放**在同一把锁内**（`revoke_and_free_frame_if_seq`）——分成两步
            // 做的话，校验通过之后槽位可能被归还并立刻换成别人的 NVBH。
            match self
                .pool
                .revoke_and_free_frame_if_seq(old_entry.nvbh, old_entry.nvbh_seq)
            {
                Ok(true) => {}
                Ok(false) => {
                    let mut counters = self.counters.lock();
                    counters.stale_skips += 1;
                }
                Err(error) => {
                    error!(
                        "rnvpc_extend: releasing the replaced NVBH failed: {:?} (bh={:?})",
                        error, old_entry.nvbh
                    );
                }
            }
            self.pending_old_pages
                .lock()
                .push((page_index, old_entry.page, old_entry.page_seq));
        }

        // 在飞账本：这一版尚未落 SSD。
        self.in_flight.fetch_add(1, Ordering::AcqRel);

        {
            let mut counters = self.counters.lock();
            counters.retires += 1;
            counters.stage_ns += stage_ns;
            counters.commit_ns += commit_ns;
            counters.publish_ns += publish_ns;
        }

        Ok(ExtendOutcome {
            page_index,
            new_page,
            replaced,
        })
    }

    /// 一页的 SSD 写已完成 —— 推进 completion（回收 NV 页 + 归还 in-flight 额度）。
    ///
    /// `flushed_page`/`flushed_generation` 一起标识刚落到 SSD 的那一版；页 id 会被
    /// 回收复用（ABA），只比页 id 无法区分"同一页"与"复用同一 id 的新页"。
    pub(crate) fn complete_flush(
        &self,
        page_index: u64,
        flushed_page: NvPageId,
        flushed_generation: u64,
    ) -> Result<()> {
        use crate::fs::fs_impls::ext4::{LATENCY_STATS, read_ns};
        let t_complete = read_ns();
        // 只有"刚刷的这一版**仍是**当前版本"才摘条目；否则条目必须保留（新版本还没
        // 落 SSD，下一轮还得刷它）。
        let removed = {
            let mut index = self.index.lock();
            let still_current = index.entries.get(&page_index).is_some_and(|entry| {
                entry.page == flushed_page && entry.generation == flushed_generation
            });
            if still_current {
                index.entries.remove(&page_index)
            } else {
                None
            }
        };
        if let Some(entry) = removed {
            // 第 7 步：这一版已在 SSD 落定，控制面引用可以撤销，NVBH 随之释放。
            // 同样要**原子地**校验+释放（见 `revoke_and_free_frame_if_seq`）。
            match self
                .pool
                .revoke_and_free_frame_if_seq(entry.nvbh, entry.nvbh_seq)
            {
                Ok(true) => {}
                Ok(false) => {
                    let mut counters = self.counters.lock();
                    counters.stale_skips += 1;
                }
                Err(error) => {
                    error!(
                        "rnvpc_extend: releasing the flushed NVBH failed: {:?} (bh={:?})",
                        error, entry.nvbh
                    );
                }
            }
        }

        // 刚刷的这一版（`EvictingDataPage::finish` 已把它置为 `Synced` 并清掉持久
        // 提交字）交给**同一个回收出口**，与先前被取代的旧版本一起处理。
        //
        // 不能在这里"直接归还、失败就算了"：页的可回收性取决于它的引用是否已撤销，
        // 而撤销发生在本函数之外的退休路径上 —— 一个刚被写回完成、但还没被下一次退休
        // 换下的版本，其旧引用可能**仍然已发布**（首轮实验的
        // `EBUSY: RNVPC data page is still referenced` 就是这个竟态）。
        // 因此失败一律留在队列里，由后续 completion 再扫。
        if let Some(flushed_seq) = self.pool.data_page_seq(flushed_page) {
            let mut pending = self.pending_old_pages.lock();
            if !pending
                .iter()
                .any(|(index, page, _)| *index == page_index && *page == flushed_page)
            {
                pending.push((page_index, flushed_page, flushed_seq));
            }
        }
        let reclaimed = self.sweep_pending_old_pages();

        LATENCY_STATS.extend_complete.record_since(t_complete);
        let _ = reclaimed;
        Ok(())
    }

    /// 取得本 inode 的发布权；`EBUSY` 时在**锁外**重试。
    fn acquire_publication(&self) -> Result<PublicationGuard> {
        loop {
            match self.pool.begin_publication_for(self.fs_id, self.inode) {
                Ok(guard) => return Ok(guard),
                Err(error) if error.error() == Errno::EBUSY => {
                    for _ in 0..64 {
                        core::hint::spin_loop();
                    }
                    crate::thread::Thread::yield_now();
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// 容量吃紧时通知回刷线程并短自旋让出，供两个 backpressure 重试共用。
    ///
    /// **不能用 `wait_until_or_timeout`**：它每次调用新装一个定时器，在高频前台
    /// 路径上形成定时器风暴、反而饿死回刷线程（`002-background-writeback/README.md`
    /// §4 与 `ext4/fs.rs` 的注释）。这里用仓库通行的"短自旋 + `yield_now()`"。
    ///
    /// `metadata` 区分等待来自哪个循环：控制面 NVBH 与数据页**共用同一个槽位池**
    /// （每版占 2 槽），分开统计才能判断前台到底卡在哪一类。
    fn wait_for_pool_capacity(&self, metadata: bool) {
        let t_wait = crate::fs::fs_impls::ext4::read_ns();
        {
            let mut counters = self.counters.lock();
            counters.in_flight_waits += 1;
            if metadata {
                counters.meta_waits += 1;
            } else {
                counters.data_waits += 1;
            }
        }
        super::notify_flush();
        for _ in 0..64 {
            core::hint::spin_loop();
        }
        crate::thread::Thread::yield_now();
        let stats = &crate::fs::fs_impls::ext4::LATENCY_STATS;
        if metadata {
            stats.extend_meta_wait.record_since(t_wait);
        } else {
            stats.extend_data_wait.record_since(t_wait);
        }
        stats.extend_in_flight_wait.record_since(t_wait);
    }

    /// 分配并写入一个 NV 数据页（并返回它的分配序号）；池容量吃紧
    /// （`EAGAIN` 数据硬上限 / `ENOSPC` 池满）时等待回刷归还容量后重试。
    fn stage_data_page_with_backpressure(&self, buffer: &[u8]) -> Result<(NvPageId, u64)> {
        const POOL_ALLOC_RETRIES: usize = 100_000;
        for attempt in 0..POOL_ALLOC_RETRIES {
            match self.pool.stage_data_page_seq(buffer) {
                Ok(page) => return Ok(page),
                Err(error) if matches!(error.error(), Errno::EAGAIN | Errno::ENOSPC) => {
                    if attempt == 0 {
                        self.counters.lock().stage_retries += 1;
                        // 诊断：把**真实 errno/消息**与池状态打出来。两个
                        // `*_with_backpressure` 最终都只报 "pool capacity did not
                        // recover"，会掩盖真正的失败源（实测踩过）。
                        // 容量回压是池满时的预期控制流，用 WARN 保留诊断但避免默认
                        // error 级性能实验被同步串口输出主导。重试耗尽仍由最终错误报告。
                        {
                            let s = self.pool.stats();
                            warn!(
                                "rNVPC data staging rejected: {:?} (inode {}, free={}, meta={}, data={}, wal={})",
                                error,
                                self.inode,
                                s.free_pages,
                                s.metadata_pages,
                                s.data_pages,
                                s.wal_pages,
                            );
                        }
                    }
                    crate::fs::fs_impls::ext4::LATENCY_STATS
                        .nvpc_eagain
                        .record_event();
                    self.wait_for_pool_capacity(false);
                }
                Err(error) => return Err(error),
            }
        }
        return_errno_with_message!(Errno::EAGAIN, "rNVPC pool capacity did not recover")
    }

    /// 分配一个控制面 NVBH frame（并返回它的分配序号）；池容量吃紧时同样回压重试。
    ///
    /// 这是 rNVPCe 与 native ext4 的**结构性差异点**：delalloc 把元数据分配推到
    /// writeback 批量做，而 soft update 必须**在 `write()` 里**为每一页拿到一个
    /// NVBH（控制面 frame）——所以这里一紧张就会直接变成 `write()` 的延迟。
    fn alloc_nvbh_with_backpressure(&self) -> Result<(NvFrameId, u64)> {
        use crate::fs::fs_impls::ext4::{LATENCY_STATS, read_ns};
        const POOL_ALLOC_RETRIES: usize = 100_000;
        let t_alloc = read_ns();
        let started = now_ns();
        for attempt in 0..POOL_ALLOC_RETRIES {
            match self.pool.alloc_frame_seq() {
                Ok(bh) => {
                    LATENCY_STATS.nvpc_alloc_nvbh.record_since(t_alloc);
                    let elapsed = now_ns().saturating_sub(started);
                    if elapsed > 0 {
                        self.counters.lock().nvbh_alloc_ns += elapsed;
                    }
                    return Ok(bh);
                }
                Err(error) if matches!(error.error(), Errno::EAGAIN | Errno::ENOSPC) => {
                    if attempt == 0 {
                        self.counters.lock().stage_retries += 1;
                        // 同 `stage_data_page_with_backpressure`：控制面 frame 分配
                        // 被拒时也要看得到真实原因。
                        {
                            let s = self.pool.stats();
                            warn!(
                                "rNVPC NVBH allocation rejected: {:?} (inode {}, free={}, meta={}, data={}, wal={})",
                                error,
                                self.inode,
                                s.free_pages,
                                s.metadata_pages,
                                s.data_pages,
                                s.wal_pages,
                            );
                        }
                    }
                    self.wait_for_pool_capacity(true);
                }
                Err(error) => return Err(error),
            }
        }
        return_errno_with_message!(Errno::EAGAIN, "rNVPC pool capacity did not recover")
    }

    /// 尽最大努力归还一个 NV 数据页（并推进 in-flight 账本与计数）。
    ///
    /// 三条路径：① 槽位已经不是这一版（已被别的路径归还并可能被复用）⇒ 丢弃该
    /// 待回收项（账已记过）；② 已经 `Synced`（写回完成、持久提交字已清）的页直接
    /// 归还；③ 仍是 `NvDirty` 的**已提交旧版本**按池的既定协议走淘汰
    /// （`NvDirty -> InFlight -> Synced`）再归还。仍被引用（引用尚未撤销）或被回刷
    /// 线程独占（`InFlight` + pin）时返回 `false`，调用方把它留在队列里。
    fn try_release_nv_page(&self, page: NvPageId, seq: u64) -> ReleaseOutcome {
        // ⓵ 槽位仍是这一版、且已满足回收前提（未提交 / 无已发布引用 / 未 pin）
        // ⇒ 直接归还。校验与归还在**同一把锁内**（ABA，见 `free_data_page_if_seq`）。
        if let Ok(true) = self.pool.free_data_page_if_seq(page, seq) {
            self.decrement_in_flight();
            self.counters.lock().pages_reclaimed += 1;
            return ReleaseOutcome::Freed;
        }
        // ⓶ 仍是 `NvDirty` 的**已提交旧版本**：按池的既定协议淘汰
        // （`NvDirty -> InFlight -> Synced`）再归还。
        if let Ok(eviction) = self.pool.begin_data_page_eviction_if_seq(page, seq) {
            let finished = eviction.finish(None).is_ok();
            if finished
                && self
                    .pool
                    .free_data_page_if_seq(page, seq)
                    .is_ok_and(|ok| ok)
            {
                self.decrement_in_flight();
                self.counters.lock().pages_reclaimed += 1;
                return ReleaseOutcome::Freed;
            }
            return ReleaseOutcome::Retry;
        }
        // ⓷ 既不能直接还也不能淘汰：槽位已不是这一版 ⇒ 已被别的路径归还（账已记过）。
        if self.pool.data_page_seq(page) != Some(seq) {
            self.counters.lock().stale_skips += 1;
            return ReleaseOutcome::Stale;
        }
        ReleaseOutcome::Retry
    }

    /// 扫一遗**全部**待回收版本（被更新版本取代的旧页，以及写回完成待归还的那一版）。
    ///
    /// 一次 completion 只保证"刚刷的那一版到 SSD 了"，而队列里其它页的可回收性依赖
    /// 各自的引用撤销时机，因此每次都整队扫一遗、能还就还、不能还的留在队列里。
    /// 队列长度受 in-flight 额度限制（不会无限增长）。
    fn sweep_pending_old_pages(&self) -> usize {
        let drained: Vec<(u64, NvPageId, u64)> = {
            let mut pending = self.pending_old_pages.lock();
            pending.drain(..).collect()
        };

        let mut keep = Vec::new();
        let mut reclaimed = 0usize;
        for (index, page, seq) in drained {
            match self.try_release_nv_page(page, seq) {
                ReleaseOutcome::Freed => reclaimed += 1,
                ReleaseOutcome::Stale => {}
                ReleaseOutcome::Retry => keep.push((index, page, seq)),
            }
        }
        let mut pending = self.pending_old_pages.lock();
        // 并发退休可能在本轮扫的过程中又推进了新的待回收页：把它们接在保留项之后。
        keep.extend(pending.drain(..));
        *pending = keep;
        reclaimed
    }

    /// in-flight 饱和度递减（永不下溢：只有本上下文记过的页才会被归还）。
    fn decrement_in_flight(&self) {
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return;
            }
            match self.in_flight.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.waiters.wake_all();
    }

    /// 等 in-flight 降到一个阈值以下（供测试/后续阶段观察）。
    pub(crate) fn wait_in_flight_below(&self, threshold: usize) {
        self.waiters
            .wait_until(|| (self.in_flight() < threshold).then_some(()));
    }
}
