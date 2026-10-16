// SPDX-License-Identifier: MPL-2.0

//! P1：rNVPC 的 per-inode WAL runtime（T1.1 / T1.4）。
//!
//! 依据 `rNVPC/porting.md §3.1`、`rNVPC/design.md §3.2/§3.4`：
//!
//! - 每个活跃 inode 一个 DRAM runtime handle，挂**固定 1024 条** op-log 的 WAL；
//! - 额度**不跨 inode 共享**：某个 inode 的日志耗尽只阻塞它自己的 update，
//!   不直接阻塞仍有额度的其他 inode（T1.4 的第二条验收）；
//! - 额度回收**只允许发生在 completion 之后**（[`InodeWalRuntime::complete_page`]），
//!   绝不因为 BIO 已提交就提前释放（porting §3.1 末段）；
//! - 记录只保留模拟所需的最小字段（inode / page index / 旧新版本 / 生命周期）；
//!   模拟器不实现可重放的通用事务日志。
//!
//! 这里只做**成本模拟**：不声称掉电一致性，也不模拟 group commit、日志压缩或跨
//! inode transaction。

use ostd::sync::WaitQueue;
use spin::Mutex;

use super::id::NvPageId;
use crate::prelude::*;

/// 每个 inode 的 WAL 额度（op-log 条数）。见 `porting.md §3.1`。
pub(crate) const WAL_CAPACITY_PER_INODE: usize = 1024;

/// 单条 op-log 记录的 PM 占用（字节）。用于把 WAL 折算成 PM 页计入容量：
/// 1024 条 × 32 B = 32 KiB = 8 个 4 KiB 页。
pub(crate) const WAL_RECORD_BYTES: usize = 32;

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

/// 把 op-log 条数折算成 PM 页数（向上取整）。
///
/// WAL 占用计入 rNVPC 的 PM 容量（design §3.2），因此容量层要按这个数记账。
pub(crate) const fn wal_pages_for(records: usize) -> usize {
    (records * WAL_RECORD_BYTES).div_ceil(PAGE_SIZE)
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
