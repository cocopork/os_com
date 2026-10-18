> **Archived:** original date: 2026-09-24. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# I5 施工图：rNVPCe 重做（并列新增 `ExtendCowContext`，去 WAL）

> 立项依据：`ext4-io-integration.md` §1.1 / §1.2；权威设计：`rNVPC/design.md` §4 / §5。
> **一次性实现整个 I5**（含把退休点从块 I/O 边界挪到 `write()`），完成后再验证正确性与性能。

## 0. 一句话目标

`rnvpc_extend` 不再复用 `rnvpc` 的 `InodeCowContext`（CoW + per-inode WAL），
改为**并列新增** `ExtendCowContext`：一致性由 **soft update**（数据先于引用、撤销先于回收）提供，
**不含任何 WAL**；同时把退休点从 `ExtentManager::submit_write_bio`（写回时）挪到 `write()`。

## 1. 验收判据

1. `rnvpc_extend` 下 `nvpc_wal*` 系列计数**恒为 0**（新增断言 + 延迟快照核对）。
2. 单页退休成本 **≤ 15 µs**（1/4 缩放档，对比 I3 基线 47 µs）。
3. 双 cfg 0 error / 0 warning；**全量 ktest 失败数不增**（基线 `722 passed; 3 failed`）。
4. `Native` 分支逐字节不变（`RnvpcMode::Native` 走原代码，无新增条件副作用）。
5. 1/4 缩放 `drain_bw` 对比 I3 基线（宁化 vs 512 MiB 档，记录在案）。
6. fsync 在 `rnvpc_extend` 下**不产生设备数据写**（`NOJOURNAL_WB` / 设备写计数不增长）。

## 2. 现状盘点

### 2.1 已有可直接用（P0–P4 建好，**不要重造**）

| 构件 | 位置 | 用途 |
|---|---|---|
| `NvObjectKind{NvPage, Nvbh}` | `descriptor.rs:34/38` | 持久对象类型 |
| `NvObjectState`（持久状态机）+ `can_transition_to` / `is_persistable` | `descriptor.rs:57/85/96` | 落盘状态合法性 |
| `NvDescriptor{…}` + `for_nv_page` / `for_nvbh` | `descriptor.rs:105/135/160` | 构造 NVPage / NVBH 描述符 |
| `NvDescriptor::state()` / `transition_to()` / `encode()` / `decode()` / `scan_descriptors()` | `descriptor.rs:195/200/234/239/260` | 恢复与状态迁移 |
| **NVBH → NVPage 引用登记**（含 `published` 标志） | `frame.rs:53-60`、`state.references` | **§5.1 第 4/5 步**（"已登记未发布"） |
| B-tree 叶子 `RuntimePageDescriptor{…}` | `frame.rs:119` | 驻留位置/运行时句柄/SSD 落点/状态 |
| "容量等待必须先于取低层 page/NVBH 锁" | `frame.rs:66/573` | §5.1 不变量 5 |
| `PoolClass::Metadata => NvDescriptor::for_nvbh(…)` | `frame.rs:845` | NVBH 走 Metadata 容量类 |
| 池原语：`capacity.before_allocate` / `stage_data_page` / `pin_data_page` / `pin_owned_data_page` / `transition_data_page` / `replace_data_page` / `free_data_page` / `data_page_state` / `begin_data_page_eviction` / `stats` | `frame.rs:284/335/340/465/894/~395/431/1040/685` | 写/读/回收 |
| 回刷骨架：`flush_candidates()` / `complete_flush()` / `notify_flush()` / `has_flush_pressure()` / 回刷线程 `rnvpc_flush_round` | `rnvpc/mod.rs`、`ext4/fs.rs:3017` | rTIO v0 已有 |

### 2.2 已核实的 API（2026-09-24 实读，**可直接照着写**）

**§5.1 的七步在池层已经逐步建好了**（`stage_reference` 注释即写"T4.1 步骤 4"，
`publish_reference`="步骤 5"，`revoke_reference`="步骤 7"）。I5 = 写**驱动层** + 接线。

| §5.1 步骤 | 池 API（`frame.rs` 行号） |
|---|---|
| 1 预留容量 | `reserve_data_pages(self: &Arc<Self>, pages) -> Result<CapacityReservation>` (519)；`take_reserved_page(&mut r) -> Result<NvPageId>` (544，耗尽 ⇒ `EAGAIN`)；`reserved_page_count(&r)` (555) |
| 2 分配 | `alloc_data_page() -> Result<NvPageId>` (245)；`alloc_frame(class) -> Result<NvFrameId>` (234，NVBH 走 `PoolClass::Metadata`) |
| 3 写数据 | `stage_data_page(input) -> Result<NvPageId>` (292)；`transition_data_page(...)` (473)；`pin_data_page` (330) / `pin_owned_data_page(id, owner)` (335) |
| 4 登记**未发布**引用 | `begin_publication(inode) -> Result<PublicationGuard>` (574，**`EBUSY` 必须在锁外重试**，不变量 4)；`stage_reference(bh: NvFrameId, key: PageKey, page: NvPageId)` (589，校验 owner 必须是 NVBH) |
| 5 发布引用 | `publish_reference(bh)` (620)；**会拒绝 `Staged` 的页** ⇒ "数据先于引用"由 API 强制 (不变量 1) |
| 7 撤销引用 | `revoke_reference(bh) -> Result<Option<NvPageId>>` (644)；回滚用 `rollback_publication(bh)` (564) |
| — 索引/提交 | `commit_data_page` (892) / `commit_data_page_in_owner` (912) / `replace_data_page` (902) / `commit_frame` (811) / `replace_metadata_frame` (822)；`index_lookup(key) -> Option<PageEntry>` (670)；`dirty_candidates() -> Vec<PageKey>` (684) |
| — 状态查询 | `data_page_nv_state(id) -> Result<Option<NvObjectState>>` (458)；`data_page_state(id)` (439)；`is_page_referenced(id)` (675)；`committed_data_page(key)` (739)；`committed_data_pages_of(fs_id, inode)` (771) |
| — 回收 | `begin_data_page_eviction` (1035) → `EvictingDataPage::finish`；`free_data_page` (389)；`free_frame` (380) |
| — 容量账 | `capacity.before_allocate(class)` (68) / `account_allocate` (79) / `account_recovered` (95) / `account_free` (101) / `account_pin` (112) / `account_unpin` (120) / `stats` (124) |

**§5.2 四态 = `NvObjectState`（`descriptor.rs:57`）**，取值**完全一致**：
`NvReclaimable=1 / NvDirty=2 / NvWriteback=3 / DramClean=4`，且
`can_transition_to`（`descriptor.rs:85`）就是 design.md §4.4 的迁移表
（`DramClean→NvDirty`、`NvDirty→NvWriteback`、`NvWriteback→NvReclaimable|NvDirty`；
`NvReclaimable` 是终态）。⇒ **`ExtendCowContext::state` 直接存 `NvObjectState`，不要另造枚举。**

**仍然要在动第 4-6 步前确认的（3 个）**：
- `PageKey` 的字段与构造方式（`index.rs`）；
- `commit_data_page` / `transition_data_page` 的完整签名与语义边界（`frame.rs:473/892`）；
- `PublicationGuard` 的 Drop 行为（是否自动释放发布权）。


## 3. 数据结构

新增 `kernel/src/fs/rnvpc/extend_cow.rs`：

```rust
/// rNVPCe 的 per-inode 上下文（**无 WAL**）。
pub(crate) struct ExtendCowContext {
    inode: u64,
    fs_id: u64,
    /// (fs, inode, page_index) -> 当前版本。§4.2 的 DRAM B-tree（复用现索引设施）。
    index: Mutex<BTreeMap<u64, ExtendEntry>>,
    /// 同 inode publish 串行（§5.1 不变量 4）；不同 inode 并行。
    publish_lock: Mutex<()>,
    /// **在飞账本**：未落 SSD 的 NV 数据页数（替代 WAL 额度）。
    in_flight: AtomicUsize,
    /// 回压等待者：in_flight 降到阈值以下才放行（替代 WAL 满等待）。
    waiters: WaitQueue,
    next_generation: u64,
    counters: Mutex<ExtendCounters>,
}

#[derive(Clone, Copy)]
struct ExtendEntry {
    page: NvPageId,        // 数据面
    nvbh: NvPageId,        // 控制面（Metadata 类）
    ssd_target: u64,
    generation: u64,       // 复用 ABA 防护（页 id 会回收复用）
    state: NvObjectState,  // ← 直接用池的类型，见 §2.2
}
```

**四态直接用 `NvObjectState`（`descriptor.rs:57`），不要另造枚举。**

> 早期草稿曾写过一个 `enum ExtendState { DramClean, NvDirty, NvWriteback, NvReclaimable }`。
> 它**已废弃**：§2.2 实读确认 `NvObjectState` 的取值与 design.md §5.2 的四态
> **完全一致**（`NvReclaimable=1 / NvDirty=2 / NvWriteback=3 / DramClean=4`），
> 且 `can_transition_to`（`descriptor.rs:85`）= design.md §4.4 的迁移表。
> 再造一个平行枚举只会引入两套状态的同步 bug（参见 `ext4_performance.md` §3 的"影子索引反模式"）。

**与 rNVPC 的 `DataPageState{Free,Staged,NvDirty,InFlight,Synced}` 的关系**：**不共享类型**。
那是 rNVPC 的运行时 5 态；rNVPCe 一律走 `NvObjectState`。

## 4. 写路径：§5.1 七步的落点

`ExtendCowContext::retire_page_extend(&self, page_index, ssd_target, apply) -> Result<Outcome>`

| # | §5.1 步骤 | 落点 |
|---|---|---|
| 1 | 预留 NV 容量（数据页 + 引用更新） | `capacity.before_allocate`，**在取任何低层锁之前**（不变量 5） |
| 2 | 分配新 NVPage + 必要 NVBH | `NvDescriptor::for_nv_page` / `for_nvbh` |
| 3 | 写 NVPage 数据 | `stage_data_page` + `transition`（Staged→NvDirty） |
| 4 | 更新 NVBH，**不发布**引用 | 引用登记 `published=false`（`frame.rs:53-60`） |
| 5 | NVPage 有效后**发布**引用 | 置 `published=true`；**取代**现在的 `extend_publish_residency` |
| 6 | 持久化上层元数据 | 现有 ext4 元数据路径**不变** |
| 7 | **撤销旧引用后**旧页才可回收 | 撤销旧 reference → 旧页入回收队列（不变量 2） |

- 已发布对象的 update **必须 CoW**，不原地破坏（不变量 3）。
- `publish_lock` 保证同 inode publish 串行（不变量 4）。
- **禁止**出现 `wal.*` / `charge_wal_delta` / `WAL_CAPACITY_PER_INODE`。

## 5. 三件 WAL 替代（§1.2）

| 原职责 | 新做法 |
|---|---|
| ① 在刷账本 | `in_flight`（未落 SSD 的 NV 数据页数）；回刷完成后减 |
| ② 回压 | `in_flight >= budget` ⇒ 前台 `spin(64) + yield_now()` 自轮询（**不用** `wait_until_or_timeout`，见 `ext4/fs.rs:2907` 注释 + 002 §4） |
| ③ 旧页回收触发 | **SSD BIO 完成事件**里：`NvWriteback → NvReclaimable` → 撤销旧引用 → 回收旧页 + 减 `in_flight` |

回刷完成链（沿用 rTIO v0，只换上下文）：
`rnvpc_flush_round` 的 `complete` 闭包 → `finish` → `extend::complete_flush(inode, page_index, page, generation)`
→ `ExtendCowContext::complete_flush` → 上面的 ③。

## 6. 接线点

| 文件 | 改动 |
|---|---|
| `rnvpc/mod.rs` | 新增 `EXTEND_CONTEXTS` 表 + `extend_context(inode)` / `existing_extend_context`；`flush_candidates()` 按模式分派两条上下文；`complete_flush()` 同理 |
| `rnvpc/extend_cow.rs` | **新增**（§3/§4） |
| `ext4/inode/extent_manager/mod.rs` | `submit_write_bio` 的 NV 分支按 `rnvpc_extend_fs().is_some()` 走 `retire_page_extend`；`submit_read_bio` 走新的 residency 查询；**删掉** `extend_publish_residency`（被第 5 步取代） |
| `ext4/inode/mod.rs` | **I5 的接入点迁移**：把"退休到 NV"从 `submit_write_bio`（写回时）挪到 `write_at` 的 `Ok(written)` 分支；`sync_data_*` 在 rnvpc_extend 下只推进持久边界 |
| `ext4/fs.rs` | 回刷线程改为按模式取候选；`[LATENCY]` 增加 `extend_*` 段（替换 `nvpc_wal` 的位置） |

## 7. 不变量与断言（ktest 里要覆盖）

- 同 inode publish 串行；不同 inode 并行。
- 新引用发布**不早于**数据有效（第 5 步在第 3 步之后）。
- 旧页回收**不早于**旧引用撤销（第 7 步）。
- `in_flight` 计数守恒：`+1` 在退休、`-1` 在回收；测试结束时必须回 0。
- rnvpc_extend 下 `nvpc_wal*` 恒 0。

## 8. 验证计划（实现完成后）

1. 双 cfg 编译 0/0。
2. **全量 ktest**：`docker exec -w /root/asterinas asterinas-dev bash -c "cargo osdk test --kcmd-args='ostd.log_level=error' --qemu-args='-accel kvm'"`，
   结果在容器 `/root/asterinas/qemu-serial.log`（**不是** stdout 那个 202 行的日志）。
   基线 `722 passed; 3 failed`，失败集合必须不变。
3. **性能**（1/4 缩放：`--wr-capacity-mb 64` + `rnvpc_size=64M rnvpc_meta_reserve=16M rnvpc_data_limit=48M`，
   small 32 MiB / large 256 MiB，nj1/8）：
   `LATENCY_ENABLED=true` 跑一轮看 `extend_*` 占比与 `nvpc_wal*` 是否为 0；再关掉插桩跑一轮看绝对 `drain_bw`。
4. 结果目录：`os_com_implementation/test/aster-ext4-baseline-test/results/<ts>_aster-ext4-baseline/`
   （含 `logs/baseline_run.log` 与各 `logs/qemu-*`）。

## 9. 施工顺序（每步后双 cfg 编译）

> **状态**：1–7 全部完成（2026-09-24）。实际落地与本文的偏差、以及过程中挖出的 4 个并发/顺序
> 缺陷记录在 §11。

1. `extend_cow.rs` 骨架 + 四态 + 计数（不接线，先编译过）。✅
2. §5.1 七步 `retire_page_extend` + `in_flight` 账本/回压。✅
3. `mod.rs` 并列新增 `EXTEND_CONTEXTS` + 分派。✅
4. `extent_manager` 写/读分支切到新上下文；删 `extend_publish_residency`。✅
5. 回刷完成链切到 `extend::complete_flush`；补 `[LATENCY] extend_*`。✅
6. 退休点迁移到 `write_at`（I5 的"整段"要求）。✅（**带一个前提**，见 §11.1）
7. ktest + 性能验证。✅（判据 1/2 达标，数字见 §11.5）

## 10. 回滚

全部改动在 `RnvpcMode::Native` 之外；回滚 = `git checkout` 本任务涉及的 6 个文件，
并把 `LATENCY_ENABLED` 复位为 `false`。

## 11. 实际实现记录（2026-09-24 施工）

改动文件（8 个）：`rnvpc/{extend_cow.rs(新) mod.rs frame.rs cow.rs tests.rs}`、
`ext4/{fs.rs inode/mod.rs inode/extent_manager/mod.rs}`。

### 11.1 与本文档的偏差

| 项 | 文档原计划 | 实际落地 | 原因 |
|---|---|---|---|
| 第 6 步退休点 | `write()` 里退休**所有**被写的页 | 只退休**落点已知**（`map_blocks` 返回 `Mapped{written:true}`）且**整页覆盖**的页 | `write()` 时 delalloc 还没分块，`ssd_target` 未知 ⇒ 版本无处可写、只占容量不可回收 ⇒ 池被撑满（首轮 journal abort）。数据来源也必须是页缓存整页，部分覆盖的页凑不出其余字节 |
| 回压额度 | per-context `high_water/2` | **全局** `pool.stats().data_pages < high_water/3`（前台超限直接跳过，不等待） | per-inode 额度 × 多 inode ≫ 池容量，等于没有额度 |
| 容量等待 | 未明确 | 写回路径 `EAGAIN \| ENOSPC` 自轮询（`notify_flush()` + 短自旋 + `yield_now()`） | 与 rNVPC 相同的"不要用 `wait_until_or_timeout`"结论（定时器风暴） |
| 池层 API | 复用现成的 `commit_data_page_in_owner` | 新增 `replace_data_page_in_owner`（暴露私有 `commit_data_page_with_key(.., replace=true)`） | 否则同 key 的旧提交版本无法被找出来回收 |
| 旧版本回收 | completion 里直接 free | 统一出口：`complete_flush` 把刚刷的页 dedup 推入 `pending_old_pages`，再 `sweep_pending_old_pages()` 全量扫 | 完成时刻的页可能仍被已发布引用持有（`EBUSY`），一次 completion 只保证"刚刷的那版到 SSD"，其余得留着重试 |

### 11.2 过程中挖出的 4 个缺陷（都是真 bug，不是性能问题）

1. **失败清理顺序**：NVBH 必须在 `replace_data_page_in_owner`（持久提交）**之前**拿到并
   `stage_reference`。否则提交之后失败会留下"已提交但无索引条目"的孤儿版本
   （容量永久泄漏；现场 `EBUSY: still referenced` / `EINVAL: never staged`）。
2. **回收竞态（nj1 挂死）**：completion 释放了仍被**已发布**引用持有的页，而待回收项又被
   `retain` 丢掉 ⇒ 容量泄漏 ⇒ `sync -f` 永不返回。改为"唯一回收出口 + 每轮全量扫 + 失败留队"。
3. **槽位复用（ABA）**：页/帧 id 会回收复用，快照里的旧 id 可能已经属于别人。
   加**分配序号** `alloc_seq`（`frame_seq` / `data_page_seq` / `pin_for_writeback_seq`）。
   其中 `pin_for_writeback_seq` 不做校验会**把别人的页内容写到旧候选记下的目标块**
   —— 静默数据损坏路径，必须校验。
4. **校验与释放必须同一把锁**（本轮最后定位）：见 §11.3。

### 11.3 关键教训：`校验 + 动作` 不能跨锁（TOCTOU）

`complete_flush` / 旧版本替换里最初写成"先 `frame_seq(id) == 记录序号` 判断，再
`revoke_reference` + `free_frame`"。第一次判断对**条目自己的**序号永远成立，看起来"ABA 防护"
生效了（`stale_skips` 恒 0）；但在两次加锁之间，槽位可能被合法归还并**立刻被另一个 retire
复用**，于是释放动作落到新主人身上，把它刚 `stage_reference` 的**未发布引用**删掉
⇒ 新主人 `publish_reference` 报 `EINVAL "RNVPC reference was never staged"`。

实测（探针打印调用点 + 被摘条目字段，1/4 档 nj1+nj8/30s）：**63 次失败 ↔ 63 条这样的窗口**，
其中一条：

```
free_frame removes an UNPUBLISHED ref: slot=159 ref_key={inode:14, page_index:3336}
  probe = RefProbe { site: 2(=complete_flush), inode: 13, key_page_index: 3147,
                     entry_nvbh_seq: 160, ... }     ← 条目自己的序号
publish failed: bh=NvFrameId(159) bh_seq=810 key=(inode:14,page:3336)
                                               ↑ 并发 retire 刚分配，810 ≠ 160
```

**修法**：池层提供**单锁原子**的释放 API，把校验与释放焊死在一起：

- `Rnvpc::revoke_and_free_frame_if_seq(bh, seq) -> Result<bool>`（`kind==Metadata && alloc_seq==seq`
  才在同一把锁内删 `references[index]` + 归还槽位）
- `Rnvpc::free_data_page_if_seq(page, seq) -> Result<bool>`
- `Rnvpc::begin_data_page_eviction_if_seq(page, seq)`（`pin_for_writeback_seq` 改为复用它）
- 调用方用 `Ok(false)`（"槽位已不是这一版"）计 `stale_skips` —— 该计数**非 0 是健康信号**，
  与 rNVPC 既有的 `generation` 失配跳过同类。

回归测试：`rnvpc_i5_atomic_release_rejects_a_recycled_slot`（构造"归还→复用→陈旧释放请求"，
断言陈旧请求被拒绝且新主人的登记完好）。

### 11.4 顺带修掉的性能陷阱：池层提交路径禁止线性扫槽位

`begin_commit` 原来用 `state.records.iter().find(...)` 找"同 key 的已提交旧版本"
⇒ 每次提交扫 `total_pages` 个槽位（1/4 档 16384 个 ≈ 1 MB 内存流量）。实测占
rNVPCe 单页退休成本的 **77%**（14 µs），且随池容量线性恶化（全尺寸 65536 档更糟）。
改为查 DRAM B-tree（`Rnvpc::committed_version_of_key`，O(log n)），14 µs → **1 µs**。

**陷阱（实际踩到过）**：这个索引**只装数据页**——元数据对象
（`ObjectKind::Metadata`）根本不在里面（所以 `committed_metadata_slot` 至今仍是扫表，
注释写着"本函数只服务于元数据对象"）。第一版把两类对象一起换成 `index.lookup()`，
立刻弄挂 2 个 metadata 用例（`rnvpc_committed_frame_requires_clear_before_reuse`、
`rnvpc_recovery_selects_latest_metadata_version_and_reclaims_older_version`）。
修正：`begin_commit` 按 `expected` 类别分流（数据页 → B-tree；元数据 → 保留线性扫，
`committed_version_scan`）。

### 11.5 验收结果（1/4 缩放：`wr-capacity 64` + `rnvpc_size=64M meta=16M data=48M`）

| 判据 | 结果 |
|---|---|
| 1. `rnvpc_extend` 下 `nvpc_wal*` 恒 0 | ✅ `nvpc_wal`/`nvpc_wal_append` count = 0；同时 `publish failed`/`UNPUBLISHED ref`/`journal abort`/`capacity exhausted`/`still referenced` 全 0 |
| 2. 单页退休 ≤ 15 µs（I3 基线 47 µs） | ✅ **nj1 6 µs**（分解：stage 1 + commit 1 + complete 4；`extend_in_flight_wait` 0）。nj8 均值 30 µs 是**回压排队**主导（`in_flight_wait` 32.7 万次），非单页成本 |
| 3. 双 cfg 0/0 + ktest 失败数不增 | ✅ 双 cfg 0 error / 0 warning；全量 ktest 见 §11.6 |
| 4. `Native` 分支逐字节不变 | ✅ 所有改动都在 `RnvpcMode::Native` 之外（`rnvpc_extend_enabled()` 为假时一行都不走） |
| 5. Historical diagnostic `drain_bw` observation versus I3 | **Diagnostic historical observation only; not validated or formal.** The recorded comparison used instrumentation and is retained solely for diagnosis. Formal results require retained `busy_bw_mib_s` evidence. |
| 6. fsync 不产生设备数据写 | ✅ 写路径在 `submit_write_bio` 里提交页缓存快照后直接 `Complete`，不落设备；设备写只由 rTIO 回刷线程产生 |

### 11.6 遗留 / 未做

**关于数字口径**：I3 基线 `results/20260923_163651`（nj1 47.41 / nj8 76.19）是**插桩开着**
测的（日志里有 723 个 `[LATENCY]` 块），所以同口径对比就是上面的 +89% / +24%。
**Historical diagnostic observation, not a validated/formal result:** 把插桩关掉（最终提交的 `LATENCY_ENABLED=false`）后是 nj1 **143.0** / nj8 **228.6** MiB/s
（`results/20260923_183520`）—— 说明插桩本身在这条路径上不便宜（每次 retire 有 5–8 处
`read_ns()` 计时点），引用数字时必须说明口径。I3 期另有关插桩轮（nj8 23.06–31.74 MiB/s），
但它们的写入量是 512–731 MiB，与 1/4 缩放档（256 MiB）**不可比**，不能拿来算倍数。

- nj8 的**回压排队**占了大头（`extend_in_flight_wait` 32.7 万次/轮）：前台退休超额度时是
  "跳过"，真正等的是写回路径。下一步该做的是回刷侧的多 worker / in-flight budget
  （`rTIO/design.md` 的 P6），不是继续压单页成本。
- `LATENCY_ENABLED` 已复位为 `false`。

