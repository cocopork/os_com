# FAST/FAIR rNVPCe 写性能

日期：2026-10-20。

## 结论

当前 rNVPCe/rTIO 在 2 GiB 实盘测试中的三轮中位数为：4 KiB 随机写单线程
13.55 MiB/s、16 线程 104.26 MiB/s；128 KiB 顺序写单线程 39.14 MiB/s、
16 线程 274.94 MiB/s。含最终 `sync` 排空后的有效带宽分别为 13.47、99.66、
35.82 和 167.59 MiB/s。

单线程结果相对 2026-10-19 的默认构建代表点明显回退。4 KiB/j1 从 133.18
降至 13.55 MiB/s（-89.8%），128 KiB/j1 从 289.14 降至 39.14 MiB/s
（-86.5%）。128 KiB/j16 从 304.26 降至 274.94 MiB/s（-9.6%）。这些历史点
来自不同源码状态，只用于定位回退方向，不能当作严格 A/B。

## 测试条件

- 设备：`/dev/nvme1n1p1`，每点重新格式化为 4 KiB EXT4。
- VM：64 vCPU、8 GiB；virtio-blk 单队列深度 1024；rTIO/virtio CPU 隔离开启。
- rNVPCe：pool 128 MiB、metadata reserve 64 MiB、data limit 64 MiB、低水线
  32 MiB、fast NVDIMM 模拟、nojournal、orderless migration。
- fio：buffered `psync`、`direct=0`、`fsync=0`、每点 2 GiB。随机写在测量前用
  direct I/O 建立完整文件，准备阶段不计入结果。
- 每个有效点均写满 2048 MiB，fio error 为 0，`sync` 和卸载成功，随后
  `e2fsck -fn` 返回 0。

## 三轮结果

| 负载 | jobs | round 1 | round 2 | round 3 | fio 中位数 | 含 drain 中位数 | mean latency | p99 latency |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite | 1 | 13.22 | 13.91 | 13.55 | 13.55 MiB/s | 13.47 MiB/s | 287.74 us | 464.90 us |
| 4 KiB randwrite | 16 | 113.41 | 101.64 | 104.26 | 104.26 MiB/s | 99.66 MiB/s | 578.28 us | 634.88 us |
| 128 KiB write | 1 | 39.35 | 39.14 | 29.25 | 39.14 MiB/s | 35.82 MiB/s | 3192.03 us | 5013.50 us |
| 128 KiB write | 16 | 274.94 | 322.42 | 188.76 | 274.94 MiB/s | 167.59 MiB/s | 6480.87 us | 147849.22 us |

带宽、mean 和 p99 均取三轮中位数。128 KiB/j16 的 fio 带宽范围为
188.76--322.42 MiB/s，排空后范围为 124.35--174.74 MiB/s，抖动明显；这里不把
最高单轮值解释为稳定吞吐。

第三轮 4 KiB/j1 首次运行受 180 秒上限截断，仅完成 2003.17 MiB，fio 本身没有
报错。该 partial 结果被排除并单独保留；把上限增至 300 秒后完整写满 2 GiB，作为
round 3 有效值。

## 测试中发现的正确性问题

首个 4 KiB 随机写点在约 300 KiB 后返回 `EINVAL`。复现表明 FAST 原地更新将新
fragment 发布到下一个 dense slot，slot 顺序不保证按逻辑块排序；FAIR 分裂却直接
按 slot 中点切分，并从最后一个 slot 计算冻结范围。随机序列可因此生成空冻结范围，
随后造成左右分区错误和 SSD migration target 冲突。

修复在节点进入 FAIR 分裂前检查 slot 顺序。若未按逻辑块排序，则先通过已有的可恢复
repair-image 协议持久化一次排序后的节点，再执行 sibling 发布、左节点 count 收缩和
parent route 发布。普通 FAST entry 更新不受影响，归一化只发生在满节点分裂边界。

16 MiB 同种子随机写复现修复后完整通过；新增随机覆盖回归在每次覆盖后检查所有已写
逻辑页的 SSD backing 映射。三轮正式测试共 12/12 个有效点通过 fio、卸载和 e2fsck。
标准 QEMU/KVM 完整 kernel ktest 为 324 passed、0 failed。

## 证据

- release ISO SHA256：`3e4fc6fdc7718e5a82c73d6c12516571bc23e1ceeb74ad7f85364da2429b812c`。
- 原始日志、manifest、status、e2fsck 输出和逐轮 CSV：
  `/home/zn/asterinas-build-env/fast-fair-perf-20261020/fixed-round-{1,2,3}/`。
- 被排除的超时样本：`fixed-round-3/partial-randwrite4k-j1/`。
- 可复现脚本：`docs/experiments/scripts/rnvpce-fix-20261018/harness/`。

