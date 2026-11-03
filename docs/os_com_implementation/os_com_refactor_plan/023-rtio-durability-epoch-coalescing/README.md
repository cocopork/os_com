# 023：rTIO durability epoch 自适应聚合

日期：2026-11-03

状态：设计已确认，进入实现。

## 1. 问题

022 将 completion 分散到 inode owner 后，四个 worker 会在单个迁移 batch 完成时立即竞争
durability leader。4 KiB 随机写、16 job 实验因此产生 70,790 次 epoch/设备 FLUSH，
而单 worker 只有 520 次。问题不是数据 BIO 并行度不足，而是 epoch 在繁忙期封闭过早。

## 2. 目标与非目标

目标：

- 繁忙期让多个 owner 的完成 batch 共用一次设备 FLUSH；
- 没有尚未完成的数据 BIO 时立即推进尾部 batch；
- sync、unmount 和显式 drain 不等待聚合阈值；
- 保持 022 的 NVM 权威、失败隔离和 inode owner 语义。

非目标：

- 不使用固定毫秒级定时器；
- 不改变 BIO 提交批次、inode owner 或 virtqueue 映射；
- 不允许 callback 执行 FLUSH、元数据发布或回收。

## 3. 计数与推进条件

`DurabilityArbiter` 增加两个挂载级原子页计数：

- `pending_data_pages`：已经提交、但所属 batch 尚未完成全部数据 BIO 的页数；
- `ready_pages`：已经进入 owner data-complete 队列、尚未被 leader 取走的页数。

batch 开始提交前一次性增加 `pending_data_pages`。最后一个数据 BIO callback 发布 batch
时，将整个 batch 的页数从 pending 转入 ready。leader 快照各 owner 队列后，从 ready
扣除快照页数。计数只描述 DRAM 调度状态，不参与掉电恢复。

后台 worker 在以下任一条件满足时才竞争 leader：

1. `ready_pages >= WRITEBACK_BATCH`，当前为 1024 页；
2. `pending_data_pages == 0`，即没有更多已提交数据可能加入当前 epoch。

第二个条件负责低负载和尾部进展，因此不需要定时器。显式 drain 使用 force 模式，只要
存在 ready batch 就立即推进。

## 4. 并发与一致性

提交端必须先登记 pending，再让 callback 可见。最后一个 callback 使用原子转移后才把
batch 放入 owner 队列并唤醒 worker。leader 仍先获得非阻塞 leader 标志，再快照队列；
FLUSH 期间不持有 worker 队列、inode 或 extent 锁。

在 leader 快照之后完成的 batch 保留在下一 epoch，并通过已有 generation 和显式唤醒
交接。数据 BIO 或 FLUSH 失败不清除 NVM 权威状态。计数不平衡视为实现错误；sync 完成
时 pending、ready、owner 队列和 in-flight 必须同时为零。

## 5. 验证

- ktest：未达到阈值且仍有 pending 数据时不执行 FLUSH；
- ktest：最后一个 pending batch 完成后立即封闭尾部 epoch；
- ktest：达到 1024 页时即使仍有 pending 数据也推进；
- ktest：force drain 忽略阈值；
- 回归：跨队列失败、sync quiescence、完整 EXT4 research routes；
- 真实 NVMe：复测 4 KiB 随机写和 128 KiB 顺序写的 1/16 job、1/4 worker 矩阵，
  比较 epoch/FLUSH 数、含 sync 带宽、容量等待和尾延迟。

