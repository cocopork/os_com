# 021 rTIO 与 VirtIO-Block 多队列

日期：2026-11-02

状态：实现完成；真实设备正确性与多 hctx 机制验收通过，性能验收失败（2026-11-02）。

8 个 2 GiB 测试点均通过 fio、sync、unmount 和只读 `e2fsck -fn`。
四 worker 均使用对应 virtqueue，QEMU 10.2.1 将各队列映射到独立 IOThread，
宿主机 `nvme_queue_rq` 跟踪确认 hctx 0–3 均提交大量请求，最大占比 25.75%–31.07%。
最终队列全部排空，durability epoch 与设备 FLUSH 数相等。

随机写含 sync 吞吐量在 1/16 job 分别提高 30.2%/36.6%，但 128 KiB 顺序写
分别回退 46.9%/42.3%，16 job 提交及容量等待也未下降。因此四 worker 不能
作为默认配置接受。测量版本 `936722960` 的 queue-aware 默认尚未在本实验中修改；
需在独立后续变更中恢复一个 worker 默认，多队列仅保留为实验路径。
详见[实验报告](../../../experiments/reports/RTIO-VIRTIO-MULTIQUEUE-2026-11-02.md)。

## 目标

把已经实现的 rTIO 多迁移线程从“多个生产者竞争一个 virtqueue”改为端到端并行的数据路径：

```text
rTIO worker i
  -> 带 Queue(i) 亲和性的 BIO
  -> 星绽统一块设备接口的第 i 个提交队列
  -> virtio-blk virtqueue i
  -> QEMU IOThread i
  -> 宿主机 NVMe blk-mq hardware context i
```

这里的最后一层必须通过宿主机跟踪数据验证。配置了多个 virtqueue、多个 IOThread 或 CPU affinity，都不能单独作为“已经利用多个 NVMe 硬件队列”的证据。

## 背景

020 已经允许多个 rTIO worker 并发扫描、claim 和提交不同的 NV extent entry，但在 021 实现前 virtio-blk 只有：

- 一个 `BioRequestSingleQueue`；
- 一个转发内核线程；
- 一个 `VirtQueue`；
- 一组请求 ID 和 completion map。

128 KiB 顺序写、16 个 fio job 的诊断中，4 个 rTIO worker 相比 1 个 worker 将累计 BIO 提交时间从 0.89 秒增加到 4.74 秒，容量等待从 1.26 秒增加到 18.16 秒。这说明只增加 rTIO 线程会把竞争推到单队列设备路径。

## 范围

### 1. 统一块设备接口

BIO 增加提交队列亲和性：

```rust
pub enum BioQueueAffinity {
    Any,
    Queue(u16),
}
```

- 普通调用者默认使用 `Any`，保持源码和行为兼容。
- `Queue(id)` 是严格指定，越界返回 `BioEnqueueError::InvalidQueue`，不能静默换队列。
- `BlockDeviceMeta` 增加 `nr_submission_queues`，始终至少为 1。
- 分区层只调整 sector offset，必须原样保留队列亲和性。
- rTIO 使用严格队列；普通 EXT4 I/O 的 `Any` 由驱动按照提交 CPU 选择。

### 2. 星绽 virtio-blk 多队列

- 协商 `VIRTIO_BLK_F_MQ` 并读取现代配置中的 `num_queues`。
- 第一版最多启用 16 个队列。
- 每个队列独立拥有软件 staging queue、virtqueue、请求/响应 DMA 区、请求 ID、completion map、descriptor wait queue、IRQ callback 和转发线程。
- 请求必须先进入 completion map，再通知设备，消除“即时 IRQ 找不到 token owner”的竞态。
- descriptor 不足时线程睡眠，由对应队列 IRQ 唤醒，不允许持锁忙等。
- 未提供 MQ 或能力预检失败时使用一个队列；多队列配置开始后的失败直接中止设备初始化，不做 reset 后二次初始化。

### 3. rTIO 队列绑定

- rNVPCe worker `i` 在整个挂载生命周期内固定使用 `Queue(i)`。
- 测量候选版本的默认 worker 数为 `min(4, max(1, online_cpu / 8), nr_submission_queues)`；性能验收失败，发布前须恢复一个 worker 默认。
- `rtio_workers=N` 大于设备提交队列数量时挂载返回 `EINVAL`。
- rNVPC WAL 仍限制为一个 worker。
- 所有队列的数据 BIO completion 仍汇聚到一个 durability coordinator。
- 一个 durability epoch 等待其数据 BIO 全部完成后，只通过 queue 0 发出一次设备级 FLUSH；FLUSH 成功后才能发布 `IoComplete` 和回收工作。

### 4. 宿主机 IOThread 与 NVMe hardware context

多队列实验使用 QEMU 9.0 或更新版本的 `iothread-vq-mapping`：

- virtqueue `i` 独占 QEMU IOThread `i`；
- 通过 QMP `query-iothreads` 获得宿主机线程 ID；
- 从 `/sys/block/nvme1n1/mq/*/cpu_list` 选择属于不同 NVMe hardware context 的 CPU；
- 将不同 IOThread 固定到这些 CPU；
- 使用 raw、direct 的真实 NVMe 分区，不经过宿主机文件系统缓存。

本机现有 QEMU 8.2.2 没有 `iothread-vq-mapping` 属性，只能用作单 IOThread 对照，不能用于多硬件队列验收。

## 一致性约束

- 不依赖跨 virtqueue 顺序；VirtIO 允许不同队列乱序完成。
- FLUSH 前，持久化 redirect 仍以 NVM 为权威。
- 数据 BIO 或 FLUSH 失败时不得清除 NVM 权威状态。
- 多队列不改变持久化格式、恢复扫描和 soft-update 协议。
- sync/unmount 先关闭 migration gate，等待全部 worker 离开提交阶段，再排空所有队列的 completion、回收和元数据导出。

## 验证

### 功能验证

- `Any`、严格队列越界、分区透传和单队列兼容测试。
- MQ 协商、配置读取、N 个队列初始化和单队列回退测试。
- 不同队列并发提交、即时 completion、descriptor 耗尽睡眠/唤醒测试。
- rTIO worker 到队列的一一映射、跨队列 durability epoch 单次 FLUSH、混合成功/失败和 sync quiescence 测试。
- 完整 EXT4/rNVPC/rNVPCe 相关回归。

### 真实设备验证

测试对象为已经授权用于清空的 `/dev/nvme1n1p1`；被占用时可使用此前指定的 `/dev/nvme1n1p2`，但必须先确认未挂载且容量匹配。每个点重新格式化，测试后执行只读 `e2fsck`。

对照配置：

- 1 rTIO worker、1 virtqueue、1 IOThread；
- 4 rTIO workers、4 virtqueues、4 IOThreads，并绑定到 4 个不同 NVMe hardware context。

负载沿用开发验证规模：

- 4 KiB 随机首次写，fio jobs 1 和 16；
- 128 KiB 顺序首次写，fio jobs 1 和 16；
- 每点总量 2 GiB，NVPool 128 MiB。

必须同时记录：

- 来宾机每个 virtqueue 的 BIO、page、最大深度、descriptor wait 和 IRQ completion；
- rTIO worker 到 virtqueue 的映射；
- QEMU IOThread 的 TID 和 CPU affinity；
- 宿主机 CPU 到 NVMe hctx 的 sysfs 映射；
- NVMe 提交点按 hctx 统计的请求数。

四队列配置至少要观察到 4 个不同 NVMe hctx 发出请求，且单一 hctx 不得占跟踪请求的 70% 以上。否则只能说明来宾机启用了多 virtqueue，不能宣称已利用 NVMe 多硬件队列。

## 文档

- [完整设计](../../../superpowers/specs/2026-11-02-rtio-virtio-multiqueue-design.md)
- [实现计划](IMPLEMENTATION_PLAN.md)
- [020 rTIO 多迁移线程](../020-rtio-multi-worker/README.md)
