# ext4 性能：踩过的坑与优化手法（沉淀）

> 用途：把 research1 阶段"逐步逼近 Linux ext4 性能"时踩过的坑、量到的数、以及有效的
> 优化手法收拢成一份可查的手册。写于 2026-09-23。
>
> 证据等级约定：**✅实测**（提交说明或日志里有具体数字）｜**⚠️仅提交说明**（当时有量，
> 数字未收录在本文件）｜**❓待补**。
> 历史全局 `results/` 已清理；后续 journal 对比产物位于 `os_com_implementation/test/ext4-journal-compare/results/`。
> Other retained diagnostics remain under `os_com_implementation/test/aster-ext4-baseline-test/`（fio 作业与 A/B 脚本）、
> `os_com_implementation/os_com_refactor_plan/009-fsync-device-roundtrips/README.md`。

---

## 0. 结论速览

| # | 坑（现象） | 根因 | 修复手法 | 提交 | 效果 |
|---|---|---|---|---|---|
| 1 | `nj1 == nj4`（都 ~1500 iops，裸设备 5300） | 每次块分配都持**全局超级块锁**跨越整个跨组扫描 | 空闲计数改 `Ext4` 上的原子；锁只包住 `journal_capture`；组扫描用**每组锁** | `26ef8b66f` | ✅ 并发写恢复扩展性 |
| 2 | 队列打不满 | virtio-blk 队列只有 64 项、modern 路径还把描述符环卡在 256 | 提到 **1024**，环跨多个 4K 帧按比例分配 | `1fcb2d996` | ✅ |
| 3 | 后台写回期间前台仍被拖住 | daemon 是**批提交 + `wait_all`**（同步等设备） | Linux 风格 **submit-then-reap**：提交不等待，per-BIO 钩子计数，in-flight 归零且元数据落盘后才移出 dirty set | `4d22b1947` | ✅ |
| 4 | 随机 4K 每次回写只分配 1 块 | 每个 dirty range 在回写时精确分配一块 ⇒ 单线程分配器上成千上万次往返 | `prealloc_blocks=N`：按 N 向上取整预分配，多出的部分留 **unwritten**（读到 0，崩溃安全） | `691f62b4c` | ✅ 默认 0 = 旧行为逐字节等价 |
| 5 | **每页一个 BIO**：1 GiB 回写 = 262144 个单页 BIO × 35.5µs = **9.32s（占作业 92% 墙钟）** | 机会式请求合并赢不了调度竞争：fsync 从调用者线程**内联**提交，块 worker 在下个请求到达前就把单个请求弹走了（合并失败计数器从未见到不连续 ⇒ 盘上明明是连续的） | 在 **Bio 构造时确定性地合并**（多段 BIO），照抄读侧五件套 | `fe0577d42` | ✅ nj_write **9.4% → 99.7%**（`97bf98821`） |
| 6 | journaled 写比 nojournal 快 7×（反常） | 日志提交线程**恰好**是成批提交，合并到 **61.9 页/请求** —— 说明差异不在"日志 vs 无日志"，而在**提交是否成批** | 同上（把成批变成确定性行为，而不是碰运气） | `fe0577d42` | ✅ |
| 7 | 前台写在注册后写进新空洞 ⇒ **journal abort** | `register_ordered_data` 按整个 `file_size` 注册，而 `allocate_pending` 只分配注册时刻已脏的页；提交线程的有序刷遇到未映射页 → 单页兜底调 `allocate_one` → "writeback hit an unallocated block with no transaction" | `submit_write_pages` **跳过空洞页并保持 dirty**，下一轮 `allocate_pending` 分配后重刷 | `90c609235` | ⚠️ |
| 8 | fsync 每次都要额外一次设备往返；提交唤醒风暴 | 并发 fsync 各发一次设备 flush；日志块逐块 `submit_and_wait`；有序数据逐 inode 等 | 可合并屏障（nojournal 实测 **0.74 次/请求、搭车率 24.2%**）+ 日志块批量异步 + 单次 `wait_all` + 有序数据跨 inode 合批 | `c59270bef` | ✅ |
| 9 | nojournal 元数据落盘把前台拖住 | 分配窗口内每块**同步写设备** | 批量缓冲 + 异步 bio 提交 + 延后 reap（模式 2→3 三臂对比），计数器可观测 | `9ba9deb6b` `f304badd5` `bc0699641` `69d500047` `7cd5b279d` | ⚠️ 三臂数据见 `ab_njwb3arms*.log` |
| 10 | 前台写把内存/回写队列冲爆 | 无脏页预算 | `writeback_capacity_mb=N` 推出 background/high/low 水线，`>high` 阻塞前台写，`<=low` 放行 | `73d7b2d79` | ⚠️ 见 §4.4 的会计坑 |
| 11 | 为"待分配块"另建了一套影子索引 | `PendingAlloc` Btree 与 PageCache 状态重复 | 删掉它，改为**在 PageCache 里就地查**待分配页，去重下沉到 EsCache 延迟层 | `25f4a2ea1` `135c29346`（先有 `a0872047d`） | ⚠️ 教训见 §4.3 |
| 12 | 前台节流把排队的 16 个线程**每轮 `wake_all`** | 唤醒本身 74ms/轮（占 writeback 总时间 **63%**，max 26.7s） | 改**自轮询**：`spin_loop(64)` + `Thread::yield_now()`，writeback 不再 `wake_all` | `73d7b2d79` 等 | ✅ 见 §4.5 |
| 13 | 用 `wait_until_or_timeout` 自唤醒"更文明"却**更慢** | 该 API **每次调用新装一个定时器**，定时器注册拿全局 `SpinLock<BinaryHeap>` + `disable_irq` ⇒ **定时器风暴**，反过来饿死写回线程 | 彻底禁止前台高频路径用定时器等待（同上，改自轮询） | 同上 | ✅ 结论留在 `fs.rs:2907` 注释里 |
| 14 | 前台写与后台 writeback **抢 `inode.inner` 锁**，writeback 被饿死 | 写回"按限额"的做法是**遍历 `dirty_ranges`（O(pages)）+ 在 per-inode 上额外读 `i_size`** ⇒ 与前台抢同一把 `inner` 锁 | 全链路返回**实际提交页数**，限量改 O(1)（`collect_marked_pages().len()`）**不再遍历**；批量提交 + 一次性等待再 reap；按页数限量（4096 页）+ `writeback_cursor` 轮转 | `002` 优化项 1/2 | ✅ `wb_total` **6.2s/轮** → 见 §4.6 |
| 15 | NV 页“已完成回刷”的事件被**陈旧完成**偷走 ⇒ 该页永不回刷 ⇒ WAL 永久满 ⇒ 无界自旋 | **页 id 会被回收复用**（ABA）：只比 `page_id` 相等不是版本判定，无法区分“同一页”与“复用同一个 id 的新页” | `CowEntry` 加**单调 `generation`**，完成事件比较 `(page, generation)` 二元组 | `4dd9011f3` | ✅ 消除 HANG |
| 16 | rNVPC WAL 满时前台写线程自旋等回刷，**定时器风暴**反过来饿死回刷 | 沿用 §0 #13 的 `wait_until_or_timeout`；但 WAL 回压是**每次写**都可能触发的高频路径，不是“每轮一次” | 改**自轮询**：`spin_loop(64)` + `Thread::yield_now()`（与 #12/#13 同一手法） | `4dd9011f3` | ✅ |
| 17 | 回刷线程持 `inode.inner` 过久，前台 72% 的 `write_total` 花在**等这把锁** | 回刷每轮持锁做 O(页数) 的工作；而 rNVPC 单页退休成本高（~48 µs）⇒ 一轮 2064 页 ≈ **100 ms 单次持锁** | 见 `rNVPC-rTIO/ext4-io-integration.md` §6；根因是单页退休成本，I5 去 WAL 直接降它 | `4dd9011f3`（测量） | ✅ I5 后 nj1 单页 47→6 µs |
| 18 | rNVPCe 在 `write()` 退休后池被撑满 ⇒ **journal abort** | delalloc 下 `write()` 时块**还没分配**，`ssd_target` 未知 ⇒ 版本无处可写、只占容量不可回收 | 只退休**落点已知**（`map_blocks` 返回 `Mapped{written:true}`）且**整页覆盖**的页；其余交给回写路径（那里块已定） | I5 | ✅ |
| 19 | `publish_reference` 报 `EINVAL "RNVPC reference was never staged"`（1/4 档一轮 63 次） | **校验与释放跨锁（TOCTOU）**：`frame_seq(id) == 记录序号` 通过之后、真正释放之前，槽位被合法归还并**立刻被另一个 retire 复用** ⇒ 释放落到新主人头上，删掉它刚 `stage_reference` 的未发布引用 | 池层提供**单锁原子**释放 API：`revoke_and_free_frame_if_seq` / `free_data_page_if_seq` / `begin_data_page_eviction_if_seq`（`pin_for_writeback_seq` 复用后者） | I5 | ✅ 现场 63 次 → 0 |
| 20 | rNVPCe 单页退休 14 µs 里 **77%** 是“找同 key 旧版本” | `begin_commit` 用 `records.iter().enumerate().find(...)` **线性扫全部槽位**（1/4 档 16384 槽 ≈ 1 MB 内存流量；全尺寸更糟） | 改查 DRAM B-tree（`Rnvpc::committed_version_of_key`，O(log n)）；**元数据对象不在索引里，必须按类别分流保留扫表** | I5 | ✅ 14 µs → **1 µs** |
| 21 | rNVPCe 被当成“比 native 慢 2.0×/11.7×” | **脏页预算不对称**：guest 脚本把 `ext4` profile 的 `writeback_capacity_mb` 写死 256，而 `rnvpc*` 用 `--wr-capacity-mb`(=64) ⇒ 4× 窗口差；同一 native 在 64 档只有 256 档的 1/2.26（randwrite nj1）/1/5.14（seqwrite nj1） | ext4/ext2like 也使用显式覆盖（默认仍 256）；对比前核对 `PARAM` 行的 `writeback_capacity_mb` | 本次 | ✅ 同预算下 nj1 rNVPCe 反而快 1.14–1.56× |
| 22 | “rNVPCe 在 `write()` 里分配元数据（NVBH）成为瓶颈”的假设 | 实测否证：`nvpc_alloc_nvbh` **0 µs**、`extend_meta_wait` **0**、容量等待 **0**；真大头是**脏页节流**（`write_throttle` 50–88%，native 也 72–88%）⇒ 差别在**排空速率**而非元数据分配 | 已加专用探针（`nvpc_alloc_nvbh` / `extend_meta_wait` / `extend_data_wait`）把两者长期分开 | 本次 | ✅ 假设被否证，方向改到 P6 |
| 23 | **rTIO 回刷逐页发 BIO**（NV→SSD 每条 1 页） | 与 #5 同一根因（机会式合并赢不了调度竞争），只是这次在 rTIO 回刷路径上 | 回刷候选按 `(inode, page_index, ssd_target)` 连续并段，一段一条多块 `BioSegment`（≤256 块），完成回调对段内每页逐个 `finish`+`complete_flush` | 本次 | ✅ 能力已证：`seqwrite-4k nj8` **33.01 页/BIO**（39842 页/1207 条）；但 4 KiB orderless 下带宽收益未确立（同会话 A/B 仅 randwrite nj8 +16.3%）。详见 `rNVPC-rTIO/rtio-flush-merge-and-thread-merge.md` |
| 24 | 合并后 `seqwrite-4k nj8` **卡死整机、串口零输出**（同构建下 randwrite/seqwrite nj1 全正常） | 合并暂存区写成**每轮一次** `vec![0; 1 MiB]`：回刷循环 ~140 轮/s ⇒ ~140 MiB/s 的 **large-slot 堆分配抖动**，在高负载下卡住（卡点把整机拖住 ⇒ 连 `[LATENCY]`/ERROR 都打不出来） | 暂存区**跨轮复用**（`rnvpc_flush_loop` 持有并传入）+ **按需增长**（小段只占 4 KiB） | 本次 | ✅ 定位靠 A/B（`MERGE_MAX_BLOCKS=1` 同一轮立即跑通 174.2 MiB/s）；修复后 216.9 MiB/s |
| 25 | “两条后台线程合并成一条”看起来职责更清 | 语义上正确（职责重劈），但**实测否掉**：首触随机写在 delalloc 下**无落点** ⇒ 前台退休≈ 0 ⇒ 退休全靠 daemon 那一趟分配块；而那一趟的频率又受 **NV 元数据捕获**约束（journal/位图/inode 表共用同一槽位池，背压 ∝ 提交频率）⇒ 提频 ~7s 灌满池 ⇒ `journal abort`；降频 1 趟/s ⇒ 带宽 **91.8 → 12.6 MiB/s** | **回退**，恢复 `ext4 daemon + rTIO` 两线程；线程启动处留注释说明机理 | 本次 | ❌ 否掉（机制与“正确做法”见同名笔记 §2） |
| 26 | 跨会话性能数字对不上（同配置、同负载） | **环境漂移**：连未改动的 `ext4` 对照路径都在漂（`randwrite nj1` 80.25 → 61.84，-23%；`nrwrite nj8` 191 → 228）⇒ 跨会话比较会把环境差异当成代码效果 | 一律用**同会话 A/B**（唯一差异 = 被测量因子），并标注“跨会话仅供参考” | 本次 | ✅ 方法学修正 |
---

## 1. 并发与锁：先怀疑"一把大锁"

- **症状最典型的形态是"加并发不涨"**：`nj1 == nj4`（都 ~1500 iops，而裸设备 5300）
  ⇒ 不要先怀疑驱动/盘，先看**分配路径上的锁粒度**。
- 手法：把**只被读的计数**（`free_blocks_count` / `free_inodes_count`）从"超级块里的字段"
  升级为 `Ext4` 上的**原子权威**，超级块字段退化为 mount 时的种子与写回时的拼接源；
  锁只保留在必须串行的收尾点（`journal_capture`、孤儿链头变更）。
- 一般规律：**跨组扫描不该持全局锁**；组级锁（per-group）就够。

### 1.1 "校验 + 动作"必须同一把锁（跨锁校验 = 假防护）

这是本项目最难查的一类并发坑（第 2 次遇到，第一次是 §0 #15 的 `generation`）：

```rust
// 错的：看起来做了 ABA 防护，其实是假防护
if pool.frame_seq(id) == Some(entry.recorded_seq) {   // ← 单独取一次锁
    pool.revoke_reference(id);                        // ← 再一次
    pool.free_frame(id);                              // ← 再审一次
}
```

- 第一次判断对"条目**自己**的序号"永远成立（计数器恒为 0，看起来很健康），
  但在两次加锁之间，槽位可能被**合法归还**并**立刻被另一个执行流复用**，
  随后的释放动作就落到**新主人**头上。症状是远处的一句
  `EINVAL "reference was never staged"`（新主人刚登记的引用被人删了）。
- **定位手法**（值得复用）：在"嫌疑释放点"打**带调用点标识 + 被摘条目全字段**的探针日志，
  与"失败点"打出的槽位当前序号做**左右对齐**。本例一剑未多打，一次运行就抓到
  "条目自己的序号 160 vs 槽位当前序号 810"。
- **修法**：把"校验 + 释放"下沉成池层的**单锁原子 API**，校验不通过就返回
  `Ok(false)`（由调用方计一个 `stale_skips` 健康计数）。
- 推论：凡是"快照（候选/账本）+ 按 id 操作"的接口，都应该有带序号的版本
  （`pin_for_writeback_seq` 不做校验会**把别人的页内容写到旧候选记下的目标块**——
  静默数据损坏，比抛错可怕得多）。

### 1.2 池层热路径禁止线性扫槽位

§0 #20：`begin_commit` 为了让“同 key 的已提交旧版本”可被替换，用 `records.iter().find()`
全表扫。这类写法在**小容量测试**里看不出问题（64 页的 ktest 毫无感知），到 16384 槽位
就变成 14 µs（占单页退休 77%），全尺寸 65536 更糟。**已有 B-tree 索引就该用它**。

> **换索引查时的陷阱（实际踩到过）**：索引不是无条件等价于扫表。本例的 DRAM 页索引
> **只装数据页**，元数据对象（`ObjectKind::Metadata`）根本不在里面（所以
> `committed_metadata_slot` 至今仍是扫表，注释就写着“本函数只服务于元数据对象”）。
> 第一版把两类对象一起换成 `index.lookup()`，一下子弄挂了 2 个 metadata 用例
> （`rnvpc_committed_frame_requires_clear_before_reuse`、
> `rnvpc_recovery_selects_latest_metadata_version_and_reclaims_older_version`）。
> ⇒ **优化一个“查找”时，先确认索引覆盖的是哪一类对象、以及原查找的全部校验条件，
> 按类别分流（数据页→B-tree，元数据→保留扫表）。**

## 2. 驱动/队列：队列深度是"免费"的吞吐

- virtio-blk 队列 64 → **1024**（`1fcb2d996`）。modern-pci 路径原本把描述符环卡在 256，
  必须让 descriptor/avail/used 环**跨多个 4K 帧**、块请求/响应 DMA 流按比例分配帧，
  否则"提高深度"是空话（当时明确 Rejected 了"保持 256"这个选项）。
- 判据：队列深度不足时，你会在设备侧看到**提交/完成串行化**，而不是 CPU 打满。

## 3. 分配：delalloc 的正确用法与"别建影子索引"

- **delalloc 的核心收益**是把分配从写路径挪到回写路径（省掉每次写的 journal handle、
  树遍历、位图捕获），但它自己会引入两类坑：
  1. **一次一块**（§0 #4）→ 用 `prealloc_blocks` 向上取整预分配，尾巴留 unwritten；
  2. **注册范围与实际脏页不一致**（§0 #7）→ 回写遇到空洞时"跳过并保持 dirty"，
     绝不能去 `allocate_one`（日志卷上没有事务会直接 abort journal）。
- **影子索引反模式**：曾为"待分配块"建过 `PendingAlloc`（先 Btree），随后被删掉，改为
  直接在 PageCache 里查待分配页、把去重下沉到 EsCache 延迟层（`25f4a2ea1`、
  `135c29346`）。教训：**已有结构中能表达的状态，不要再建一套平行索引**——两份状态
  必然要同步，而同步点就是 bug 与开销的来源。
- 保留旧行为：`prealloc_blocks=0`（默认）必须与改动前逐字节等价，这样既有 ktest 的
  精确空闲块断言才成立。

## 4. 写回与 fsync

### 4.1 同步写回（wait_all）是吞吐杀手

批提交 + `wait_all` 会让 daemon 在设备往返上睡掉；改成 submit-then-reap（`4d22b1947`）
后，需要一套新的**在飞会计**：`writeback_pending`（per-inode）+ `INFLIGHT_WRITEBACK_BIOS`
/`INFLIGHT_WRITEBACK_PAGES`（全局），in-flight 归零**且**元数据写回后才把 inode 移出 dirty set。
注意：fsync 与 FS 级 sync **保持同步语义**，只有后台 daemon 走异步。

### 4.2 "每页一个 BIO"是读写两侧都踩过的同一个坑

- 读侧先踩（每个单页 BIO 都在等待设备），写侧随后发现是它的镜像（§0 #5）。
- 关键洞察：**合并不该靠机会**。`fe0577d42` 的探查结论写得很清楚——合并失败计数器从未
  看到不连续，但 fsync 内联提交输给了块 worker 的弹出节奏；journaled 之所以快，只是
  因为提交线程**碰巧**成批。⇒ **在 Bio 构造时确定性合并**，别赌调度。
- 手法：多段 BIO（`write_segments_async` / `submit_write_pages`）+ 每 BIO 一个完成钩子，
  在飞计数按页数增减，提交失败要把计数回滚并把页重新标脏。
- **rTIO 回刷侧是第三个实例**（2026-09-24）：NV→SSD 原本每页一条 BIO。做法与上面同构：
  候选按 `(inode, page_index, ssd_target)` 连续并段，一段一条多块 `BioSegment`（≤256 块），
  完成回调对段内每页逐个 `finish` + `complete_flush`。实测 `seqwrite-4k nj8` 达 **33.01 页/BIO**。
  ⚠️ 但在这组 4 KiB orderless 负载上**带宽收益未确立**（同会话 A/B 仅 randwrite nj8 +16.3%）
  ⇒ “合并 BIO 一定提速”不成立，要看负载是不是 BIO 数瓶颈。
- **合并自身的坑**：暂存区若不跨轮复用而每轮 `vec![0; 1 MiB]`，大块堆分配（ostd large-slot）
  在长段负载下会卡死整机（见 §0 #24）。写这类“每轮一次”的缓冲时，一律改成跨轮复用 + 按需增长。

### 4.3 fsync 的设备往返与"搭车"

- 三件事一起做（`c59270bef`）：① journaled 且"确实等过提交"时**不再**补发设备 flush
  （提交内两道 barrier + 有序数据 flush 已覆盖）；② nojournal 的并发 fsync 合并到同一次
  设备往返（实测 **0.74 次/请求、搭车率 24.2%**）；③ 日志块批量异步 + 单次 `wait_all`、
  有序数据跨 inode 合批。
- 一般规律：**任何"每次操作都发一次设备往返/唤醒"的路径，都值得先问一句能不能合并**；
  但要给出可测的合并率，别只凭感觉。

### 4.4 脏页会计与节流（这块的坑我们踩得很深）

- `DIRTY_PAGE_COUNT` 是**进程全局的生产计数**（不是调试计数）：水线节流与后台唤醒都读它。
  权威数据结构在各自 `Vmo` 的脏页索引里，全局量只是 O(1) 摘要。
- **坑 A（全量卡死的真因）**：脏标记随条目消失（VMO 拆除 / 条目移除）时会计不退 ⇒ 计数
  永久虚高 ⇒ daemon 只在 `dirty_inodes` 非空时才刷、而节流等的是全局计数 ⇒ 前台写
  **永久等待**（`[WBIDLE] dirty>0 dirty_inodes=0`）。修复：VMO 两处退还（`cbbd99db7`），
  并改成**饱和递减**防下溢（`6bd12c511`）。
- **坑 B（journaled 目录页没有归宿）**：目录页的持久性由 journal 端到端拥有，
  `sync_data_pages` 对 `Dir && journal` 直接 `return Ok(0)` ⇒ 脏标记永不随写回清除，只能
  等页缓存/VMO 拆除（时点绑在"最后一个 `Arc` 何时归零"）⇒ 计数虚高、目录负载误触节流、
  ktest 里跨用例残留。完整诊断与修复方案见
  `rNVPC-rTIO/ktest-dirty-page-residue.md`（**方案已定稿，未动代码**）。
- **坑 C**：把清零放在 `Drop for Ext4Fixture` 里会**下溢**——`Drop::drop` 体执行时字段
  还没析构，inode/页缓存仍活着。正确落点是"下一个 fixture 建立之前"。

### 4.5 前台节流路径的等待策略：唤醒风暴与定时器风暴（两个都踩过）

容量耗尽是**正常**状态（缓存打满 ⇒ 前台必须等后台落盘），但"怎么等"决定成败。
002 阶段的记录（`os_com_refactor_plan/002-background-writeback/README.md` §4）：

- **坑 1：每轮无条件 `wake_all`。** writeback 每轮末尾唤醒 **16 个**排队的前台线程，
  `wb_wake` 实测 **74ms/轮，占 writeback 总时间 63%，max 26.7s** —— 唤醒本身就吃掉一半
  以上的写回时间。
- **坑 2：改用 `wait_until_or_timeout` 自唤醒。** 这是"看起来更文明"的做法，实际更糟：
  该 API **每次调用都新装一个定时器**，而定时器注册要拿全局 `SpinLock<BinaryHeap>` 并
  `disable_irq` ⇒ 高频前台路径上形成**定时器风暴**，反过来饿死写回线程。这条结论至今
  留在代码注释里：`ext4/fs.rs:2907` —— "a `wait_until_or_timeout` here arms a fresh
  timer per call — a storm that starved the writer"。
- **采用的解法：自轮询。** 前台在 `dirty + inflight > high` 时 `spin_loop(64)` 短忙等 +
  `Thread::yield_now()`，自己按调度节奏复查；writeback 不再 `wake_all`。
  结论原话：**"前台高频路径禁用 `wait_until_or_timeout`（每次创建定时器），也禁用每轮
  `wake_all`；自轮询（spin+yield）是当前 Asterinas 下最省的两个选择。"**
- **适用边界**：这一条只针对**前台高频路径**。低频守护路径仍可用带超时的等待 ——
  `writeback_loop` 的 `wait_until_or_timeout`（谓词：stop 或 `DIRTY >= background`）
  （每轮一次）保留。
  ⚠️ **但 rNVPC WAL 回压等待（`rnvpc/wal.rs::request_writeback_and_wait`）不属于“低频”**：
  它是**每次写**都可能踏到的路径（WAL 满时）⇒ 最初沿用了 `wait_until_or_timeout`，
  **已改为自轮询**（`spin_loop(64) + Thread::yield_now()`，`4dd9011f3`）。

同一阶段还踩到三个会把**吞吐测量搞错**的语义坑：

1. **按 inode 数限量 ⇒ 一轮只刷 ~800KB**：`writeback_dirty_inodes` 一度按"每轮扫 N 个
   inode"限量，在"多文件、每 inode 脏页少"的场景下清空水线要 **16 轮**，脏页数降不下来
   ⇒ 改成**按页数限量**，并用 `writeback_cursor`（有序 `BTreeSet` 上的轮转游标）避免小
   ino 饿死大 ino。
2. **水线语义**：前台等的是**容量上限（high）**而非 low —— `dirty <= high` 全速写，只有
   `> high` 才限速；high/low 只驱动后台写回（background 启动、刷到 low 停）。让前台等
   low 会把吞吐按低水线砍掉一截。
3. **节流必须把在飞 bio 算进去**：水位取 `DIRTY_PAGE_COUNT + INFLIGHT_WRITEBACK_PAGES`。
   只数脏页会漏掉"提交了 bio 还没落盘"的页 ⇒ **带宽虚高**（数字漂亮，数据还在内存）。

效果（002 文档的 release 实测，`/dev/sda4`、smp=24、60s；具体 FS 以该文档为准）：

| 场景 | 最初基线 | 优化后 | direct 落盘天花板 |
|---|---|---|---|
| nj16 / 16 文件 | 1128.95（**虚高**） | **202.16** | 260.78 |
| nj16 / 256 文件 | **2.49** | **190.54** | 252.59 |

两个数分别对应上面第 3 条（1128.95 **超过**落盘天花板 ⇒ 虚高被压回）与第 1 条
（2.49 → 190.54，**76×**）。⚠️ 这是早期矩阵、且 nj16 有 3–10× 双稳（§6 第 3 条）
⇒ 只作"手法有效性"的证据，**不作最终性能结论**。

### 4.6 前台与后台竞争 `inode.inner` 锁（以及"别冤枉这把锁"）

**症状**：写回线程被前台写饿死，`wb_total` 一度到了 **6.2s/轮**（002 文档）；
量化工具有现成的分段计数：`LATENCY_STATS.wb_inner_lock` / `write_inner_lock`。

**根因（002 原文）**：写回要"按限额"控制每轮页数，而当时的做法是**遍历 `dirty_ranges`
（O(pages)）并在 per-inode 上额外读一次 `i_size`** —— 两件都在争同一把 `inner` 锁，
于是前台写和后台写回互相把对方挤出去。原文："关键教训：per-inode 的 `inner` 锁争抢
（额外读 `i_size`）会饿死 writeback"。

**解法（三件，002 优化项 1/2）**：

1. **让"限额"不再需要访问 inode**：`flush_range_async` → `sync_data_pages` →
   `sync_data_and_meta_no_barrier_async` **全链路返回 `Result<usize>`（实际提交页数）**，
   写回直接用返回值扣减预算；限量用 O(1) 的 `collect_marked_pages().len()`，
   **禁止再遍历 `dirty_ranges`**（O(pages) 且锁竞争）。
2. **批量提交 + 一次性等待**：一轮里批量异步提交 bio，然后
   `INFLIGHT_ZERO_WAITER.wait_until(==0)` 等整批完成再 `reap`。避开两个极端：
   per-inode 同步 flush（设备队列饥饿）与纯异步（dirty 计数不降、前台空等）。
3. **按页数限量 + 公平轮转**：`WRITEBACK_CHUNK_PAGES = 4096`（16 MiB，对齐 Linux
   `MAX_WRITEBACK_PAGES`）；`writeback_cursor`（`dirty_inodes` 是有序 `BTreeSet`，
   游标轮转）避免小 ino 饿死大 ino。

**铁律（iron law 1，代码里 15+ 处引用）：任何等待都不得发生在锁下** —— 等提交、
等设备、等空间都不行。典型落实：

- fsync 的 `log_wait_commit` 在**释放 `inner` 之后**；
- 前台节流的自轮询**必须在 `inner` 释放之后**（`write_at` / `write_direct_at` 的 `Ok`
  分支，此时 `write_at_once` 的 guard 已 drop）——否则前台持锁自旋会彻底饿死写回；
- `commit_and_wait_running` 在锁外调用；
- 锁序固定 `inner` ① → journal handle ②，不可反序；读路径的 `ra`（预读检测器）锁在
  `page_cache()` **之前** drop（后者会短暂取 `inner`），避免叶锁嵌套。

**反例：别把锁当替罪羊。** 008 的分解表里 `wb_inner_lock` 只有 **2–4 µs（快区）/ 
10–32 µs（慢区）**，同一份表显示真正的慢在 `wb_journal_capture`（~30–60×）与
`wb_alloc_pending`（~20–60×），而前台 `write_total` 里 `write_throttle` 占 **96.6%**
—— 那是**症状**不是原因。⇒ 方法学：**先把每一段计时打出来再下结论**（`LATENCY_STATS`），
不要凭直觉归因为"锁竞争"。

**副产品**：这把锁也提供了一条简化保证 —— 同一 inode 的元数据操作被它串行，008 的
"按执行流分桶"（owner 取执行流而非 inode）正是靠这一点论证"重合场景已被全部覆盖"。

## 5. 正确性对性能测量的污染（别把 bug 当性能）
- `O_DIRECT` 读会看到旧的设备内容：读之前必须把重叠的 buffered 写**物化并转 written**
  （`allocate_pending` → `mark_range_written`），否则 DIO 测的是"错误数据 + 假吞吐"
  （`51e60ae20`）。本项目**已放弃 O_DIRECT 适配**，性能实验只测 **fsync 与 buffered**。
- 未写扩展的转换、两阶段偏移、目录块增长捕获等（`96065ce98`）属于正确性修复，
  但它们会改变回写路径的 BIO 形态 ⇒ 性能对比必须固定在同一提交基线上。

## 6. 测量方法学（这部分同样会"踩坑"）

1. **一切性能实验串行**：两个容器共享宿主机 `/dev/kvm`、`/dev/sda3`、`/dev/nvme1n1p1`，
   并发跑会互相污染吞吐，且真实盘同一时刻只能被一个仓库独占。
2. **跑前清残留 QEMU**：`docker exec asterinas-dev pkill -9 -f qemu-system-x86_64`，
   否则 `Could not set up host forwarding rule 'tcp::40144-:22'` 会让整轮作废。
3. **方差与双稳**：`nj16 orderless` 存在 **3–10× 双稳**；`nj8` 也有方差 ⇒ **≥3 轮取中位**，
   不拿单轮数作结论，也不拿不同基线的数字做严格等式比较。
4. **观测口径要能证伪**：不要只看 fio 的吞吐，要同时收：设备写计数、`[NOJOURNAL_WB]`
   系列计数器、`wb_*`、在飞页/BIO、`DIRTY_PAGE_COUNT` 水线、以及 rNVPC/rTIO 自己的计数
   （`in_flight_peak` 等）。**"fsync 不产生真实同步写"必须有计数证据**，不能靠主张。
5. **插桩零开销**：`LATENCY_STATS` 在 `LATENCY_ENABLED=false` 时零开销；分段计时
   （`fsync_*` / `wb_*` / `write_*`）是把"哪一段贵"定位出来的主要手段。
   解析工具：`os_com_implementation/test/ext4-journal-compare/parse_latency.py`、`parse_summary.py`。
6. **A/B 要固定驱动变量**：三臂对比（同步/批量缓冲/异步 bio）的作业与脚本见
   `test/aster-ext4-baseline-test/ab_*.sh`、`*_compare.py`、`barrier_compare.py`。
7. **对比必须对齐「脏页预算」**（2026-09-24 踩到）：guest 脚本曾把 `ext4` profile 的
   `writeback_capacity_mb` **写死 256**，而 `rnvpc*` 用 `--wr-capacity-mb`（1/4 档 = 64）
   ⇒ 两边脏页窗口差 **4×**。同一 native 在 randwrite-4k 上：64 档 **79.75 MiB/s** vs
   256 档 **180.28 MiB/s**（**2.26×**）——纯配置伪影，却被当成"rNVPCe 比 native 慢 2 倍"。
   ⇒ 任何 `rnvpc*` vs `ext4` 的对比，**先核对 `=== PARAM ===` 行里的 `mount_opts`
   两边的 `writeback_capacity_mb` 一致**。已把 ext4/ext2like 也改为使用显式覆盖
   （默认仍是 256，保持 Linux 对齐语义）。
8. **脏页节流必须单列**（否则会把系统水位等待误算成 fs 内部开销）：这套配置下
   `write_throttle`（`balance_dirty_pages` 等价）占 `write_total` 的
   **native 72–88%**、**rNVPCe 50–88%**（1/4 档、randwrite、插桩开）。
   结论：**前台 `write()` 的时间大头是「等脏页排空」，不是 fs 自己的元数据/分配操作**；
   而排空速率才是 fs 之间真正的差别（同预算下 nj8 rNVPCe 慢 1.39×，nj1 反而快 1.14×）。

## 7. 已解决 / 未解

### 7.1 已解决（2026-09-24 核对）

- **rNVPCe 去 WAL（I5）**：已完成。`ExtendCowContext` 并列新增（不复用 `InodeCowContext`、
  零 WAL），退休点落在 `write()`（带落点已知 + 整页覆盖两个前提）。1/4 缩放实测：
  `nvpc_wal*` 恒 0（判据 1）、单页退休 **nj1 6 µs**（I3 基线 47 µs，判据 ≤15 µs）、
  `drain_bw` nj1 **89.8** vs I3 47.41 MiB/s（**+89%**）。期间修掉 4 个并发/顺序缺陷
  （§0 #18/#19/#20、§1.1）+1 个池层性能陷阱。详见 `rNVPC-rTIO/I5-extend-cow-implementation.md` §11。
- **journaled 目录页的脏标记归宿**：B 片（`sync` 边界给目录页脏标记显式归宿）**已实施**；
  A 片的兜底（fixture 前置清零 + VMO 饱和递减）已在 `6bd12c511` 落地。残留家族消失，
  详见 `rNVPC-rTIO/ktest-dirty-page-residue.md`。
- **全量 ktest 跑不完（~413/719）**：**已解决**。现在跑完，结果为
  `722 passed; 3 failed; 0 filtered out`，失败集合固定为 superblock 计数器家族
  （`fs.rs:5198` 246 vs 245、`fs.rs:5679` 2027 vs 2028），且**不再带走内核**。
- **`parse_baseline.py` 解析错误**：`FIO_RE` 的 fs 组用 `[a-z0-9]+` 匹配不到 `rnvpc_extend`
  （下划线）⇒ 只出 1 行、该段吞掉两轮 write 块、`drain_bw` 对不上轮次。已修为
  `[a-z0-9_]+?`（旧 CSV 存档为 `summary.buggy.csv`）。
- **I4 前置**：池容量已走 cmdline（`rnvpc_size` 等）且 harness 已参数化（`--wr-capacity-mb`）；
  “三对象严格串行 + 每次 fresh mount”已作为前提约定。
- **rNVPC 的 completion 边界**：rTIO v0 已落地（`4dd9011f3`），completion 现在驱动
  WAL 退休与旧页回收。注意 **fsync 的语义就是“推进到 NV 持久边界”**（design.md 明确定义），
  **不是**“等 SSD 写完成”——之前把这两件事混同的表述已作废。

### 7.2 未解 / 待验证

- **rNVPCe 前台 `write()` 的时间大头是「脏页节流」，不是 fs 内部操作（2026-09-24）**：
  1/4 档实测 `write_throttle` 占 `write_total` 的 rNVPCe **50–88%**、native **72–88%**；
  而 rNVPCe 的退休仅 **5–7 µs**、元数据（NVBH）分配 **0 µs**、容量等待 **0**。
  ⇒ 两边差别只体现在**排空速率**（nj8 慢 1.4–1.7×，nj1 反而快 1.14–1.56×，**同预算下**）。
  完整分解、元数据假设的否证、以及脏页预算伪影的更正见
  `rNVPC-rTIO/seqwrite-decomposition.md`。
- **回刷线程持 `inode.inner` 过久**：1/4 缩放实测回刷每轮持锁 ~100 ms，前台 `write_total`
  有 72% 在等这把锁（角色与 002 相反：这里是回刷饿死前台）。
  **I5 已把单页退休成本从 47 µs 降到 6 µs（nj1）**，持锁窗口随之缩小；但没有重测这项。
- **nj8 下回压排队占大头**：I5 后的 1/4 缩放轮里 `extend_in_flight_wait` 达 **32.7 万次/轮**，
  nj8 单页退休均值 30 µs（对比 nj1 的 6 µs）几乎全是**排队**而非单页成本。
  ⇒ 下一个真正的瓶颈是**回刷侧的并发度**（多 worker / in-flight budget，P6），
  而不是继续压单页成本。
- **rTIO 多 worker 池**：v0 是**单线程回刷** ⇒ 目前**没有** `in_flight_peak` 之类的深度证据
  可报；"多 worker 提高设备并发度"仍是**待验证主张**（服务实现-tasks §4 R5 的已知例外）。
- **数据集 < 容量 ≠ 零回刷**：WAL 额度按**更新次数**（1024 op-log/inode）计，fio randwrite
  对同一 inode 极易超；`rnvpc_extend` 在 I5 后改用"未落 SSD 的 NV 数据页数 + 全局 1/3 池容量"，
  同样会因容量有限而回刷。不要把"数据集 < 容量"当作"无回刷"的依据。

## 8. 相关文档与资产索引

| 资产 | 位置 |
|---|---|
| 性能矩阵 / journal 对比脚本 | `os_com_implementation/test/ext4-journal-compare/` |
| 测量结果（每轮一个目录） | `os_com_implementation/test/ext4-journal-compare/results/<时间戳>_{ext4_journal_compare,ext4_perf_matrix,single_round}/` |
| fio 作业与 A/B 脚本 | `os_com_implementation/test/aster-ext4-baseline-test/` |
| fsync 深挖（009，含 §7–§12 结论） | `os_com_implementation/os_com_refactor_plan/009-fsync-device-roundtrips/README.md` |
| 后台写回与节流深挖（002：唤醒风暴/定时器风暴/按页数限量/虚高/`inner` 锁饿死，含实测表） | `os_com_implementation/os_com_refactor_plan/002-background-writeback/README.md` |
| nojournal 元数据回写深挖（008：分段计时表、"按执行流分桶"、`wb_inner_lock` 不是瓶颈） | `os_com_implementation/os_com_refactor_plan/008-nojournal-delalloc-meta-writeback/README.md` |
| nojournal vs journal 写回分解（011：每 inode 漏斗各阶段耗时表） | `os_com_implementation/os_com_refactor_plan/011-nojournal-vs-journal-writeback/README.md` |
| 读写路径现状（Batch A / B1 清单） | `rNVPC-rTIO/read-write-path-delta.md` |
| 脏页会计残留诊断 | `rNVPC-rTIO/ktest-dirty-page-residue.md` |
| 交接单 | `NEXT-SESSION.md` |
