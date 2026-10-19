> **Archived:** original date: 2026-09-23. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# 设计变化前后对照

本文记录 rNVPC-rTIO 从旧路线到当前路线的变化。当前语义以 [design.md](rnvpc-original-design.md) 和
[rTIO/design.md](rtio-original-design.md) 为准；本文件只用于解释变化，不同时维护第二套规范。

## 1. 总体变化

| 主题 | 变化前 | 变化后 |
|---|---|---|
| rNVPC 定位 | NVPool 本体及其持久对象管理 | Rust + PM PageCache 假设的性能模拟：DRAM PageCache + WAL + CoW |
| 扩展形态 | 未与基础 rNVPC 清楚分离 | 独立命名为 rNVPC-extend，保留现有 NVPool 的数据面思路 |
| PM 内容 | 数据与大量控制结构混合 | rNVPC-extend 只持久化 NVPage、NVBH 和最小恢复描述 |
| 运行时控制 | 部分索引、计数或 handle 被视作持久状态 | B-tree、锁、计数器、free list、队列和 handle 全在 DRAM |
| 恢复 | 依赖既有持久控制信息 | 扫描 NV 数据面重建全部 DRAM 控制结构 |
| 地址模型 | SSD 与 NV 统一物理编址，维护全局物理索引 | 每页 descriptor 记录 SSD backing address；无全局物理索引 |
| 元数据术语 | Frame/NVFrame | NVBH（Non-Volatile Buffer Head） |
| writeback 目标 | 优先寻找跨 inode 物理连续页 | 以 inode 为 worker 所有权单位，通过并发提高设备队列深度 |
| clean NV page | 可转换/保留为 DRAM page | completion 成功后直接进入可回收状态，不转成 DRAM page |

## 2. rNVPC 变化

| 主题 | 变化前 | 变化后 |
|---|---|---|
| 实现目标 | 尽量复现完整持久池与崩溃协议 | 以最简单方式模拟关键性能成本，不追求完整崩溃一致性 |
| WAL 范围 | 未固定或可能全局共享 | 每 inode 独立 WAL，固定 1024 个 op-log |
| 容量 | WAL 与 page 容量关系不明确 | WAL 明确计入 PM 容量使用 |
| update | 可沿用原地更新模型 | PageCache update 使用 CoW 模拟 |
| WAL 满 | 未形成固定反压语义 | 触发该 inode writeback，等待 completion 释放记录后继续 |
| 日志回收 | 可能在提交边界推进 | 只在 SSD completion 后退休对应 op-log |
| 论文主张 | 易与真实 PM 实现混淆 | 只报告软件路径性能，不证明真实 PM 延迟或掉电正确性 |

## 3. rNVPC-extend 变化

| 主题 | 变化前 | 变化后 |
|---|---|---|
| 数据页 | NVPage | 仍为 NVPage，4 KiB 数据对象 |
| 元数据对象 | Frame/NVFrame | NVBH，显式表达 EXT4 metadata buffer 语义 |
| 查找索引 | 全局物理地址到 NV 对象的映射 | DRAM B-tree，key 为 `(fs, inode, page_index)` |
| DRAM page | 独立于 NVPool 查找模型 | 可由 B-tree 分配和索引，descriptor 显式标记 `Dram` |
| NV page | 可能通过统一地址层访问 | descriptor 显式标记 `Nv`，自身携带 SSD backing address |
| 持久更新 | 宽泛的 pool commit/anchor 路线 | NVPage 数据先于 NVBH 引用发布，旧引用撤销先于回收 |
| 应用 write | 元数据可能延后准备 | write 后立即分配所需 NVPage 与 NVBH，并按 soft update 顺序落 NV |
| 恢复输入 | 可能包含运行时结构 | 仅包含持久数据面；DRAM entry、锁和 worker 状态全部丢弃 |

## 4. rTIO 变化

| 主题 | 变化前 | 变化后 |
|---|---|---|
| 调度主线 | 从物理视图优先选择连续页 | 从 inode 候选队列选择工作，多 worker 并发提交 |
| worker 粒度 | 页、extent 或全局 batch 均可能参与竞争 | 每个 worker 在一次 lease 中独占一个 inode |
| 同 inode 并行 | 可能由多个提交者处理 | 默认禁止，避免 publish/writeback 顺序复杂化 |
| 不同 inode 并行 | 受全局调度阶段限制 | 明确允许并行，以增加 in-flight BIO |
| 连续 I/O | 核心选择目标 | 只在已选 inode 内自然合并，不跨 inode packing |
| 分层切换 | 后台回写主导 | 读写路径可主动切换 DRAM/NV residency |
| completion | 主要更新 I/O 状态 | 同时是 NVPage 回收的唯一成功边界；rNVPC 下额外是 WAL 退休边界 |

## 5. 保留的不变量

路线变化不取消以下约束：

1. 新 NVPage 数据有效后，NVBH 才能发布对它的引用。
2. 持久引用撤销后，旧 NVPage/NVBH 才能回收。
3. 已发布对象更新采用 CoW，不能破坏唯一可恢复版本。
4. BIO completion 前必须保留 writeback 所需的 NV 内容。
5. I/O 失败必须保留数据并允许重试。
6. DRAM page 不能成为掉电恢复所需的唯一副本。

## 6. 明确废弃的路线

以下内容不再进入实现计划或论文主张：

- SSD/NV 的统一物理地址空间；
- 从 SSD physical block 到 resident NV object 的全局索引；
- 跨 inode 按物理连续性选择 writeback 候选；
- 持久化 Rust pointer、锁、引用计数、队列节点或 worker ownership；
- completion 后把 clean NVPage 转换成 DRAM page；
- 用 DRAM 模拟实现声称真实 PM crash consistency；
- 把旧的 `Frame`/`NVFrame` 继续作为元数据对象术语；
- **在 rNVPCe 里复用 rNVPC 的 per-inode WAL（I3 的做法）**。

### 6.1 已废弃：rNVPCe 复用 rNVPC 的 CoW + WAL（I3，`2126b6d3c`）

**曾这样做**：把 I2（`rnvpc`）的 `InodeCowContext`（CoW + per-inode WAL）整套搬到
`rnvpc_extend`，只额外在 `extent_manager` 多调一次 `extend_publish_residency`。

**为什么废弃**：

1. **与设计直接冲突**：本文件的 §3 已经把 rNVPC-extend 的持久更新定义为
   “NVPage 数据先于 NVBH 引用发布，旧引用撤销先于回收”（= soft update）；
   [design.md](rnvpc-original-design.md) §5 明确“soft update 是 NVPage 与 NVBH 之间的局部依赖协议”，
   并未规定 WAL。加上 WAL 后，**两套一致性机制并存**，而 soft update 已经足以保证
   顺序 ⇒ WAL 退化为**纯开销**。
2. **实测代价**：1/4 缩放延迟分解（`results/20260923_163651_aster-ext4-baseline`，
   rnvpc_extend / nj1 / 256 MiB）显示单页退休 `nvpc_retire_total` = 47 µs，其中
   **`nvpc_wal` = 30 µs（64%）**、`nvpc_publish` = 14 µs（30%），而
   **`nvpc_extend_publish`（真正的 rNVPCe 机制）≈ 0 µs**。
3. **数据模型不同**：rNVPCe 需要 **NVBH 与 NVBH 生命周期**（数据面/控制面分离），
   rNVPC 的 per-inode CoW index 没有这个概念。

**正确做法**：**并列新增** `ExtendCowContext`（不继承 `InodeCowContext`，不得出现
`if extend` 分支），一致性完全由 soft update 提供。施工图见
[../I5-extend-cow-implementation.md](achillesfs-extend-implementation.md)。
