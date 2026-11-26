# EXT4 组级分配器实现与性能验证（2026-10-17）

## 结论

已实现 **组级分配锁、内存空闲空间索引、统一容量预留账本**。本轮真实盘测试没有证明稳定的整体吞吐提升：128KiB 顺序扩展写的两轮均值在单线程下降 12.8%，16 线程下降 1.5%。不能将“移除了搜索外围的超级块大锁”等同于性能优化已经完成。

4KiB 随机覆盖写单次样本上升，但这项负载先预填充，主要不触发新块分配，且未重复采样；不能据此确认分配器带来加速。

本任务基于 `recovery/rnvpc-rebuild`、HEAD `8ce7d0bfd30c200ad561f95503fc0e9aace61a37` 及开始时已有的未提交缓存压力改动。前后对照都包含这些缓存压力改动。本次没有提交或推送。

## 实现范围

- `allocator/free_space.rs`：按 64 块 word 维护连续空闲区间的 prefix/suffix/longest 分层摘要，保留 goal、完整区间优先和最长部分区间语义。一个完整 32768 块组约 12KiB，按需构建，重放后丢弃重建。它不是完整移植 Linux mballoc，也不是 DRAM PageCache 分配器。
- `block_group.rs`：独立组分配 gate 覆盖空闲索引、bitmap/GDT 修改及 free 的 pin 发布窗口。所有分配/释放入口遵守同一组保护协议。
- `allocator/mod.rs`：物理空闲、inflight claim、rNVPCe promise 使用同一短互斥协议授权。RAII 在 drop/部分消费时归还未使用额度；不确定错误保守消费额度并走现有 abort 语义。普通请求保留 metadata floor 和 rNVPCe promise，其他 intent 保留既有权限。
- `fs.rs`：容量授权后释放容量锁，再按组搜索/修改；退出组保护后才短暂更新、捕获超级块计数。数据和 extent 元数据继续通过原有分配入口接入。临时 inflight 占满不再误报永久 ENOSPC，等待时不持有容量、组或超级块锁。
- `journal/mod.rs`：提供 pending-free 区间总量及新鲜快照。搜索在组 gate 下临时屏蔽 pin，再从权威 bitmap 恢复索引。提交仍由 journal 退休 pin，下次搜索自然看到可复用空间，不新增 commit -> group 回调。
- `allocator/metrics.rs`：默认关闭的编译期开关，另行构建诊断镜像；吞吐样本未打开探针。

未实现 inode PA、locality-group PA、per-CPU extent 池或 delalloc；磁盘格式和 journal 持久性语义未作调整。

## 一致性与残余锁

当前 journal 的 Model A admission 不允许旧事务仍有活动 handle 时启动后继事务。分配操作的 handle 在组修改和超级块计数捕获完成前保持打开，因此后继事务的计数不会泄入前一个事务的镜像。共享 GDT block 继续使用既有 journal 字段 patch 序列化更新。

没有 group -> superblock 嵌套：组 gate/metadata 在超级块发布前释放。现有 inode 分配的 superblock -> group 路径仍存在；journal 不反向调用组或容量锁。

仍有短容量互斥、超级块发布锁、journal 状态锁和 GDT patch 串行点。冷元数据捕获仍可能在 journal leaf lock 下做同步 I/O，冷超级块 seed 也可能延长发布锁。全局 pending-pin 求并集、复制 pin 集合的成本随 pin 数增加；这是本次审查记录的后续优化项，本轮覆盖写没有针对碎片释放压力验证它。

详细状态表、锁图和实现取舍见 [protocol.md](/home/zn/asterinas-build-env/ext4-group-allocator-20261017/protocol.md)。

## 回归与审查

最终 EXT4 + PageCache OSDK ktest：**593 passed / 0 failed / 158 filtered**，QEMU KVM、SMP=1、4GiB。这不是全部内核测试，也不把 SMP=1 线程测试称为多核压力测试；真实盘 fio 使用 SMP=16。

新增 8 项测试包含：索引与 bitmap 模型的 2000 步对照、goal/碎片/尾组、容量部分消费/drop/floor/rNVPCe/pin、普通与 rNVPCe 临时 claim 等待、持有超级块锁时组搜索仍可推进、并发所有权与共享 GDT block 计数。旧的 pin/revoke、近满盘 metadata reserve、故障中止、提交/checkpoint 和崩溃重放测试也已运行。

观察到的 RED -> GREEN 证据保存在 `index-red.log`、`integration-red.log`、`capacity-inflight-red.log`、`rnvpce-inflight-red.log` 和最终 `regressions.log`。最后一次等待语义修正时，旧测试把同线程仍持有的临时 claim 当成永久 ENOSPC，导致测试停住；修正为真正超出物理容量的请求后，完整回归通过。该停住的运行没有计入通过结果。

独立代码审查发现 1 项 Important：rNVPCe promise 会被临时 claim 误拒绝。已经补充复现测试并修复，最终完整回归覆盖该修复；没有 Critical。审查是源码审查，不代替这里的运行验证。Rustfmt 和 diff whitespace 检查通过。

## 实验配置与吞吐

- 唯一真实目标为已授权清空的 `/dev/nvme1n1p1`，256GiB；每个样本独立格式化，卸载后只读 `e2fsck -fn`。未操作整盘或其他分区。
- Docker `os-com-recovery-tools:20261017`，QEMU 10.2.1、KVM、16 vCPU、8GiB；virtio-blk 单队列、queue-size=64、host cache=none、aio=threads。
- rNVPC/rNVPCe/rTIO 均关闭，EXT4 有 journal。512MiB resident / 128MiB dirty 的已有缓存压力配置保持一致。
- fio 3.39、buffered psync、iodepth=1、thread=1、每样本合计 16GiB。128KiB 写新文件；4KiB randwrite 先逐文件顺序预填充并 sync，再固定随机 seed=20261017 覆盖。所有样本完整写满 16GiB，没有触发 480 秒上限。
- 顺序写各 2 对，第二对反转顺序；随机写各 1 对。表中顺序值为算术均值，不是统计显著性结论。

| 负载 | 每版本样本数 | 旧版 fio MiB/s | 新版 fio MiB/s | 变化 | 旧版含 sync MiB/s | 新版含 sync MiB/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 128KiB 顺序，1 线程 | 2 | 321.8 | 280.6 | -12.8% | 318.7 | 277.9 |
| 128KiB 顺序，16 线程 | 2 | 299.8 | 295.2 | -1.5% | 297.4 | 292.3 |
| 4KiB 随机覆盖，1 线程 | 1 | 227.4 | 260.5 | +14.5% | 226.0 | 258.4 |
| 4KiB 随机覆盖，16 线程 | 1 | 224.7 | 230.5 | +2.6% | 222.9 | 228.9 |

顺序原始样本：单线程旧版 335.6/308.0、新版 264.1/297.0；16 线程旧版 300.7/298.9、新版 285.3/305.1 MiB/s。吞吐确有波动，16 线程两轮高低顺序相反。含 sync 的结果与 fio 结果接近，不能用“只测到了缓存速度”解释本轮结果。

## 诊断与数据校验

独立诊断为 4GiB、128KiB、16 线程，各版本 1 个样本。仅诊断镜像启用探针，并将诊断行设为 warn、日志阈值设为 warn，避免 virtio 的逐请求 info 日志干扰。最初的 noisy-info 诊断启动已停止并归档到 `excluded-info-profile`，不纳入任何结果。

| 分配阶段 | 旧版平均等待 μs | 新版平均等待 μs | 旧版平均持有 μs | 新版平均持有 μs |
| --- | ---: | ---: | ---: | ---: |
| 超级块：旧版整个分配 / 新版仅计数发布 | 3.432 | 4.868 | 7.954 | 2.676 |
| 组分配：新版 gate；旧版已由超级块串行化 | 不单独计量 | 39.098 | 5.952 | 12.534 |
| 容量授权 mutex | 不存在 | 5.075 | 不存在 | 1.407 |

旧版 32881 次分配、33453 次组访问；新版 32880 次授权、33449 次组访问，均约 1.017 次组访问/请求。新版没有观测到 pinned denial，授权次数也没有出现 inflight 重试放大。该负载不代表碎片搜索或 pin-heavy 释放压力。

**观察**：超级块持有范围/均时缩小约 66%，但超级块等待没有下降，新版还存在显著组 gate 等待。**推断边界**：组持有时间同时包含索引、metadata lock、journal 捕获和调度影响；当前探针不能将回退独立归因于某一项，也没有记录每组请求分布或单独 journal lock 时间。探针和原子计数本身会扰动运行，跨线程累计时间不能当作墙钟时间百分比。下一步性能分析应拆开组内索引更新与 journal capture，并检查请求的组分布，不能只继续缩短超级块锁。

另用最终无探针镜像完成 **4GiB、16 线程 CRC32C**：先写入并 sync/卸载，再重新挂载，fio `verify_only=1` 读取校验完整 4GiB，所有 job error=0。该数据校验与 e2fsck 结构检查分别记录；12 个吞吐样本、2 个诊断样本及这 1 个数据校验样本，共 **15 次 e2fsck -fn 全部 exit=0**。

诊断源码均在 finally 中按字节恢复；最终默认构建关闭探针。恢复清单见 `profile-source-restoration.json`，锁计数见 `lock-metrics.json`，综合验证见 `validation-summary.json`。

## 证据与复现

原始日志、冻结源码、前后 ISO/ELF、脚本和磁盘统计保存在：
`/home/zn/asterinas-build-env/ext4-group-allocator-20261017/`。

- `baseline.diff` / `baseline-source` / `baseline-manifest.json`：任务起点未提交改动；`feature-only.diff` / `final-source` / `final-feature-manifest.json`：本项变更与冻结源码。
- `baseline.iso` SHA256：`08d2368c522ee702564c8577e728ffef6615d543dac773a19a332990ef8c0cd9`。
- `group.iso` SHA256：`4ccb9fb5b4b401fe2468d796cbd2c6359446b736037a07b5f6004c5e8b0c9f82`。
- `results.csv` / `summary.json`：完整吞吐及同步时间、延迟、host 分区写带宽；`*-status.json` 记录每个样本实际 ISO 哈希。
- `run-regressions.py` / `build-release.sh` / `run-comparison.py` / `run-extra-checks.py` / `run-case.py` / `guest-fio.sh`：复现脚本；其中格式化脚本仅适用于本次已明确授权的专用分区，不能换目标盲目运行。

本轮没有做 allocator 新增状态每一个指令边界的故障穷举，也没有全盘 256GiB 长稳测试。更全面的故障注入和 pin-heavy 释放压力仍是后续验证范围。
