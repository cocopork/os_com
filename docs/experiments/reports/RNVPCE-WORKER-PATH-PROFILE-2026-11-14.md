# rNVPCe 4/32线程 worker 路径分解

## 结论

32线程随机覆盖写的主要后台瓶颈是候选选择。当前 rTIO ready queue 只定位到 inode，
worker 随后每轮都从该 inode 的 `entries: BTreeMap` 起点扫描全部entry，并跳过非
`Dirty` 状态。随机4 KiB写产生大量离散entry后，这一过程形成显著扫描放大。

轻量插桩下，4线程每认领一个dirty entry平均检查 `4.00` 个entry；32线程需要检查
`210.36` 个，放大 `52.6` 倍。候选选择从 `1.15 us/page` 增至
`40.61 us/page`。相反，BIO构建与提交、owner completion的每页成本没有随线程数
增加，因此不是本轮首要瓶颈。

## 测试条件

- 真实设备：`/dev/nvme1n1p2`；2 GiB数据集，128 MiB rNVPC池。
- 负载：4 KiB随机覆盖写，4和32个fio job，4个rTIO worker和4个virtqueue。
- 文件在测试前顺序创建；测试结束执行sync、卸载和e2fsck。
- 计时与计数均按worker独立维护，仅在sync边界输出。全局逐阶段直方图关闭。
- 轻量插桩性能为4线程 `522.98 MiB/s`、32线程 `237.09 MiB/s`，与无插桩
  波动范围一致。

## 分解结果

| 指标 | 4线程 | 32线程 | 变化 |
| --- | ---: | ---: | ---: |
| 总迁移页 | 524,288 | 524,288 | 相同 |
| productive rounds | 3,652 | 44,114 | 12.1x |
| 平均页/round | 143.56 | 11.89 | 降低91.7% |
| 检查entry数 | 2,095,771 | 109,909,785 | 52.4x |
| 成功认领entry数 | 523,716 | 522,494 | 相近 |
| 检查/认领 | 4.00 | 210.36 | 52.6x |
| candidate累计时间 | 0.602 s | 21.292 s | 35.4x |
| candidate时间/页 | 1.15 us | 40.61 us | 35.4x |
| BIO构建和提交/页 | 24.78 us | 21.12 us | 未恶化 |
| owner completion/页 | 0.94 us | 0.64 us | 未恶化 |
| 最大NV在途页 | 7,117 | 13,271 | 1.86x |
| durability leader尝试/成功 | 435/391 | 7,340/456 | 协调放大 |

随机覆盖使SSD目标同样离散，约每个页对应一个BIO；但32线程的最大NV在途页更多，
各virtqueue也能形成数百至数千请求的staging峰值。虽然个别队列存在descriptor等待，
BIO提交累计成本没有随线程数上升，因此设备提交不是造成4到32线程断崖的第一原因。

completion累计时间低于 `1 us/page`，也不是主要限制。32线程中 durability leader
竞争明显增加，约16次尝试才产生一次成功epoch；这是第二级协调开销，但其发生在
候选批次已经碎片化之后。

## 代码对应关系

`claim_ready_entries_for_worker()` 从worker ready queue取出inode后，对
`inode_map.entries.values()` 从头遍历。每次只对仍处于 `Dirty` 的entry执行
`Dirty -> Migrating`，达到页预算后结束。已迁移、等待durability或等待回收的entry
仍留在同一BTreeMap中，因此后续round会再次检查这些entry。

ready queue状态机只能防止同一inode在运行期间重复入队，不能避免inode内部的重复
全表扫描。这解释了为什么上一项记账优化几乎没有吞吐收益。

## 后续方向

下一步应给每个inode维护直接指向dirty entry的DRAM索引/队列，而不是再建立一套
extent-tree路由缓存：

1. entry发布或从迁移回滚到Dirty时，只在 `not-ready -> ready` 边沿进入dirty队列；
2. worker直接弹出dirty entry，并用状态CAS认领，避免扫描所有非Dirty entry；
3. entry扩展只更新已有队列项的页数，不重复入队；
4. completion和回收不需要从候选队列删除历史项，旧项由CAS失败惰性跳过；
5. durability leader只在形成有意义的批次后竞争，减少碎片化引发的重复尝试。

这套索引解决的是迁移候选定位，与EXT4 ESTree的逻辑块到SSD路由缓存职责不同。

## 原始证据

- 轻量worker分解：`/home/zn/asterinas-build-env/rnvpce-worker-path-profile-20261114/light-results/`
- entry扫描计数复测：`/home/zn/asterinas-build-env/rnvpce-worker-path-profile-20261114/scan-results/`
- 全局直方图重插桩仅用于发现探针扰动，不用于最终定量结论：
  `/home/zn/asterinas-build-env/rnvpce-worker-path-profile-20261114/results/`
