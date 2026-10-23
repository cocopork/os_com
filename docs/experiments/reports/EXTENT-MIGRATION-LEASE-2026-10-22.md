# Extent migration lease 实现与验证（2026-10-22）

## 实现

- rTIO candidate 按 `(inode, leaf_slot, entry_index)` 去重；claim 在短 completion publication
  临界区内取得 leaf mutation guard，只读取一次 leaf，并冻结、验证和 pin seed entry 表示的
  一个完整 NV extent。
- data BIO 与 SSD flush 期间由共享 leaf context 保留 guard 和全部 range freezes。completion
  不再重新取得 inode publication，也不再逐页调用 `migration_extent_length` 或
  `validate_migration_and_reverse`。
- 连续 SSD targets 的每个 NV extent 使用 claim 快照完成一次 8B target store，随后批量发布
  credit phase 并回收 NV descriptors/pages。
- 前台 `write_rnvpce_at` 在 inode/publication 锁外预检并预留 NVPages；锁内发现 reservation
  不足或写范围被冻结时，释放 mutation、inode lock 和 reservation 后等待并从头重试。
- `InFlight` 再次 claim 现在明确返回可重试的 `EBUSY`，不再误报 `EINVAL`。
- leaf guard 跨 I/O 保留期间安装 inode 范围 migration gate，避免前台持有 inode lock 等待
  leaf、completion 持有 leaf 等待 inode 的锁序死锁。该 gate 是当前实现的并发限制。

## 真实设备性能

配置：`/dev/nvme1n1p1`，rNVPCe nojournal，128 MiB NVPool（64 MiB metadata reserve、
64 MiB data limit、32 MiB low watermark），64 vCPU/8 GiB guest，virtio-blk 单队列深度
1024，buffered psync，iodepth=1，每项写入 2 GiB。最终非诊断镜像 SHA-256 为
`f6e5c1877a27c725912a64d4164911bbec206e22be7b0c8aac6c3093e248e0db`。

| workload | fio MiB/s | 完整 drain MiB/s | 前一版 fio MiB/s | fio 变化 |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB randwrite, 1 job | 9.19 | 9.13 | 9.57 | -3.9% |
| 4 KiB randwrite, 16 jobs | 33.28 | 32.45 | 34.91 | -4.7% |
| 128 KiB write, 1 job | 31.53 | 28.88 | 34.90 | -9.7% |
| 128 KiB write, 16 jobs | 183.33 | 122.86 | 57.11 | +221.0% |

四项 fio 都以 `error=0` 结束，正常卸载后 `e2fsck -fn` 均为 clean。原始结果在
`/home/zn/asterinas-build-env/extent-migration-lease-20261022/final-v5/`。

结论是优化对多线程大 extent 明显有效：128 KiB/16 jobs 的 fio 带宽为原来的 3.21 倍，
完整 drain 带宽为原来的 2.50 倍。4 KiB 随机写和两个单线程点没有改善，回退约 4%--10%。

## 延迟分解

诊断镜像 SHA-256 为
`05b08b875f1d39b12d13807bca97ee5868dff62a61d2b72001e2690e198a75fb`。128 KiB/16 jobs
得到 182.84 MiB/s；相对前一版同类诊断的 55.96 MiB/s，主要累计阶段变化如下：

| 阶段 | 前一版 | 当前 | 变化 |
| --- | ---: | ---: | ---: |
| foreground total | 543.24 s | 170.02 s | -68.7% |
| capacity wait | 468.66 s | 66.13 s | -85.9% |
| flush total | 26.88 s | 9.69 s | -64.0% |
| candidate select | 12.30 s | 5.76 s | -53.2% |
| completion total | 37.38 s | 3.11 s | -91.7% |
| extent publish | 37.13 s | 1.53 s | -95.9% |
| publication wait | 27.64 s | 2.62 s | -90.5% |
| publication hold | 267.07 s | 49.29 s | -81.5% |

`leaf_detail_*` 和 `leaf_update_*` 调用均为 0，说明 completion 旧的逐页 leaf 读取和验证已从
正常路径消除。当前 128 KiB/16 jobs 的剩余大项是前台累计时间 170.02 s 和容量等待
66.13 s；后台 candidate、completion 和设备 flush 已不再是主瓶颈。

4 KiB randwrite/16 jobs 为 31.07 MiB/s。随机覆盖形成大量单页 NV extents，因而仍有
524,285 次 reclaim（25.14 s）、22.49 s candidate select、61.90 s completion，以及
151.86 s completion queue wait。每个 extent 只有一页，extent lease 无法摊薄这些操作；同时
inode 范围 gate 会串行化同一文件上互不重叠的迁移和前台写。这是下一步应解决的主要问题。

原始诊断结果在
`/home/zn/asterinas-build-env/extent-migration-lease-20261022/diagnostic/`。诊断常量在实验后已
恢复为关闭。

## 验证结果

- `cargo fmt --all -- --check`：通过。
- production `cargo osdk check --package aster-kernel`：通过。
- ktest 配置 `cargo osdk check --ktests --package aster-kernel`：通过。
- `rnvpce_extent_migration_claim_owns_the_complete_entry`：通过。
- `rnvpce_extent_migration_claim_is_bounded_to_the_seed_entry`：通过；同 leaf 的其他 NV entry
  不占用本次迁移预算。
- `rnvpce_candidate_identity_distinguishes_entries_in_one_leaf`：通过。
- `rnvpce_extent_migration_claim_rolls_back_partial_page_claim`：通过；中间页忙时前两页恢复
  `NvDirty`，range freeze 被解除。
- `rnvpce_claimed_extent_publishes_one_ssd_target_and_reclaims_all_pages`：通过；四页连续 extent
  一次切换到 SSD base 900，随后四页全部回收。
- `ext4_research_routes`：前六个 rNVPCe 场景通过；在既有
  `rnvpce_fragmented_batch_and_partial_rollback` 场景停止，精确错误为
  `EAGAIN: claimed extent SSD targets are fragmented`。

## 限制

碎片化失败源于 015 的既有表示约束：一个 NV range 只能用一个连续 SSD base 替换。当前代码
保留 NVM 权威副本并拒绝回收，因此没有悬空引用或错误数据，但还不能在只剩非连续 SSD blocks
时完成迁移。需要先实现有崩溃一致性证明的 NV extent split；本轮没有使用整 leaf 镜像重写，
也没有把失败的集成套件描述为通过。已运行上述真实设备 fio；尚未运行完整 328 项 kernel
ktest。
