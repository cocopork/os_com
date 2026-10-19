# 003 EXT4 组级块分配、内存空闲索引与容量预留

## 状态与决策

**Implemented / measured（2026-10-17）：核心实现完成，未证明稳定吞吐提升；扩展故障矩阵等专项验证仍待补齐。**

本阶段实现：**组级分配锁＋内存空闲空间索引＋容量预留计数**。
暂不实现 inode preallocation（inode PA）或 per-CPU locality-group PA。

实现基线为 `/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild` 的 `recovery/rnvpc-rebuild` 分支及后续明确记录的工作树版本。这里记录新的 rebuild 开发决策，不把历史目录的“已实现”说明当成当前 rebuild 的功能证据。本次实现和运行证据已同步到 `../../STATUS.md`；历史目录的状态不替代本次证据。

本决定取代前一份 `2026-10-17-ext4-block-allocation-locking-design.md` 中“组内搜索、全局锁复核提交”的第一阶段方案作为本项最终目标。旧方案可用于理解讨论过程，但不能单独作为本项完成证据。

## 目的

消除 `Ext4::alloc_blocks_with_intent()` 使用 `super_block.write()` 包围跨组搜索、位图分配和日志捕获的粗粒度串行化，让不同 block group 的分配能够并行推进，并减少碎片化场景的线性扫描成本。

不承诺所有锁消失：journal 状态、共享 GDT buffer、设备队列仍可能竞争；共享容量计数也需要原子更新或短临界区。目标是没有一把文件系统级锁覆盖整个搜索、组内分配和元数据 I/O。

## 当前代码与必须保留的语义

以下路径均相对 rebuild 仓库：

- `kernel/src/fs/fs_impls/ext4/fs.rs`：分配、释放、超级块空闲计数、rNVPCe 容量预留、statfs/sync 等入口。
- `kernel/src/fs/fs_impls/ext4/block_group.rs`：组位图、组描述符、连续区间搜索与 journal capture。
- `kernel/src/fs/fs_impls/ext4/journal/{mod.rs,transaction.rs,commit.rs,checkpoint.rs}`：事务镜像、释放块 pin、事务提交和 checkpoint。
- `kernel/src/fs/fs_impls/ext4/inode/extent_manager/{mod.rs,tree.rs}`：数据块及 extent 元数据块分配调用方。

实施前的全局锁不只是保护计数，也使 `bitmap clear → pin_freed_run` 相对分配器不可分割。直接删除它会允许重用尚未提交事务释放的块。新的锁和索引必须承担这一职责。

## 设计

### 1. 每组的分配状态与锁

每个 block group 拥有自己的分配状态保护范围，包含磁盘 bitmap 的内存表示、空闲索引、组级计数以及待提交释放区间的排除状态。复用或细分现有组锁的具体形式由实现者决定，但锁序必须在编码前覆盖所有调用方。

不同组可并行搜索和占用资源；同组的分配、释放、索引更新串行化。全局容量授权完成后释放计数保护，再获取组锁，不能持有全局容量锁搜索或执行 I/O。

审计现有 `superblock → group → journal` 锁序及其所有反向边。不得简单改成 `group → superblock` 而保留旧入口。任何可能等待事务进展的操作都应释放组锁及容量保护；组自旋锁内不得执行阻塞 I/O。

### 2. 内存空闲空间索引

采用每组 buddy/分层空闲区间索引，加速连续块查询。它是可重建的内存结构，不新增磁盘格式，也不等同于 PageCache 的 DRAM 页分配器。

有效可分配集合应满足：

`组中合法块 − bitmap 已分配块 − 未提交事务的 pending-free 区间 − 当前占用中的区间`

索引初始化需要处理 BLOCK_UNINIT、系统保留块、组尾不完整区间及校验错误；挂载恢复完成后从权威 bitmap 重建，不从旧运行时缓存恢复。搜索保留 goal 连续性、先满足整段再允许部分段的现有语义；组摘要用于筛选候选组，不能代替最终容量和所有权校验。

分配时在组锁下占用索引和位图，防止重复分配。失败时须根据是否已经产生不可撤销的日志修改决定回滚或中止文件系统，不能只恢复索引而留下不同步的位图/日志。

### 3. 容量预留计数

引入独立于 `SuperBlock` 元数据锁的运行时容量账本及 RAII reservation。容量预留只是额度授权，不预留某个具体磁盘 extent，因此不是 PA。

至少区分：已分配、可授权容量、进行中的分配额度、事务尚未提交的释放容量、rNVPCe 未兑现额度。明确物理空闲统计与当下可分配容量的区别；pending-free 不可复用，即使持久化位图已经在事务中表示释放。

普通分配不得侵占 metadata reserve floor 或 rNVPCe 已预留容量；MetadataReserve 和 RnvpceReserved 保留现有权限语义。所有入口使用同一授权协议，特别是 `reserve_rnvpce_blocks` 与普通分配必须互斥地检查额度，不能通过分别读两个原子变量产生超额预留。

先授权再执行组内分配：成功只消费实际分配量，部分成功归还剩余额度；无空间、取消和可回滚错误释放 reservation。不可撤销错误进入既有 abort 路径，不能重新开放可能已分配的容量。计数器采用原子或短锁保护的具体实现由实现者确定，不要求第一阶段实现 per-CPU 计数缓存。

### 4. 释放与事务提交

在同一组保护协议下，先建立阻止重用的 pending-free 状态，再让位图释放结果可见；或保证两者在同一不可分割窗口内完成。事务提交成功后才能将对应区间加入可分配索引并发布容量。

提交回调、组锁和 journal 锁不能形成环：先在 journal 保护下确定可释放批次，离开其锁范围后按组应用，具体发布步骤须有明确线性化点。跨组释放逐组处理，不同时持有多个组锁。

错误、journal abort、重复完成及旧提交回调必须防止重复发布或重用仍受保护的块。

### 5. 超级块计数与持久化

运行时容量授权不再通过每次持有 `super_block.write()` 实现，但磁盘超级块、GDT 和 bitmap 的计数一致性仍须保留。

实现前明确运行时账本到事务内超级块镜像的发布协议：不能把更晚、尚未提交事务的计数写进更早事务的镜像。也不能在 sync 时简单读取全局原子计数写盘，绕过 journal 顺序。可保留短暂元数据发布锁，但不得把搜索、组级分配、冷元数据读取全部包在该锁中。

不同组可能共享一个 GDT 磁盘块，必须保护字段 patch/校验和更新及事务镜像，防止旧整块镜像覆盖其他组的更新。不得为减少争用关闭 journal、校验和或崩溃恢复约束。

## 本阶段不包含

- inode PA、locality-group PA、per-CPU 独占磁盘 extent 缓存。
- delalloc；当前立即分配的 write 路径必须独立受益。
- PageCache 限流/timer 改造、DRAM allocator 重写、virtio 多队列。
- journal 全面并行化，或以修改持久性语义换取带宽。

## 验收标准

1. 不同组的分配能并行推进，无文件系统级锁覆盖全部分配过程。
2. buddy 索引与 bitmap、pending-free、实际所有权一致；无重复分配、泄漏或提前复用。
3. 并发及近满盘情况下 Normal/MetadataReserve/RnvpceReserved 额度正确，部分分配和错误无 reservation 泄漏。
4. journal 提交、checkpoint、恢复后的 bitmap/GDT/超级块计数一致；数据校验与 e2fsck 均通过。
5. 保存冻结的前后版本与配置，测 128KB 顺序扩展写的 1/16 线程；预填充 4KB 随机覆盖写作为不分配块的对照。
6. 记录锁等待/持有时间、扫描成本、额度等待、pending-free 等待及吞吐；不预设加速百分比，不将全部性能变化归因于分配器。

## 实施记录

详细步骤及仍未闭环的专项测试见 [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)。

本次采用 64 块 word 的分层连续区间摘要（约 12KiB/完整组），独立组 gate，统一 physical/inflight/rNVPCe 容量账本；不包含 PA 或 delalloc。pending-free 通过组 gate 下的新鲜 pin 快照延迟可见，不增加 commit -> group 回调。超级块发布位于组保护范围之外，仍保留短计数/capture 临界区。

- 回归：EXT4 + PageCache **593 passed / 0 failed**（ktest SMP=1）。
- 真实盘：SMP=16、8GiB、每样本 16GiB buffered fio，共 12 个吞吐样本；4GiB 重新挂载后 CRC32C 通过，包含诊断样本共 15 次 e2fsck 全部通过。
- 顺序写两轮均值：1 线程 **321.8 -> 280.6 MiB/s（-12.8%）**，16 线程 **299.8 -> 295.2（-1.5%）**；随机覆盖单次上升不能确认为分配器收益。
- 诊断：超级块平均持有 **7.954 -> 2.676μs**，但新版组 gate 平均等待 **39.098μs**，容量授权及 journal/GDT 仍有共享开销。
- 结论：实现结构目标已落地，性能提升尚未成立，单线程顺序写回退需要后续分析。未自动提交或推送。

完整实现、源码范围、实验配置、原始样本及限制：[实现报告](../../../experiments/reports/EXT4-GROUP-ALLOCATOR-2026-10-17.md)。
原始证据：`/home/zn/asterinas-build-env/ext4-group-allocator-20261017`。

## 参考

- [Linux mballoc 源码](https://github.com/torvalds/linux/blob/master/fs/ext4/mballoc.c)：组级分配、buddy、容量计数和 PA 的参考，不要求完整移植。
- [Linux EXT4 分配策略](https://docs.kernel.org/filesystems/ext4/allocators.html)。
