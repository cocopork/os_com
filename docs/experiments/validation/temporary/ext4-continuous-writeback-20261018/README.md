# EXT4连续回写与低水位（2026-10-18）

## 行为

用户要求取消忙碌轮间的固定等待，持续写回直到cache降至低水位。基于提交dfec99b6f，修改范围仅kernel/src/fs/fs_impls/ext4/writeback.rs。

- 启动条件保留：脏页＋在途回写达到64 MiB、resident超过384 MiB或存在前台背压等待者。
- 进入回写后，持续到脏页压力不高于32 MiB、resident不高于384 MiB、没有前台等待者；不是清空全部脏页。默认高水位仍为resident512 MiB、dirty128 MiB。
- 每轮仍至多回写1024页/4 MiB、回收4096页/16 MiB，并轮转inode。每轮释放所有文件系统和inode锁后yield，让调度和RCU可前进。
- **仍需drain且本轮有写回或回收进展时，立即继续；不创建或等待周期timer。**
- 高于低水位但无进展（例如外部引用/RCU延迟释放）时保留最多10 ms退避，以容量预留释放等progress epoch变化或压力消退为唤醒条件；避免空转及快速累计100次stalled错误。前台pressure等待及原有10 ms超时不在本次修改范围。
- 无需drain时以既有启动阈值等待通知，保留1秒fallback，避免低阈值反复启停；shutdown及I/O失败锁存行为保留。

无进展时才退避；这不意味着没有锁、I/O和调度等待。inode写锁仍覆盖flush和I/O等待，未改变ordered journal并发契约。yield不是强制context switch；实际RCU进展还依赖调度切换、I/O等待、既有idle报告，若退休帧尚未释放则进入无进展退避。

## 验证

- RED：新增测试在旧10 ms策略下实际失败：left10ms vs right0ns。见red.log。
- 首次聚焦命令使用部分名称过滤，0 passed/301 filtered；该次明确不算验证通过。
- GREEN：完整kernel ktest **301 passed、0 failed、0 filtered out**，包括2项新增测试（有进展无timer、无进展退避/空闲等待；触发线以下继续drain、只读清洁回收、无进展返回及回读）及原有全部kernel测试。
- cargo fmt --all --check、git diff --check通过。
- 无探针release镜像，在真实授权分区/dev/nvme1n1p1逐项重新格式化为journal EXT4，显式delalloc，关闭rNVPC/rNVPCe/rTIO；Docker/QEMU/KVM16vCPU、8GiB、virtio-blk单队列，cache=none、aio=threads。fio buffered/psync/iodepth1，1/16任务，每项16GiB，随机写先填满并sync。连续回写四项、旧10 ms随机写对照两项，共六项完整写入并sync、卸载成功，六次e2fsck -fn退出0。没有fio数据校验、掉电测试或长期运行验证。

## 实测带宽

| 工作负载 | 任务 | fio MiB/s | 含最终sync MiB/s | 宿主分区写入MiB/s | sync秒 |
|---|---:|---:|---:|---:|---:|
| 128KB顺序写 | 1 | 463.1 | 456.6 | 487.7 | 0.17 |
| 128KB顺序写 | 16 | 774.9 | 751.2 | 772.8 | 0.08 |
| 4KB随机覆盖写 | 1 | 180.8 | 180.0 | 178.2 | 0.13 |
| 4KB随机覆盖写 | 16 | 285.1 | 282.1 | 292.9 | 0.23 |

### 同时段旧10 ms策略对照（随机覆盖写）

| 任务 | 旧10 ms含sync MiB/s | 连续回写含sync MiB/s | 单次样本变化 |
|---|---:|---:|---:|
| 1 | 120.4 | 180.0 | +49.5% |
| 16 | 146.9 | 282.1 | +92.0% |

以上对照按时间先后单次执行，包含低水位策略的共同变更；只能描述本轮观测，不能单独归因于删除10 ms等待。顺序写未补跑同时段旧策略对照。

### 随机写准备阶段（逐文件顺序填充，非随机覆盖阶段）

| 配置 | 文件数 | 填充fio总耗时秒 | 平均填充MiB/s | 单文件最小/最大MiB/s |
|---|---:|---:|---:|---:|
| continuous-rand4k-j1-r1 | 1 | 32.98 | 496.8 | 496.8 / 496.8 |
| continuous-rand4k-j16-r1 | 16 | 203.52 | 80.5 | 34.7 / 323.7 |
| control10ms-rand4k-j1-r1 | 1 | 68.41 | 239.5 | 239.5 / 239.5 |
| control10ms-rand4k-j16-r1 | 16 | 66.82 | 245.2 | 168.7 / 318.4 |

16文件逐个填充阶段，连续回写仅80.5 MiB/s，旧策略245.2 MiB/s，本轮出现明显回退。这一阶段未计入随机覆盖带宽，不能被随机写提升掩盖。全量inode扫描、前后台inode锁竞争及调度行为是后续排查候选，当前没有动态证据确定原因。

16GiB远大于缓存预算，六项没有触发ENOMEM或回写EIO，结合admission/回收测试说明本轮持续写入可工作；没有额外采样缓存峰值，不能声称实测到了整个运行窗口的精确resident峰值或停写后的低水位收敛时间。低水位逻辑通过内核测试验证。

每配置一个样本。此前10 ms无探针样本的含sync带宽分别为顺序1/16任务285.1/282.7 MiB/s、随机1/16任务238.6/213.5 MiB/s。跨时段差异包括运行噪声。另补跑本时段10 ms随机写对照，见附表；尚非交错多轮A/B。此前1 ms诊断版16任务顺序1026.6 MiB/s、随机418.2 MiB/s，同时带探针且没有本次脏页低水位策略；因此当前无固定等待版本不保证优于那次1 ms实验。

本次证明了“有进展时不必固定等待”能实际运行，并记录当前吞吐；并未证明零固定等待是最佳策略。下一步若继续调优，需在同一时段、同一低水位策略下交错重复yield/短等待实验，并观测前台inode锁、CPU调度及无进展退避频率。

## 产物与复现

证据目录：`/home/zn/asterinas-build-env/ext4-continuous-wb-20261018/`，含red.log、full.log、格式检查、source.patch/源码SHA256、build.log、continuous.iso（新策略）、baseline-10ms.iso（旧策略）、guest及host脚本、fio日志、宿主diskstats、e2fsck日志及results.csv。run-suite.py会清空专用测试分区；复用时须确认设备身份及授权适用。

ISO SHA256：`8f3928996df986d18eb8a1a4eb3883ba610f83bf16ee3ff29125b80e770b5639`。

未提交或推送本次修改；此前延迟分解报告及产物保持保留。
