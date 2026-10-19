# rTIO 回刷：连续页合并大 BIO（已落地）/ "只留一条后台线程"（实测否掉，已回退）

日期：2026-09-24　分支：`os_com` `nvpool-port`　对照基线：`test/aster-ext4-baseline-test/results/20260924_010943`、`20260924_011056`

## 0. 一页结论

| 要求 | 状态 | 关键证据 |
|---|---|---|
| **1. 回刷具备 native 式"连续页合并大 BIO"能力** | ✅ 已实现并验证有效 | `seqwrite-4k nj8`：**33.01 页/BIO**（39842 页 / 1207 条 BIO ≈ 132 KiB/条）；修掉一个自引入的致命 bug（§1.2） |
| **2. rNVPC 下只保留一条后台线程** | ❌ 实测否掉，**已回退** | 语义正确但吞吐 **91.8 → 12.6 MiB/s（-7.3×）**；且提频会把 NV 元数据捕获灌满池 ⇒ `journal abort`（§2） |

> ⚠️ **方法学前提（本轮踩到）**：跨会话的性能数字**不可比** —— 连未改动的 `ext4` 对照路径都在漂
> （`randwrite nj1` 80.25 → 61.84，-23%）。本轮所有结论都基于**同会话、同参数、唯一差异**的 A/B。

## 1. 要求 1：连续页合并大 BIO

### 1.1 实现（`fs/fs_impls/ext4/fs.rs`）

```
rnvpc_flush_round(budget, staged)
  ├─ candidates = rnvpc::flush_candidates(budget)
  ├─ sort_unstable_by_key(|c| (inode, page_index))        ← 不再依赖上游顺序
  ├─ 逐页 pin（pin_for_writeback_seq）→ 读进 staged（按需增长）
  ├─ 连续判定：同 inode ∧ page_index+1 ∧ ssd_target+1 ⇒ 并入当前段
  └─ submit_flush_run(段)：BioSegment::alloc(k) + 整段一次拷入 + write_blocks_async(起始块)
       完成回调：对段内 k 页逐个 evicting.finish(None) + rnvpc::complete_flush(...)
```

- 单段上限 `RNVPC_MERGE_MAX_BLOCKS = 256`（= 1 MiB，与 O_DIRECT 路径的 `DIRECT_MAX_RUN_BLOCKS` 同值）。
- 事件计数 `nvpc_flush_bios`；宿主 `latency_nvpc.py` 直接打印**合并倍率 = pages_flushed / bios**。
- 每页的 NV 回收出口仍然唯一（`complete_flush` → `complete_page`），段只是把"写设备"这一步批量化。

### 1.2 踩到的致命 bug（必读）

第一版把暂存区写成 **每轮一次** `vec![0u8; 256 * PAGE_SIZE]`（1 MiB）：

- 回刷循环 ~140 轮/s ⇒ ~**140 MiB/s 的 large-slot 堆分配抖动**；
- 在 `seqwrite-4k nj8`（连续分配 ⇒ 长段）下**卡死整机，且串口一个字都没打**（无 `[LATENCY]`、无 ERROR）
  ⇒ 一开始被误判成"设备/合并逻辑死锁"。
- **A/B 定位**：把 `RNVPC_MERGE_MAX_BLOCKS` 临时设为 1（退化回逐页）⇒ 同一轮立刻跑通 **174.2 MiB/s**
  ⇒ 确认在合并路径（且不是"合并本身"，而是它引入的大块分配）。
- **修法**：暂存区**跨轮复用**（由 `rnvpc_flush_loop` 持有并传入）+ **按需增长**
  （`if staged.len() < offset + PAGE_SIZE { staged.resize(...) }`）⇒ 随机写（段长 1）只占 4 KiB。
- 教训：内核里"每轮/每循环一次大块分配"必须按 ostd **large-slot 分配器**评估。

### 1.3 效果（同会话 A/B，`busy_bw` MiB/s）

| 轮次 | 关合并（控制组） | 开合并 | Δ | （旧基线，跨会话仅供参考） |
|---|---|---|---|---|
| randwrite-4k nj1 | 83.66 | 82.05 | -1.9% | 91.76 |
| randwrite-4k nj8 | 137.63 | **160.00** | **+16.3%** | 137.63 |
| seqwrite-4k nj1 | 96.60 | 98.84 | +2.3% | 112.28 |
| seqwrite-4k nj8 | 153.29 | 151.48 | -1.2% | 213.33 |

**结论（不夸大）**：合并**能力**已确证（倍率 33 页/BIO），但在这组 4 KiB orderless 负载上
**带宽收益未确立**（4 格中 1 格 +16%，其余在噪声带内）。合理解释：4 KiB 粒度 + nj8 的队列深度下，
每-BIO 提交开销不是瓶颈，NV→SSD 拷贝 + 池记账才是。

**后续可验证的方向**（尚未做）：`--modes fsync`（每次写后 fsync ⇒ BIO 数与往返敏感）、
更大 IO 尺寸、以及把合并阈值（`RNVPC_MERGE_MAX_BLOCKS`）与"最小合并长度"做参数化扫描。

## 2. 要求 2：只留一条后台线程（否掉，已回退）

### 2.1 我当时的实现 ≠ 理想设计

我做的合并是**"把 daemon 那一趟和回刷放进同一条线程顺序执行"**：

```
loop { ① flush（有压力才做）; ② writeback_dirty_inodes(预算) + 元数据入队; wait(10ms) }
```

**没有**把"分配元数据"搬进 `write()`。这正是它与下面"理想形态"的本质差别：

```
理想形态：write() 内分配块（落点已知）⇒ 前台自己退休 NVPage+NVBH
           ⇒ 后台只剩 ① 回刷 NV→SSD ② 完成回调回收 NV 容量 ⇒ 单线程自然成立
```

### 2.2 为什么失败：两个约束互斥

1. **频率决定 NV 元数据足迹**：rNVPC 把元数据块（journal / 位图 / inode 表）也捕获进**同一个槽位池**，
   回收依赖"同一块被新版本取代" ⇒ 捕获**背压 ∝ 提交频率 × 驻留窗口**。
   实测：那一趟 **20 趟/s** ⇒ 池在 ~7 s 内被 `meta=8192 + data=8192`（满 16384 槽）吃满 ⇒
   `journal commit failed, aborting the journal: EAGAIN` ⇒ **EROFS** ⇒ fio 永久卡住。
   **1 趟/s** ⇒ 同样负载 30 s 池全程健康。
2. **吞吐 = 那一趟的频率 × 预算**：随机写首触页在 delalloc 下**没有落点**，而
   `retire_written_pages_extend` 的准入是"整页覆盖 + **落点已知**"⇒ 前台退休≈0
   ⇒ 退休全靠那一趟分配块 ⇒ 1 趟/s 时带宽只有 **12.6 MiB/s**。
   证据：`wb_map_blocks == extend_retire_total`（13104 vs 13108，退休 100% 来自那一趟）、
   `write_throttle max ≈ 975 ms`（写者在等那一趟）、`[BGLOOP] pass=5 vs iters=500`。

⇒ 无论怎么调频率：调高 ⇒ 撞池 + journal abort；调低 ⇒ 吞吐 -7.3×。

### 2.3 关键诊断证据（保留的资产）

```
[4.818] rNVPC data staging rejected: ENOSPC "RNVPC capacity is exhausted"
        (inode 13, free=0, meta=8192, data=8192, wal=0, slots=16384)
[NVPOOL] total=16384 free=0 high_water=12288 meta=8192 data=8192 flushable=8192
[BGLOOP] iters=500 pressure=67 pass=115 flush_calls=67 retire=17153
```

两个顺带确认的事实（写进容量模型）：
- **一次退休占 2 个槽**（数据页 + NVBH）⇒ 有效页容量是 `total/2 = 8192`，**不是** `data_limit = 12288`；
- `PoolClass::Metadata` **没有独立上限**（`before_allocate` 只判 Data 的 `data_limit`）⇒ 元数据可以吃掉数据区。

### 2.4 若要重做这条要求（正确路径）

1. `Inode::write_at_once`（`inode/mod.rs:1279`）：在 `retire_written_pages_extend` **之前**，
   对本次写的整页范围**批量分配块**（一个事务覆盖整段，而非每页一个事务），
   再让 `map_blocks` 必然命中 ⇒ 准入通过 ⇒ 前台成为退休主力。
2. 前置条件（否则元数据会把池灌满）：
   - 元数据侧要有**独立上限**（否则元数据吃数据区）；
   - **journal 的 NV 捕获要随 journal 环复用/检查点显式回收**（它的驻留集合随提交频率线性增长，
     是唯一没有自然界的部分）。
3. 判据：前台退休占比成为主导、`wb_map_blocks` 大幅下降、`write_throttle` 峰值回落、
   `[NVPOOL]` 的 `meta` 不随写速率单调上升；此时再合并线程。

## 3. 本轮新增/保留的诊断资产

| 资产 | 位置 | 作用 |
|---|---|---|
| `[NVPOOL]` 行 | `dump_latency_stats()` | 池占用（total/free/high_water/meta/data/wal/pinned/hw_events/bp_events/flush_reqs）+ **当前可回刷页数**；一眼看穿"池满却无可回刷对象" |
| 真实拒绝原因 | `extend_cow.rs` 两个 `*_with_backpressure` 循环 | 原实现只报 `pool capacity did not recover`，把真因（`ENOSPC: capacity is exhausted` / `EAGAIN: data hard limit`）藏住了 |
| `nvpc_flush_bios` + 合并倍率 | `fs.rs` / `latency_nvpc.py` | 合并效果的直接量化 |
| `[BGLOOP]`（本轮临时） | 合并循环 | 区分"压力为假"与"回刷被饿死" |

## 4. 回退后的形态（当前 HEAD 工作区）

```rust
ext4.start_writeback_thread();                       // 恒常驻
if mode != Native { ext4.start_rnvpc_flush_thread(); } // rNVPC/rNVPCe 才起
```

- `rnvpc_flush_loop` 恢复为"有压力才回刷"的原形态（保留两条线程：rTIO 专职还容量，daemon 专职分配/退休/元数据）。
- 线程启动处留有注释说明"曾合并、实测 -7.3×、故回退"及其机理，避免以后重犯。

## 5. 验收记录

- 双 cfg `cargo osdk check`：0 error / 0 warning。
- 全量 ktest：见 `ktest-logs/`（期望 `724 passed; 3 failed`，失败集合为固定的 superblock 计数器家族）。
- 插桩全矩阵：`results/20260924_032853_aster-ext4-baseline`（8/8 轮跑通，含超时格修复后复验）。
- 受控 A/B：`results/20260924_032853`（开）/ `results/20260924_033042`（关）。
