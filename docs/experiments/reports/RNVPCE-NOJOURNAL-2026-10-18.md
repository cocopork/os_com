# rNVPCe nojournal 接入与验证（2026-10-18）

已拆分两项需求，并先接入rNVPCe的无JBD2运行路径及其nojournal native元数据导出适配。NVPage批量事务未实施。**本次不构成全部EXT4元数据掉电一致性的完成证明**，剩余项保留在009需求文档。

- [009 rNVPCe nojournal需求](../../os_com_implementation/os_com_refactor_plan/009-rnvpce-nojournal/README.md)
- [010 NVPage批量事务需求](../../os_com_implementation/os_com_refactor_plan/010-nvpage-batch-transactions/README.md)

## 实现

1. `fs.rs::open`在rNVPCe模式不创建Journal、不启动JBD2线程、不设置RECOVER，不生成journal handle。原生EXT4和rNVPC/WAL仍沿用原日志生命周期。磁盘原有HAS_JOURNAL保持不变，干净内部日志只校验、不修改。
2. RECOVER、待处理orphan或非零journal s_start使rNVPCe挂载返回EUCLEAN；前两类在构造Ext4前拒绝，后一类在启动后台线程和安装Journal前拒绝。测试断言整个磁盘镜像和flush计数均不变。待恢复日志应先经原生EXT4处理；无日志镜像上的异常orphan/标记需要文件系统修复，不能把拒绝挂载当作已恢复。
3. rTIO使用既有nojournal元数据同步分支。SSD分配持久化、数据完成后flush、NVM extent发布和NVPage回收顺序均保留，未删除设备持久化屏障。
4. 首轮真实盘暴露native导出逐页同步写inode/extent节点的开销。现按最多64个逻辑块合并节点after-image，节点写回成功后再写inode；失败节点after-image保留。此处不设置delalloc owner，因为数据块已分配；没有修改NVPage目标分配、描述符提交、额度标记或回收事务。

## 回归与错误路径

- 原挂载行为的RED：`rNVPCe must not start JBD2`。
- 导出计数RED：9页对应9次inode写；最终按一个有界区间处理，测试约束不超过2次。
- 最终全套kernel ktest，SMP=1：**302 passed / 0 failed**。新增场景整合在已有`ext4_research_routes`，没有新增独立ktest数量。
- 覆盖干净journal保留不变、无handle/RECOVER、5类脏状态拒绝、部分页与稀疏写、SSD原始块读回、外部leaf首次创建失败回滚、已有leaf after-image失败保留、inode表写失败及重试、既有迁出flush失败路径。
- 一个中间完整测试301/1：新测试误认为首次leaf创建失败后重试不应分配；实际首次失败回滚，重试应恰好分配一页。修正该断言，并另外测试已有leaf失败后的无额外分配重试，最终全套通过。该中间日志保留，未伪装为通过。
- 最终release构建、改动Rust文件rustfmt、Python编译、shell语法、git diff空白检查通过。独立审查未发现本次挂载/导出改动的正确性阻塞。

## 真实NVMe测试

设备`/dev/nvme1n1p1`，沿用用户已授权清空的256GiB分区，每点重新格式化。四个主点磁盘保留干净journal，但rNVPCe运行时不启用JBD2；dumpe2fs记录实际磁盘特性，内核测试直接验证无Journal和handle。

每点总量2GiB；psync、direct=0、fsync=0、iodepth=1，每线程独立文件。随机写预先由原生EXT4 direct写初始化并卸载，再挂载rNVPCe。64vCPU/8GiB，virtio单队列1024；NVPool128MiB，元数据预留64MiB，数据上限64MiB，fast模拟模式未关闭persist。fio在CPU0–31，virtio62，rTIO工作线程沿用Kernel角色32–59。

单位MiB/s。每点一次，非统计结论；含sync带宽不包含卸载时间。

| 工作负载 | 线程 | fio带宽 | 含最终sync带宽 |
| --- | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 137.9 | 131.4 |
| 4KiB随机写 | 16 | 111.5 | 105.6 |
| 128KiB顺序写 | 1 | 142.7 | 112.0 |
| 128KiB顺序写 | 16 | 102.5 | 83.0 |

四点均完整写满2GiB、fio error=0、sync和umount正常，后置e2fsck exit=0。另用**不含HAS_JOURNAL**的磁盘格式运行2GiB/128KiB顺序写/1线程，同样通过：fio137.0MiB/s，含sync110.3MiB/s。该额外点单独保存，避免与相同case名混淆。

在已经关闭JBD2、尚未适配导出的首轮顺序写中，fio122.7MiB/s、含sync9.55MiB/s；最终sync耗时197.26s。导出适配后的相同顺序写单点sync耗时3.64s。两点均成功完成；这是本轮直接观察，不作为稳定收益倍数。未重新跑原生EXT4全矩阵，不使用历史EXT4数字计算本轮提升比例。

## 尚未完成的保证

- 全部SSD位图、inode/目录/链接、创建/删除/截断与native导出的掉电一致性覆盖仍未完成。不能把去掉JBD2等同于已有完整soft-update替代协议；原本不支持的操作未启用。
- 本轮真实盘fio未做CRC或跨VM重启逐文件校验，e2fsck仅检查SSD文件系统结构。DRAM fast后端不模拟真实NVDIMM完整刷写延迟；fault/restart测试的持久镜像模型不能替代硬件掉电实验。
- 本轮完整内核回归为SMP=1；真实盘普通启动为64vCPU。没有据此宣称历史SMP=2 RCU回收问题已解决。
- NVPage逐页分配、通用commit_frame、额度标记和回收fence仍在010待实现范围。本轮未增大128页迁出在途上限。

## 证据与复现

[验证摘要](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/experiment-runs/rnvpce-nojournal-20261018/validation.json)、[最终主测试CSV](/home/zn/asterinas-build-env/rnvpce-nojournal-20261018/final-export/results.csv)、[最终kernel日志](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/experiment-runs/rnvpce-nojournal-20261018/development/kernel-final2-smp1.log)、[完整未提交kernel补丁](/home/zn/asterinas-build-env/rnvpce-nojournal-20261018/final-export/kernel-source.diff)。raw目录包含RED、中间失败、GREEN、每点guest/host/manifest/磁盘特性/e2fsck日志以及本轮[harness](../scripts/rnvpce-nojournal-20261018/harness/README.md)。补丁相对HEAD包含此前已存在的恢复修复，不全是本需求新增代码。

- 基础提交：`93c2b45dd4ac49b14daab7e5c4ecaab7af61d286`；本次未提交或推送。
- 最终ISO SHA256：`f7abf7d14f71b9527e3432acb5af3c6e0e5a2ee84e4784a10065804b9346f9cb`。
- 冻结kernel diff SHA256：`be221817a32b8e1d289850df15a4febc0aecd539a752b777339e0faca52447a8`，核验与当前代码一致。
- 大型镜像与原始日志：`/home/zn/asterinas-build-env/rnvpce-nojournal-20261018/`；根目录保留仅关闭JBD2的首轮，`final-export/`是最终四主点，`no-disk-journal/`是无磁盘journal的额外点。
