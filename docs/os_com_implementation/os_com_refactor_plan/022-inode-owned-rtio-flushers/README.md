# 022：inode 归属的 rTIO flusher 与分布式完成处理

日期：2026-11-03

状态：设计已确认，等待实现。

## 1. 背景与目标

021 的真实 NVMe 实验确认 virtio-blk 四队列已经映射到四个独立 IOThread 和四个
NVMe hctx，但 128 KiB 顺序写在四 worker 下仍回退 42.3%-46.9%。分解数据表明：

- 同样迁移 524,288 页，单 worker 形成 512 个 1024 页迁移批次；四 worker
  形成 1,437-2,489 个批次，平均每批只有 211-365 页；
- 所有 BIO completion 汇聚到一个 completion 线程；该线程同步执行设备 FLUSH，
  完成后才发布 entry completion、回收工作和容量唤醒；
- 四 worker 的最大设备 FLUSH 延迟约 508 ms，最大容量等待约 521 ms；
- 四条 virtqueue 没有 descriptor 等待，宿主 hctx 分布均衡，因此普通数据提交
  驱动不是主要串行点。

本设计完成两项重构：

1. 删除集中式 completion 线程，将批次完成后处理交还给提交该批次的 flusher；
2. 将一个 inode 的 dirty NV extent entry 固定交给一个 flusher，避免多个 flusher
   扫描和争用同一 inode entry mapping。

单个热文件只使用一个 flusher 和一条数据 virtqueue。这是明确接受的取舍：优先
保持完整批次和低协调开销，并行度来自多个 inode，而不是拆分一个 inode。

## 2. 非目标

- 不改变 rNVPCe 持久化格式、NVM 权威规则或掉电恢复协议；
- 不改变 rNVPC WAL 的单 worker 路径；
- 不实现同 inode 按 leaf、逻辑区间或 extent 的二级并行；
- 不实现任意 work stealing；
- 不将 inode 归属写入 NVM；
- 不把 extent 更新、回收或设备 FLUSH 放入 IRQ/BIO callback；
- 不用 inode 的 VFS/extent-tree 读写锁保护调度状态。

## 3. 总体流水线

```text
前台范围写
  发布 dirty NV extent entry
  短暂访问 inode 调度状态
  将 inode 放入所属 flusher 的 ready-inode 队列（去重）
                         |
                         v
flusher i
  从本地 ready-inode 队列取 inode
  只扫描该 inode 的 entry map
  批量 claim dirty entry
  通过 Queue(i) 异步提交数据 BIO
  BIO callback 仅将完成批次放入 flusher i 的本地 ready-batch 队列
                         |
                         v
任一 flusher 尝试成为 durability leader
  收集各 worker 当前已完成批次
  对成功批次执行一次全局设备 FLUSH
  仅发布 epoch durable/failed 结果并把批次送回 owner
                         |
                         v
原 flusher i
  批量执行自己的 entry completion
  发布/执行自己的 reclaim
  归还容量并唤醒等待者
```

设备 FLUSH 仍是设备级全局持久化屏障，因此保留一个很薄的 durability leader，
但不再保留专门 completion 线程。durability leader 只处理批次状态和一次 FLUSH，
不遍历或修改其他 worker 所属 inode 的 entry，不执行 reclaim。

## 4. inode 与 flusher 归属

### 4.1 归属键与生命周期

归属键为 `(fs_id, inode)`。归属状态只存在于 DRAM，挂载恢复后根据重建出的 dirty
entry 重新分配。

一个 inode 第一次出现 dirty entry 时，从当前 worker 负载快照中选择负载最低者：

```text
worker_load = queued_dirty_pages
            + in_flight_pages
            + completion_pending_pages
```

负载值使用每 worker 原子计数；选择时允许近似快照，不获取全局 worker 锁。负载
相同时通过轮转游标打散。归属一旦建立，在该 inode 的活动 entry 数归零前保持不变。

活动 entry 包括以下状态：

- dirty；
- claimed/migrating；
- data-I/O in-flight；
- completion-pending/durable-pending；
- reclaim-pending。

最后一个 entry 完成回收后，调度器在该 inode 的短临界区内确认活动计数为零，
解除归属。后续再次写脏时重新按负载选择 worker。不得仅根据某个无锁计数的瞬时值
解除归属；新 entry 发布与最后 entry 退出必须在同一个 per-inode 调度状态下排序。

### 4.2 细粒度同步

每个 inode 的重定向内存对象增加独立的迁移调度状态。该状态使用短持有的
per-inode spinlock 或等价原子状态机，只保护：

- owner worker ID 与归属 generation；
- active/queued/in-flight/completion/reclaim 计数转换；
- ready-inode 去重位；
- 最后一个 entry 退出与新 dirty entry 发布之间的排序。

以下操作禁止发生：

- 为读取或修改归属而获取全局 pool state 锁；
- 获取整个文件 inode 的 VFS/extent-tree 读写锁；
- 在持有 inode 调度锁时扫描 extent tree、提交 BIO、等待 I/O 或执行 FLUSH；
- 同时持有两个 inode 的调度锁；
- 在持有 worker ready 队列锁时获取 inode entry-map 锁。

获取 inode map 的分区锁只用于查找/创建 `Arc<InodeRedirectMap>`，得到 `Arc` 后立即
释放。候选扫描只获取目标 inode 自己的 entry-map 锁。这样不同 inode 即使落在同一
索引分区，也不会在一次完整候选扫描期间互相阻塞。

### 4.3 ready-inode 队列

每个 flusher 拥有一个本地 MPSC ready-inode 队列。前台发布 dirty entry 时：

1. 在 per-inode 调度状态内建立或读取 owner；
2. 增加该 worker 的 `queued_dirty_pages`；
3. 通过 `queued` 位保证同一 inode 最多入队一次；
4. 释放 inode 调度锁后，将 inode handle 推入 owner 队列并唤醒该 worker。

flusher 取出 inode 后先清除 `queued` 位，再批量 claim 至多 1024 页。如果仍有 dirty
entry，则重新入队一次。它不扫描未归属于自己的 inode，也不遍历所有 redirect
partition。

## 5. worker 本地完成路径

每个 `MigrationBatch` 记录不可变 `owner_worker_id`。每个 worker 拥有：

- ready-inode 队列；
- data-complete 队列；
- durable-result 队列；
- queued、in-flight、completion-pending 页计数；
- 一个只唤醒该 worker 的 wait queue。

BIO callback 仍保持 IRQ-safe：记录 BIO 状态、递减 batch 的剩余请求数；最后一个
BIO 完成时只把 batch 推入 owner 的 data-complete 队列并唤醒 owner。callback 不
获取 inode 调度锁，不执行 FLUSH，不修改 redirect entry，也不释放 NVPage。

flusher 每轮按以下顺序工作：

1. 处理 durable-result 队列，完成 entry 状态转换与 reclaim；
2. 尝试推进 durability epoch；
3. 从 ready-inode 队列 claim 并提交新迁移；
4. 无工作时睡眠，等待本地队列、容量压力、sync 或失败通知。

处理完成结果时只访问该 worker 所属 inode。失败的数据 BIO 恢复对应 entry 为 dirty，
保留 NVM 权威并触发现有 mount-wide migration failure 策略；本方案不增加无界自动
重试。成功且 epoch durable 的 entry 转为 `IoComplete`，随后遵守现有 soft-update
顺序清除 redirect 和释放 NVPage。容量归还后唤醒前台等待者。

## 6. 无专用线程的 durability epoch

挂载级 durability arbiter 只包含：

- 一个 leader 原子标志或短互斥；
- 各 worker data-complete 队列的快照入口；
- epoch generation、状态和等待队列；
- mount-wide failure latch。

被 BIO completion 唤醒的 worker 使用非阻塞方式竞争 leader。只有 leader：

1. 从各 worker 队列取出当前已完全完成的 batch，形成封闭 epoch；
2. 分离数据 I/O 成功和失败的 batch；
3. 若存在成功 batch，同步执行一次 `block_device.sync()`；
4. 将 epoch 结果发布到各 batch owner 的 durable-result 队列；
5. 释放 leader 并唤醒相应 worker。

未获得 leader 的 worker不得等待 leader 锁；它继续处理自己的 durable result 或提交
新 BIO。leader 在设备 FLUSH 期间不持有 inode 调度锁、entry-map 锁、worker 队列锁
或 migration gate 之外的元数据锁。

arbiter 维护原子的 ready-batch 总数和推进 generation。leader 封闭 epoch 后到释放
leader 期间新到达的 batch 不并入当前 epoch；释放 leader 时若 ready-batch 仍非零，
必须显式唤醒一个拥有待处理 batch 的 worker接棒。worker 准备睡眠前必须在注册等待后
重查本地队列、ready-batch 总数和 generation，并再次尝试推进，避免 batch 落在快照
之后而无人继续执行 FLUSH。

FLUSH 成功前 redirect 仍以 NVM 为权威。FLUSH 失败时所有成功数据 batch 也按失败
结果返回 owner，保留 NVM 权威并触发现有 mount-wide migration failure 策略。

## 7. sync、卸载和失败处理

sync/unmount 沿用 migration gate：

1. 获取 exclusive permit，阻止新 worker 提交轮次；
2. sync 上下文按 owner ID 从各 ready-inode 队列取出 exclusive 之前已经排队的 dirty
   entry，并通过该 owner 对应的 `Queue(i)` 提交；它只代行 owner 的 drain，不改变归属；
3. 反复执行第 2 步，直至 queued dirty 归零，再等待所有已提交数据 BIO 完成；
4. 由 sync 上下文反复推进 durability arbiter，直到所有 data-complete 队列为空；
5. 唤醒并等待所有 owner worker 排空 durable-result/reclaim；
6. 确认每 worker 的 queued/in-flight/completion/reclaim 计数归零；
7. 再导出 native EXT4 extent 和分配元数据。

exclusive drain 使用与正常 worker 相同的 worker-specific claim 接口，且不得直接扫描
全局 redirect index。这样不会遗留 exclusive 前已排队的 dirty entry，也不会破坏
inode owner 和 virtqueue 亲和性。

worker 退出或 mount-wide 失败时不得遗失本地队列中的 batch。失败清理按 owner 路由
归还 lease；任何 worker 都不能直接释放另一个 worker 所属 inode 的 entry。

恢复时，持久化 redirect 仍是权威。DRAM owner、队列、epoch 和负载计数全部丢弃，
重建 redirect entry 后重新执行初始归属，不增加恢复格式或恢复阶段。

## 8. 可观测性

保留 021 的 per-queue 数据提交计数，并新增每 worker：

- owned inode 数；
- ready inode 出队次数；
- claim 页数和 batch 页数直方图；
- data-complete/durable-result 队列深度；
- durability leader 获取/失败次数；
- completion、reclaim 和容量唤醒耗时；
- 归属建立、静止解除和重新分配次数。

测试必须证明单 inode 从不被两个 worker claim；诊断不得通过获取 inode 或全局 pool
大锁来生成快照。

## 9. 验收标准

### 正确性

- 同 inode 的 dirty entry 只由一个 worker claim；不同 inode 可由不同 worker 并发；
- BIO callback 只入本地队列，entry completion/reclaim 在 owner worker 上执行；
- 多 worker 共享 epoch 仍只产生一次设备 FLUSH；
- 数据 BIO/FLUSH 失败保留 NVM 权威且不泄漏 lease、容量或 worker 负载计数；
- sync/unmount 可以排空所有本地队列并得到干净 e2fsck；
- 归属检查不获取全局 pool 锁或 inode 读写锁；不同 inode 的候选扫描互不持锁等待。

### 性能验证

先使用 2 GiB、128 MiB NVPool：

- 128 KiB 顺序首次写，1/16 job；
- 4 KiB 随机首次写，1/16 job；
- 1 worker/1 队列作为对照，显式 4 worker/4 队列作为实验组。

必须比较 batch 页数分布、durability epoch、设备 FLUSH、容量等待、worker 本地完成
队列和四个 hctx。四 worker 的 128 KiB 路径不应再把同一 inode 的 1024 页批次拆成
多个 worker 小批次；多文件 16 job 应体现跨 inode 并行。单文件 1 job 不要求使用
多个 worker，但不得相对单 worker 出现显著协调回退。

## 10. 与既有方案的关系

- 继承 017 的软更新回收顺序，但将完成和回收责任路由回 owner flusher；
- 继承 018/019 的 NV extent entry 粒度，不退回 per-page completion；
- 替代 020 的“所有 worker 扫描全局 redirect index”候选策略；
- 保留 021 的 worker 到 virtqueue、IOThread 和 NVMe hctx 映射；
- 021 的单 completion 线程只保留在 rNVPC WAL 兼容路径，rNVPCe 使用本设计。

实现步骤见[实现计划](IMPLEMENTATION_PLAN.md)。
