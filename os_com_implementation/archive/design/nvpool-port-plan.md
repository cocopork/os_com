> **Archived:** original date: 2026-09-22. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# 010 · NVPool 移植方案（os_com）

> ⚠️ **已被取代（2026-09-23）**：路线已重构为 **rNVPC / rNVPC-extend / rTIO**，
> 阶段划分改为 **P0–P6**。当前权威设计与任务排期见
> [`rNVPC-rTIO/`](../../rNVPC-rTIO/)（阅读顺序：`rNVPC/design.md` → `rTIO/design.md`
> → `rNVPC/porting.md` → `rNVPC/design-evolution.md` → `implementation-tasks.md`）。
> 本文档保留价值：**S1 移植产物**（`os_com` 的 `kernel/src/fs/nv_pool/`，14 文件/3355 行）
> 是新路线 **T2.1（NVPage/NVBH 数据面收敛）的原料**；S2 已废弃，S3/S4 由 P2–P4 取代
> （对照表见 `implementation-tasks.md` §3）。下文 S2–S4 的方案内容仅作历史参考。

> 目标：把 `asterinas_ext4`（`kernel/core/src/fs/`）的 **NVPool** 子系统移植到 `os_com`
> （`kernel/src/fs/`），作为**第三条元数据/数据路径**与现有 `journal` / `nojournal` 并存，
> 由 **内核 cmdline + 挂载选项双开关**控制，**默认关闭**。
>
> 分支：`os_com@nvpool-port`（从 `zn-research-v2@c59270bef` 起）。
>
> 本文档给出：源侧契约 → 两侧差异 → **API 映射表** → S2/S3/S4 详细方案与备选对比 →
> 实施顺序 / 工作量 / 风险 / 验收。

---

## 1. 源侧 NVPool 是什么（契约提炼）

### 1.1 本体（已移植，S1 完成）

`asterinas_ext4/kernel/core/src/fs/nv_pool/`，14 文件 **3355 行**：

| 文件 | 行数 | 职责 |
|---|---|---|
| `frame.rs` | 1094 | `NvPool` 本体：帧分配/pin/commit/staging、`MetaKey`、`PinnedFrame`、持久化栅栏 |
| `dram.rs` | 443 | `DramNvBackend`：用 `ostd::mm` 帧模拟 NV 设备（`Fast`/`Model` 两种模式、重启模拟） |
| `reclaim.rs` | 183 | 回收与 `ReclaimTracker`（对象/epoch） |
| `config.rs` | 173 | cmdline：`nv_pool_size` / `nv_meta_reserve` / `nv_data_limit` / `nv_sim_mode` |
| `persist.rs` / `recover.rs` / `capacity.rs` / `layout.rs` / `data_page.rs` / `fault.rs` / `backend.rs` / `id.rs` | 29–156 | 持久化、恢复、容量策略（元数据/数据配额与水位）、布局、数据页状态机、故障注入、后端 trait、`NvPageId` |

移植结果：**零依赖改动**（只用到 `ostd::mm::{FrameAllocOptions,HasSize,Segment,VmIo,VmIoOnce}`、
`aster_cmdline::define_kv_param!`、`spin::Once`），在 os_com 上 **0 error / 0 warning**。

### 1.2 ext4 侧契约（`ext4/nv.rs`，859 行 + 散落集成点）

`Ext4NvState` 是**每个挂载点一座桥**：把 ext4 的 "home block"（元数据块）与全局 NVPool
命名空间映射起来（`namespace = superblock.uuid`）。API 面：

| API | 语义 |
|---|---|
| `read_meta_block(home) -> MetaFrameRef` | 取元数据块的 NV 权威副本（pin 住，读取期间字节稳定） |
| `update_meta_block(...)` / `update_meta_at::<T>` / `read_meta_at::<T>` | 原地更新/按偏移读写 NV 里的元数据 |
| `read_authoritative_block(...)` | 读"当前权威"版本（NV 或 SSD，取决于是否 resident） |
| `evict_meta_block(home)` | 把 NV 版本落回 SSD 并释放 residency（要求无读者） |
| `pin_metadata_roots()` / `preflight_pinned_roots()` | **挂载时 pin 住 superblock + GDT + 全部分配位图**；池太小则挂载失败 |
| `commit_transient_block` / `clear_transient_block` | 临时块（如 extent 分裂时的新叶）提交/清理 |
| `pending_extent_mutation_inodes()` | 崩溃恢复时要修的 inode 列表 |
| `drain_all_metadata()` | 把所有元数据 home 写回 SSD 并清 residency（卸载/`sync`） |
| `stage_data_segment` / `stage_data_page` / `read_data_segment` | **数据页也可以落 NV**（staging → publish → 之后可迁回 SSD） |
| `data_page_target` / `write_data_page_to_ssd` / `release_data_page` / `reclaim_orphan_data_pages` | NV 数据页的生命周期与回迁 |
| `read_ssd` / `commit_new` / `evict_locked` / `key(home)->MetaKey` / `offset(home)` | 内部：SSD 后备读写、新建提交、淘汰、home→池偏移 |

**统一命名空间**：`ext4/inode/block_mapping/extent/node.rs` 里

```rust
const NV_TIER_BIT: u64 = 1 << 47;
enum TierBlock { Ssd(Ext4Bid), Nv(NvPageId) }   // 编码进 extent 的 start_hi/start_lo
```

⇒ 一个 extent 的物理地址最高位标记它落在 **SSD 还是 NVPool**；`super_block` 增加
`NV_TIER` incompat 特性位，由 `Ext4::enable_nv_tier()` 在**首个 NV 数据块写回时按需开启**。

**挂载/生命周期语义**（由 `nv.rs` 的 ktest 名字直接印证）：

| ktest | 说明的语义 |
|---|---|
| `mount_pins_super_gdt_and_allocation_bitmaps` | 挂载即 pin 根元数据 |
| `group_zero_allocation_sync_changes_nv_bitmap_not_ssd_bitmap` | 分配只改 **NV** 位图，SSD 位图不动 |
| `inode_table_update_is_committed_to_nv_before_ssd` | inode 表更新先提交到 NV |
| `metadata_update_is_visible_from_nv_before_ssd_eviction` | NV 是权威副本，SSD 是后备 |
| `normal_drain_writes_all_metadata_homes_and_clears_residency` | drain 才全量落 SSD 并清 residency |
| `filesystem_drain_converts_nv_metadata_to_ssd_state` | drain 后文件系统状态等价于纯 SSD |

**崩溃一致性靠"锚"，不靠 journal**（NVPool 模式下 journal 不参与持久化）：

| 源侧文件 | 作用 |
|---|---|
| `extent_mutation_anchor.rs` | `PendingExtentMutation::{home,persist,load,clear}`：extent 分裂/合并的固定尺寸持久状态，崩溃后可修复 |
| `rename_anchor.rs` | rename 的两阶段锚（`RENAME_ANCHOR_HOME`），崩溃后最多短暂双名、不丢 inode |
| `references.rs` | 跨 inode 引用计数（NV 元数据的引用生命周期） |

**`sync` / `drain` 语义分支**（`impl_for_vfs/fs.rs`）：

* `sync()`：NVPool 挂载时**不发设备 barrier**（"NVPool commits are the persistence boundary"）；
* `drain()`：先 `block_device().sync()`，再 `drain_nv_data()` + `drain_all_metadata()` +
  `finish_nv_drain()`，最后清状态。

---

## 2. 两侧 ext4 不是同一实现（决定了"搬不动胶水"）

| 维度 | `asterinas_ext4`（源） | `os_com`（目标） |
|---|---|---|
| 源码根 | `kernel/core/src/` | `kernel/src/` |
| ext4 共同文件分歧 | `inode/mod.rs` **8749 行**、`fs.rs` **7820 行**、`journal/mod.rs` **6201 行**、`inode/dir/mod.rs` 3259 行 | 同左（两侧是**独立演进**的两套实现） |
| 元数据访问 | 统一漏斗 `metadata_cache.rs`（`Ext4.metadata_cache: Arc<MetadataBlockCache>`） | **没有漏斗**；`block_group.rs` / `extent_manager` / `inode` 各处直接 `block_device().read_val/write_val` |
| extent 树 | `inode/block_mapping/extent/{mod,node,tree}.rs`（`TierBlock` 已内建） | `inode/extent_manager/{mod,node,tree,...}.rs` |
| 源侧独有 | `nv.rs`、`metadata_cache.rs`、`references.rs`、`rename_anchor.rs`、`extent_mutation_anchor.rs`、`stats.rs`、`writeback/`、`buddy_cache.rs`、`xattr.rs`、`inode/{file,sync,disk,attrs,linear}.rs` | — |
| os_com 独有 | — | 完整 **jbd2 风格 journal**（`journal/{commit,checkpoint,recovery,revoke,transaction}.rs`）、`shared_barrier.rs`、`checksum.rs`、`inode/dir/{hash,htree}.rs` |

**结论**：`nv.rs` 不能复制，只能**在 os_com 的结构上重新实现同一语义**；且它依赖的三个
前提（元数据漏斗、`TierBlock`、三个 anchor）在 os_com 都**不存在**。

---

## 3. API 映射表（源侧 → os_com）

| 源侧 | os_com 现状 | 移植动作 |
|---|---|---|
| `nv_pool/*`（3355 行） | — | **已完成（S1）**：`kernel/src/fs/nv_pool/`，`fs/mod.rs` 调 `nv_pool::init()` |
| cmdline `nv_pool_size` 等 | — | **已可用**（`aster_cmdline` 同名同宏） |
| `Ext4.metadata_cache` | 无漏斗 | **S2 新增**（见 §4） |
| `Ext4NvState`（859 行） | 无 | **S3 新增** `ext4/nv.rs`，按 os_com 的 `block_group`/`extent_manager`/`inode` 重写 |
| `Ext4::{nv_state, uses_nv_pool, release_nv_data_page, enable_nv_tier}` | `Ext4` 在 `ext4/fs.rs` | **S3 新增**字段与方法（`nv_state: Option<Arc<Ext4NvState>>`） |
| `MountOptions.nvpool`（`nvpool=1`） | `ExtMountOptions` 在 `ext4/fs.rs`（已有 `nojournal_*` / `fsync_barrier` 先例） | **S3 新增**解析 + 与 cmdline 双开关 |
| `TierBlock` + `NV_TIER_BIT` | `extent_manager` 的物理块字段 | **S4'（可选）**：若数据也要落 NV 才需要；元数据-only 版本不需要 |
| `NV_TIER` superblock 特性位 | `super_block.rs` 的 `FeatureIncompatSet` | 同上（元数据-only 版本不需要） |
| `pending_extent_mutation` / `rename_anchor` / `references` | 无（os_com 用 journal 保一致） | **S4**：三选一，见 §6 |
| `writeback::NvWritebackPublisher` | os_com 的 writeback 管线在 `vm/page_cache` + `inode/mod.rs` | S3 里做"NV 数据页发布"的等价物（若数据也落 NV） |
| `impl_for_vfs/fs.rs` 的 `sync`/`drain` 分支 | `impl_for_vfs/fs.rs` 存在 | **S3 新增**分支（NVPool 挂载时 sync 不发 barrier；drain 全量落盘） |
| `inode/sync.rs` 的 `if !uses_nv_pool() && !uses_journal()` | os_com 的 `inode/mod.rs::sync_data_and_meta` | **S3 新增**同一判断 |
| `inode/dir/mod.rs` 的 10 处 `uses_nv_pool` 分支（rename/unlink 阶段同步） | os_com 的 `inode/dir/mod.rs` | **S3 逐处移植**（语义：每个改名/删名步骤后立刻 `sync_data_pages()`，保证崩溃后不丢 inode） |
| `xattr.rs` 的 2 处 | os_com **无 xattr.rs** | 暂缺 ⇒ 记录为差异项（NVPool 模式下 xattr 走什么路径需单独设计） |
| `test_utils::Ext4FixtureBuilder::with_nv_pool` | os_com `test_utils.rs` 有 fixture builder | **S3 新增** builder 方法（供 ktest 造 NVPool 挂载） |

---

## 4. S2：元数据 home-block 漏斗（先建通路）

**为什么需要**：NVPool 要"接管元数据"，前提是元数据读写**有唯一入口**。os_com 现在是散点
式 `block_device().read_val/write_val`，直接接 NVPool 会散成几十处。

**两种做法**：

| | A. 泛化已有的 nojournal 元数据缓冲（**推荐**） | B. 照源侧引入独立 `metadata_cache.rs` |
|---|---|---|
| 起点 | `journal/mod.rs` 的 `store_nojournal_meta_block` / `nojournal_meta_cached_block` / `flush_nojournal_meta_blocks` / `begin|end_nojournal_meta_defer`（方案 A + write-behind，已验证） | 源侧 `metadata_cache.rs`（358 行） |
| 改动 | 小：把"缓冲 + 归类 + 落盘"抽成 `MetadataBlockCache`，把散点读写收敛进去；开关语义保持 | 大：新写一层 hash 索引 + 引用计数 + 与 writeback 管线的协作，需要重新验证 |
| 风险 | 低：既有 `nojournal_meta_batch=0|1|2` / `nojournal_meta_writeback=0|1|2` 开关与实测基线可做回归 | 中：新模块与既有 writeback/`sync` 语义交互面大 |
| 与源侧语义距离 | 中（缓存结构不同，但"权威副本 + 后备写回"的骨架一致） | 近 |
| 验收 | `uses_nv_pool()==false` 时行为逐字节不变 + 复用 `aster_ext4_baseline` 回归（nj1/8/32）| 同上 |

**推荐 A**：先把通路建好并回归（不引入 NVPool 语义），S3 再把 NVPool 接到这个通路上。

#### S2 改动面实测（2026-09-22，只算生产代码，`#[cfg(ktest)] mod tests` 内的不计）

| 落的函数 | 处数 | 作用 |
|---|---|---|
| `Ext4::read_blocks_async` / `write_blocks_async`（定义本体） | 6 | **extent 节点 + 元数据页的统一异步入口**；`extent_manager/*` 的所有设备访问都走它 ⇒ 那边**零改动** |
| `Ext4::store_inode_slot` | 3 | inode 表槽写（读-改-写） |
| `Ext4::write_back_inode_desc` | 1 | 非日志分支的 inode RMW |
| `Ext4::sync_metadata` | 2 | 元数据落盘 |
| `Ext4::open` / `reload_metadata_after_replay` | 2 | 挂载期 superblock / GDT / 位图读取 |
| `block_group.rs`（位图 / GDT / superblock 槽） | 10 | 分配元数据 |
| `inode/dir/mod.rs`（目录块） | 7 | 目录数据 |
| `inode/mod.rs` | 2 | — |
| **合计** | **≈ 33** | |

⇒ **漏斗只需收敛到 ~6 个方法**（`meta_read_blocks` / `meta_write_blocks`、`meta_read_val` /
`meta_write_val`、inode-slot 三件套、`sync_metadata`），比初稿估计小得多。这使 S2 成为一次
范围清楚、可用 009 基线（`nj1/8/32`）做回归的重构。

落地清单（修订版，预计）：

1. 新模块 `ext4/metadata_cache.rs`：`MetadataBlockCache`（`read/write/pin/evict/flush_all`）
   + `Backing` 枚举（`Device` = 现有行为 / `NvPool` 留给 S3）；
2. 把上表 ~33 处改为走 cache；`extent_manager` 不动；
3. 现有 nojournal 缓冲（`journal/mod.rs` 的 `store_nojournal_meta_block` 等）改为 cache 的一个
   后端实现，`nojournal_meta_batch` / `nojournal_meta_writeback` 开关与语义保持不变。

---

## 5. S3：NVPool 真正接管（第三模式并存）

1. `ext4/nv.rs`：`Ext4NvState`（按 §1.2 的 API 面，在 os_com 结构上重写）+ `MetaFrameRef`；
2. `Ext4` 增 `nv_state: Option<Arc<Ext4NvState>>`、`uses_nv_pool()`、`release_nv_data_page()`、
   `drain_all_metadata()` 等；
3. 开关：cmdline `nv_pool_size=`（提供池）+ 挂载选项 `nvpool=1`（本挂载启用），**双开关都满足才生效**，默认关；
4. 挂载：`Ext4::open` 里 `nvpool=1` 时取全局池 → `Ext4NvState::new` → `pin_metadata_roots()`
   （池不够则挂载失败 `ENOSPC`，与源侧一致）；
5. 生命周期：`sync()` 不发设备 barrier；`drain()`/卸载把全部元数据 home 落 SSD 并清 residency；
6. 分配路径：位图/GDT 更新只改 NV 副本（SSD 位图不动），这与现有 journal/nojournal 完全不同，
   **必须在 S2 的漏斗上做**。

**范围选项**：
* **S3-meta（元数据-only）**：数据页仍全部落 SSD，只有元数据落 NV ⇒ **不需要 `TierBlock`/`NV_TIER` 特性**，风险与工作量都小得多（推荐作为第一个可用版本）；
* **S3-full**：再加 `TierBlock` + `NV_TIER` + NV 数据页 staging/回迁（源侧完整语义），工作量约为 S3-meta 的 1.5–2 倍。

---

## 6. S4：崩溃一致性（三种方案对比，待你选）

| 方案 | 内容 | 代价 | 风险 | 与源侧距离 |
|---|---|---|---|---|
| **① 全实现三个 anchor** | 在 S2 漏斗上重写 `extent_mutation_anchor`（split/merge 固定尺寸状态）、`rename_anchor`（两阶段改名）、`references`（跨 inode 引用计数） | 最大（源侧这三个加起来数百行，且要在 os_com 的 extent/dir 结构上重新设计 + 各自的崩溃注入测试） | 中高（崩溃一致性是本项目最需要证据的部分） | 一致 |
| **② 只做 extent mutation anchor** | 先保 block mapping 的崩溃安全（分配/分裂最要命），rename/references 二阶段 | 中 | 中（rename 崩溃语义暂时弱于源侧） | 部分一致 |
| **③ 继续走 journal** | NVPool 模式下元数据仍由 os_com 已充分验证的 journal 保崩溃一致；NVPool 只做"容量受限的持久缓存 + 少设备往返" | 最小 | 低（复用已验证机制） | **偏离源侧设计**（NVPool 的价值主张之一是摆脱 journal 的设备往返） |

**判读建议**：若目标是"先把 NVPool 跑起来、量它的收益"，选 ③（配 S3-meta）；
若目标是"复现源侧架构并做对比研究"，选 ①（配 S3-full）。② 是两者之间的过渡。

---

## 7. 实施顺序 / 验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| **S1 ✅** | `nv_pool` 本体 + cmdline + `fs/mod.rs::init()` | `cargo osdk check` 0 error / 0 warning；`make ktest` 跑 nv_pool 的 38 个测试 |
| S2 | 元数据漏斗（§4A），`uses_nv_pool()==false` 行为不变 | 编译 + 回归：`aster_ext4_baseline` nj1/8/32 与 009 归档数据同量级（同一内核、多次重复） |
| S3-meta | `Ext4NvState` + 双开关 + pin/drain 语义 | 编译 + ktest（挂载/分配/位图/超小池失败路径）+ 能挂载并跑 fio |
| S4-③ 或 ①/② | 崩溃一致性 | 崩溃注入 ktest（断电点注入已在 `nv_pool::fault` 有基础） |
| S5 | 性能对比（可选） | `aster_ext4_baseline` 加 `nvpool` 臂 |

**风险清单**：① os_com 无元数据漏斗 ⇒ S2 是最容易被低估的一步（散点收敛涉及 `block_group`/`extent_manager`/`inode`/`dir`）；
② `TierBlock` 需要动 extent 的物理地址编码与 superblock 特性位，属"格式级"改动，必须慎之又慎；
③ 三个 anchor 是**新的一致性机制**，必须配崩溃注入测试，不能只靠编译通过；
④ ktest 目前存在与本次改动**无关的既有失败**（`ext4::fs::tests::sync_metadata_*` 等，需先在干净提交上确认基线）。

---

## 8. 附：S1 已完成的证据

* 分支 `os_com@nvpool-port`，提交 `521c5ddc4`；
* `kernel/src/fs/nv_pool/`（14 文件 3355 行，源码逐字节复制，**未改一行**）；
* `kernel/src/fs/mod.rs`：`pub mod nv_pool;` + `init()` 里 `nv_pool::init()`（无 cmdline 时直接返回）；
* `cargo osdk check --package aster-kernel`：**0 error / 0 warning**；
* `nv_pool/tests.rs`：仅把 38 个 `#[ktest]` 测试名加了 `nvpool_` 前缀（原样拷贝会与其它模块重名/难过滤，
  且 osdk 的过滤器语义见下）。

### 8.1 ktest 用法（踩过的坑，务必照此执行）

```bash
docker exec -w /root/asterinas asterinas-dev bash -c \
  "make ktest CARGO_OSDK_TEST_ARGS='<精确测试名>'"
```

* **必须走 `make ktest`**（它会先建 `test/initramfs/build/initramfs.cpio.gz`；直接
  `cargo osdk test` 会报 "Cannot canonicalize path ... initramfs.cpio.gz"）。
* **过滤器是「路径后缀」而不是子串**：osdk 把 `TESTNAME` 写进 `KTEST_TEST_WHITELIST`，
  测试内核侧转成 `SuffixTrie`（`osdk/deps/test-kernel/src/path.rs`，比较用
  `KtestPath::ends_with`）。测试完整路径形如
  `aster_kernel::fs::nv_pool::tests::<name>`，因此：
  * `nv_pool` / `nvpool_` / `nv_pool::tests` ⇒ **0 匹配**（`717 filtered out`）；
  * `nvpool_config_is_disabled_without_pool_size`（精确名）⇒ **1 passed; 716 filtered out** ✓
  * 一次只能指定一个测试名 ⇒ 跑全套要循环（42 个测试 ≈ 40 分钟）。
* **并发跑 ktest 会假性失败**：qemu 会占住 `test/initramfs/build/ext2.img`
  （"Is another process using the image"），并与 `target/osdk/iso_root` 上的
  `grub-mkrescue` 冲突（MISHIP → 建出**旧 ISO** ⇒ 过滤器看起来 0 匹配）。跑之前先确认没有残留
  `cargo-osdk` / `qemu-system-x86_64`（清理要用 `pkill -9 -f "[c]argo-osdk"` 这类括号技巧，
  否则 pkill 会连自己那行命令一起杀掉）。

### 8.2 注册数核对（证明测试确实在跑）

| 状态 | aster-kernel 的 ktest 总数 |
|---|---|
| 基线（`git stash -u` 掉移植改动） | **675** |
| 移植后（`nv_pool` 在场） | **717** |

差的 **42** = `nv_pool/tests.rs` 的 38 + `nv_pool/reclaim.rs` 内联的 4 ⇒ 移植过来的测试
**已全部注册**（此前 `0 匹配` 纯属过滤器写法问题）。

### 8.3 与本次改动无关的既有失败（待基线复核）

os_com 的 ktest 里有 3 个 ext4 测试失败：

`ext4::fs::tests::{sync_metadata_round_trip_lossless, sync_metadata_persists_inode_counts,
journaled_superblock_capture_patches_all_mutable_fields}`

它们位于 `ext4/fs.rs` 的测试模块，与 `nv_pool`（默认关闭、只被 `fs::init()` 调一次且立即返回）
无关；已安排用「`git stash -u` 后在基线跑同 3 个精确名」做对照，结论待补。
