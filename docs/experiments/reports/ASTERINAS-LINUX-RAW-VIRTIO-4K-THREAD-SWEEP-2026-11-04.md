# 星绽与Linux裸virtio-blk 4 KiB thread sweep

日期：2026-11-04

## 结论

在相同真实NVMe分区、QEMU四virtqueue、四IOThread及宿主hctx绑定下，Linux guest的
4 KiB同步随机写在1/2/4/8/16线程分别达到
106.05/207.27/380.88/695.65/857.62 MiB/s。星绽对应结果为
44.83/87.42/161.57/282.21/405.06 MiB/s。

Linux在全部线程数下均更快，吞吐为星绽的2.12至2.47倍。主要差距是低并发单请求延迟：
单线程Linux平均延迟36.53 us，星绽86.75 us；16线程分别为68.00和144.06 us。
两侧从1到16线程分别扩展8.09倍和9.04倍，说明星绽多队列的相对扩展曲线并不差，
但每个同步请求的基础软件延迟显著较高。

本轮Linux 16线程结果证明，在当前QEMU和真实设备配置下，4 KiB QD1/job路径至少可以
达到857.62 MiB/s。星绽16线程达到该值的47.2%。这给出了驱动/块层对照，但不改变
rNVPCe仍低于星绽裸设备路径的事实；rNVPCe内部差距另行分析。

## 同口径配置

- 真实设备：`/dev/nvme1n1p2`，父设备`nvme1n1`，256 GiB；
- QEMU 10.2.1/KVM，64 vCPU、8 GiB，`cache=none,aio=threads`；
- 四个virtqueue分别绑定四个QEMU IOThread；IOThread再绑定到宿主NVMe hctx 0-3
  对应CPU；queue depth 1024；关闭request merging、indirect descriptor、event index、
  discard和write zeroes；
- fio 3.39：`randwrite`、4 KiB、`psync`、`direct=1`、iodepth 1/job、总量2 GiB，
  job数1/2/4/8/16；每个job使用互不重叠的设备区间；
- 不创建或挂载文件系统，不执行最终FLUSH；结果代表裸块设备请求完成吞吐，不代表
  断电持久化吞吐；
- 每点一次，属于开发阶段扫描，不提供统计置信区间。

星绽使用`main@fe6f09ff7c02294232c5e24e19ef0b7f7d0038ec`，release ISO SHA-256为
`88d7e3517d050da0c048016f6144f4004b383caba6b10e493bebe2144421dadf`。
Linux内核为6.16.0，`vmlinuz` SHA-256为
`829d4e998dc29b6a0a6ea549bb063ff283cc61b26da37abc3ec89febd4d211d3`，最终initramfs
SHA-256为`96c1650ada88249df7ac8583e3116e8c4f80b5b820b19eb5f514a8d13ea0192c`。

Linux blk-mq把guest CPU 0-15、16-31、32-47、48-63分别映射到四个virtqueue。
Linux fio CPU集合从`/sys/block/vdc/mq/*/cpu_list`读取并按队列轮转选择，使2线程覆盖
两个队列，4线程及以上覆盖四个队列。星绽沿用CPU 0-31 split和自身队列选择逻辑；
两侧最终都由宿主hctx跟踪验证实际队列展开，而不是仅依据配置推断。

## 性能结果

| job | Linux MiB/s | 星绽 MiB/s | Linux/星绽 | Linux平均延迟 us | 星绽平均延迟 us | Linux P99 us | 星绽 P99 us |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 106.05 | 44.83 | 2.37x | 36.53 | 86.75 | 70.14 | 173.06 |
| 2 | 207.27 | 87.42 | 2.37x | 37.20 | 87.90 | 71.17 | 125.44 |
| 4 | 380.88 | 161.57 | 2.36x | 37.36 | 93.67 | 72.19 | 130.56 |
| 8 | 695.65 | 282.21 | 2.47x | 44.37 | 103.82 | 97.79 | 214.02 |
| 16 | 857.62 | 405.06 | 2.12x | 68.00 | 144.06 | 144.38 | 280.58 |

相对各自单线程吞吐：

| job | Linux扩展倍率 | 星绽扩展倍率 |
| ---: | ---: | ---: |
| 1 | 1.00x | 1.00x |
| 2 | 1.95x | 1.95x |
| 4 | 3.59x | 3.60x |
| 8 | 6.56x | 6.30x |
| 16 | 8.09x | 9.04x |

两条曲线的相对扩展形状接近。星绽不是随着线程增加更快失去扩展性，而是在每个并发点
都承担更高的请求延迟。16线程时Linux开始出现更明显的延迟增长，因此两侧吞吐比从
8线程的2.47倍收窄到2.12倍。

## 队列验证

表中仅列宿主NVMe hctx 0-3上的请求数；每点另有少量只读ready探测请求落到其他hctx，
不影响数据请求分布。

| job | Linux hctx 0/1/2/3 | 星绽 hctx 0/1/2/3 | 门槛 |
| ---: | --- | --- | --- |
| 1 | 524294/0/0/0 | 19/0/0/524288 | 单同步线程，单队列符合预期 |
| 2 | 262149/262145/0/0 | 17/0/262144/262146 | 两个数据队列 |
| 4 | 131078/131072/131072/131072 | 131091/131072/131072/131072 | 四队列通过 |
| 8 | 131077/131073/131072/131072 | 131091/131072/131072/131072 | 四队列通过 |
| 16 | 131078/131072/131072/131072 | 131089/131072/131072/131074 | 四队列通过 |

## 无效尝试与修正

最初Linux启动未带网络设备，而通用`bench_runner`要求配置eth0；该次在fio前退出。
第二次启动发现initramfs没有预挂载proc；该次同样在fio前退出。修正启动环境后的一轮
Linux矩阵虽然完成fio，但fio CPU 0-31全部位于Linux mq/0和mq/1映射范围，且16个job
实际只占CPU 0-15，导致所有数据请求进入virtqueue 0。这组单队列数据不纳入正式结论。

最终脚本从sysfs读取每个队列CPU集合并跨队列选择CPU。最终2/4/8/16线程点分别通过
两队列或四队列宿主hctx门槛；单线程状态3只表示没有使用四个hctx，不是I/O失败。

## 正确性和证据

- 最终5点fio均返回0并精确写入2 GiB；
- 2/4/8/16线程的宿主队列分布符合预期，所有QEMU和跟踪容器均正常排空；
- 原始Linux最终证据：
  `/home/zn/asterinas-build-env/rnvpce-raw-thread-sweep-20261103/linux-matrix-final/`；
- 星绽证据：
  `/home/zn/asterinas-build-env/rnvpce-raw-thread-sweep-20261103/matrix-current/`及
  `smoke-raw-j1/`；
- 无效尝试分别保留在`linux-matrix/`、`linux-matrix-fixed/`、
  `linux-matrix-procfix/`和`linux-mq-diagnostic/`，不混入最终数据；
- 可复现脚本：`docs/experiments/scripts/rnvpce-raw-thread-sweep-20261103/`和
  `test/initramfs/src/benchmark/fio/raw_virtio_thread_sweep/`。

最终Linux raw点覆盖了p2文件系统元数据。实验结束后，宿主`lsblk`仍显示缓存的`ext4`
类型，但在具有设备权限的容器内执行只读`e2fsck -fn`返回`Bad magic number in
super-block`（状态8）；因此`/dev/nvme1n1p2`当前不能作为有效EXT4文件系统使用，重新
挂载前需要格式化。
