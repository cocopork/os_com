# EXT4 磁盘块分配锁细化（待确认设计）

## 目标与范围

用户希望减少 buffered write 扩展文件时磁盘空闲块分配的全局锁竞争，提出细粒度锁或 per-CPU free 资源缓存两条方向。本草案选择先缩短全局锁范围；这是实现方建议，尚未由用户确认。

第一阶段把跨 block group 搜索及寻找连续空闲区间的 bitmap 扫描移出 super_block.write()。保留短于原范围的全局提交段，不宣称消除全部全局串行化：日志捕获可能同步读取元数据，仍可能延长提交段。原有内存页 per-CPU allocator 与本项磁盘块分配无关。

## 当前约束

- fs.rs::alloc_blocks_with_intent 持有超级块写锁进行两轮跨组搜索，随后更新组位图、组描述符和超级块日志镜像。
- free_blocks 在同一锁下执行 revoke、bitmap clear、pin_freed_run；clear 到 pin 的窗口不能暴露给分配器。
- Normal、MetadataReserve、RnvpceReserved 的容量门槛不同，必须保留 rNVPCe 预留额度约束。
- 未提交事务释放的块不能提前复用。现有日志 credits、失败后中止语义、checksum 与 BLOCK_UNINIT 处理不能削弱。
- 多组描述符可以共享一个磁盘块；不得用未经同步的整块镜像覆盖其他组更新。

## 第一阶段算法

1. 短暂读取超级块几何与容量提示，释放锁。提示只用于搜索，不能授权分配。
2. 在单个组的 metadata 锁下只读搜索候选区间；不得在搜索中修改 bitmap、计数或产生日志 capture。保留 FullRunOnly 优先、BestEffort 后备和 goal 定位语义。pin 信息只能作为搜索提示，最终检查必须更新。
3. 释放组锁，然后按现有顺序获取 super_block.write() → 目标组 metadata 锁；在该提交窗口内重新检查容量门槛、目标 bits、系统保留区域以及最新 journal pins。
4. 候选仍有效时执行精确区间分配及原有 bitmap/GDT/superblock 日志更新。不在全局锁下重新搜索整个组或遍历其他组。候选长度可按最新容量缩减，但不得绕过 reserve floor。
5. 候选失效时释放锁重新搜索。采用有界乐观重试；持续竞争时回退当前串行搜索算法，保证进展，记录 fallback 次数。重试次数由实现者决定并在测试中覆盖。
6. 搜索提示得出的“无空间”不能直接返回永久 ENOSPC；最终用现有串行路径重新确认，保留 pinned-free commit-and-retry 协议。

不引入 group → superblock 嵌套锁边。free_blocks 暂保留现有保护，使 pin 发布与复核的安全关系不变。搜索阶段观察到 clear-but-not-pinned 也只能产生候选，最终复核必须拒绝尚未提交的释放块。

## 后续方向（不属于第一阶段交付）

彻底允许不同组并行修改，需要重新设计容量预留、pin 发布、超级块计数与事务镜像发布的协议，并审计所有相反锁序。

per-CPU 可先缓存搜索 hint（组号、位置），但 hint 不是独占 free extent。若缓存可消费的空闲 extent，必须明确仅内存预留或持久化预分配，统一所有分配入口的排除规则，并处理跨 CPU 消费、容量回收、truncate、unmount、ENOSPC、崩溃与事务归属；不能简单把已在磁盘标记占用但未归属 inode 的区间存入 CPU 私有列表。

## 验证与验收

- 候选搜索不改变 bitmap、组/全局计数、journal credits 或捕获数量。
- 两个线程选中同一区间后，最多一个提交成功，另一个重试；无重复物理块所有权。
- 搜索与提交之间插入 free/pin、普通分配及 rNVPCe 容量预留，最终复核保持正确。
- 近满盘、部分连续区间、MetadataReserve、RnvpceReserved、未提交释放块、事务错误与 checksum 回归通过。
- 检查冷元数据 capture 失败的 rollback/abort 语义，不引入成功返回但计数或日志不一致的路径。
- 并发扩展不同文件后核对数据及 bitmap/GDT/superblock 计数；同步、卸载后 e2fsck 检查。
- 使用既有已授权专用分区和冻结的旧 ISO 作对照，测 128KB 顺序扩展写的 1/16 线程；4KB 已预填充随机覆盖写作为不分配块的对照，不用它证明分配优化有效。
- 增加可关闭的统计：全局提交锁等待/持有时间、扫描组数、候选冲突、串行 fallback。收益不预设百分比；必须报告仍被 journal I/O 占用的时间和退化情况。

仅修改相关分配逻辑与必要测试；保留当前未提交的 cache-pressure 等改动，不自动提交或推送整个工作树。
