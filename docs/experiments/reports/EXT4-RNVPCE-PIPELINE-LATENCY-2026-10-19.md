# EXT4 与 rNVPCe 三阶段延迟分解

## 结论

rNVPCe 当前的首要瓶颈在前台写事务，不是 rTIO 长时间休眠。2 GiB 测试中，
rTIO 对每个 workload 都处理约 512 个、每批 1024 页的 migration batch；128K
顺序写的 completion queue 等待仅 0.016--0.131 us/page，说明 completion 能及时被
唤醒。相比之下，rNVPCe 前台持久化在 128K 单线程下平均 573 us/call，16 线程下
达到 6.49 ms/call；原生 EXT4 写 PageCache 分别为 58.5 us 和 98.0 us。

4K 随机写还有第二个独立问题：16 线程下 migration batch 在 data/metadata 都完成
以后，到 completion 线程开始处理之前，平均排队 90.4 ms，累计 46.47 s。此时
completion 的 extent 发布为 10.35 us/page，completion 本体为 18.93 us/page，
服务速度不足以跟上到达速度。取消全局 in-flight 上限后没有出现长时间空闲，反而
暴露了 completion backlog。

## 测试条件

- 设备：`/dev/nvme1n1p1`，virtio-blk 单队列，queue size 1024。
- VM：64 vCPU、8 GiB；fio CPU 0--31，rTIO flush/completion 和 virtio 工作线程隔离。
- fio：buffered `psync`、`direct=0`、`fsync=0`，每点 2 GiB。
- workload：4K `randwrite`、128K `write`，1/16 线程。
- 缓存：rNVPC data limit 64 MiB；原生 EXT4 dirty cache limit 64 MiB、低水线 32 MiB。
- rNVPC 模拟：`rnvpc_sim_mode=fast`；诊断统计单独开启，旧的 rNVPC 详细统计关闭。
- 8/8 点完整写入，sync 和卸载成功，后置 `e2fsck -fn` exit 0。

## 总体结果

| workload | EXT4 fio MiB/s | rNVPCe fio MiB/s | rNVPCe/EXT4 | EXT4 drain MiB/s | rNVPCe drain MiB/s |
|---|---:|---:|---:|---:|---:|
| 4K randwrite, 1 thread | 428.0 | 137.5 | 32.1% | 390.8 | 131.6 |
| 4K randwrite, 16 threads | 372.2 | 136.1 | 36.6% | 340.2 | 129.4 |
| 128K write, 1 thread | 1654.3 | 166.5 | 10.1% | 1144.1 | 120.5 |
| 128K write, 16 threads | 2043.9 | 206.8 | 10.1% | 1356.3 | 137.0 |

`drain` 从 fio 开始计时到最终 sync 返回。诊断埋点会带来开销，因此本表用于同镜像
内的相对比较，不替代关闭统计后的正式带宽结果。

## 三阶段对比

以下时间按实际处理单位归一化。前台是一次 fio `write()`；flush 和 completion 是
每页。多线程累计时间会重叠，不能相加得到 wall time。

| workload | EXT4 前台总计 us/call | rNVPCe 前台总计 us/call | EXT4 flush us/page | rTIO flush us/page | EXT4 I/O completion us/page | rTIO completion us/page | rTIO queue us/page |
|---|---:|---:|---:|---:|---:|---:|---:|
| 4K randwrite, 1 thread | 8.29 | 27.35 | 6.87 | 7.87 | 3.40 | 7.00 | 4.58 |
| 4K randwrite, 16 threads | 164.93 | 454.97 | 10.88 | 9.12 | 4.05 | 18.93 | 88.64 |
| 128K write, 1 thread | 73.24 | 748.22 | 2.39 | 8.06 | 0.13 | 5.43 | 0.13 |
| 128K write, 16 threads | 886.41 | 9558.57 | 1.94 | 18.51 | 0.31 | 11.70 | 0.02 |

### 前台 write

| workload | EXT4 PageCache body us/call | rNVPCe persist body us/call | EXT4 capacity wait total s | rNVPCe capacity wait total s |
|---|---:|---:|---:|---:|
| 4K randwrite, 1 thread | 8.06 | 18.92 | 0.00 | 0.66 |
| 4K randwrite, 16 threads | 17.48 | 236.13 | 43.82 | 56.05 |
| 128K write, 1 thread | 58.50 | 573.02 | 0.24 | 0.00 |
| 128K write, 16 threads | 98.03 | 6485.03 | 12.62 | 15.11 |

容量等待不足以解释主要差距。最明显的是 `commit_range_write_with_mutation` 所在的
持久化 body 在并发下膨胀：4K 为原生 body 的 13.5 倍，128K 为 66.2 倍。下一步
应在该事务内部继续拆分 NVM data persist、descriptor/control persist、authority/
mutation 锁等待和 extent-tree 更新，而不是先调整 rTIO 唤醒周期。

### rTIO flush 与 completion

| workload | candidate select us/page | target prepare us/page | data submit us/page | metadata submit us/page | async ready us/page | device flush us/page | extent publish us/page | reclaim us/page |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 4K randwrite, 1 thread | 1.52 | 0.27 | 6.05 | 0.02 | 11.64 | 2.32 | 3.90 | 0.58 |
| 4K randwrite, 16 threads | 3.76 | 0.13 | 5.21 | 0.02 | 35.27 | 6.57 | 10.35 | 1.81 |
| 128K write, 1 thread | 1.91 | 1.57 | 1.89 | 2.68 | 2.72 | 0.14 | 4.00 | 1.12 |
| 128K write, 16 threads | 10.17 | 4.59 | 1.37 | 2.38 | 2.40 | 0.15 | 9.85 | 1.61 |

顺序写中设备 flush 只有 0.14--0.15 us/page，NVPage reclaim 为 1.12--1.61
us/page；它们不是十倍带宽差距的主因。extent 发布更重，并在 16 线程下明显恶化。
随机写的设备完成等待和单次 flush 成本更高，但最严重的现象仍是 16 线程
completion queue backlog。

## 判断与后续顺序

1. 先分解并优化前台 `commit_range_write_with_mutation`。这是四种 workload 都存在、
   且随线程数急剧恶化的共同瓶颈。
2. 对 completion 的 extent publication 合并跨 batch/同 inode 更新，减少 128 页
   soft-update chunk 的重复锁与持久化边界；同时保留 1024 页的每轮上限。
3. 在 4K 随机写下记录 ready queue 长度和 completion 服务率，评估是否需要按设备
   完成速度做 backpressure。当前无全局上限会形成 90 ms/batch 的队列等待。
4. flush 线程没有统一的长期休眠证据。顺序写 queue wait 接近零，随机写约 512 个
   batch 均被处理；调整 10 ms idle retry 不是当前优先项。

## 证据

- 可复现脚本：`docs/experiments/scripts/pipeline-latency-20261019/`。
- 原始日志、manifest、CSV 和镜像保存在
  `/home/zn/asterinas-build-env/pipeline-latency-20261019/`，不纳入 Git。
- 最终诊断 ISO SHA256：
  `0474339ab319b15865a8c2b58b821a43aef5af69d9bfbc7286f3136238ff73a6`。
- histogram 的 P50/P95/P99 与 count/sum/max/units 保存在原始
  `pipeline-metrics.csv`；报告表使用精确 sum/units 或 sum/count。
