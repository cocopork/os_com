# EXT4 / rNVPCe 大块冷随机读分解实验（2026-11-21）

## 目的与配置

本实验分析容量对齐后，Asterinas EXT4和rNVPCe-SU的大块冷随机读为何仍明显
低于Linux NVLog。测试使用2 GiB预置文件、buffered `randread`、`psync`、1 job、
iodepth 1，块大小为256/512/1024 KiB。PageCache为128 MiB；rNVPCe的物理
NVPool为256 MiB，metadata reserve和data limit均为128 MiB。NVMe使用8个
virtqueue和4个rTIO worker，SATA使用1个virtqueue和1个rTIO worker。

阶段探针只累计时间、调用数和处理页数。每个正式点运行一轮；因果消融只复测
NVMe 1024 KiB点。吞吐用于确认机制影响，不替代三轮正式性能结果。

## 代码约束

1. 原生EXT4的`read_at`按`IO_CHUNK=256 KiB`切分请求。每个内部块分别调用
   read-ahead detector、预取和PageCache读取。
2. rNVPCe每个冷读窗口固定为32页，即128 KiB。窗口内提交BIO后立即
   `wait_all()`，下一窗口不能与当前窗口流水重叠。
3. 两条路径的单BIO上限均为61个4 KiB segment。超过244 KiB的连续读会拆成
   多个BIO；该限制来自当前virtio queue descriptor预算。
4. rNVPCe窗口内仍对每页调用一次`current_page_mapping()`，读取权威inode并
   遍历持久化extent tree。

对应实现位于：

- `kernel/src/fs/fs_impls/ext4/writeback.rs`：`IO_CHUNK`；
- `kernel/src/fs/fs_impls/ext4/inode/mod.rs`：`RaState::on_read()`、
  `populate_rnvpce_read_window()`和`read_rnvpce_at()`；
- `kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs`：
  `MAX_RUN_SEGMENTS`和批量预取BIO构造；
- `kernel/src/fs/rnvpc/write_transaction.rs`：`current_page_mapping()`。

## EXT4分解

时间单位为秒。`I/O页倍率`是实际提交读页数除以有效数据页数524,288。

| 设备 | BS | 带宽 MiB/s | 总时间 | prefetch | PageCache body | 读BIO数 | I/O页倍率 |
|---|---:|---:|---:|---:|---:|---:|---:|
| NVMe | 256K | 122.22 | 16.72 | 12.55 | 4.17 | 16,833 | 1.00x |
| NVMe | 512K | 189.21 | 10.80 | 8.11 | 2.69 | 20,201 | 1.48x |
| NVMe | 1024K | 198.43 | 10.31 | 7.70 | 2.60 | 30,625 | 2.19x |
| SATA | 256K | 210.89 | 9.69 | 1.55 | 8.13 | 16,435 | 1.00x |
| SATA | 512K | 104.37 | 19.58 | 7.63 | 11.94 | 19,979 | 1.47x |
| SATA | 1024K | 116.63 | 17.54 | 5.31 | 12.21 | 26,675 | 2.18x |

EXT4没有发生前台容量等待：六个点的`native_pressure_wait`均为0，reserve阶段
累计时间不足2 ms。主要问题是同一个fio大请求被切成多个256 KiB内部读后，
read-ahead detector把请求内部的后续块当成连续读，继续扩展预取窗口。随机1 MiB
负载最终读取约2.2倍有效数据，SATA上的512 KiB和1 MiB带宽因此反而低于256 KiB。

## rNVPCe分解

| 设备 | BS | 带宽 MiB/s | 总时间 | route | submit | wait | publish | 窗口/BIO数 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| NVMe | 256K | 132.64 | 15.38 | 9.65 | 0.57 | 3.83 | 0.46 | 16,385 |
| NVMe | 512K | 190.85 | 10.70 | 5.28 | 0.60 | 3.65 | 0.35 | 16,384 |
| NVMe | 1024K | 237.92 | 8.59 | 2.84 | 0.47 | 3.09 | 1.35 | 16,386 |
| SATA | 256K | 127.75 | 15.98 | 2.21 | 0.77 | 11.47 | 0.36 | 16,387 |
| SATA | 512K | 125.26 | 16.32 | 2.03 | 0.84 | 11.73 | 0.48 | 16,386 |
| SATA | 1024K | 130.21 | 15.71 | 2.36 | 0.91 | 10.48 | 0.44 | 16,385 |

无论fio块大小是多少，2 GiB数据都被拆成约16,384个128 KiB窗口；每个窗口
恰好提交一个32页BIO并同步等待。SATA的窗口等待占总时间约67%--72%，所以带宽
在127--130 MiB/s附近基本不随BS增长。NVMe还承担524K次逐页权威映射查询；
256 KiB随机访问时route占总时间约63%，大BS下因连续页提高节点缓存局部性而下降，
但查询次数没有减少。

容量同样不是直接阻塞点：六个rNVPCe点的容量等待均为0，窗口reserve累计低于
4 ms，CachePage分配只占0.06--0.14秒。

## 因果消融

只改变两个互不重叠的模式内常量：EXT4把`IO_CHUNK`从256 KiB改为1 MiB；
rNVPCe把冷读窗口从32页改为128页。其余配置不变。

| 模式 | NVMe 1 MiB基线 | 消融后 | 变化 | 关键计数变化 |
|---|---:|---:|---:|---|
| EXT4 | 198.43 | 851.56 MiB/s | +329.2% | I/O页1.15M -> 530K；BIO 30,625 -> 14,880 |
| rNVPCe | 237.92 | 448.24 MiB/s | +88.4% | 窗口16,386 -> 4,452；wait 3.09s -> 1.70s |

EXT4消融后一次fio请求只进入一次内部读，预取量回到有效数据量附近。rNVPCe
消融后一个窗口最多拆成3个BIO并在同一个batch内并发提交；尽管61-segment上限
仍存在，带宽仍接近翻倍，说明主要问题不是segment上限本身，而是原设计每128 KiB
就形成一次提交后同步等待的串行边界。

### 1 MiB窗口稳定复测

为判断仅扩大窗口能否达到800 MiB/s，关闭分解插桩后，在相同NVMe、2 GiB、
1 job、psync、iodepth=1、1 MiB随机冷读条件下，分别将窗口设为512 KiB
（128页）和1 MiB（256页），每个配置独立运行三轮。

| 窗口 | 第1轮 MiB/s | 第2轮 MiB/s | 第3轮 MiB/s | 平均 MiB/s | 范围 MiB/s |
|---|---:|---:|---:|---:|---:|
| 512 KiB（128页） | 154.26 | 140.17 | 167.70 | 154.05 | 140.17--167.70 |
| 1 MiB（256页） | 369.68 | 318.06 | 286.71 | 324.82 | 286.71--369.68 |

1 MiB窗口比512 KiB窗口的平均带宽提高110.9%，但只达到800 MiB/s目标的
40.6%，因此**仅把rNVPCe窗口设置为1 MiB不能稳定达到800 MiB/s**。此前带分解
插桩的128页单轮消融得到448.24 MiB/s，与本次无插桩三轮基线不一致，说明单轮
结果受运行环境波动影响较大，不能外推为稳定水平。扩大窗口消除了部分逐窗口同步
等待，但逐页权威映射、单BIO的61-segment上限和窗口末尾`wait_all()`仍然存在。

## 冷读权威发布消融

按PageCache冷读设计做了一次最小实现：PageCache miss仍先检查dirty NV entry和
旧版精确页redirect，确认页面不在NVPage后，直接用原生EXT4 extent manager解析
SSD映射并填充`CleanDram`。冷读路径不再为只读数据创建NVM inode authority，
也不再逐页调用`current_page_mapping()`遍历持久化权威树。写路径、恢复路径和
NVPage读取路径没有改成以SSD为权威。

第一版只移除权威发布，但未保存跨128 KiB窗口的SSD路由，每个窗口都会重新读取
extent节点，三个大块点均退化到约129--135 MiB/s。修正版把已证明的SSD range
记录到现有ESTree的rNVPCe route缓存；extent修改和整树重建同时使对应缓存失效。

下表均为NVMe、2 GiB、1 job、psync、iodepth 1、每点三轮。`无EST缓存`用于说明
配套缓存的必要性，不是候选实现。256 KiB修正版首组三轮受明显运行波动影响，表中
采用独立补测三轮，并保留两组原始数据。

| BS | 历史路径 MiB/s | 去权威发布、无EST缓存 | 去权威发布、EST缓存 | 相对历史 |
|---:|---:|---:|---:|---:|
| 256 KiB | 329.87 | 134.50 | 319.79 | -3.1% |
| 512 KiB | 298.12 | 128.88 | 325.88 | +9.3% |
| 1024 KiB | 253.82 | 131.86 | 296.21 | +16.7% |

结果说明，**消除冷读权威发布能够提高512 KiB和1 MiB冷读性能，但不能单独解决
全部大块读瓶颈**。它消除了只读数据不必要的NVM元数据事务；现有128 KiB窗口、
窗口末尾同步等待和61-segment BIO上限仍在。256 KiB没有稳定收益，也说明该点
主要受I/O等待和运行波动影响，而非权威发布。

## 结论

按证据强度排序：

1. **高置信度：EXT4的256 KiB内部切分与read-ahead状态更新组合产生过度预取。**
   1 MiB随机读产生约2.2倍设备I/O；扩大内部块后额外I/O消失且带宽提高4.29倍。
2. **高置信度：rNVPCe的32页窗口是固定同步批次上限。** 总窗口数与fio BS无关，
   SATA主要时间位于逐窗口等待；扩大窗口使NVMe带宽提高88%。
3. **高置信度：rNVPCe逐页权威映射是额外软件开销。** 每轮固定524K次查询，
   NVMe 256 KiB点累计9.65秒。应按extent/range批量路由，而不是逐页遍历。
4. **高置信度：128 MiB容量上限不是本轮前台停顿来源。** 两种模式均无容量等待，
   reserve和页分配时间很小。容量会决定回收频率，但不能解释当前主要阶段时间。
5. **中高置信度：冷读不应发布NVM权威映射。** 直接使用原生SSD extent并将数据
   填入DRAM PageCache后，512 KiB和1 MiB分别提高9.3%和16.7%；但必须复用ESTree
   保存跨窗口SSD路由，否则重复extent读取会抵消收益并造成约2倍退化。

## 证据位置

- 基线12点完整日志：
  `/home/zn/asterinas-build-env/cold-randread-decomposition-20261121/results/`
- NVMe 1 MiB消融日志：
  `/home/zn/asterinas-build-env/cold-randread-decomposition-20261121/ablation/`
- 基线NVMe诊断ISO SHA-256：
  `53adc3de6bf11202d7526a3d8611ec719a6b6d9f063ca608fc1bb11fc4d7a0ad`
- 基线SATA诊断ISO SHA-256：
  `9312cc28bc9cb321e371ebd656f15d79b19454e6ec39632dfc968856fc3b90a9`
- 消融ISO SHA-256：
  `6aab1cb5054a4cbc31be08c3ae8e68df7b0403d44a9bf9db20616e8c2f0c2e34`
- 无插桩窗口稳定复测日志：
  `/home/zn/asterinas-build-env/rnvpce-read-window-ab-20261121/`
- 512 KiB窗口ISO SHA-256：
  `97237d9fd426149c6b1eb424bc9393f7ac61b234dbe3265877ed31a31b6985b5`
- 1 MiB窗口ISO SHA-256：
  `2a647a69417acaf7699c49642c543f582984c584fab2a6d3ab0636a66e577943`
- 去权威发布、无EST缓存结果：
  `/home/zn/asterinas-build-env/rnvpce-cold-read-no-authority-20261121/results/`
- 去权威发布、EST缓存结果：
  `/home/zn/asterinas-build-env/rnvpce-cold-read-no-authority-20261121/results-est-cache/`
- 256 KiB独立补测结果：
  `/home/zn/asterinas-build-env/rnvpce-cold-read-no-authority-20261121/results-est-cache-256k-repeat/`
- EST缓存修正版ISO SHA-256：
  `6c61099656083656a62037a674091bf80c66ae5ea912e4de0dfb0c964894ad34`

原有14轮和新增6轮均完成2 GiB fio读取、正常卸载并通过只读e2fsck。实验结束后，
临时探针保持关闭，消融常量恢复为原值。
