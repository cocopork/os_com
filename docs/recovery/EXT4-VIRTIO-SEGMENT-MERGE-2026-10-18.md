# Virtio-blk 连续 DMA segment 合并（2026-10-18）

## 结果

已在 rebuild 的 virtio-blk 读、写路径实现连续 DMA 地址合并。在相同的 128 KiB 顺序 buffered 写、16 线程实验中，两轮合并后的 descriptor 不足阻塞请求均为 0。软件队列的等待请求数显著降低；不是把软件等待转成了同样的 descriptor 等待。

| 指标 | 合并前，第 1 / 2 轮 | 合并后，第 1 / 2 轮 |
|---|---:|---:|
| 软件队列等待峰值，不含正在取出的请求 | 30 / 20 | 9 / 1 |
| 软件队列等待数，dequeue 事件平均 | 2.117 / 2.584 | 0.0157 / 0.0068 |
| descriptor 不足导致阻塞的请求数 | 8023 / 8084 | 0 / 0 |
| descriptor 阻塞总时间 | 1.212 / 0.899 秒 | 0 / 0 秒 |
| descriptor 不足自旋次数 | 56,701,719 / 39,407,705 | 0 / 0 |
| 驱动内在途请求峰值 | 1 / 1 | 17 / 16 |
| 写请求平均 descriptor 数，包含头尾 | 59.87 / 60.05 | 3.635 / 3.633 |
| 仅需一个数据 descriptor 的写请求比例 | 0.452% / 0.454% | 63.55% / 63.07% |
| descriptor 占用峰值，队列容量 64 | 64 / 64 | 51 / 48 |
| 请求队列空闲时间比例 | 60.81% / 60.26% | 75.30% / 66.35% |
| 带最终 sync 的写入带宽，MiB/s | 580.2 / 812.7 | 752.9 / 1157.1 |

确实消除了本次实验中的 descriptor 容量阻塞。合并后队列仍然经常为空，平均在途请求只有 0.935 / 0.823；该改动尚不能证明已填满设备队列，也不能把全部空闲时间归因于某一个软件锁。两轮 2 GiB 短测有明显波动，带宽只作为伴随观测，不宣称固定提升比例。

## 实现与边界

- `kernel/comps/virtio/src/device/block/segment_merge.rs`：保存首个 DMA buffer 的借用及合并长度，按原顺序合并**设备 DMA 地址**恰好相邻的范围；检查地址与长度加法溢出、descriptor 长度上限。
- `kernel/comps/virtio/src/device/block/device.rs`：读写均按合并后的段数提交；头部与状态字节始终独立，数据方向保持原有读写语义。128 KiB 的 32 个连续 4 KiB 页对应 1 个数据 descriptor，连同头尾为 3 个，而不是原来的 34 个。
- 不复制数据。原 BioRequest、所有原始 BIO 和 DMA buffers 保留到 completion；写端同步每个原始 DMA slice，读完成后仍同步每个原始 slice，再完成所有原始 BIO。
- 地址不连续时按实际段数回退，不能保证任意一个请求只占一个数据 descriptor。实测平均写请求约 231 KiB，通常由 61 个原始 page segments 构成，合并后平均约 1.63 个数据 descriptor。
- 保留原始 BIO 的段数准入限制，保证最坏的完全不连续输入仍可放入队列；本次没有改 MQ、队列容量或 EXT4 BIO 聚合大小。
- `SmallVec` 的 descriptor 引用视图在本次准入范围内使用栈内空间；原有数据段容器仍使用一次 Vec 分配。提交成功后结束临时借用，再把完整 BioRequest 移入完成跟踪表。

## 测试设置与统计口径

- 专用分区 `/dev/nvme1n1p1`，每轮在用户已授权范围内重新格式化 EXT4；journal、delalloc 开启，rNVPC、rNVPCe、rTIO 关闭。
- 每轮总量 2 GiB，16 jobs，每 job 128 MiB；128 KiB 顺序 buffered `psync`，`direct=0, fsync=0, iodepth=1, thread=1`。
- resident/dirty 上限均 64 MiB、低水线均 32 MiB，持续写回策略不变。
- QEMU/KVM，16 vCPU、8 GiB、单 virtio 队列 64，`indirect_desc=off,cache=none,aio=threads`。fio 0–7，普通内核 8–11，writeback 12，completion 预留 13，virtio 提交 14，journal 15。
- 执行顺序为 before-1、after-1、after-2、before-2。不是长时间稳态基准；每轮窗口含文件创建、元数据和最终 sync。
- 软件队列在 dequeue 后采样。原始计数 `software_pending_*` 包含刚取出的请求，表格统一减 1；平均是 **dequeue 事件平均，非时间加权平均**。单独处于提交路径的请求不计入“软件队列等待”，其 descriptor 阻塞另行统计。
- 在途请求从 descriptor 发布到 IRQ 回收，包含设备已完成但尚未回收的请求，不等价于物理 NVMe 队列深度。descriptor 和请求占用率用每次状态变化的时间加权直方图统计。
- 合并前 descriptor 数由旧代码“一 segment 一 descriptor，加头尾两个”和原始段直方图推导；合并后直接记录实际 descriptor 数。
- 所有窗口均验证初始/结束在途数与 descriptor 为 0、提交数等于完成数、直方图时间总和等于窗口时长、fio 实际写满 2 GiB。

## 正确性验证

- TDD：先用未合并实现运行新增测试，32 对 1 的断言按预期失败；实现后通过。
- virtio 专项 ktest：1 项通过，真实 DMA allocation，覆盖 32 页连续合并、间隙、逆序、重复地址、长度限制。
- 完整 `aster-kernel` ktest：302 项通过。OSDK 启动时列出的依赖 crate 用例没有计入这 302 项；不将其宣称为全部执行。
- 4 轮性能测试均完整写入 2 GiB、正常卸载，宿主 `e2fsck -fn` 均返回 0。
- 另用无临时队列探针的 release 内核进行 2 GiB CRC32C 写入、sync、卸载重挂、读回校验，实际写入和校验读取均为 2,147,483,648 字节，fio 两阶段 error=0，CRC_VERIFY_READ_PASS，QEMU exit=33，e2fsck exit=0。

## 复现与产物

实验目录：`/home/zn/asterinas-build-env/ext4-segment-merge-20261018`。

保留 `implementation-plan.md`、`run-tests.py`、`red.log`、`virtio.log`、`full.log`、`before.iso`、`after.iso`、`unprofiled.iso`、各轮完整串口日志和 e2fsck 日志、`queue-analysis.json`、`install-probes.py`、`diagnostic.patch`、`queue_profile.rs`、探针前源码及带探针源码。

仓库内可直接查看 [带宽原始汇总](EXT4-VIRTIO-SEGMENT-MERGE-2026-10-18-bandwidth.csv) 和 [队列原始汇总](EXT4-VIRTIO-SEGMENT-MERGE-2026-10-18-queues.csv)。临时统计锁与 syscall 输出均已移除，最终 release 构建不含 `virtio_queue_profile=on`。本次不执行提交或推送。
