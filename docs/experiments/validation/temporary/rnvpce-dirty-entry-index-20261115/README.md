# rNVPCe dirty entry 精确候选索引优化

## 结论

本次优化消除了 rTIO worker 在 inode 的全部 redirect entry 中反复寻找 `Dirty`
entry 的扫描放大。每个 inode 现在额外维护按逻辑页号排序的 DRAM `dirty_entries`
索引；entry 只在进入可迁移状态时登记，worker 直接弹出候选，不再遍历已经迁移、
等待 durability 或等待回收的历史 entry。

结构性目标已经达到：回归测试构造 32 个非候选 entry 和 1 个候选 entry，旧实现
访问 33 个 entry，新实现只访问 1 个。按逻辑页号排序的索引仍保留相邻 BIO 合并
机会。owner 变更、claim 失败、lease 回滚和 extent 扩展后，候选均会重新登记，且
同一 entry 不会重复入队。

端到端结果没有证明 32 线程吞吐已经稳定修复。两轮最终 release 测试的 32 线程
结果分别为 292.45 和 141.46 MiB/s，波动仍然很大。因此本次提交只应解释为消除
一个已量化的候选定位瓶颈；剩余瓶颈已经转移到或仍存在于小批次形成、worker
负载不均和 durability 协调，不能把吞吐波动归因于候选索引查找。

## 实现

- `InodeRedirectMap` 新增按 `logical_start` 排序的 `dirty_entries` BTreeMap。
- `DirtyNvExtentEntry::candidate_queued` 用原子状态去重登记。
- `claim_ready_entries_for_worker()` 从精确索引弹出候选，只对候选执行
  `Dirty -> Migrating` CAS。
- claim 失败、lease drop 和扩展中止时重新登记候选。
- inode ready 状态使用 `IDLE/QUEUED/RUNNING/RERUN` 状态机，避免运行期间遗漏新工作
  或重复 ready queue 节点。
- 修正 extension 和首次发布的可见性顺序：先发布页数并完成 dirty-page 记账，再把
  entry 暴露给 worker。该修复消除了并发 32 线程测试中出现的
  `migration dirty page accounting changed`。

## 测试驱动证据

| 测试 | 旧实现 | 新实现 |
| --- | --- | --- |
| `rnvpce_worker_claim_visits_only_ready_entries` | 失败：访问 33 个 entry | 通过：访问 1 个 entry |
| `rnvpce_dirty_index_claims_entries_in_logical_order` | FIFO 原型失败：`[4, 0, 2]` | 通过：`[0, 2, 4]` |
| `rnvpce_stale_worker_yields_after_inode_owner_turnover` | 不适用 | 通过 |

另行验证了按 worker 设置 1024 页激活阈值。该策略虽减少 ready inode dequeue，
却把 32 线程带宽降至 157.25 MiB/s，并放大 worker 不均衡和 durability 空转，故已
完整回退；最终代码继续使用原有全局激活条件。

## 最终性能验证

条件：真实 `/dev/nvme1n1p2`，2 GiB 低碎片预建文件，128 MiB rNVPC 池，4 KiB
随机覆盖写，4 个 rTIO worker，4 个 virtqueue。每点完成 sync、卸载和 e2fsck。

| 轮次 | 4线程 MiB/s | 16线程 MiB/s | 32线程 MiB/s |
| --- | ---: | ---: | ---: |
| final-v3 | 194.23 | 320.85 | 292.45 |
| final-v4 | 450.70 | 342.36 | 141.46 |

所有六个点均通过数据量检查、sync、卸载和 e2fsck。结果不能取最好值来声称性能
提升：4线程和32线程都存在明显跨轮波动，且32线程第二轮仍回退。相比修改前轻量
插桩的 `4T=522.98/32T=237.09 MiB/s`，现有数据不足以证明统计显著的端到端收益。

临时轻量指标进一步显示，精确索引之后 32 线程仍产生约 38,579 个 completion
批次，平均约 13.6 页/批；4线程约 3,484 批，平均约 150.5 页/批。四个 worker 的
提交页数也不均衡。这解释了为什么消除全表扫描后，吞吐仍受批次碎片化支配。

## 验证边界

- `cargo osdk check --ktests --package aster-kernel` 通过。
- redirect index 相关定向 ktest 通过。
- 完整 kernel ktest 运行到既有场景
  `nv_route_rnvpce_foreground_allocation_is_independent_of_completion` 时挂起。阶段标记
  确认子线程尚未开始执行，未进入本次修改的候选索引或 claim 路径；临时标记已
  删除。该完整套件不能记为通过，需要单独修复旧测试的调度等待方式。

## 原始证据

- 最终性能：
  `/home/zn/asterinas-build-env/rnvpce-dirty-entry-queue-20261115/final-results-v3/`
  和 `final-results-v4/`。
- 结构测试日志：`/tmp/rnvpce-dirty-queue-red.log`、
  `/tmp/rnvpce-dirty-queue-green.log`、`/tmp/rnvpce-dirty-index-order-red.log`、
  `/tmp/rnvpce-dirty-index-order-green2.log`。
- 轻量 worker 指标：
  `/home/zn/asterinas-build-env/rnvpce-dirty-entry-queue-20261115/metrics/`。
- 被否决的 per-worker 阈值实验：
  `/home/zn/asterinas-build-env/rnvpce-dirty-entry-queue-20261115/per-worker-threshold/`。
