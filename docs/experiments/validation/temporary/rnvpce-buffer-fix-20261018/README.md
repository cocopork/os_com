# rNVPCe容量回压、迁出与卸载修复（2026-10-18）

已修复此前约64MiB即向fio返回EAGAIN，以及新文件顺序写后的卸载EINVAL。最终代码下，真实NVMe的4KiB随机写/128KiB顺序写、1/16线程共4个rNVPCe点均完整写入2GiB，正常同步、卸载且后置e2fsck通过。带宽仍低于EXT4 buffered write，尚未达到接近基线的预期。

## 修改与一致性约束

1. **容量预留和事件等待。** EXT4在inode锁和NV publication guard下读取一致的权威映射，计算并预留本次所需新NV页；数据容量不足时，释放两层锁，等待容量事件，再重新规划。预留发生在消费用户reader及扣除SSD admission额度之前，避免容量重试重复消费数据或扣额度。单次请求超过整个数据池的容量会明确返回ENOMEM，不会无限等待。`rtio=off`仍允许在耗尽时返回EAGAIN，因为没有后台迁出服务。
2. **完整的容量唤醒。** 使用独立容量WaitQueue，释放/中止暂存页会在释放池锁后通知；迁出完成按批通知，迁出失败和shutdown也会唤醒并返回错误。正常容量等待不创建前台定时器。针对私有预留回滚后的无迁出场景，新增回归用例已先复现挂起、修复后通过。
3. **后台持续推进。** 有可提交工作时按MAX_IN_FLIGHT=128页补充流水线，不再每个成功轮次等待10ms；完成回调/处理路径唤醒提交线程。无工作或暂时无法推进的回合仍保留10ms重试间隔。
4. **BIO上限修正。** 设备公布max_nr_segments_per_bio=62，迁出允许提交62段，但块层队列原先用`>=`拒绝恰好62段，产生“Bio is too big”并使export/sync/umount失败。现改为`>`，上限内接受、超限拒绝，并有边界回归测试。
5. **合并分配持久化屏障。** 同一迁出批次的SSD目标块先保持私有，统一持久化分配后，才逐一发布NV描述符目标，再提交数据BIO。分配/屏障失败时回滚这一批私有块；若目标发布失败，可能已形成持久引用，则保留该有界批次并停止迁出，交由恢复判定，避免释放可能仍被引用的块。SSD数据持久化、SSD extent发布、NV页回收的原顺序保留。

主要代码：`kernel/src/fs/fs_impls/ext4/inode/mod.rs`、`migration.rs`、`fs.rs`，`kernel/src/fs/rnvpc/frame.rs`、`write_transaction.rs`，以及`kernel/comps/block/src/request_queue.rs`。没有更改持久化结构格式，也没有关闭模拟persist。

## 最终性能复测

与首次测试相同：`/dev/nvme1n1p1`、每点新格式化journal EXT4、psync/direct=0/fsync=0/iodepth=1，每线程独立文件，总量2GiB；随机写预先由native direct写完整初始化，顺序写是新文件分配写。64 vCPU/8GiB、virtio单队列1024，数据上限64MiB。rNVPCe池128MiB、元数据保留64MiB、fast模式启用persist模拟；全局模式关闭，由mount选项切换。每点一次，runtime上限提高到180秒以便发现低吞吐时的不完整写，所有最终点均在上限前完成。

CPU隔离仍按历史配置启用：fio0–31、native flush60、virtio62、journal63；rNVPCe的两个迁出线程使用通用Kernel角色CPU32–59，与fio隔离，没有新增专用单核绑定。

单位MiB/s，下面是fio工作阶段带宽：

| 工作负载 | 线程 | EXT4 buffered | rNVPCe | rNVPCe / EXT4 |
| --- | ---: | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 303.7 | 121.6 | 40.0% |
| 4KiB随机写 | 16 | 354.7 | 126.4 | 35.6% |
| 128KiB顺序写 | 1 | 1423.2 | 125.2 | 8.8% |
| 128KiB顺序写 | 16 | 1688.4 | 96.1 | 5.7% |

含启动fio至最终sync返回的带宽（不含卸载耗时）：

| 工作负载 | 线程 | EXT4 | rNVPCe |
| --- | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 288.9 | 111.6 |
| 4KiB随机写 | 16 | 328.7 | 116.1 |
| 128KiB顺序写 | 1 | 1137.8 | 108.2 |
| 128KiB顺序写 | 16 | 1219.0 | 82.0 |

另一个32MiB顺序写诊断点也完整通过：fio 240.6MiB/s，含sync 33.7MiB/s。该短测不代表稳态性能。

此前的完整2GiB rNVPCe点因EAGAIN失败，因此没有有效的修复前完整吞吐可用于计算提升倍数。修复途中仅启用容量/BIO修正时，逐页分配屏障的诊断点在180秒只写入545.625MiB（约3.0MiB/s），虽然最终卸载成功，仍属于不完整测试；合批修正后才正常完成2GiB。这个诊断结果不作为正式前后性能比。

1/16线程的性能差异尚未做延迟分解；不能仅凭这些吞吐将剩余差距归因于某一把锁、设备或某项persist。16线程未带来rNVPCe吞吐扩展，需要后续单独分析。首次EXT4对照与本轮数值也有差异，单点一次不足以形成统计结论。

## 验证

- 最终代码：OSDK内核全套，SMP=1，**302 passed / 0 failed / 0 filtered out**。
- aster-block边界测试：**1 passed / 0 failed**，覆盖低于、等于及高于上限。
- 四条失败证据保留：原容量压力返回EAGAIN、62段边界拒绝、32页逐页32次分配屏障、预留回滚后写者未唤醒。修复后均通过；场景整合在现有`ext4_research_routes` ktest内。
- 新增批量迁出场景验证native inode大小、导出的native extent对应SSD原始数据内容，以及分配屏障失败时free-block计数恢复、所有NV目标仍未发布。
- 最终9个真实盘点（4个rNVPCe主测试、4个EXT4对照、1个32MiB诊断）均完成、正常卸载、e2fsck exit=0。runner已改为将fio错误、字节数不足、卸载失败视为失败，避免guest结束标记被误认为测试通过。
- release构建、改动文件rustfmt检查、Python编译/guest shell语法和git diff空白检查通过。

本轮SMP=2完整内核回归（容量通知补充前）出现4个RCU/缓存回收失败：`ext4_native_read_and_clean_reclaim`、`ext4_native_cache_pressure`、`pressure_accounting_and_pinned_reclaim`、`pressure_accounts_inflight_redirty_and_error`。用HEAD原始7个文件只读覆盖挂载到同一构建环境、相同SMP=2配置运行这4项，得到**0 passed / 4 failed**，确认本次改动之前即存在。没有把这4项当作通过；历史标准内核回归使用SMP=1，真实盘性能测试使用正常启动环境的64 vCPU。

这轮真实盘fio未配置CRC校验，也未做跨VM重启的逐文件内容校验或断电恢复实验；数据内容回归由内核场景的SSD原始块读回覆盖。e2fsck通过仅说明SSD文件系统结构检查通过，不能替代崩溃一致性验证。

## 可复现证据

[CSV](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/experiment-runs/rnvpce-fix-20261018/results.csv)、[验证摘要](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/experiment-runs/rnvpce-fix-20261018/validation.json)、[最终内核测试日志](/home/zn/asterinas-build-env/rnvpce-fix-20261018/kernel-final-capacity-events.log)、[冻结内核补丁](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/experiment-runs/rnvpce-fix-20261018/kernel-source.diff)。同目录保存每点guest日志、manifest、主机状态、e2fsck日志及四条失败测试日志，`harness/`保存脚本快照。

- 基础提交：`93c2b45dd4ac49b14daab7e5c4ecaab7af61d286`，本次代码尚未提交或推送。
- 最终ISO SHA256：`fd7fbdb23a099a6d8ecab26b890ece6a27acbcb7958659d550776a5783037054`。
- 内核diff SHA256：`2e08c6baeb1f9bff382d271e2496cd519179739bd5739caaf2df0a5788ae148e`。
- 大型镜像/完整实验目录：`/home/zn/asterinas-build-env/rnvpce-fix-20261018/final-notify/`。
