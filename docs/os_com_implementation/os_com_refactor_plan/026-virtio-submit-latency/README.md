# 026 星绽 virtio-blk 单请求延迟诊断与直接提交实验

日期：2026-11-18

状态：Planned。现有数据确认星绽裸 virtio-blk 每个同步 4 KiB 请求存在约
`50--60 us` 的额外基础延迟，但尚未通过分阶段时间戳确定各部分占比。

## 目标

解释并降低星绽 OS 相比 Linux guest 的裸 virtio-blk 单请求延迟。工作分为两个阶段：

1. 在不改变请求语义的前提下分解端到端延迟，确定额外延迟发生在提交交接、
   virtqueue 发布、设备处理、完成处理还是线程恢复阶段；
2. 只有在测量确认软件暂存队列和转发线程交接是主要来源后，才实现调用线程直接向
   对应 virtqueue 提交的 A/B 实验路径。

本项工作针对星绽通用块层和 virtio-blk 驱动，不针对 EXT4、rNVPCe 或 rTIO。

## 已有测量证据

相同真实 NVMe 分区、QEMU 10.2.1、四个 virtqueue、四个 IOThread 和宿主 NVMe
hardware context 绑定下，使用 `4 KiB randwrite`、`psync`、`direct=1`、
`iodepth=1/job` 对裸块设备测试，得到：

| jobs | Linux 平均延迟 | 星绽平均延迟 | 星绽额外延迟 | Linux/星绽吞吐 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 36.53 us | 86.75 us | 50.22 us | 2.37x |
| 2 | 37.20 us | 87.90 us | 50.70 us | 2.37x |
| 4 | 37.36 us | 93.67 us | 56.31 us | 2.36x |
| 8 | 44.37 us | 103.82 us | 59.45 us | 2.47x |
| 16 | 68.00 us | 144.06 us | 76.06 us | 2.12x |

证据来源：[星绽与 Linux 裸 virtio-blk 对比报告](../../../experiments/reports/ASTERINAS-LINUX-RAW-VIRTIO-4K-THREAD-SWEEP-2026-11-04.md)。

这组数据支持以下结论：

- 差距不是严格的二倍乘法关系，而是星绽在低并发下每请求固定多出约
  `50--60 us`；
- 两个系统从 1 到 16 jobs 的相对扩展曲线接近，因此多队列未展开不是首要解释；
- 16 jobs 的额外差距扩大到 `76.06 us`，表明固定开销之外还可能存在调度、队列锁
  或完成处理竞争；
- 测试不经过文件系统，也不执行最终设备 FLUSH，不能把差距归因于 EXT4、rNVPCe、
  rTIO 或持久化协议。

## 当前路径与待验证假设

当前星绽提交路径为：

```text
fio线程
  -> Bio::submit
  -> 每virtqueue的软件暂存FIFO
  -> 唤醒对应的virtio转发线程
  -> 转发线程出队并准备DMA描述符
  -> 发布到virtqueue并通知QEMU
  -> 宿主设备完成
  -> 来宾IRQ处理
  -> Bio完成并唤醒fio线程
```

代码证据：

- `BioRequestSingleQueue` 使用 `Mutex<VecDeque>` 和 `WaitQueue`：
  `kernel/comps/block/src/request_queue.rs`；
- 入队路径在新增请求后调用 `wake_all`：
  `kernel/comps/block/src/request_queue.rs:59`；
- 每个 virtqueue 创建一个长期运行的转发内核线程：
  `kernel/src/device/registry/block.rs:29`；
- 转发线程一次出队并处理一个请求：
  `kernel/comps/virtio/src/device/block/device.rs:170`；
- 请求发布前还执行 ID 分配、DMA 同步、completion map 插入及 virtqueue 通知：
  `kernel/comps/virtio/src/device/block/device.rs`；
- virtqueue avail ring 发布和通知判断包含 `SeqCst` 屏障：
  `kernel/comps/virtio/src/queue.rs:353` 和 `kernel/comps/virtio/src/queue.rs:497`；
- BIO 完成通过通用 `WaitQueue` 恢复同步提交线程：
  `kernel/comps/block/src/bio.rs:310` 和 `ostd/src/sync/wait.rs:271`。

按当前证据排序的假设如下。

| 优先级 | 假设 | 证据状态 |
| ---: | --- | --- |
| 1 | 软件暂存 FIFO 加专用转发线程引入一次额外的跨线程调度和跨 CPU 状态传递 | 高可信推断，尚无阶段时间戳 |
| 2 | 每请求的 ID 分配、DMA sync、临时容器、completion `BTreeMap` 和多次锁操作累计形成明显开销 | 中可信推断 |
| 3 | 通用完成路径及同步等待线程恢复比 Linux blk-mq 路径更重 | 中低可信推断 |
| 4 | virtqueue 的强内存屏障产生可测成本 | 机制存在，独立占比未知 |

这些假设不能在插桩前写成确定根因。

## 阶段一：端到端延迟分解

### 时间点

为每个请求分配一个仅用于诊断的关联编号，并记录以下单调时间戳：

| 时间点 | 含义 | 建议位置 |
| --- | --- | --- |
| T0 | `Bio::submit` 准备调用设备 `enqueue` | `kernel/comps/block/src/bio.rs` |
| T1 | 请求已进入软件暂存 FIFO | `enqueue_on_queue` 返回前 |
| T2 | virtio 转发线程完成出队 | `handle_requests` 中 `dequeue` 后 |
| T3 | 请求已加入 avail ring 并完成 notify 判断/通知 | `finish_submit` 返回前 |
| T4 | IRQ 路径取得 used descriptor | `handle_irq` 中 `pop_used` 后 |
| T5 | `SubmittedBio::complete` 发布完成并执行唤醒 | `Bio::complete` 返回前 |
| T6 | 同步等待者恢复并观察到完成状态 | `BioMetadata::wait` 返回前 |

对应的阶段指标为：

```text
submit_enqueue      = T1 - T0
forwarder_handoff   = T2 - T1
virtqueue_submit    = T3 - T2
device_and_host     = T4 - T3
guest_completion    = T5 - T4
waiter_resume       = T6 - T5
end_to_end          = T6 - T0
```

必须记录每个阶段的 `count`、平均值、P50、P95、P99 和最大值。汇总应使用每 CPU 或
每 virtqueue 的预分配定长缓冲区，测试期间不得打印逐请求日志，也不得让所有队列竞争
一个全局统计锁。缓冲区满后停止采样并增加 dropped 计数，不能覆盖未导出的样本。

### 基准矩阵

沿用原报告的硬件和虚拟机配置，先执行最小矩阵：

- `4 KiB randwrite`、`psync`、`direct=1`、`iodepth=1/job`；
- jobs：1、4、16；
- 每点至少 5 轮，报告均值、标准差和 95% 置信区间；
- Linux 对照沿用相同设备区间、CPU/virtqueue 映射和 QEMU 参数；
- 同时保留无插桩构建，量化插桩本身的吞吐和延迟扰动。

插桩构建相对无插桩构建的平均延迟增幅应不超过 5%。超过该阈值时，数据只能用于判断
阶段排序，不能直接作为绝对延迟归因。

### 阶段一判定

如果 `forwarder_handoff` 的平均值加 P95 抖动能够解释星绽相对 Linux 的主要差距，且
1 job 下该阶段占星绽端到端平均延迟至少 30%，进入阶段二。

如果不满足该条件，不实施直接提交重构。应按测量结果继续细分占比最高的阶段：

- `virtqueue_submit` 高：分别测 ID 分配、DMA sync、描述符构造、completion map 和
  notify/barrier；
- `device_and_host` 高：检查两侧 QEMU queue/IOThread/hctx 映射和通知合并差异；
- `guest_completion` 高：分别测 virtqueue lock、`BTreeMap::remove`、DMA sync、ID 回收
  和 BIO callback；
- `waiter_resume` 高：检查 IRQ CPU 到 fio CPU 的唤醒、调度和 CPU affinity。

## 阶段二：调用线程直接提交 A/B 实验

### 实验目的

直接提交实验只回答一个问题：去除软件暂存 FIFO 和转发线程交接后，是否能消除大部分
固定基础延迟。它首先是实验路径，不直接替代通用块层接口。

### 机制

- 保留现有路径作为 `forwarded` 对照；
- 为 virtio-blk 增加仅由实验配置启用的 `direct` 路径；
- `direct` 路径仍按照 `BioQueueAffinity` 或提交 CPU 选择 virtqueue；
- 调用线程直接取得对应队列的提交锁，完成 ID 分配、DMA 准备、descriptor 发布、
  completion ownership 注册和 notify；
- completion ownership 必须在设备可观察到 avail descriptor 之前注册；
- descriptor 不足时必须释放提交锁并睡眠，IRQ 回收 descriptor 后唤醒，不允许持锁等待；
- IRQ completion、BIO 完成语义、错误传播、多队列隔离和设备级 FLUSH 语义保持不变；
- 不允许同时让 direct 和 forwarded 路径修改同一请求或重复通知设备。

### 正确性不变量

1. 一个 BIO 只进入一个 virtqueue，并且只完成一次；
2. used descriptor 对应的 completion owner 在 notify 前已经可见；
3. descriptor、请求 ID 和 DMA slot 在 completion 前不得复用；
4. 不得在持有 virtqueue 提交锁时睡眠；
5. 不同 virtqueue 不共享提交锁或 completion map 锁；
6. `Flush` 仍遵守设备级语义，不能因为 direct 模式变为普通队列局部操作；
7. 分区设备必须原样传递队列亲和性；
8. 实验开关关闭时，现有 forwarded 行为不变。

### A/B 矩阵

同一内核版本分别构建 `forwarded` 和 `direct` 两种配置，执行：

- 4 KiB 同步随机写：jobs 1、2、4、8、16；
- 每点 5 轮；
- 记录吞吐、平均延迟、P50、P95、P99；
- 记录六阶段时间戳、每 virtqueue 请求数、最大 staging depth、descriptor wait、IRQ
  completion 和宿主 NVMe hctx 分布；
- 追加 4 KiB 同步随机读，以确认优化不是仅对写路径有效；
- 运行现有 virtio-blk、BIO、分区队列亲和性和多队列相关 ktest。

### 验收标准

直接提交路径只有同时满足以下条件才可作为后续正式重构方向：

- 1 job 平均延迟相对 forwarded 降低至少 30%，或与 Linux 对照的差距缩小至少一半；
- jobs 1--16 的每一点吞吐均不得出现超过 5% 的统计显著回退；
- P99 不得因提交锁竞争增加超过 10%；
- 2 jobs 使用至少两个 virtqueue，4 jobs 及以上使用全部四个 virtqueue；
- 宿主 hctx 分布继续满足多队列报告中的机制验收条件；
- 所有请求计数守恒，测试结束后 staging、in-flight 和 descriptor wait 状态全部排空；
- 功能回归通过，不出现重复完成、请求遗失、descriptor/ID 提前复用或死锁。

若平均延迟改善不足 10%，应删除实验路径，避免长期保留两套提交实现；转而优化阶段一测得
占比最高的 per-request 记账或完成路径。

## 实施顺序

- [ ] 固定并复现 2026-11-04 裸设备基线，确认设备、QEMU、CPU 和 hctx 映射一致。
- [ ] 增加低扰动六阶段采样，并验证请求关联编号和样本计数守恒。
- [ ] 对比有无插桩构建，确认测量扰动是否在 5% 内。
- [ ] 完成 jobs 1/4/16 各 5 轮的阶段分解并更新实验报告。
- [ ] 按阶段一判定规则决定是否实施 direct 路径，不得跳过该决策点。
- [ ] 若进入阶段二，先增加 direct/forwarded 行为和错误路径测试。
- [ ] 实现最小 direct A/B 路径，保持原路径为默认值。
- [ ] 完成功能回归、裸设备性能矩阵和宿主 hctx 验证。
- [ ] 根据验收结果决定正式重构或删除实验路径。

建议每完成一个独立步骤单独提交，避免将插桩、机制改动和性能报告混在同一提交中。

## 非目标

- 不在本项中修改 rTIO worker、rNVPCe 迁移或 EXT4 writeback；
- 不把裸设备、不含最终 FLUSH 的带宽解释为文件系统持久化带宽；
- 不因为 direct 实验有效就立即删除软件暂存队列；普通块设备、请求合并和异步高队深
  工作负载仍需独立评估；
- 不在缺少阶段数据时先优化 `BTreeMap`、内存屏障或调度器；
- 不以单轮结果作为性能验收结论。

## 交付物

- 六阶段插桩及其单元测试；
- 原始 CSV/JSON 数据和中文实验报告；
- forwarded/direct A/B 配置及正确性测试（仅在阶段一满足条件时）；
- 更新后的裸 virtio-blk 对比图；
- 对“固定额外延迟来自哪里”的定量结论，明确区分测量证据和推断。
