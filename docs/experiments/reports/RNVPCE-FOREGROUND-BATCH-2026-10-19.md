# rNVPCe 前台多页事务批处理实现与验证

日期：2026-10-19。

## 结论

本轮实现消除了 `commit_range_write()` 在一个多页 write 中逐页发布 extent soft-update
事务的主要开销。诊断构建下，128K 顺序写带宽相对同条件旧实现提高 75%--105%；
单线程前台事务成本从 19.25 us/page 降至 7.24 us/page。4K write 只有一页，没有
可合并的 leaf 或 descriptor 事务，诊断矩阵中的变化为 -8%--+3%。

默认无插桩构建的代表点也复现了收益：128K/j1 从 150.25 提高到 289.14 MiB/s，
128K/j16 从 206.81 提高到 304.26 MiB/s。所有实盘点均写入 2 GiB，完成 sync、
卸载并通过后置 `e2fsck -fn`。

## 实现

- `replace_tree_extents()` 先路由到目标 leaf，将连续且容量允许的 replacements 合并，
  一个 leaf group 只生成一次 repair image、pending mark、节点修复和 reverse refresh。
  满 leaf、跨 leaf 和需要 split 的边界继续使用原 top-down split 协议。
- `reserve_data_pages()` 在一次 allocation 临界区领取本 write 的全部页，保留 data
  hard limit、高水线通知、ABA allocation sequence 和 repair workspace 回收语义。
- 新 NVPage 的 data persist 先按批发出，再用一批 descriptor copy 和 selector 发布；
  每批最多 128 页，共用两次 fence。data persist 完成于 descriptor 可达之前。
- page-index 注册在一次全局 state 临界区完成；extent 发布后，reverse locator 校验和
  dirty queue 插入也在一次 state 临界区完成。

## 诊断矩阵

条件与旧报告 `RNVPCE-FOREGROUND-SCALING-2026-10-19.md` 相同：NVMe、64 vCPU、
单 virtio 队列深度 1024、2 GiB、64 MiB data limit、32 MiB 低水线、fast NVDIMM
模拟、rTIO/virtio CPU 隔离。

| BS | jobs | 旧 MiB/s | 新 MiB/s | 变化 | 旧 mean us | 新 mean us |
|---:|---:|---:|---:|---:|---:|---:|
| 4K | 1 | 117.74 | 115.21 | -2.2% | 32.84 | 33.58 |
| 16K | 1 | 144.61 | 213.62 | +47.7% | 107.71 | 72.82 |
| 64K | 1 | 156.90 | 296.77 | +89.1% | 397.69 | 209.98 |
| 128K | 1 | 157.87 | 298.72 | +89.2% | 790.55 | 417.26 |
| 4K | 4 | 179.49 | 165.01 | -8.1% | 86.41 | 93.40 |
| 128K | 4 | 194.88 | 398.60 | +104.5% | 2500.82 | 1248.60 |
| 4K | 16 | 121.41 | 125.35 | +3.3% | 502.61 | 491.29 |
| 128K | 16 | 209.36 | 366.83 | +75.2% | 9436.42 | 5144.72 |

128K 的主要阶段变化如下，数值均为 us/page：

| jobs | body | extent | data register | enqueue |
|---:|---:|---:|---:|---:|
| 1 | 19.25 -> 7.24 | 14.52 -> 2.30 | 1.10 -> 1.07 | 0.48 -> 0.49 |
| 4 | 59.76 -> 18.35 | 45.32 -> 5.43 | 4.94 -> 2.21 | 1.35 -> 0.67 |
| 16 | 201.74 -> 49.00 | 128.37 -> 14.84 | 46.12 -> 6.91 | 10.66 -> 1.10 |

128K/j1 的 repair mark 从每 write 32 次降到约 2 次，reverse refresh 同样从 32 次
降到约 2 次。不是严格 1 次的原因是长期顺序写会跨 leaf 或触发 split；事务次数已从
页数收敛到实际触及的 leaf group 数。

## 默认构建抽查

| BS | jobs | 旧 MiB/s | 新 MiB/s | 变化 |
|---:|---:|---:|---:|---:|
| 4K | 1 | 113.30 | 133.18 | +17.5% |
| 128K | 1 | 150.25 | 289.14 | +92.4% |
| 128K | 16 | 206.81 | 304.26 | +47.1% |

默认构建只抽查三个代表点，因此 4K 的单点增幅不能外推为稳定收益。多页 write 的
诊断矩阵和默认构建方向一致，说明提升来自事务合并，而不是 latency instrumentation。

## 正确性

- 新增前台回归：连续两页写入同一 leaf 后，leaf generation 只增加一次；旧实现的
  红灯结果为 generation 1 -> 3，新实现为 1 -> 2。
- 新增持久化回归：3 个 data frame 批量提交只增加 2 次 fence 和 9 次 persist；
  模拟掉电后全部 descriptor 和数据均可恢复。
- 7 个 rNVPCe 关键测试通过，覆盖并发容量账本、pending mark reader、迁移额度、
  前台/迁移 leaf batching、短写 durable prefix 和 5 个迁移故障阶段恢复。
- 最终完整 kernel ktest 为 311 passed、0 failed。
- 8/8 诊断点和 3/3 默认构建点均 fio error 0、sync/umount 成功、e2fsck exit 0。

## 证据与边界

- 诊断原始数据：`/home/zn/asterinas-build-env/foreground-batch-20261019/`，ISO SHA256
  `ce36acd7cc8b1fc0947ef524dc2284199b22481bbf3179575904d15b5ba0fdb6`。
- 默认构建数据：`/home/zn/asterinas-build-env/foreground-batch-unprofiled-20261019/`，
  ISO SHA256 `a24a4158c64754fa547a04538351e62d2e3be4980aa22b4e92ef5237446845b8`。
- 可复现实验脚本：`docs/experiments/scripts/foreground-scaling-20261019/`。
- 本轮优化降低了 BS 相关的逐页成本；4K 多线程仍受跨 inode 共享的 repair metadata、
  pool allocation/state 和 persist 统计 cache line 竞争影响，尚未消除线程扩展瓶颈。
