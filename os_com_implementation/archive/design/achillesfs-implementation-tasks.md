> **Archived:** original date: 2026-09-23. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# rNVPC / rNVPC-extend / rTIO 实现任务排期

本文把 [rNVPC/design.md](rnvpc-original-design.md)、[rTIO/design.md](rtio-original-design.md) 与
[rNVPC/porting.md](rnvpc-porting.md) 的阶段定义（P0–P6）拆成**可执行、可验收**的任务，
并对照 `os_com` 当前代码状态给出差距与阻塞。

排序原则：**先解锁门（P0）→ 先做能最快产出可测数据的部分（P1 纯软件模拟）与数据面收敛
（P2）并行 → 顺序推进 P3→P4→P5 → P6 实验**；任何阶段都不改变默认路径的行为。

> **⚙️ 本文常为历史排期**：P0–P5 已实现完成（见 §0.1）；I1–I4 的 ext4 接入与分片计划在
> [../ext4-io-integration.md](achillesfs-ext4-io-integration.md) §5。
> **当前施工任务 = I5**（rNVPCe 去 WAL 重做），详见
> [../I5-extend-cow-implementation.md](achillesfs-extend-implementation.md)。

---

## 0. 起点基线（**历史**，写于 P0 之前；已全部不再成立）

> 保留本节只用于对照“开工时的起点”。**当前状态见 §0.1。**

| 项 | 当时 | 证据 |
|---|---|---|
| 代码分支 | `os_com` @ `nvpool-port`，HEAD `6c24c02d4` | `git log` |
| 已移植模块 | `kernel/src/fs/nv_pool/`（14 文件 / 3355 行），**ext4 侧 0 引用** | `ls`/`grep` |
| cmdline 参数 | `nv_pool_size` 等（旧 NVPool 命名） | `nv_pool/config.rs:24-33` |
| 单测 | 42 个 `nvpool_*` | 011/010 文档 |
| 阻塞 | 全量 ktest 卡死在 `direct_write_fills_hole`（预先存在） | 011 §10 |

**结论**：新路线下，`nv_pool/` 是 **P2 的原料**，不是已完成品；P1（rNVPC 模拟）是**全新层**。

---

## 0.1 当前状态（2026-09-24）

| 项 | 状态 |
|---|---|
| 代码分支 | `os_com` @ `nvpool-port`，HEAD `97ae9c67e`，工作区干净 |
| 模块与命名 | `kernel/src/fs/rnvpc/`（已由 `nv_pool` 改名，术语 NVBH 已统一） |
| cmdline 参数 | `rnvpc_size` / `rnvpc_meta_reserve` / `rnvpc_data_limit`（已前缀化） |
| **P0–P5** | ✅ 全部已实现并提交 |
| **I1/I2**（ext4 × `ext4-rNVPC`） | ✅ 已接入，全量 ktest 零回归 |
| **I3**（`ext4-rNVPCe`） | ⚠️ 实现偏离 `design.md §5`（引入了 WAL） |
| **rTIO** | 🚧 v0 已落地（单线程回刷 + 容量回压 + 回收竞态修复）；多 worker 池待做 |
| ktest | ✅ 跑得完：`722 passed; 3 failed`（失败集固定为 superblock 计数器家族） |
| **待办** | **I5：为 rNVPCe 重做无 WAL 的 soft-update 写路径** |

交付进度以 [../NEXT-SESSION.md](../../STATUS.md) 为**唯一入口**；本文件只描述任务范围与验收。

---

## 0.2 临时要求：实现期“只编译不跑测试”（**已于 2026-09-24 解除**）

> 该约定在 P0–P5 实现期（2026-09-23 起）有效，**现已结束**。
> 当前节奏：**每个分片（I5 的每一步）都跑双 cfg 编译；全量 ktest 作回归门禁**
> （基线 `722 passed; 3 failed`，失败集合不得变化）。

---

## 1. 任务重排总览

| 阶段 | 任务 | 交付物 | 主要验收 | 依赖 | 可并行 |
|---|---|---|---|---|---|
| **P0** | T0.1 代码改名与术语收敛 | `nv_pool`→`rnvpc`；元数据对象 `Frame/NVFrame`→`NVBH`；cmdline `nv_pool_size`→`rnvpc_size` | `cargo osdk check` 0 warning；42 个单测改名后全绿 | — | T0.1/T0.2 可并行 |
| **P0** | T0.2 三互斥开关 | `rnvpc` / `rnvpc_extend` / `rtio`（cmdline + 挂载选项），默认全关 | 同时开 → `EINVAL`；任意单开不影响其它路径 | — | ↑ |
| **P0** | T0.3 开关矩阵与基线不变性 | ktest + harness 基线对比 | 全关时 ext4 结果与 011 基线一致（±双稳噪声） | T0.1/T0.2 | — |
| **P1** | T1.1 per-inode WAL runtime | `InodeWalRuntime{inode_id,head,tail,used,records[1024],waiters}`（DRAM） | 1024 条可用、1025 条触发回压 | P0 | 与 P2 并行 |
| **P1** | T1.2 WAL + CoW 容量记账 | WAL 与 CoW page 均计入 PM 容量 | 容量计数守恒（ktest） | T1.1 | ↑ |
| **P1** | T1.3 CoW update 路径 | PageCache 修改前 `reserve→append→CoW→切换索引`（禁止原地改） | 每次 update 产生新 page + 1 条 op-log | T1.1/T1.2 | ↑ |
| **P1** | T1.4 WAL-full 回压 | 只阻塞该 inode；等待 completion 退休记录 | 单 inode 满不阻塞其他 inode（ktest） | T1.3 + T5.3 | 与 P5 有环，见 §4-R4 |
| **P2** | T2.1 NVPage/NVBH descriptor | 从 `nv_pool` 收敛：类型、粒度、恢复标识、引用、per-page `ssd_backing_block` | descriptor 自描述可恢复；无全局索引 | P0 | 与 P1 并行 |
| **P2** | T2.2 清除旧路线残留 | 移除/停用全局物理索引、SSD/NV 统一编址依赖 | `design-evolution §6` 逐条核对为 0 残留 | T2.1 | — |
| **P2** | T2.3 持久状态机 | NV 侧只留数据面 + 最小恢复描述；`DramClean/NvDirty/NvWriteback/NvReclaimable` | 状态迁移 ktest；clean NVPage 不转 DRAM | T2.1 | — |
| **P3** | T3.1 DRAM B-tree | key `(fs,inode,page_index)`，叶子存 residency/handle/ssd_backing/state | 可同时区分 `Dram`/`Nv` entry | T2.3 | — |
| **P3** | T3.2 纯 DRAM 控制面 | 锁/pin/容量/ free list/队列/worker ownership 只在 DRAM | 持久区无 Rust pointer/锁/refcount（静态检查 + ktest） | T3.1 | — |
| **P3** | T3.3 扫描重建 | 重建 `Nv` entries + 计数器 + 候选集合；丢弃 Dram entries | 恢复后无旧锁/旧计数器复用；中间态→可重试 | T3.2 | — |
| **P4** | T4.1 发布顺序 | NVPage ready → NVBH ready → 发布引用 → 上层元数据 → 撤销旧引用 → 回收 | 不变量 1/2/3 专项 ktest | T3.3 | — |
| **P4** | T4.2 串行/并行边界 | 同 inode publish 串行、不同 inode 并行；容量预留先于低层锁 | 并发 ktest（无死锁、无顺序违规） | T4.1 | — |
| **P4** | T4.3 失败回滚 | 保留数据、可重试、不丢 dirty | 故障注入 ktest | T4.1 | — |
| **P5** | T5.1 rTIO 接口 | `claim_dirty_inode/collect_inode_pages/mark_submitted/complete/release_inode` | 同 inode 单 owner；不同 inode 并行 | T4.3 | — |
| **P5** | T5.2 多 worker 池 | 现有单线程 writeback daemon → 多 worker（worker 数/每 worker in-flight/收集页数/全局上限） | 实测 in-flight 深度 > 1 | T5.1 | — |
| **P5** | T5.3 completion 边界 | `NvWriteback→NvReclaimable`、退休 WAL、释放容量、唤醒；不持粗锁 | 完成即回收、失败不丢；持锁扫描（无前台锁） | T5.2 | — |
| **P5** | T5.4 候选队列与公平性 | WAL 满/sync/最久未服务/后台优先级；预算后释放 lease 重入队 | 公平性日志 + 大 inode 不独占 | T5.2 | — |
| **P6** | T6.1 rTIO 消融 | rTIO §8 六组实验 | 吞吐/IOPS/P99/in-flight/公平性曲线 | P5 | 分组可并行 |
| **P6** | T6.2 rNVPC / extend 对比 | 与原生 PageCache、无 WAL、无 CoW 对比 | 报告注明 DRAM 模拟边界 | P1/P4 | — |
| **前置** | **B1** 修 `direct_write_fills_hole` 卡死 | 全量 ktest 能跑完 | ✅ 已解决（现 `722 passed; 3 failed`） | 独立（007 范围） | — |

推荐推进顺序：**B1 ∥ P0 → (P1 ∥ P2) → P3 → P4 → P5 → P6**。

---

## 2. 任务明细（改动面与验收证据）

### T0.1 代码改名与术语收敛（低成本、解锁后续）
- **改动面**：`kernel/src/fs/nv_pool/` → `rnvpc/`（模块名 `rnvpc`）；类型 `NvPool*`→`Rnvpc*`；
  cmdline `nv_pool_size`→`rnvpc_size`（`nv_meta_reserve`/`nv_data_limit`/`nv_sim_mode` 同步
  前缀化）；`fs/mod.rs` 的 `pub mod`/`init()`；`nv_pool/tests.rs` 42 个测试名
  `nvpool_*`→`rnvpc_*`（ktest 过滤器是**路径后缀**，会随改名失效，必须一起改）。
- **术语**：元数据对象语义名统一为 **NVBH**（保留底层 frame 作为承载类型名），
  见 design.md §2.2。
- **验收**：`cargo osdk check` 0 error/0 warning；42 个改名后单测可被精确名字运行；
  全关状态下 ext4 行为不变（T0.3 一起验）。
- **注意**：改名要**独立提交**，便于与后续语义改动区分（也便于上层 rebase）。

### T0.2 三互斥开关
- **改动面**：`rnvpc/config.rs`（或新 `fs/rnvpc/mode.rs`）加 `RnvpcMode{DramNg, Rnvpc, RnvpcExtend}`
  + `rtio` 布尔；cmdline（`rnvpc=…`、`rtio=…`）与 ext4 挂载选项（`rnvpc=`/`rnvpc_extend=`/`rtio=`）
  双入口；互斥校验：`rnvpc` 与 `rnvpc_extend` 同时生效 → 挂载/初始化返回 `EINVAL`。
- **验收**：开关矩阵 ktest（全关/单开/冲突）；冲突时**明确报错**而不是静默取一个。
- **边界**：默认全关；不改任何既有路径的行为（011 的 `nojournal_*` 开关不受影响）。

### T1.x rNVPC 模拟（纯软件成本）
- **要点**：复用 Asterinas DRAM PageCache；**不为真实 PM 做驱动**；只模拟 WAL append、
  CoW 分配/复制、WAL 容量占用、WAL-full 回压四类成本（design §3.1）。
- **验收（porting §7.1 前三条 + §7.2）**：
  - 第 1024 条可用、第 1025 条触发该 inode writeback 回压（边界 ktest）；
  - 一个 inode WAL 满**不**阻塞仍有额度的其他 inode（并发 ktest）；
  - WAL 与 CoW page 都计入容量（记账守恒 ktest）；
  - 计数：`wal_append_ns`、`cow_alloc_copy_ns`、`wal_full_wait_ns`、每 inode 未退休记录数。
- **风险**：**不要把 CoW 做成额外 memcpy 的假成本**——必须走真实的新页分配 + 索引切换，
  否则论文里的"CoW 成本"不成立。

### T2.x rNVPC-extend 数据面
- **要点**：从现有 `nv_pool/` 收敛出 `NVPage`（4 KiB 数据）与 `NVBH`（≤FS block 元数据），
  各自带恢复标识 + per-page `ssd_backing_block`；**删掉**全局物理索引与统一编址依赖
  （design-evolution §6 逐条核对）。
- **验收**：descriptor 可扫描恢复；无全局 `SSD block → NV object` 映射；clean NVPage 不转
  DRAM page。

### T3.x DRAM 控制面
- **要点**：`PageKey=(filesystem_id, inode_id, page_index)` 的 DRAM B-tree；叶子
  `{residency, runtime_handle, ssd_backing_block, state}`；锁/pin/计数/free list/队列/ownership
  全在 DRAM；扫描重建只产 `Nv` entries。
- **验收**：B-tree 同时存在 `Dram`/`Nv` entry；恢复后无 DRAM pointer/旧锁/旧计数复用；
  不可达对象交由后台回收（先提供索引即可服务）。

### T4.x soft update
- **要点**：固定发布顺序（design §5.1）；同 inode publish 串行、不同 inode 并行；容量等待
  在取低层锁**之前**；失败保留数据可重试。
- **验收**：不变量 1–5 逐条 ktest（尤其"NVBH 不得引用未 ready 的 NVPage"与"旧对象不得在
  引用撤销前回收"）。

### T5.x rTIO
- **要点**：inode lease 单 owner；worker **提交后不等待**，达到 in-flight budget 才等
  completion；completion 只短持状态锁。
- **验收**：见 §7.1 最后两条 + rTIO §8 消融；**必须实测 in-flight 深度**（`in-flight` 平均/
  峰值），否则"提高了并发度"只是主张。
- **与现有代码的关系**：002 的单线程 writeback daemon（`wb_round`/`wb_sync_total` 计数器）
  升级为 worker 池；`NOJOURNAL_META_IO_DEPTH` 那套"批量提交 + 延后 reap"是 rTIO 提交侧的
  现成参考实现。

### T6.x 实验
- 复用 harness（`run-baseline.sh` + `latency_decomp.py`/`wb_fs_compare.py`）；新增 mount
  选项驱动 rTIO 的 worker 数/in-flight 预算；观测项按 porting §7.2。

---

## 3. 与旧路线的关系

| 旧 | 新 | 处置 |
|---|---|---|
| S1（模块平移，已做） | 无 | **保留**，作为 T2.1 的原料；改名进 T0.1 |
| S2（泛化 nojournal 元数据缓冲） | 无 | **废弃**（不属于 rNVPC/extend 路线） |
| S3-meta / S3-full（ext4 元数据接管） | P2–P4 | 拆成 descriptor / 控制面 / soft update 三层 |
| S4 三案（锚点 / extent-only / 保留 journal） | P4 | soft update 顺序 + 失败回滚 取代"锚点方案"选择 |
| 全局物理索引 / 跨 inode packing | 已废弃 | T2.2 显式清除 |
| `Frame/NVFrame` 术语 | `NVBH` | T0.1 改名 |

---

## 4. 风险与前置阻塞（必须先处理）

- ~~**R1（阻塞）ktest 卡死**~~：**已解决**。全量 ktest 现在跑得完
  （`722 passed; 3 failed`），失败集固定为 superblock 计数器家族，不再带走内核。
  保留本条作为历史。
- **R2 改名波及测试过滤**：ktest 过滤器是路径后缀，改名 42 个 `nvpool_*` 时必须同步；
  建议改名后立刻跑一次"精确名单"验证（单测逐条名字）。
- **R3 不要把 DRAM 模拟说成真 PM**：design §9 / porting §1 已明确；所有报告必须标注。
- **R4 P1↔P5 环依赖**：T1.4（WAL-full 回压等 completion）与 T5.3（completion 退休 WAL）
  互相引用。**解法**：T1.4 先用现有 writeback 路径（002 的 daemon + completion 回调）打通
  语义，P5 落地后再把退休边界切到 rTIO 的 completion。

- **R5（重要，有实测反例）rTIO 的核心假设需要先做一次低成本验证**：
  rTIO 主张"多 worker + 多 in-flight BIO ⇒ 提高设备并发度 ⇒ 提升吞吐"。但 011 §9/§11 在
  **同一设备、同一负载（nj16 randwrite-4k orderless）** 上实测：把元数据写从"串行同步"
  改成 **31 深度在飞 bio**，线程可见时间 4.17 s→0.25 s/轮，**吞吐不变**（147/167/145 MiB/s
  中位）；011 §12 的结论是"离散 4 KiB 随机写无法 merge ⇒ 设备带宽降低"。
  → 若设备在该负载下已由**4 KiB 随机 IOPS/带宽**而非**队列深度**限制，单纯加深在飞数不会
  有收益。
  **建议前置实验（成本很低，1 天内）**：把 `NOJOURNAL_META_IO_DEPTH` 与 worker 提交方式做成
  挂载选项，扫 in-flight 深度 1/8/32/…，同时记录**数据页流**与**元数据流**两条队列利用率。
  结论若为"深度无收益"，则 P5 必须在设计上补一条：**inode 内合并成大 I/O**（rTIO §3 允许
  "inode 内自然合并连续 SSD blocks"）或把 rTIO 的定位明确到"数据页流 + 混合流"，否则论文
  里 rTIO 的价值主张站不住。
  > **（2026-09-23 决策）R5 取消，不单独做前置验证。** 时间优先，直接实现组件并跑测试；
  > 代价是 P5 的"深度是否有效"变成**先假定有效**。相应地，P5 实现时**默认保留"inode 内
  > 合并大 I/O"这一选项**（rTIO §3 允许），把它当作与深度扫描并列的可调旋钮，而不是等
  > 实验结论再补。深度扫描与 P6 消融（同样需要真实 VM）一起排在 VM 恢复之后。

---

## 5. 起步清单（**历史**，已全部完成）

> 本清单是开工时的拆步计划；现在 1–4 已完成，5 已取消，6 已进入并完成。
> 保留作历史记录；新任务请从 [I5 施工图](achillesfs-extend-implementation.md) §9 开始。

1. ~~**B1**：定位 `direct_write_fills_hole` 卡死~~ → 已解决。
2. ~~**T0.1a**：`nv_pool` → `rnvpc` 改名~~ → 已提交 `503a256b6`。
3. ~~**T0.1b**：cmdline 参数前缀化~~ → 已提交 `4742032a2`。
4. ~~**T0.2**：三互斥开关 + 冲突报错 + 开关矩阵 ktest~~ → 已提交 `3a3b0e581`。
5. ~~**R5 前置实验**~~ → 已取消（见 §4 R5）。
6. ~~进入 P1 与 P2 并行~~ → P0–P5 均已实现完成。

> 完成状态只以提交 + 测试输出 + 实验目录证明（porting §8）；本文件不记录"已完成"。
