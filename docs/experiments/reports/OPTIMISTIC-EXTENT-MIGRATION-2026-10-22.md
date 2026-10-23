# 乐观 extent 迁移实现与验证（2026-10-22）

## 实现结果

- rTIO 按完整连续 NV extent 认领候选。在短 completion-publication 临界区内验证 entry、
  pin 全部 allocation generation，然后在返回 I/O 层前释放 publication 和 leaf guard。
- SSD target 分配、data/metadata BIO、设备 flush 与等待期间不保留 inode publication、leaf
  mutation guard 或 inode 范围 gate。
- completion 按 `(fs, inode, logical_start, length, NV base, generation vector)` 重新解析当前
  FAST/FAIR 树；候选 locator 仅为 hint。一个完整 extent 只验证一个 entry、执行一次 8B
  target store，并批量释放 reservation credit 和回收 NVPages。
- 前台在已有 `current_page_mapping()` 查询中识别 `InFlight` generation；此时尚未预留容量或
  消费用户数据，释放 inode lock 和 mutation state 后在锁外等待 lease epoch 改变。提交阶段
  的普通 `EBUSY` 不会被泛化为 migration retry，避免重复消费已经前移的 `VmReader`。
- stale identity 只有在全部当前映射仍指向原 pinned generations 时才整体重新排队；若权威映射
  已改变则报告 `EUCLEAN`，避免旧 completion 发布或回收新权威副本。
- 连续 NV extent 遇到碎片化 SSD targets 时返回 `EAGAIN`，不改变 NVM authority，也不进行逐页
  publication fallback。持久化 extent split 仍是独立工作。

## 正确性验证

定向 ktests 覆盖以下行为：

- claim 返回前已释放 metadata coordination，同 inode 非重叠前台 publication 可以继续；
- claim 中任一 member generation 失配时，已认领成员全部回滚到 `NvDirty`；
- completion 不依赖旧 locator，而是按 logical identity 重新路由当前 entry；
- generation mismatch、incomplete extent 和碎片化 SSD targets 均不发生部分 publication/reclaim；
- target store 前后故障点分别保留完整 NV 或完整 SSD authority；
- target publication 不改变 leaf generation，重试保持幂等，reservation credit 和 NVPage 批量回收。

验证命令及结果：

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `git diff --check` | 通过 |
| Docker 内 `make kernel` | 通过 |
| `cargo osdk test rnvpce_` | 通过 |
| 单独 `ext4_research_routes` | 通过 |
| 不带 filter 的 kernel `cargo osdk test` | 退出码 0 |

一次 workspace `make ktest` 重跑因 OSDK 同时启动多个测试 crate 时重复占用 QEMU host-forward
端口而退出，属于测试编排问题；因此没有把该次 workspace 结果计为全套通过。随后直接运行不带
filter 的 kernel ktest 命令退出码为 0。此前被碎片化 SSD targets 卡住的
`rnvpce_fragmented_batch_and_partial_rollback` 已按本设计的 non-goal 改为验证整段 `EAGAIN`
及 NVM authority 保留。

## 真实 NVMe 性能

配置：`/dev/nvme1n1p1`，rNVPCe nojournal，buffered psync，2 GiB/项，NVPool 128 MiB，
metadata reserve 64 MiB，data limit 64 MiB，low watermark 32 MiB，64 vCPU/8 GiB guest，
virtio-blk queue depth 1024，并沿用历史 virtio CPU isolation。镜像 SHA-256：
`e8e52895be803e55978bf929474695a0e4698cec626ac7f13f13e4522a91454b`。

| workload | fio MiB/s | 含最终 drain MiB/s | lease 版 fio MiB/s | fio 变化 | lease 版 drain MiB/s | drain 变化 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite, 1 job | 10.26 | 10.16 | 9.19 | +11.6% | 9.13 | +11.3% |
| 4 KiB randwrite, 16 jobs | 37.38 | 36.04 | 33.28 | +12.3% | 32.45 | +11.1% |
| 128 KiB write, 1 job | 44.32 | 40.35 | 31.53 | +40.6% | 28.88 | +39.7% |
| 128 KiB write, 16 jobs | 165.48 | 116.10 | 183.33 | -9.7% | 122.86 | -5.5% |

四项 fio 均以 `error=0` 结束，正常卸载后的 `e2fsck -fn` 均 clean。结果说明移除跨 I/O gate
改善了随机写和单线程大块写；128 KiB/16 jobs 的提交带宽仍有回退，但最终 drain 带宽与 lease
版接近。以上均为单次对比，用于判断方向，不作为统计显著性结论。

原始日志、manifest、status、`e2fsck` 输出和 CSV 位于
`/home/zn/asterinas-build-env/optimistic-extent-migration-20261022/final-v3/`。第一次运行使用了被
ktest 重建且不含 fio 的 initramfs，日志位于相邻 `final/` 目录，仅作为环境失败记录，不纳入结果。

## 剩余限制

- 碎片化 SSD targets 需要可恢复的 NV extent split 协议；当前实现保留 NVM authority 并重试。
- 尚缺真实并发的 `write_rnvpce_at`/completion 测试，以及 completion 等待期间在线触发 FAIR
  split 的端到端测试；当前证据是锁边界审查、状态传播 ktest 和真实 fio 并发运行。
- 128 KiB/16 jobs 的 fio 提交阶段仍有约 9.7%、含 drain 约 5.5% 的单次回退，需要使用修正后的
  `NV_EXTENT_PUBLISH` 探针继续区分 completion publication、reclaim 与前台 capacity wait。
- workspace 多 crate ktest 需要为每个 QEMU 实例分配独立 host-forward 端口，才能取得可信的
  workspace 总数结果。
