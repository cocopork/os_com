# Filebench EXT4 / rNVPCe-SU 对比结果

## 结论

表中为三轮 60 秒测试的 `ops/s` 中位数；相对差异以 EXT4 为基准。

| 设备 | Workload | EXT4 (ops/s) | rNVPCe-SU (ops/s) | rNVPCe 相对差异 |
| --- | --- | ---: | ---: | ---: |
| NVMe | fileserver | 15,096 | 14,279 | -5.42% |
| NVMe | varmail | 7,125 | 5,607 | -21.31% |
| NVMe | webserver 10:1 | 35,267 | 35,415 | +0.42% |
| NVMe | webserver 1:1 | 16,976 | 14,193 | -16.39% |
| SATA | fileserver | 5,567 | 6,998 | +25.71% |
| SATA | varmail | 3,745 | 3,494 | -6.70% |
| SATA | webserver 10:1 | 12,315 | 17,590 | +42.84% |
| SATA | webserver 1:1 | 3,964 | 5,949 | +50.08% |

NVMe 上，rNVPCe 的 fileserver 与读密集 webserver 10:1 已接近 EXT4；
varmail 和 webserver 1:1 仍分别低 21.31% 和 16.39%，说明写事务、容量回收
或读写交替路径仍有优化空间。SATA 上 rNVPCe 在 fileserver 和两种 webserver
中高 25.71%--50.08%，但 varmail 低 6.70%。

`varmail` 的结果需要结合语义理解：EXT4 的每次 `fsync` 要把 journal/SSD
状态推进到持久点；rNVPCe 的文件数据和固定 inode 已在 NVM 中持久化，因此
单文件 `fsync` 不要求立即把全部 NV 数据迁移到 SSD。两者都提供持久性，但
持久介质路径不同。

## 有效性检查

- 48 个测试点全部生成 `result.json`。
- 48 个测试点均有 `FILEBENCH_COMPLETE status=0`。
- 48 个测试点均满足 `sync_status=0`、`umount_status=0`。
- 48 次宿主侧 `e2fsck -fn` 均返回 0。
- 每个设备、模式、workload 均有 3 次独立重复；图和上表使用中位数。

完整逐轮数据见 `raw-results.csv`，包含 `ops/s`、`MB/s` 和平均操作延迟；
中位数、均值、最小值和最大值见 `summary.csv`。图片只展示主指标 `ops/s`，
避免把不同 flowop 组合的 MB/s 当成跨 workload 的统一指标。

## 测试中发现并修复的问题

1. rNVPCe PageCache 的清洁页回收通过 XArray range 逐索引扫描空洞；扫描尾部
   会趋向 `u64::MAX`，导致 10000 文件测试在关闭时卡住。现改为只遍历
   present slot，并加入容量受阻时的强制回收路径。
2. 前台首次写已持久化 SSD 分配后，若 rTIO 抢先取得 baseline mutation guard，
   写事务以 `EAGAIN` 重试但未回滚本轮 SSD blocks。诊断轮记录遗留 3 块，
   `e2fsck` 同样报告对应 3 块无引用分配；现于重试前执行持久化
   clear-before-release 回滚。
3. 批量 NVPage 回收释放多个槽位却只唤醒一个容量等待者，可能让其余线程在
   已有空闲容量时永久睡眠。现改为批量释放后唤醒全部等待者，并在仅阻塞的
   容量/lease 慢路径中每 1 秒重新检查条件，封闭 wake-before-sleep 窗口。

修复后，SSD 分配竞争场景连续完成 5 轮 60 秒 fileserver；容量等待场景连续
完成 8 轮 60 秒 webserver 1:1。正式矩阵随后使用最终代码重新运行全部
rNVPCe 点。

## 边界

- 128 KiB 是预分配阶段的平均初始文件大小；追加 workload 会在运行期间增大
  文件，因此 128 MiB 总缓存约为初始 1.25 GiB 数据集的 10%。
- 结果是当前 2 GiB guest image、64 vCPU、8 GiB VM 内存和指定真实设备上的
  前期宏观对比，不替代论文最终的大容量、长时间完整评估。
- NVMe 使用 4 个 rTIO worker/virtqueue；SATA 使用 1 个。设备之间不应仅凭
  本表判断文件系统算法优劣。
