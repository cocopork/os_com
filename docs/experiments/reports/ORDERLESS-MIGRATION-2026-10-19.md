# rNVPCe Orderless Migration 实现与测试

日期：2026-10-19。

## 结论

012 的核心批次协议已经成为 rNVPCe rTIO 的唯一迁移路径：一个迁移轮次先提交所有 data BIO，再写该轮触及的 EXT4 分配 metadata；两类请求完成后只执行一次 SSD flush，成功后才发布 NVM SSD 映射、归还额度和回收 NVPage。prepare 阶段不执行 SSD flush，旧的两屏障 ordered migration 路径及其挂载开关已经删除；rNVPC WAL 路径保持原有顺序。

真实 NVMe 三轮 A/B 证明收益具有负载选择性。128KiB 顺序写16线程含 drain 中位数由94.95提升至106.39 MiB/s（+12.1%），fio阶段由118.33提升至140.66 MiB/s（+18.9%）；单线程含 drain基本持平（-0.7%）。4KiB随机写1线程含 drain提升2.7%，16线程回退11.2%。因此不能声称012带来普遍带宽提升。

## 实现

- `migration/batch.rs` 保存DRAM批次计数、metadata状态和唯一入队门控。BIO回调只保存结果、递减计数和唤醒，不执行持久化或回收。
- `migration.rs` 先批量分配SSD目标并持久写入NVM描述符，再提交合并后的data BIO；data在途时写受影响块组的bitmap/GDT及superblock。成功批次由completion工作者执行一次设备flush，再批量发布NVM extent、归还额度和回收页。
- data提交、metadata写入或flush失败时不发布SSD权威引用，不归还额度，不回收NVPage。已经持久命名的target保留并在重试中复用。
- `frame.rs` 暴露pending target扫描；挂载rNVPCe时，`fs.rs`在启动分配与rTIO前将SSD bitmap中仍为空闲的target标为占用，从修复后的bitmap重算GDT/superblock计数并执行一次flush。落入EXT4系统元数据区的target会以`EUCLEAN`拒绝挂载。
- rNVPCe仍拒绝未恢复的journal；native EXT4和rNVPC WAL路径没有切换到新批次。

## 正确性验证

定向 `ext4_research_routes` 覆盖13个场景并通过，包括prepare零flush、成功批次单flush、data/metadata/flush失败、flush失败后保留NVM权威和target、重试复用目标、挂载修复未落盘bitmap及从bitmap重算计数。独立批次状态测试覆盖data/metadata不同完成顺序和唯一入队。

最终完整 kernel ktest 为308 passed、0 failed、0 filtered；删除开关后的 rNVPCe 默认配置 release 镜像构建通过；`cargo fmt --all -- --check`和`git diff --check`通过。测试使用Docker内OSDK/QEMU/KVM。真实盘24个release点均满足：完整写入2GiB、fio error=0、sync和umount成功、QEMU exit=33、e2fsck exit=0。

## 独立诊断：当前 rTIO 调用次数

使用开启 `batch_metrics` 的独立 release 镜像，每点运行一次；工作负载、设备、CPU隔离和容量参数与性能实验相同。每点迁移524,288页。表中的“批次”取完成阶段 `data_barrier` 调用数；分配、叶发布、额度提交和回收是对应函数的计时调用数。16线程下额度提交和回收按批次内inode分组执行，不能解释为SSD flush次数。随机写使用预分配文件，因此没有新SSD块分配和额度提交页；空目标集合仍会产生一次很短的目标发布计时调用。

| 负载 | 线程 | 批次 | 平均页/批 | 分配调用/页 | 目标发布调用/页 | 叶发布调用 | 额度提交调用/页 | 回收调用/页 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 4,096 | 128.0 | 0 / 0 | 4,098 / 0 | 455,895 | 4,096 / 0 | 4,096 / 524,288 |
| 4KiB随机写 | 16 | 5,189 | 101.0 | 0 / 0 | 5,191 / 0 | 473,249 | 66,227 / 0 | 66,227 / 524,288 |
| 128KiB顺序写 | 1 | 4,096 | 128.0 | 4,096 / 524,288 | 4,098 / 524,288 | 12,622 | 4,096 / 524,288 | 4,096 / 524,288 |
| 128KiB顺序写 | 16 | 4,096 | 128.0 | 16,436 / 524,288 | 4,098 / 524,288 | 24,739 | 16,436 / 524,288 | 16,436 / 524,288 |

顺序写单线程每次分配正好覆盖128页；16线程因批次内inode边界拆分，平均31.9页/分配调用。010对应的顺序写分配调用为10,632/26,657次，当前分别减少61.5%和38.3%。随机写16线程平均每批101页，说明并发下批次没有总是填满128页窗口。

NVM后端事件仍包含前台、元数据和迁出阶段，不全部属于rTIO工作者；表内为“总次数 / 每迁移页次数”。

| 负载 | 线程 | data persist | control persist | fence |
| --- | ---: | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 4,655,244 / 8.879 | 4,725,239 / 9.013 | 7,285,009 / 13.895 |
| 4KiB随机写 | 16 | 4,672,913 / 8.913 | 4,727,080 / 9.016 | 7,365,936 / 14.049 |
| 128KiB顺序写 | 1 | 2,764,830 / 5.273 | 6,911,811 / 13.183 | 5,467,882 / 10.429 |
| 128KiB顺序写 | 16 | 2,788,898 / 5.319 | 6,911,168 / 13.182 | 5,528,538 / 10.545 |

## 独立诊断：当前 rTIO 阶段时间

单位为所有调用累计秒数，括号内是相对010诊断镜像的变化。不同后台阶段可能重叠，不能相加为墙钟时间。`data barrier` 在012中只测完成工作者的一次批次SSD flush；010还包含分配metadata持久化路径，因此二者变化反映orderless协议调整。`allocation barrier` 当前没有调用。

| 负载 | 线程 | 目标分配 | 分配持久化屏障 | 目标描述符发布 | 数据完成屏障 | 叶发布 | 额度提交 | NV回收 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 0 | 0 | 0.001 (-68.3%) | 0.561 (-87.4%) | 2.045 (-16.9%) | 0.0004 (-65.9%) | 0.218 (-27.5%) |
| 4KiB随机写 | 16 | 0 | 0 | 0.075 (-26.0%) | 0.964 (-76.6%) | 4.552 (+12.5%) | 0.012 (-80.5%) | 0.338 (-55.0%) |
| 128KiB顺序写 | 1 | 0.021 (-12.2%) | 0 (-100%) | 1.220 (+128.8%) | 0.713 (-92.7%) | 0.174 (-8.2%) | 0.779 (+43.9%) | 0.566 (+47.8%) |
| 128KiB顺序写 | 16 | 0.019 (-74.4%) | 0 (-100%) | 1.532 (-11.7%) | 0.541 (-91.9%) | 0.333 (-39.2%) | 1.054 (-36.3%) | 0.774 (-27.2%) |

每批SSD flush平均耗时为：随机写1/16线程137.1/185.8us，顺序写1/16线程174.1/132.1us。按页摊销的数据屏障为1.071/1.839/1.360/1.032us。012已经消除了010中顺序写约10.6秒的分配持久化屏障，并把数据完成屏障从9.70/6.64秒降至0.71/0.54秒。当前顺序写较大的控制面阶段转为目标描述符发布、额度提交和回收；随机写16线程的叶发布累计4.552秒、平均每次9.62us，是所测软件阶段中最大的残留项。

诊断镜像自身带宽为：4KiB随机写1/16线程124.65/79.81 MiB/s，128KiB顺序写1/16线程138.88/123.70 MiB/s；含drain分别118.93/76.99/105.73/97.02 MiB/s。该镜像启用了高频原子计数，只用于阶段定位，不替代下文关闭诊断后的三轮release结果。

## 性能结果

设备为`/dev/nvme1n1p1`，每点重新格式化为4KiB、nojournal EXT4。VM为64 vCPU、8GiB，virtio-blk单队列深度1024；fio为buffered psync、fsync=0。rNVPCe池128MiB、metadata reserve 64MiB、data limit 64MiB，并启用历史CPU隔离。测试镜像中的rNVPCe rTIO热路径与当前默认路径相同。每个版本每点三轮，奇偶轮交换运行顺序。

| 负载 | 线程 | 010 fio MiB/s | 012 fio MiB/s | 变化 | 010 含drain | 012 含drain | 变化 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 123.63 | 129.01 | +4.4% | 119.14 | 122.41 | +2.7% |
| 4KiB随机写 | 16 | 103.07 | 91.29 | -11.4% | 99.08 | 88.01 | -11.2% |
| 128KiB顺序写 | 1 | 165.55 | 164.06 | -0.9% | 123.37 | 122.49 | -0.7% |
| 128KiB顺序写 | 16 | 118.33 | 140.66 | +18.9% | 94.95 | 106.39 | +12.1% |

三轮含drain范围分别为：随机1线程010 113.71–123.60、012 116.17–127.36；随机16线程010 96.33–106.89、012 79.13–92.80；顺序1线程010 122.71–123.52、012 119.91–126.50；顺序16线程010 86.12–95.12、012 104.70–110.23 MiB/s。样本数只有三轮，不据此声称统计显著性。

## 失败候选与边界

实现过程中测试了在每个migration batch内批量安装native EXT4 extent的候选。虽然每个inode只获取一次锁，但它会为后续仍可能覆盖的页反复写中间映射，首轮随机写1/16线程含drain仅87.45/39.48 MiB/s。该候选已撤回，日志保留在外部实验目录的`throughput-native-extents-regression`。

当前metadata并行范围只有本批次新分配目标影响的bitmap、GDT和superblock。native extent/inode仍在显式sync/unmount时从最终NVM权威状态导出；尚未实现块级不可变metadata快照、同块跨批次版本排序、完整注册inode恢复基线和SSD孤儿回收。因此本轮测试验证了核心单flush门控与pending-target保护，没有证明README中的完整崩溃矩阵、torn native metadata恢复或真实掉电。

16线程随机写回退的直接测量事实是总带宽下降。新增独立诊断表明其每批平均只有101页，数据屏障按页耗时为四点最高值1.839us，叶发布累计4.552秒且较010增加12.5%；这些是明确的相关瓶颈位置。由于诊断计数本身会扰动并发执行，仍不能仅凭这一轮把release回退精确归因到单个函数，需要单变量A/B确认因果。

## 证据

- 复现脚本：`docs/experiments/scripts/orderless-migration-20261019/`；原始数据：`/home/zn/asterinas-build-env/orderless-migration-20261019/`。
- 当前默认orderless路径的独立诊断：[聚合JSON](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/experiment-runs/orderless-migration-20261019/profile-default.json)、[原始日志与状态](/home/zn/asterinas-build-env/orderless-migration-20261019/profile-default/)、[解析器](../scripts/orderless-migration-20261019/aggregate-profile.py)。
- 完整原始日志、镜像和编排：`/home/zn/asterinas-build-env/orderless-migration-20261019/`。
- 诊断ISO SHA-256：`a76af01bf8674a5aecbdae983c06413a65cac01e3cb3a572ce34ece02b3b2bc4`；冻结kernel diff SHA-256：`2ebcf4e05c8a224adc0c7d0a7d8275db30eed8a2150f8bacbc0e0e893754311a`。
- 010 ISO SHA-256：`4682e722dc92d17a1ed6b9597e1393d5dc1801665432002a473f814559769609`。
- 012 ISO SHA-256：`f2f858e436bf4190a0a44a3160729cd35f4df3135879f67ca0d01afda343ebb9`。
- 010冻结kernel diff SHA-256：`b3daf2f2cc094c5b20d8d81df813bfb56be07558ab2e778ceebbbb1a056f0df2`。
- 012冻结kernel diff SHA-256：`2dd06c7b570e7b9827de1a2c0017e61bbbad9c085c9edf48a40c6d9d4d359bca`。

没有执行提交或推送。
