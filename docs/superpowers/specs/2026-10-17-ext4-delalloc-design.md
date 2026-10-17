# EXT4 journal / nojournal delayed allocation

用户授权：在 rebuild 实现两种模式的 delalloc，详细遵循历史计划007/008及 ext4_performance.md 的约束。原工作树已有未提交实现，全部保留，不自动提交推送。

## 行为边界

Native regular-file buffered write 默认启用 delalloc；nodelalloc 保留立即分配行为。rNVPC/rNVPCe 独立路径不变，O_DIRECT 与 fallocate 仍显式分配。

前台只进行容量预留、inode/extent-status 内存状态维护、PageCache 分配/复制、size/timestamp 更新；不打开 journal handle，不修改块组位图、extent磁盘树、GDT 或 inode-table。前台保留短时预留锁、inode 锁和页面锁，不能声称完全无锁/无内存分配。

待分配状态集成到现有 extent-status 层，避免另建 PendingAlloc 影子树。重复覆盖必须去重；容量预留计入统一 physical/inflight/rNVPCe 账本，包含数据和树增长元数据，不能允许普通分配偷用。复制故障/短写、truncate/punch/unlink及丢弃必须释放恰当额度。

writeback 在 inode 串行化下按有界逻辑范围分配物理空间、更新 unwritten extent，再按实际写入范围转 written 并提交 data。journal 模式通过有界 handle 捕获 bitmap/extent/inode，注册 ordered data，任何可能暴露新映射的事务提交前必须先完成数据。nojournal 不宣称掉电原子性，但 sync/fsync 返回成功前必须持久化必要 bitmap/GDT/extent/inode 和数据并执行设备屏障，错误不能被吞掉。

所有 durability funnels（后台worker、fsync/fdatasync、sync、卸载）必须处理 delayed；非写入元数据操作不能提前捕获包含未分配数据的 i_size；读/部分页写遇 pending hole 必须读零或页缓存。mmap hole writeback 不允许在无 handle 时偷偷 allocate_one；不能因注册范围大于脏页范围而分配干净空洞。

## 验收

两模式分别验证：前台不分配/不启动事务；读回；重复与部分覆盖；稀疏洞/预分配；容量竞争与接近满盘；错误回滚与可重试；truncate/punch/删除；fsync/sync与重新挂载；journal ordered/replay。运行完整 kernel ktest，独立最终审查，并记录未验证的故障窗口。性能与真实设备实验只在功能正确后开展，不把编译或历史测试当本次通过。

实现细节由当前代码约束决定并记录在实施台账，包括预留估计、事务切分、default fixture 如何区分nodelalloc旧语义。
