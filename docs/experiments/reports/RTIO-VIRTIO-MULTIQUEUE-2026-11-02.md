# rTIO VirtIO 多队列验证报告 - 2026-11-02

## 结论

队列亲和机制通过了真实设备后端的正确性检查和硬件队列门槛。8 个 2 GiB 测试点均成功完成 fio、sync、卸载和只读 `e2fsck -fn`。每个四 worker 测试点都通过四个不同的 virtqueue 提交请求，并使用 NVMe hctx 0-3；观测到的单个 hctx 最大占比低于 32%。最终所有队列均已排空，每个就绪的持久化 epoch 只发出一次设备 FLUSH。

性能验收**失败**。随机写性能有所提升，但计入 sync 后，单 job 顺序写吞吐量回退 46.9%，16 job 顺序写吞吐量回退 42.3%。与此前四 worker/单队列诊断相比，16 job 顺序写还增加了累计提交时间和容量等待时间。因此，多 worker 路径应继续作为实验功能，生产默认值保持一个 worker。测试候选版本 `936722960` 采用按队列数自动选择 worker 的默认策略；后续审查修复已将当前默认值恢复为一个 worker，同时保留有上限的显式配置。该修复没有改变已固化的测试镜像及其结果。

## 测试方法与来源

本实验不是 fio 直接读写裸分区。宿主机真实分区 `/dev/nvme1n1p1` 作为 raw 块设备直通给 QEMU；星绽系统通过 virtio-blk 访问它，在来宾系统中格式化并挂载 nojournal EXT4/rNVPCe，然后 fio 对挂载点内的文件执行 buffered `psync` 写入。因此，报告中的“真实设备”表示后端是真实 NVMe 分区，不表示 fio 使用裸设备 direct I/O。

- 源码版本：`936722960e29285bcf80c732580535f5a7ed85d3`。构建时仅有一项临时差异：将 `pipeline_profile::ENABLED` 从 `false` 改为 `true`。差异保存为 `build-source.diff`；复制 release ISO 后立即恢复。测试期间没有修改其他内核源码，也没有重新构建。
- release 内核检查和构建均以状态 0 结束。这是启用插桩的 release 实验，每个测试点只运行一次，不是无插桩性能结果，也不构成统计置信区间。
- 8 个测试点共用一个 ISO，SHA-256 为 `fbb3259fe8322138ead265868e930dfd0eb7e468400db4e5b0ae1e52cf553e35`。
- 工具镜像：`os-com-recovery-tools:workspace-20261018`，镜像 ID 为 `sha256:6a1ae86b819913bdb9e39b0a8b47fc802ea73aed93e2f5d6bc7f9839d56d4254`。使用容器内 QEMU 10.2.1，没有使用宿主机 QEMU 8.2.2。
- 宿主机：Linux `6.8.0-147-generic`，x86-64；DAPUSTOR `DPRP5104T0TG03T8000`，固件 `FC003126`，调度器 `none`。
- 只直通了 `/dev/nvme1n1p1`：精确容量 274,877,906,944 字节，major:minor 为 `259:6`，父设备为 `nvme1n1`（`259:2`）。每个测试点开始前都将它重新格式化为 4 KiB nojournal EXT4。没有写入 p2、p3 或整个磁盘。格式化和 QEMU 直通前均重新检查目标身份、挂载、swap、holders、子设备以及宿主 root 视角下的打开者。
- 虚拟机：KVM、q35、64 vCPU、8 GiB 内存、OVMF；raw 后端使用 `cache=none` 和 `aio=threads`，virtqueue 深度为 1024。目标设备关闭请求合并、discard、write-zeroes、event index 和 indirect descriptors。
- rNVPCe：128 MiB NVM 池、64 MiB 元数据预留、64 MiB data limit；持久化模拟模式为 `fast`，启用 CPU 隔离和跟踪。ISO 没有设置 worker override：测试候选版本在 1 队列时选择 1 worker，在 4 队列时选择 4 worker。Native/WAL 挂载仍为单 worker。
- fio：buffered `psync`，每线程深度 1，使用 CPU 0-31，总数据量精确为 2 GiB，1 或 16 job，固定随机种子。随机写测试先在 native EXT4 下通过 direct 顺序写准备文件；被测阶段是 rNVPCe 对这些已有 SSD 后端文件的首次随机覆盖写。顺序写测试创建新文件。每个测试点均从新格式化的文件系统开始。
- 先串行运行单队列对照组，再串行运行四队列实验组。来宾系统中的 including-sync 区间从 fio 开始持续到最终 sync 完成、卸载之前；该区间包含 profiling 和串口输出开销。

原始证据位于 Git 仓库之外：`/home/zn/asterinas-build-env/rtio-virtio-multiqueue-20261102/`。每个 `q{1,4}-{randwrite-4k,write-128k}-j{1,16}` 目录都包含 ISO 与输入哈希、QEMU 日志和状态、mkfs/e2fsck 日志、完整流水线快照、QMP 回复、IOThread TID、亲和性、sysfs 队列映射、探针源码和预检结果、原始跟踪、最终 hctx 计数、root helper 证据以及解析后的 `result.json`。

## 吞吐量

所有速率单位均为 MiB/s。“变化”比较计入 sync 后的四队列和单队列吞吐量。

| 工作负载 | job 数 | 1 队列 fio | 4 队列 fio | 1 队列（含 sync） | 4 队列（含 sync） | 变化 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| randwrite-4k | 1 | 204.15 | 267.96 | 189.28 | 246.45 | +30.2% |
| randwrite-4k | 16 | 199.12 | 287.64 | 185.51 | 253.47 | +36.6% |
| write-128k | 1 | 2409.41 | 1030.70 | 1312.82 | 696.60 | -46.9% |
| write-128k | 16 | 801.88 | 400.78 | 607.72 | 350.68 | -42.3% |

## 延迟与流水线计数器

平均延迟是 fio 总写延迟，p99 是 fio completion latency，单位均为 us。提交时间和容量等待时间是所有调用者的累计秒数，不是墙钟时间。最终快照中 epoch 数与 FLUSH 数相等。

| 测试点 | 平均延迟 us | p99 us | `nv_data_submit` s | 容量等待 s | epoch / FLUSH | 峰值在途页数 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| q1-randwrite-4k-j1 | 18.83 | 25.73 | 9.882 | 3.492 | 512 / 512 | 2048 |
| q1-randwrite-4k-j16 | 310.09 | 8978.43 | 9.961 | 73.037 | 512 / 512 | 2048 |
| q1-write-128k-j1 | 51.57 | 134.14 | 0.655 | 0.037 | 512 / 512 | 2048 |
| q1-write-128k-j16 | 2473.16 | 8454.14 | 1.021 | 2.186 | 507 / 507 | 6144 |
| q4-randwrite-4k-j1 | 14.23 | 28.29 | 14.391 | 0.854 | 508 / 508 | 6144 |
| q4-randwrite-4k-j16 | 213.55 | 514.05 | 15.591 | 13.653 | 561 / 561 | 16850 |
| q4-write-128k-j1 | 120.92 | 329.73 | 3.330 | 0.639 | 227 / 227 | 16384 |
| q4-write-128k-j16 | 4916.78 | 39583.74 | 8.741 | 30.742 | 221 / 221 | 16384 |

此前四 worker/单队列诊断中，16 job 顺序写的提交时间为 4.74 s，容量等待为 18.16 s；本实验相应结果为 8.74 s 和 30.74 s，因此该下降门槛同样未通过。历史实验使用不同的内核路径，只作为背景证据；上表中相同镜像的单队列对比已经独立证明性能回退。四队列顺序写产生了更多 ready batch：单 job 为 2,489，16 job 为 1,437，而两个对照点均为 512；同时，它们将更多批次合并到较少的 FLUSH（分别为 227 和 221）。迁移更碎片化和累计提交延迟仍需继续调查；本次测量不能据此确定某一把锁就是原因。

## 队列均衡与排空状态

表中以斜杠分隔的值依次对应队列 0/1/2/3；对照组只有队列 0。worker BIO 计数不包含最终静止阶段由队列 0 执行的 sync 排空。驱动计数包含 native 准备、元数据和 sync I/O。深度为采样到的最大 staging depth；descriptor wait ticks 是驱动读取的原始 TSC tick，不是纳秒。最终每个队列的 staging depth 和在途请求数都为 0。每个队列都报告一个活动转发线程，转发线程峰值也为 1。

| 测试点 | worker 提交 BIO | worker 提交页数 | 最大 staging depth | descriptor 等待次数 | descriptor 等待 tick | IRQ completion 数 |
| --- | --- | --- | --- | --- | --- | --- |
| q1-randwrite-4k-j1 | 517120 | 517120 | 107 | 0 | 0 | 572525 |
| q1-randwrite-4k-j16 | 517120 | 517120 | 184 | 37 | 11713220 | 575257 |
| q1-write-128k-j1 | 16163 | 517120 | 10 | 0 | 0 | 21495 |
| q1-write-128k-j16 | 16163 | 517120 | 5 | 0 | 0 | 21737 |
| q4-randwrite-4k-j1 | 131072/123904/131072/134144 | 131072/123904/131072/134144 | 195/126/128/212 | 2/18/26/4 | 8567324/5095106/6467220/11769154 | 135603/123952/135342/177763 |
| q4-randwrite-4k-j16 | 122084/127832/134225/133043 | 122084/127832/134225/133043 | 956/1540/1576/1597 | 541/181/330/273 | 630721508/634252876/614614596/204552138 | 140676/141501/147947/144754 |
| q4-write-128k-j1 | 4093/3895/3787/4179 | 130944/124608/121184/133696 | 11/8/3/9 | 0/0/0/0 | 0/0/0/0 | 4962/3912/5967/6369 |
| q4-write-128k-j16 | 3479/4433/4122/4302 | 111328/141632/131904/137664 | 21/11/17/9 | 0/0/0/0 | 0/0/0/0 | 5966/4447/6354/4676 |

所有四 worker 日志都包含 worker 到队列映射 `0->0, 1->1, 2->2, 3->3`，且每个 worker 都提交了非零的数据 BIO 和页。完整的排队/提交 BIO 与页计数保留在各自的 `result.json` 中。共享协调器的最终快照显示活动 worker 数为 0；四队列实验组的 worker 峰值为 4。Task 5 的独立跨队列故障测试已经验证失败语义；本次成功 I/O 矩阵补充了物理设备证据，但没有重复故障注入，也不宣称重新执行了完整内核回归测试。

## 宿主机硬件队列的权威证据

QMP 将 virtqueue `i` 只映射到 `iothread<i>`。选定的宿主 CPU/hctx 组合为 `(1,0)`、`(3,1)`、`(5,2)`、`(7,3)`；对照组只使用 `(1,0)`。工具记录了 QMP 返回的宿主 TID，在恢复虚拟机运行前设置亲和性，并保存 `taskset` 输出。类型化 `kprobe:nvme_queue_rq` 探针过滤父 request queue，统计 `blk_mq_hw_ctx.queue_num`。每次跟踪都观测到就绪标记，探针一直存活到来宾退出，然后经 SIGINT 正常退出并产生完整的最终 map 边界。所有 QEMU 都通过来宾 debug-exit 以状态 33 退出。

| 测试点 | hctx 0/1/2/3 请求数 | 其他 hctx 请求 | 活动 hctx 数 | 最大占比 | 四 hctx 门槛 |
| --- | --- | --- | ---: | ---: | --- |
| q1-randwrite-4k-j1 | 572026/0/0/0 | 48:1, 105:1 | 3 | 100.00% | 对照组，不适用 |
| q1-randwrite-4k-j16 | 576520/0/0/0 | 14:1 | 2 | 100.00% | 对照组，不适用 |
| q1-write-128k-j1 | 20980/0/0/0 | 6:1, 90:1 | 3 | 99.99% | 对照组，不适用 |
| q1-write-128k-j16 | 21218/0/0/0 | 15:1, 87:1 | 3 | 99.99% | 对照组，不适用 |
| q4-randwrite-4k-j1 | 135108/123952/135342/177763 | 86:1, 127:1 | 6 | 31.07% | 通过 |
| q4-randwrite-4k-j16 | 140648/141832/148329/145122 | 98:1, 124:1 | 6 | 25.75% | 通过 |
| q4-write-128k-j1 | 4732/3912/5967/6369 | 32:1, 122:1 | 6 | 30.35% | 通过 |
| q4-write-128k-j16 | 5734/4448/6354/4676 | 47:1, 48:1 | 6 | 29.95% | 通过 |

其他 hctx 上的一两个请求与记录的只读就绪探测吻合。跟踪数据不区分请求阶段，覆盖整个来宾运行过程，包括随机文件准备、元数据操作、被测 fio 和 sync，并非仅统计 fio。在每个四队列测试点中，hctx 0-3 都独立承载了数千个请求，因此就绪探测不会造成观测到的四队列结果。70% 门槛按设计应用于所有被跟踪请求。

## 测试工具修复与验证

首次预检在格式化前停止，因为宿主 psmisc 的 `fuser -s -- PATH` 会输出用法并返回状态 1。证据保存在 `failed-fuser/` 下。调用真实 CLI 检查已打开临时文件的回归测试先失败；从规范化绝对路径参数中删除 `--` 后通过。root helper 对状态、输出和身份的检查仍然严格；初步宿主检查也会拒绝状态为 1 且带诊断输出的情况，该行为由先失败后通过的测试覆盖。

首个成功对照点的 fio JSON 键名被一条 CPU 隔离串口日志从中间打断。该工作负载和 e2fsck 已成功，无需重跑。新增解析器回归测试并确认失败后，只过滤已知 CPU 隔离消息和 ANSI 颜色码，同时保留字节级行尾。现在 8 份原始日志均可解析，并强制要求数据量精确为 2 GiB、fio error 为 0、sync 为 0、unmount 为 0、e2fsck 为 0。新增反向测试会拒绝短写、fio/sync 失败、空闲或错误路由的 worker、未完成请求、额外 FLUSH 和未通过的 hctx 门槛。

工具新增并测试了 1/4 队列选择、匹配的 IOThread 对象、重复目标检查、显式格式化、900 秒虚拟机时限和串行矩阵封装。只有机制门槛失败时返回状态 3；正确性失败或证据不完整会停止矩阵。修改后，`--self-test`、Shell 语法检查、Python 编译和 `git diff --check` 均通过。Git 没有跟踪原始镜像、JSON、日志或跟踪数据。

Task 6 遗留的已知限制：QMP 时限覆盖连接过程，但没有覆盖所有 greeting/reply 读取；工具记录了镜像 digest，但 Docker 执行仍使用可变 tag。本次每个测试点记录的镜像 ID 均一致。

## 历史测试镜像复现方法

在工作树中，使用上述容器和已记录的 profiling 修改进行构建，然后复制并计算生成 ISO 的指纹，最后恢复源码标志：

```sh
cargo osdk check --release --package aster-kernel
cargo osdk build --release --init-args='/ext2/rebuild-fio.sh' \
  --kcmd-args='console=ttyS0 ostd.log_level=error rnvpc=off rnvpc_extend=off rtio=off rnvpc_size=128M rnvpc_meta_reserve=64M rnvpc_data_limit=64M rnvpc_sim_mode=fast rtio_cpu_isolation=on rtio_cpu_trace=on'
```

准备一个由调用者管理的外部证据目录，并将已固化的 ISO、`ext2.img` 和 `exfat.img` 放在 `SOURCE_ART` 下：

```sh
ROOT_ART=/path/to/new-matrix-root \
SOURCE_ART=/path/to/source \
TOOLS_IMAGE=os-com-recovery-tools:workspace-20261018 \
bash docs/experiments/scripts/rtio-virtio-multiqueue-20261102/run-matrix.sh \
  --authorized-destructive-target=/dev/nvme1n1p1
```

已记录的实验使用 `MATRIX_QUEUES`/`MATRIX_SPECS`，依次运行首个对照点、其余三个对照点和全部四队列测试点，同时保留失败预检以及重新解析后的首个成功对照证据。当前及未来版本默认只有一个 worker；四 worker 实验必须显式配置。复现当前源码时，应在开始任何测量前分别准备并固化 worker-1 和 worker-4 镜像，在内核命令行中分别加入 `rtio_workers=1` 或 `rtio_workers=4`。随后使用对应的 `SOURCE_ART` 目录，分别以 `MATRIX_QUEUES=1` 和 `MATRIX_QUEUES=4` 运行封装脚本。更多说明见[测试工具 README](../scripts/rtio-virtio-multiqueue-20261102/README.md)。

## 实验后审查修复

当前运行时默认值固定为一个 worker，不再根据 CPU 数量或提交队列数量自动增加。仍支持显式 `rtio_workers=N`，上限为 16，且不能超过设备声明的队列数；native/WAL 行为仍固定为一个 worker。策略 ktest 已更新，并通过宿主 Rust 适配器执行其中的纯函数；kernel 和 ktest 包检查也验证了集成代码可以编译。该修复没有重新执行 QEMU/真实设备实验。

独立运行器现在按验证后的目标 major:minor 获取公共锁，不再依赖证据目录。矩阵要求运行器在同一锁生命周期内执行只读 e2fsck，因此格式化、QEMU、运行后检查和清理都受同一把锁保护，同时不会产生嵌套加锁。非破坏性进程测试证明：不同证据目录不能同时使用同一设备，不同的已验证设备使用不同锁，而且模拟 e2fsck 执行时竞争者也会被排除。已有 8 个测试点的证据不受该修复影响。
