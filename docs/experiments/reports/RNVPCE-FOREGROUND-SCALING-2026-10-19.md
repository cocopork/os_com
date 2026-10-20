# rNVPCe 前台写延迟的 BS 与线程扩展分析

## 结论

前台写延迟随 BS 增长，主要因为当前 syscall-sized transaction 仍逐 4K 页执行
extent soft-update。单线程从 4K 到 128K 时，`fg_body` 从 24.53 us/write 增至
616.01 us/write，但每页成本从 24.53 us 降至 19.25 us，并没有随 BS 增长。
128K 的一次 `write()` 包含 32 次树更新；它不是一次合并的 32 页 extent 更新。

线程增长是另一种现象。fio 每个 job 写独立文件，所以不是多个前台线程争同一个
inode。`write_inner_wait` 和 per-inode `publication_wait` 始终低于 0.5 us/write；
延迟增长发生在已经进入事务后的共享 NVPool 路径。16 线程时，4K 的
`fg_body` 是单线程的 14.2 倍，128K 每页 `fg_body` 是单线程的 10.5 倍。
增长最大的阶段是 extent soft-update、逐页 data register、全局容量分配以及全局
dirty queue。

## 测试条件

- `/dev/nvme1n1p1`，每点重新格式化，完成后 `e2fsck -fn`。
- 64 vCPU、8 GiB、virtio-blk 单队列 1024；fio 与 rTIO/virtio CPU 隔离。
- rNVPCe nojournal、buffered psync、direct=0、每点 2 GiB。
- NVPool 128 MiB、metadata reserve 64 MiB、data limit 64 MiB、fast 模拟。
- 顺序写；每个 fio job 使用独立 inode。
- 8/8 点完整写入、sync、卸载成功，`e2fsck` exit 0。

## BS 扩展：单线程

| BS | fio mean us/write | `fg_body` us/write | `fg_body` us/page | extent us/page | data us/page |
|---:|---:|---:|---:|---:|---:|
| 4K | 32.84 | 24.53 | 24.53 | 14.58 | 1.90 |
| 16K | 107.71 | 82.98 | 20.74 | 14.53 | 2.08 |
| 64K | 397.69 | 308.20 | 19.26 | 14.47 | 1.87 |
| 128K | 790.55 | 616.01 | 19.25 | 14.52 | 1.99 |

BS 变大后每次调用的页数按 1、4、16、32 增长，而 `fg_extent` 维持约
14.5 us/page。代码中的 `replace_tree_extents()` 对 `replacements` 逐项循环；每项
独立准备根节点并调用 `replace_one_tree_extent_top_down()`。叶更新随后独立执行
repair image、pending mutation mark、应用、验证、清理和 reverse locator 刷新。

128K 单线程的一次 write 中，extent 共 464.58 us，其中 repair image 31.32 us、
mark 39.15 us、apply 43.73 us、verify 25.16 us、cleanup 33.05 us、reverse refresh
58.88 us。它们都近似为 4K 数值的 32 倍。

## 线程扩展

以下均换算为 us/page；4K 的一页就是一次 write。

| BS | jobs | body | extent | data register | reserve | enqueue | inode lock wait us/write | publication wait us/write |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 4K | 1 | 24.53 | 14.58 | 1.05 | 2.50 | 0.57 | 0.06 | 0.18 |
| 4K | 4 | 64.26 | 34.70 | 4.11 | 8.45 | 1.59 | 0.16 | 0.46 |
| 4K | 16 | 348.53 | 156.31 | 46.99 | 50.33 | 18.72 | 0.16 | 0.42 |
| 128K | 1 | 19.25 | 14.52 | 1.10 | 0.11 | 0.48 | 0.07 | 0.20 |
| 128K | 4 | 59.76 | 45.32 | 4.94 | 0.35 | 1.35 | 0.12 | 0.32 |
| 128K | 16 | 201.74 | 128.37 | 46.12 | 1.47 | 10.66 | 0.13 | 0.36 |

`fg_extent` 的 16 线程每页成本是单线程的 8.8--10.7 倍。128K 的 repair mark
从 39.15 us/write 增至 1081.74 us/write，data register 从 1.10 us/page 增至
46.12 us/page。锁外排队接近零，说明线程在事务内部的共享结构上消耗时间。

## 代码原因

1. `replace_tree_extents()` 名义上接收批量 replacements，实际逐页调用完整树更新。
   每页都创建并持久化 repair image 和 mutation mark，再应用、验证并回收临时对象。
2. NVPool 有一个全局 `state: SpinLock<PoolState>`，保护 page index、metadata index、
   references 和全局 dirty queue。data commit 前后、reverse/dirty 操作都会进入它。
3. NVPool 有一个全局 `allocation: SpinLock<AllocationState>`。`reserve_data_pages()`
   仍逐页调用 `alloc_data_page()`，metadata repair 对象也从同一全局 free list 分配。
4. 对象槽本身已经是 per-slot lock，不是旧式全表锁；但 data commit 在获取 slot lock
   前后仍获取全局 state，dirty enqueue 也获取全局 state。
5. fast NVDIMM 后端的每次 data/control persist 和 fence 都更新共享原子计数器。
   多个 CPU 高频写同一 cache line 会进一步放大上述逐页、逐阶段操作。实验确认
   持久化阶段随线程恶化，但本轮没有单独测量这些原子的 cache-line 等待，故这一项
   是由代码和扩展趋势支持的推断，而不是独立锁等待测量。

## 后续定位顺序

第一优先级应是把同一 syscall 中落在同一叶的 replacements 合并为一次 soft-update：
一次读取叶、一次 repair transaction、一次 reverse 批量刷新。它同时降低 BS 斜率和
共享控制面的调用频率。第二步再将 data-page reservation 改为一次锁内批量弹出，
并把 data register 的全局 page-index 更新和 dirty enqueue 改成批量接口。完成这两项
后再单独评估 fast backend 的共享统计原子；在当前每页十余次控制操作未消除前，先
优化模拟计数器会掩盖真实的软件事务开销。

## 证据

- 可复现实验：`docs/experiments/scripts/foreground-scaling-20261019/`。
- 原始日志和生成 CSV：
  `/home/zn/asterinas-build-env/foreground-scaling-20261019/`。
- 有效诊断 ISO SHA256：
  `e3d6921442b8a04d409c0ccef6150876f1462cbb013146ffdc0ffa3a2e2b4ff4`。
- 首轮因日志级别未输出 `[LATENCY]`，保存在 `invalid-no-latency/`，不参与结论。
