> **Archived:** original date: 2026-09-24. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# ext4 I/O 路径接入 rNVPC / rNVPC-extend（规格与分片计划）

- 目标：让 fio 能对**三个对比对象**跑同一负载，且**原生 ext4 路径零影响**。
- 模式由**挂载选项**选择（`rnvpc=on` / `rnvpc_extend=on`，默认全关＝原生）。

## 1. 三个对比对象

| 名称 | 挂载选项 | 语义（写路径） | 读路径 | fsync |
|---|---|---|---|---|
| `ext4-orderless`（基线） | 默认（全关） | 现有 ext4 buffered + delalloc + 后台写回（**一行不改**） | 现有 PageCache | 现有语义（真下发设备 flush） |
| `ext4-rNVPC` | `rnvpc=on` | PageCache + **per-inode WAL（1024 op-log）+ CoW**：先取 WAL 额度 → append → 分配新 NV 页并 copy+apply → 切索引；禁止原地改 | 命中 DRAM 索引 / 从 NV 页读 | **不做真同步写**：只推进持久边界（WAL 退休依据 completion，不由 fsync 触发设备写） |
| `ext4-rNVPCe` | `rnvpc_extend=on` | soft update 发布顺序（数据先于引用、撤销先于回收）＋ **DRAM B-tree 索引**（`(fs,inode,page_index)`） | 索引命中 NV 页 | 同上（只到 NV 持久边界） |

`rtio` 正交开关：**预留**。论文里的 rTIO **不是一个代码对象**，而是两条机制：
① **多线程**——多个 writeback worker 并行刷 dirty NV page；② **读写分流**——
rNVPCe 用 DRAM page 承接读数据，不占用 NV 池容量额度。

### 1.1 ⚠️ rNVPC 与 rNVPCe 是**两条独立路线**，不得复用实现（2026-09-24 强调）

这是本项目的**方向性约束**，与 `rNVPC/design.md` 一致（§3 vs §4/§5）：

| | **rNVPC**（`rnvpc=on`） | **rNVPCe**（`rnvpc_extend=on`） |
|---|---|---|
| 设计出处 | `design.md §3`「WAL-protected PM PageCache」 | `design.md §4`「持久数据面/DRAM 控制面分离」+`§5`「soft-update 写路径」 |
| **崩溃一致性机制** | **Per-inode WAL**（1024 op-log/ino）+ CoW | **soft update**（数据先于引用、撤销先于回收） |
| 数据单元 | NVPage | **NVPage（数据面）+ NVBH（控制面）分离** |
| 索引 | per-inode DRAM 索引 | **`(fs, inode, page_index)` DRAM B-tree** |
| 是否需要 WAL | **需要**（WAL 就是它的正确性来源）| **不需要**（soft update 取代之） |
| 回刷记账 / 回压 | WAL 额度（1024 记录） | **必须是另一套**（见 §1.2） |
| 恢复 | 重放 WAL | 按 NVBH / soft-update 顺序恢复 |

**⇒ 在 `rnvpc_extend` 路径上出现 WAL，就是实现错误，不是"暂时复用"。**

> **2026-09-24 现状：I3 的实现方式不可取，已立项 I5 纠正。**
> I3 的做法是「把 I2（`rnvpc`）的 CoW+WAL 整套搬过来，只在 `extent_manager` 多调一次
> `extend_publish_residency`」。结果是 **soft update 与 per-inode WAL 两套机制并存**，
> WAL 退化为**纯粋的开销**。
>
> 实测代价（1/4 缩放延迟分解，`results/20260923_163651_aster-ext4-baseline`，
> rnvpc_extend / nj1 / 256 MiB）：`nvpc_retire_total` 47 µs/页，其中
> **`nvpc_wal` 30 µs = 64%**、`nvpc_publish` 14 µs = 30%；而
> **`nvpc_extend_publish`（B-tree 发布，真正的 rNVPCe 机制）≈ 0 µs**。
> 即：**当前 rNVPCe 的成本结构里，64% 花在它本不该有的 WAL 上。**

### 1.2 I5：为 rNVPCe 实现**无 WAL** 的 soft-update 写路径

WAL 目前在实现里**兼任三件事**，去掉它必须逐条给出替代：

| WAL 现在承担的职责 | rNVPCe 的正确来源 |
|---|---|
| ① **在刷账本**（`wal.complete_page(page_index)` 是"哪一页的 SSD 写完成了"的唯一依据）| 池的 `NvDirty` / `InFlight` 页状态本身即可作为在飞账本（soft update 天然要求"数据先于引用"）|
| ② **回压**（WAL 满 → 前台等）| 以**未落 SSD 的 NV 数据页数**为额度（对应论文 rTIO 的 in-flight budget）|
| ③ **触发旧页回收**（`retired > 0` 才回收 `pending_old_pages`）| 挂到**同一个 SSD 完成事件**上（当前已在完成链里 free 当前页，旧页一并处理）|

约束（与 §3 不变量同级）：
- rNVPCe 写路径**不得**调用 `charge_wal_delta` / `wal.try_append` / `wal.complete_page`；
- **禁止**用"`rnvpc` 的 `InodeCowContext` 加一个 `if extend`"的方式实现——两者数据
  模型不同（rNVPCe 需要 NVBH 与 NVBH 生命周期）；
- 验收：`rnvpc_extend` 下 `nvpc_wal*` 系列计数恒为 0，且单页退休成本显著下降（目标 ≤ 15 µs）。

> **现状（2026-09-23）与目标语义的差距**：
> - **目标**：`rnvpc` 与 `rnvpc_extend` 都是 **write() 即持久化、fsync() 直接返回**
>   （write 返回前就按各自规则把数据写进 NVPage：`rnvpc` = WAL + CoW，WAL 满要主动
>   回刷并等待；`rnvpc_extend` = soft update 依次更新数据与元数据，全部完成才返回）。
> - **现状**：退休点在 **`ExtentManager` 的块 I/O 边界（写回时）**，不是 write() 时
>   ⇒ 目前是"writeback 即持久化"。`rtio` 只被解析、未被 ext4 路径消费；两种模式都
>   由**原有 writeback 线程**驱动（前台 fsync + 后台 daemon → `PageCache::flush_range*`
>   → `submit_write_bio` → 退休到 NV）。
> - **当前阶段决策**：性能实验先沿用原有 writeback 线程。上述 ① ② 两条机制后续实现，
>   再用读写混合负载体现优势。因此现在**没有** `in_flight_peak` 之类的 rTIO 计数可报，
>   也没有"数据集 < NV 容量就不触发写回"这回事（脏页仍按水位被刷并退休到 NV）。

## 2. 挂接点（已定位，`kernel/src/fs/fs_impls/ext4/`）

| 位置 | 现状 | 接入动作 |
|---|---|---|
| `fs.rs` `Ext4.rnvpc_mode: RnvpcModeConfig` | ✅ I1 已去掉 `dead_code`，I/O 路径可读 | 已完成 |
| `extent_manager/mod.rs` `submit_write_bio` / `submit_read_bio` | ✅ **I2 实际挂接点在这里**（块 I/O 边界）：`nvpc_retire()` ⇒ `InodeCowContext::retire_page()`；读先查 NV 索引 | 读/写分支按模式分派 |
| `extent_manager/mod.rs` `submit_write_pages` | ✅ rNVPC 下强制 `run = None` 改走逐页（多段 BIO 直连设备会绕过闸门）；`submit_read_pages` 预读直接放弃 | 已完成 |
| `inode/mod.rs` `write_at` | I1 已按模式分派；**退休点仍在写回时** | **I5：把退休点从写回时挪到 `write()` 返回前** |
| `inode/mod.rs` `read_at` | PageCache 读 | 命中索引时从 NV 页读（已落地） |
| `inode/mod.rs` `sync_data_and_meta*` | 真同步数据 + 元数据 | rNVPC/rNVPCe 下**只推进持久边界**，不下发设备数据写 |
| `rnvpc::pool()/init()`（`rnvpc/mod.rs`） | ✅ 全局池，cmdline 配尺寸；未配置 + 挂载 `rnvpc=on` ⇒ `EINVAL` | 已完成；卸载时打印 `[RNVPC]` 统计供 fio 采集 |

> 行列号会随提交飘，以符号名为准。I5 的完整接线点表见
> [I5-extend-cow-implementation.md](achillesfs-extend-implementation.md) §6。

## 3. 不变量（硬约束）

1. **默认全关 ⇒ 与改动前逐字节等价**：所有分派都以 `RnvpcMode::Native` 走原代码路径，不新增任何条件副作用（不允许"顺手优化"）。
2. 原生 ext4 的 journal / nojournal 流程**不受影响**：不修改现有 `journal/*`、`nojournal*`、`delalloc`、`writeback daemon` 的行为；rNVPC 只在新分支里出现。
3. rNVPC 的写不原地改（CoW）；发布顺序遵守 T4.1（数据先于引用、撤销先于回收）。
4. fsync 在 rNVPC/rNVPCe 下**不得**产生真实设备数据写（这是本研究的主张之一，必须有可证伪的观测口径：设备写计数 / `NOJOURNAL_WB` 计数不增长）。
5. **rNVPC 与 rNVPCe 不得共用一致性机制**（§1.1）：`rnvpc_extend` 路径上不得出现
   per-inode WAL；反之 rNVPC 也不得依赖 soft update 的 B-tree 发布顺序。

## 4. fio 方案（本阶段）

- 测试点：`aster-ext4-baseline-test` 的 **nj8 nvme randwrite**（nojournal、8 并发 job、nvme 设备、随机写）。
- 三方各跑 ≥3 轮取中位（nj16 orderless 存在双稳噪声；nj8 也要看方差）。
- **容量档：1/4 缩放**（快速验证档）——`--wr-capacity-mb 64` 且
  `rnvpc_size=64M rnvpc_meta_reserve=16M rnvpc_data_limit=48M`；
  small 32 MiB / large 256 MiB。完整档为 256 MiB / 128 MiB / 1024 MiB。
- 数据集规模：**小于 NV 池容量**——但**这不等于不触发写回**（见§6 风险）：
  fio randwrite 对同一 inode 的写次数极易超过 per-inode 额度，回刷是**必然**发生的。
  数据集 < 容量只能保证“NV 是数据源”，不能保证“零回刷”。
- fsync：**只跑 `ext4-rNVPC` 与 `ext4-rNVPCe`**，观察 fsync 下不产生真实同步写、
  且吞吐不因 fsync 崩塌。
- 观测项：吞吐/IOPS、写放大、`[NOJOURNAL_WB]`/设备写计数、`[RNVPC]` 池统计
  （WAL 页、CoW 页、索引条目、引用数）、`[LATENCY]` 分段耗时；
  在飞深度需要 rTIO 多 worker 落地后才有可报的量（目前 v0 是单线程回刷）。

## 5. 分片计划

| 片 | 内容 | 完成判据 | 状态 |
|---|---|---|---|
| **I1** | 模式接线：`write_at`/`read_at`/`sync_*` 按模式分派，`Native` 分支保持原样；池未配置时报 `EINVAL` | 双 cfg 0 error/0 warning；**全量 ktest 失败数不增** | ✅ 已提交 `f09d4825d` |
| **I2** | `ext4-rNVPC` 写/读/fsync 落地（WAL+CoW+索引） | 单文件 buffered 写读回一致；fsync 不触发设备数据写 | ✅ 已提交 `7fa441761` |
| **I3** | `ext4-rNVPCe` 写/读/fsync 落地（soft update + DRAM 索引） | 同上 + 引用/发布顺序不变量 | ⚠️ **已接入但偏离设计（引入 WAL）**，由 I5 重做 |
| **I4** | 统计暴露 + fio 三轮对比 + fsync 双模式 | 结果表 + 方差说明 | 🚧 统计已暴露（`9f807d742`），首轮数字已出，1/4 缩放延迟分解已做 |
| **I5** | **为 rNVPCe 重做无 WAL 的 soft-update 写路径**（§1.2）：并列新增 `ExtendCowContext` + 以 NV 数据页状态为在飞账本/回压 + 完成事件驱动旧页回收 + 退休点迁到 `write()` | `rnvpc_extend` 下 `nvpc_wal*` 恒为 0；单页退休成本 ≤ 15 µs；全量 ktest 失败数不增；1/4 缩放档 `drain_bw` 对比 I3 基线 | ⬜ **当前唯一待办**（施工图） |

## 6. 风险

- **池容量**：元数据也占 NV 容量（每 block 一个 NVBH）且元数据可借用数据额度；工作集估算
  要留余量。
- **回刷必然发生**：`rnvpc` 的 WAL 额度按**更新次数**（1024 op-log/inode）计，fio randwrite
  对同一 inode 极易超过 1024 次写 ⇒ 即使**数据集 < 容量**也会满 WAL。
  （`rnvpc_extend` 在 I5 之后改用**未落 SSD 的 NV 数据页数**做额度，同样会因容量有限而回刷。）
  不要在报告里把“数据集 < 容量”当作“零回刷”的依据。
- **接入面**：`write_at` 是 ext4 的核心路径，改动风险高。I1 已把“零影响”钉死
  （全量 ktest 失败数不增），I5 再进 `write_at` 时必须重跑同一门禁。
- **回刷线程持 `inode.inner` 过久**：1/4 缩放实测回刷每轮持锁 ~100 ms（≈48 µs/页 × 2064 页），
  而前台 `write_total` 有 **72%** 花在等这把锁上；参见
  `../os_com_refactor_plan/002-background-writeback/README.md`（同类问题，角色相反）。
  I5 降单页退休成本会直接缩小这个窗口。
