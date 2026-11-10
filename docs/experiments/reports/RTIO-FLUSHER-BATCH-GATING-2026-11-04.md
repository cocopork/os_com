# rTIO flusher dirty 队列批量门控实验

日期：2026-11-04

## 结论

本次修改成功消除了双线程覆盖写中的逐页候选提交和逐页设备 FLUSH，但没有显著提高
4 KiB 随机覆盖写吞吐。因此，“前台每发布一页就立即迁移”是实际存在的调度缺陷，
但不是当前双线程性能差距的主因。

## 修改

- 普通后台迁移要求挂载级 queued dirty 页达到 1024 页；
- inode 发布不足一批时只登记 owner 队列，不立即唤醒全部 flusher；
- 容量等待和 migration-progress 等待仍可强制迁移；
- sync/unmount 强制排空尾批次；
- queued dirty 页未归零时禁止 idle durability 提前封闭 epoch。

## 真实设备结果

设备为 `/dev/nvme1n1p2`，虚拟机 64 vCPU、4 个 rTIO worker、4 个 virtqueue，rNVPCe
数据上限 64 MiB，工作负载为 2 GiB、4 KiB、`randwrite`、`psync`、warm overwrite。
每个测试点均通过 fio、sync、umount 和只读 `e2fsck -fn`。

| 版本 | 线程 | fio MiB/s | 含 sync MiB/s | 平均延迟 us | P99 us |
|---|---:|---:|---:|---:|---:|
| 修改前诊断镜像 | 1 | 195.89 | - | 19.53 | 34.56 |
| 修改前诊断镜像 | 2 | 56.19 | 55.34 | 136.54 | 160.77 |
| 修改后 release | 1 | 208.68 | 202.37 | 18.36 | 28.54 |
| 修改后 release | 2 | 57.23 | 56.50 | 132.22 | 156.67 |

修改后 profiling 镜像的双线程 fio 为 54.36 MiB/s；该数值只用于提取调用计数，不与
关闭插桩的 release 数值直接比较。

## 双线程迁移粒度

下表为 524,288 个测量页的阶段差值。修改后计数取 profiling 日志中 warm prepare
快照与最终快照之差，因此不把预写阶段重复计算进测量阶段。

| 指标 | 修改前 | 修改后 | 修改后平均粒度 |
|---|---:|---:|---:|
| migration batch | 521,443 | 1,013 | 517.6 页/批 |
| candidate select | 3,809,397 | 约 1,020 | 约 514 页/次 |
| 设备 FLUSH | 221,394 | 509 | 1,030 页/FLUSH |
| capacity wait | 0 | 0 | 不适用 |

修改后每个数据页仍形成一个 4 KiB BIO；这是 4 KiB 随机写工作负载本身的请求粒度，
与本次修复的 migration batch 粒度不同。两个活跃 inode 分别绑定到 worker 0/1，
每个 worker 约提交 262,144 页，其余 worker 无数据提交。

## 证据

- 修改前：`/home/zn/asterinas-build-env/rnvpce-j2-overwrite-diagnosis-20261104/`
- 修改后：`/home/zn/asterinas-build-env/rnvpce-flusher-batching-20261104/`
- 最终 release ISO SHA256：`9d5f6462a06f72f99a6ab6a8653a77ec60051095d004d1996d86945cca2caa6b`
- profiling ISO SHA256：`1c340caec18e2a0e89d8f39351c15a3b02cfb225c63bc7e1ec8e18b658f72ea5`

## 后续定位

后续不应继续通过缩短唤醒周期或增加 durability epoch 频率优化双线程点。后续细分实验
已经将 18.36 us 到 132.22 us 的主要差值定位到前台对碎片化 SSD baseline extent tree
的两次重复路由，而不是 BIO、virtqueue 或 completion。数据和优化顺序见
[1/2 线程前台 write 路径延迟分解](RNVPCE-J1-J2-WRITE-PATH-LATENCY-2026-11-04.md)。
