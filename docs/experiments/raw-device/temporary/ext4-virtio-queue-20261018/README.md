# 128 KiB顺序写、16线程的virtio队列占用（2026-10-18）

## 结论

两次带探针测量均观测到：descriptor峰值64/64，但请求在途峰值只有1；队列为空占测量窗口55.5%–61.4%。因此descriptor资源阶段性耗尽与请求队列未形成并发同时存在，不能把“64个descriptor满”解释为“64个IO请求在途”。

目前每个设备共享一个virtqueue、一个提交线程，不是每个fio线程各有一个virtqueue。当前16个fio线程不能使此工作负载形成多个大IO在途；这既有direct descriptor布局导致的容量限制，也有上游提供请求的间隙。不能仅凭空队列就量化某个全局锁或断言物理NVMe性能上限。

## 条件与窗口

- /dev/nvme1n1p1专用授权分区，逐配置重新格式化为journal EXT4、显式delalloc；rNVPC/rNVPCe/rTIO关闭。
- Docker/QEMU/KVM，16 vCPU、8 GiB；用户/fio CPU0–7、writeback12、virtio14、journal15。缓存与脏页上限64 MiB、低水位32 MiB。
- fio buffered psync、direct=0、fsync=0、iodepth=1、16线程，每线程128 MiB，合计精确2 GiB；128 KiB顺序写新文件。
- 目标virtio-blk单队列，queue-size=64，indirect_desc=off、cache=none、aio=threads。驱动本身QUEUE_SIZE也为64。
- 一次无探针对照、两次带探针测试。临时探针在每次提交/IRQ回收处累计状态保持时间，逐状态求和，非瞬时快照/定时采样。
- fio前sync输出并重置统计，fio后最终sync输出本轮统计。测量窗口包含程序启动、文件创建、数据写入、metadata及最终持久化，不是只截取steady-state。统计重置/快照与shell时间点存在数十毫秒偏移。
- stats描述符及请求从发布至驱动IRQ回收计数，包含设备已完成但驱动尚未回收的请求。不是直接测量物理NVMe submission queue，也没有分离完成中断延迟。

## 时间加权结果

| 指标 | 带探针第1次 | 带探针第2次 |
|---|---:|---:|
| 窗口秒 | 3.485 | 2.829 |
| 队列为空时间比例 | 61.41% | 55.48% |
| 平均在途请求 | 0.386 | 0.445 |
| 峰值在途请求 | 1 | 1 |
| 平均已分配descriptor | 23.42 | 27.03 |
| 峰值descriptor | 64 | 64 |
| 63或64 descriptor占用时间比例 | 35.41% | 40.66% |
| 恰好64 descriptor占用时间比例 | 0.095% | 0.130% |
| 因descriptor不足自旋等待秒 | 1.156 | 1.098 |
| 因descriptor不足等待占窗口比例 | 33.17% | 38.83% |
| 发生descriptor等待的请求数 | 7823 | 8014 |
| 重试循环次数 | 50329335 | 53677599 |
| 提交线程空软件队列取请求等待秒 | 2.241 | 1.638 |
| 取请求时软件积压峰值（含已取出的1个） | 21 | 23 |
| 提交线程准备请求/DMA同步累计秒 | 0.0176 | 0.0210 |
| 写请求数（含metadata） | 9104 | 9072 |
| 写请求平均KiB | 230.4 | 231.2 |
| 61段写请求占写请求数比例 | 88.18% | 88.34% |
| 61段写请求占写字节比例 | 93.40% | 93.23% |
| 含最终sync带宽MiB/s | 585.1 | 718.6 |

这些比例不是可以相加的CPU归因：descriptor等待与请求在途时间重叠；软件队列等待也可能与已提交请求的在途时间重叠。软件积压长度来自dequeue事件，非时间平均；峰值含刚取出的请求，因此21/23对应至少20/22个剩余待提交请求的观测。

无探针含sync带宽588.5 MiB/s，两次探针585.1/718.6 MiB/s。短测试存在明显波动；三个按时间先后执行的样本不能精确量化探针代价或声称吞吐提升。小于5秒，原宿主diskstats分析器的预热过滤没有足够样本，本报告不据此报告宿主设备利用率。

## “占位符”实际含义

| 层次 | 128 KiB（32页）请求 | 244 KiB（61页）请求 |
|---|---:|---:|
| avail ring发布的chain head | 1项 | 1项 |
| 数据descriptor（忽略头尾） | 32项 | 61项 |
| 计入请求头/响应尾的主descriptor表占用 | 34项 | 63项 |

因此用户期望的“每请求只占1个主表descriptor”当前没有实现。avail ring虽然每请求只有一项，但指向一条消耗多个主表descriptor的链。两个128 KiB请求需要68项，超过64；244 KiB请求只剩1项，连一个最小头+数据+尾请求都放不下。软件队列有时合并为62段，恰好占满64项；本轮占比很少（约0.095%/0.130%窗口）。

应用write(128 KiB)不等于设备请求128 KiB。delalloc/writeback收集相邻页，当前MAX_RUN_SEGMENTS=61，约88%的写请求、93%的写字节已经是61段/244 KiB。这证明并非完全没有批量合并；限制来自主descriptor表消耗和pipeline间隙，而不是“每个4 KiB页都成为独立IO请求”。

要实现主表每请求1项，需要实现indirect descriptor（各段仍在间接表中描述）或其他DMA连续段聚合。当前add_dma_bufs始终逐输入/输出分配主表descriptor；仅修改QEMU的indirect_desc参数不会自动改变这条驱动提交路径。本次没有修改队列容量、descriptor格式或提交策略。

## 软件侧推断及尚未定位部分

直接观测：55%–61%窗口没有任何已提交且未回收请求，提交线程有1.64–2.24秒在空软件队列取请求路径等待；另有1.10–1.16秒因descriptor不足自旋，超过5000万次重试。说明请求供给不连续与容量阻塞交替发生。

代码事实：writeback_cache_pages在inode.inner写锁内完成delalloc和flush_some_pages；flush_some_dirty_pages提交一批后wait_all；ExtentManager对每个4 KiB页逐个锁页、申请DMA段、复制快照，再将run作为一个BIO提交。以上均是后续拆分供给空窗的候选。探针prepare_ns仅覆盖virtio驱动内部请求头/响应、向量、DMA同步等准备阶段，不覆盖前述EXT4页复制和分配，不能把17–21毫秒误认为整个软件栈开销。

本轮未进一步归因到特定锁、分配器、journal等待或调度，也未测量后端设备已完成但IRQ尚未回收的独立时长。

## 源码证据

- kernel/comps/virtio/src/device/block/device.rs:210：QUEUE_SIZE=64；244：仅创建queue0。
- kernel/comps/virtio/src/device/block/device.rs:441：每个BioSegment生成一个DMA slice，455计算总descriptor，462不足时自旋。
- kernel/comps/virtio/src/queue.rs:289：逐输入分配descriptor；337按输入输出数增加已用数量；347/353每个chain仅发布一个head。
- kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs:1146：MAX_RUN_SEGMENTS=61；1414：逐页构建DMA段并复制；1434：run作为一个BIO提交。
- kernel/src/fs/fs_impls/ext4/inode/mod.rs:3759：writeback持有inode.inner写锁；kernel/src/vm/page_cache/vmo/mod.rs:826：批次完成等待。

## 验证与产物

三组fio完整写入2147483648字节，error=0；sync、卸载完成，QEMU退出33；三次e2fsck -fn均退出0。未进行fio内容校验或掉电测试。

探针统计校验：窗口初始/最终请求数与descriptor占用均0；每个时间直方图之和精确等于窗口时长；提交数=完成数=请求segment直方图计数=dequeue样本数，写请求计数与写segment直方图相等。

临时探针已移除，四个被修改文件逐字节恢复，新探针模块删除；恢复后维护源码SHA256与CPU隔离任务保存的manifest一致。此次未重跑此前302项内核测试；维护源码不变。恢复后的正常release镜像重新构建，后续快速测试继续使用无探针镜像。

证据目录：`/home/zn/asterinas-build-env/ext4-virtio-queue-20261018/`，包括原始fio/VQPROFILE日志、diskstats、e2fsck日志、bandwidth.csv、queue-summary.csv、queue-analysis.json、diagnostic.patch、before/、instrumented-source/、profiled.iso、unprofiled.iso、构建日志、restoration-verification.json。探针和CPU入口日志可能插入串口fio JSON，解析器仅删除完整已知日志标记后重建JSON，原始日志不改。
