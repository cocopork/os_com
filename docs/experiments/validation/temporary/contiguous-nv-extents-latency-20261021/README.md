# 连续 NV extent 版本延迟分解（2026-10-21）

本实验定位连续 NV extent 与整段 migration 实现相对前一版 FAST/FAIR 的性能回退。
结论是：批量 leaf 更新本身已经生效，当前主要瓶颈是 rTIO 在候选选择和 completion
阶段按页重复读取、解码并校验同一 4 KiB extent leaf，而且这些操作位于 inode
publication 临界区内。completion 释放 NVPage 变慢后，前台绝大部分新增延迟表现为
容量等待或 publication 等待。SSD I/O、设备 flush、NVPage 回收和锁原子操作均不是
本轮的主要瓶颈。

## 实验条件

- 分支：`recovery/rnvpce-fast-fair`，提交 `862fa8eb7` 加当前未提交的连续 NV extent 实现。
- 设备：真实 NVMe `/dev/nvme1n1p1`；每点重新格式化，结束后卸载并运行 e2fsck。
- guest：64 vCPU、8 GiB；virtio 单队列深度 1024；fio、virtio 和 rTIO CPU 隔离。
- rNVPCe：nojournal、NVPool 128 MiB、metadata reserve 64 MiB、data limit 64 MiB、
  low watermark 32 MiB、orderless migration、fast NVDIMM 模拟。
- fio：buffered psync、iodepth=1、每点 2 GiB；4 KiB randwrite 和 128 KiB sequential
  write，各 1/16 线程。
- 诊断镜像同时打开 `LATENCY`、`PIPELINE` 和 `NVPAGE-BATCH`；测试后均恢复为默认关闭。

四点均完整写入 2 GiB、fio error=0、正常卸载且 e2fsck clean。

## 带宽与顶层阶段

累计时间是跨线程/批次之和，阶段之间可能嵌套，不能直接相加为墙钟时间。

| workload | jobs | fio MiB/s | 含排空 MiB/s | 前台累计 s | 容量等待 s | flush 累计 s | completion 累计 s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite | 1 | 9.31 | 9.20 | 219.24 | 32.67 | 40.24 | 75.02 |
| 4 KiB randwrite | 16 | 35.29 | 34.39 | 866.10 | 648.14 | 22.59 | 57.80 |
| 128 KiB write | 1 | 34.04 | 31.21 | 60.12 | 9.88 | 27.88 | 34.11 |
| 128 KiB write | 16 | 55.96 | 47.51 | 543.24 | 468.66 | 26.88 | 37.38 |

当前无探针版本的对应带宽为 9.57、34.91、34.90 和 57.11 MiB/s，说明诊断镜像
保持了回退形态。历史 repair-image 批处理默认构建的代表点为 4 KiB/j1 133.18、
128 KiB/j1 289.14、128 KiB/j16 304.26 MiB/s；这些数据来自不同源码状态，仅用于说明
长期差距。下面的严格阶段对照使用相同三组探针的前一版 FAST/FAIR 日志。

## 最大回退点：128 KiB、16 线程

| 阶段 | 前一版 FAST/FAIR | 当前版本 | 变化 |
| --- | ---: | ---: | ---: |
| fio 带宽 | 256.67 MiB/s | 55.96 MiB/s | -78.2% |
| 前台 body | 25.56 s | 23.86 s | 没有变慢 |
| 前台容量等待 | 55.38 s | 468.66 s | 8.5x |
| rTIO flush | 6.55 s | 26.88 s | 4.1x |
| 候选选择 | 0.80 s | 12.30 s | 15.3x |
| completion 总计 | 7.73 s | 37.38 s | 4.8x |
| completion extent 发布 | 6.91 s | 37.13 s | 5.4x |
| completion 页身份/locator 校验 | 0.52 s | 11.09 s | 21.3x |
| 实际 leaf 更新 | 5.58 s | 0.75 s | 降至 13.4% |
| publication 等待 | 0.005 s | 27.64 s | 新增串行等待 |
| publication 持有 | 67.20 s | 267.07 s | 4.0x |
| 异步 data/metadata 完成等待 | 4.50 s | 1.34 s | 更低 |
| 设备 flush | 0.42 s | 0.09 s | 更低 |
| NVPage reclaim | 0.33 s | 0.12 s | 更低 |

每个 128 KiB 请求的前台 body 从约 1.56 ms 降到 1.46 ms，说明前台连续 extent
提交没有造成该并发点的额外回退。其总前台延迟变大，是 rTIO 释放容量过慢后产生的
背压。当前 completion 的处理能力约为 `2048 MiB / 37.38 s = 54.8 MiB/s`，与 fio
观察到的 55.96 MiB/s 接近。

## 单线程 4 KiB 佐证

4 KiB/j1 没有应用线程之间的竞争，但当前 publication 等待仍从前一版的 0.10 s
增至 169.99 s；completion extent 发布从 17.69 s 增至 74.54 s，候选选择从
1.38 s 增至 26.03 s，completion 校验从 0.55 s 增至 9.74 s。与此同时前台 body
从 118.49 s 降至 103.39 s。由此可知问题不是 16 个应用线程争用一把普通锁，而是
前台和单个 completion 线程在同一 inode publication 协议上互相等待；completion
在临界区内执行了过多按页工作。

## 源码对应关系

1. `rnvpce_flush_candidates()` 按页调用 `migration_extent_length()`，取得 inode
   publication 后，`migration.rs` 又按页调用一次做完整性复核。该函数每次都用
   `read_authoritative_metadata::<ExtentNodeRecord>()` 读取完整 leaf。一个 32 页 NV
   extent 在提交 I/O 前因而读取同一 leaf 64 次。
2. completion 先按页调用 `validate_migration_and_reverse()` 重新分组；随后
   `migrate_mapped_nv_pages_to_ssd_by_reverse()` 又对同一批的每页调用同一函数。
   `validate_migration_and_reverse()` 每次再次读取完整 leaf 并检查 entry forward edge。
   因此一个 128 KiB extent 在真正更新 leaf 之前最多已有约 128 次校验性 leaf 读取。
3. 上述 completion 重组、重复校验和所有 leaf 发布都包在 inode publication guard
   内。原子 owner 锁等待只有约 2 ms；27.64 s 是等待语义 publication owner 释放，
   说明临界区长度而非 Mutex 实现是问题。
4. 128 KiB/j16 的实际 leaf 更新已从 5.58 s 降至 0.75 s，证明“一个完整 NV extent
   用一个 entry、整段切换 target”的方向有效。继续优化 persist/fence、SSD flush 或
   reclaim 不能解决当前回退。

## 建议修复顺序

1. 候选以 `(inode, leaf_slot, entry_index)` 为单位分组，每个唯一 extent 只读取和验证
   leaf 一次，并由该 entry 生成整段页身份；不要逐页调用 `migration_extent_length()`。
2. 引入只在 publication guard 内产生的 `ValidatedMigrationExtent`，把已验证的 leaf
   identity、entry range、NV 首槽和 SSD 首块传给批量发布函数，删除 completion 中
   前后两轮重复的 `validate_migration_and_reverse()`。
3. 在 guard 外完成排序、分桶和缓冲区分配；guard 内只做“每个唯一 leaf/entry 一次
   最终校验 + target 持久化”，随后立即释放。
4. 再引入 dirty-extent DRAM 索引，直接选择完整 extent，消除当前扫描页队列后反复
   重排和重新入队的开销。这是候选查找优化，优先级低于删除锁内重复 leaf 读取。

## 证据

- 当前诊断镜像和原始日志：
  `/home/zn/asterinas-build-env/contiguous-nv-extents-latency-20261021/current/`。
- ISO SHA-256：`0cdd91cd0c5917aca12587b3e5f0e49a0da68c11c204366a95599067ff715ad9`。
- 同探针对照：
  `/home/zn/asterinas-build-env/fast-fair-persist-ablation-20261021/run-2-fast/`。
- repair-image 批处理历史报告：`RNVPCE-FOREGROUND-BATCH-2026-10-19.md`。
