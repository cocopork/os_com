# rTIO 128K 页 in-flight 窗口实验

日期：2026-10-19。

> 后续实现状态：本报告记录的是扩大固定全局窗口的探索性实验。最终代码已取消
> `nv_in_flight` 全局准入上限，改为与 EXT4 writeback 共用 1024 页（4 MiB）
> 单轮批次；完成计数仍保留。下列性能数字证明旧 128 页全局窗口存在瓶颈，但尚未
> 重新测量最终的 1024 页分批流水线，不能作为最终方案的性能结果。

## 结论

将 `nv_in_flight` 上限从 128 页提高到 128K 页后，当前 64 MiB rNVPCe data limit 将实际并发窗口限制为最多 16,384 页，即相对旧窗口最多扩大 128 倍。单轮真实 NVMe 实验显示，多线程带宽明显提高：128 KiB 顺序写 16 线程含 drain 从 012 的三轮中位数 106.39 MiB/s 提高到 140.08 MiB/s（+31.7%），4 KiB 随机写 16 线程从 88.01 提高到 123.82 MiB/s（+40.7%）。这支持“原 128 页窗口限制了 rTIO 批量迁移”的判断。

单线程收益较小。顺序写含 drain 为 129.13 MiB/s，相对 122.49 MiB/s 提高 5.4%；随机写为 117.10 MiB/s，相对 122.41 MiB/s 降低 4.3%，但仍在 012 的三轮范围 116.17--127.36 MiB/s 内。当前只有一轮新版本结果，不能据此声称随机写单线程发生回退或给出统计显著性结论。

## 实现约束

原来的 128 不只是调度窗口，也是三个 soft-update 持久化接口的事务上限。只修改 `MAX_IN_FLIGHT` 会在第一次 64 MiB 迁移时触发 `EINVAL`，随后因部分目标已经分配而进入 `ambiguous migration target requires recovery`，fio 在 64 MiB 边界失败。

当前实现保留 128 页持久化原子子事务：目标描述符发布按 128 页切分，每 inode 的 extent 发布、容量额度提交和 NVPage 回收也按 128 页切分；外层 migration batch 可以容纳整个可用 data pool。data BIO 和 EXT4 allocation metadata 仍在同一个 orderless migration window 内提交，completion 在整窗 I/O 完成后只执行一次 SSD flush。completion queue 保存的是 migration batch，不按页预分配 128K 个槽位。

因此本实验主要减少 SSD flush、迁移工作者轮次和容量唤醒次数，并没有把 NVM 描述符的 soft-update 原子事务扩大到 128K 页。

## 性能结果

设备为 `/dev/nvme1n1p1`，每点重新格式化为 4 KiB nojournal EXT4。VM 为 64 vCPU、8 GiB，virtio-blk 单队列深度 1024；fio 使用 buffered psync、fsync=0，每点写入 2 GiB。rNVPCe pool 为 128 MiB，metadata reserve 和 data limit 均为 64 MiB，并启用历史 CPU 隔离。012 数据为三轮中位数，新窗口数据为一轮探索性测量。

| 负载 | 线程 | 012 fio MiB/s | 128K fio MiB/s | 变化 | 012 含 drain | 128K 含 drain | 变化 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB 随机写 | 1 | 129.01 | 121.25 | -6.0% | 122.41 | 117.10 | -4.3% |
| 4 KiB 随机写 | 16 | 91.29 | 128.38 | +40.6% | 88.01 | 123.82 | +40.7% |
| 128 KiB 顺序写 | 1 | 164.06 | 185.04 | +12.8% | 122.49 | 129.13 | +5.4% |
| 128 KiB 顺序写 | 16 | 140.66 | 211.46 | +50.3% | 106.39 | 140.08 | +31.7% |

新窗口四点均完成 2 GiB 写入，fio error=0，sync 和 unmount 成功，QEMU exit=33，随后只读 e2fsck exit=0。额外 16 MiB 顺序写冒烟点也完整通过。

## 验证边界

release 构建和 `cargo fmt --all`、`git diff --check` 通过。现有 `ext4_research_routes` 定向 ktest 首次运行明确输出前 9 个 rNVPCe 场景通过，但错误的 make 变量覆盖移除了 KVM 参数并启动了无关 workspace crate 扫描；终止后重跑没有留下完整测试汇总。因此本报告不声称定向 ktest 全部通过，正确性证据以五个真实盘 fio/sync/unmount/e2fsck 点为准。

原始镜像、日志、manifest、文件系统特征和 e2fsck 输出位于 `/home/zn/asterinas-build-env/rtio-inflight-128k-20261019/`。初始直接放大窗口的失败证据保留在 `valid/write-j1`，修正后的正式四点位于 `full/`。
