# 连续 NV extent 与整段 migration 实现报告（2026-10-21）

本轮实现了 015 设计：前台把逻辑和 NVDIMM 槽位均连续的 NVPages 表示成一个 NV extent，
后台只迁移完整的当前 NV extent。功能和一致性验收通过；fio 性能仍明显低于 EXT4 buffered
目标，不能把本轮结论解释为性能目标已经达成。

## 实现结果

- `ExtentRecordEntry` 支持多页 NV range，查询通过逻辑偏移解析 NV slot；切片、反向索引和
  recovery 都逐页验证 descriptor identity。
- NV 数据预留优先取得连续槽位。前台按逻辑地址和槽位连续性合并 maximal runs；128 KiB
  连续写在槽位连续时用一个 `length=32` entry 表示。
- completion 候选必须覆盖提交时的完整 extent。SSD blocks 必须与逻辑页连续，leaf 内只用
  一次 8B target 持久化把整段从 NV 切换到 SSD。
- 候选二次校验和 completion 发布分别使用短 inode publication 临界区；data BIO 与设备 flush
  期间不持锁。completion 重新解析当前 forward edge，避免并发 leaf rewrite 后使用旧 entry index。
- EXT4 每轮最多 1024 页，NV descriptor/reclaim 事务最多 128 页。实现先按当前完整 extent
  分组，再将若干完整 extent 装入 128 页事务，不在 extent 内切批。

## 正确性验证

`cargo osdk check --ktests --package aster-kernel` 通过。定向 ktest 覆盖：

- 前台连续页合并成单一 NV extent，leaf generation 不发生逐页 rewrite；
- completion 缺一页返回 EAGAIN 且不切换 target，完整范围一次切换；
- migration 的五个持久化阶段模拟崩溃后，recovery 只恢复 NV 或 SSD 权威 forward graph；
- 短 reader 只发布 durable prefix，恢复后范围内页面仍可查询。

最终四个 fio 样本均 error=0、完整写入 2 GiB、正常卸载，宿主 e2fsck exit=0。4 KiB
单线程因 180 秒只能完成 1701.5 MiB，最终样本单独将上限提高到 300 秒，其他参数不变。

## 性能结果

配置：真实 `/dev/nvme1n1p1`，64 vCPU、8 GiB、virtio queue 1024、CPU 隔离；rNVPCe
nojournal、NVPool 128 MiB、metadata reserve 64 MiB、data limit 64 MiB；buffered psync、
iodepth=1。单位 MiB/s。

| workload | 线程 | fio 带宽 | 含最终排空 |
| --- | ---: | ---: | ---: |
| 4 KiB randwrite | 1 | 9.57 | 9.47 |
| 4 KiB randwrite | 16 | 34.91 | 33.99 |
| 128 KiB write | 1 | 34.90 | 32.16 |
| 128 KiB write | 16 | 57.11 | 49.23 |

作为同分支改动前参考，FAST-FAIR persist 消融报告中的正常组为 11.14、37.86 和
256.67 MiB/s（4 KiB/1、128 KiB/1、128 KiB/16）。因此当前实现验证了范围表示与整段
completion 的正确性，但 128 KiB/16 吞吐明显回退。主要新增串行点是 completion 为当前
locator 重分组和 target 发布取得的短 inode publication guard；前台 FAST/FAIR extent 更新
与持久化本身也仍占主要开销。后续需要用阶段探针区分 publication 等待、完整 extent 候选
不足和 descriptor/reclaim 事务成本，再决定是否引入 dirty-extent DRAM 索引和 leaf 级稳定身份。

## 证据

- 128 KiB/16 线程日志：`/home/zn/asterinas-build-env/contiguous-nv-extents-20261021/extent-packed-v10/`；
  128 KiB/单线程日志：同级 `extent-packed-v10-j1/`。
- 最终 4 KiB 样本和当前源码镜像：`/home/zn/asterinas-build-env/contiguous-nv-extents-20261021/final-v11/`，
  ISO SHA-256 `40e314e3be42b6bec4ebd845a4ca1b6dfc5ba07b5c212dc6d3840fe59f86a5cf`。
- 128 KiB 样本使用 `extent-packed-v10` 镜像，SHA-256
  `8de6d183a2491346e441255c8a568879979ecfe3c4e6bab213c173e4209950eb`。后续源码只补充
  completion 幂等重试校验和单页预留 O(1) 路径，不改变正常 128 KiB 首轮迁移路径。
- 源码基线：分支 `recovery/rnvpce-fast-fair`，提交 `862fa8eb7` 加本报告对应未提交修改。
