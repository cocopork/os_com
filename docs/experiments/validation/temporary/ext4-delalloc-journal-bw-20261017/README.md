# EXT4 journal + delalloc 带宽实测（2026-10-17）

本轮使用当前 rebuild 工作树的 release 构建，原生 EXT4 journal + delalloc，关闭 rNVPC、rNVPCe 和 rTIO。四项均完成16 GiB写入、sync、卸载；QEMU退出33（正常退出协议），四次 `e2fsck -fn` 均退出0。最终分区头确认 `has_journal`、4096字节块、clean、Journal start=0。

| 工作负载 | fio 任务数 | fio MiB/s | 包含最终 sync MiB/s | 宿主分区写入 MiB/s | 最终 sync 秒 |
|---|---:|---:|---:|---:|---:|
| 128KB 顺序写 | 1 | 287.6 | 285.1 | 286.8 | 0.13 |
| 128KB 顺序写 | 16 | 284.5 | 282.7 | 281.5 | 0.11 |
| 4KB 随机覆盖写 | 1 | 240.8 | 238.6 | 244.8 | 0.27 |
| 4KB 随机覆盖写 | 16 | 215.3 | 213.5 | 217.6 | 0.23 |

## 测量条件

- 授权清空的真实分区 `/dev/nvme1n1p1`，容量256 GiB；每个样本重新格式化，其他分区未操作。
- Docker `os-com-recovery-tools:20261017` 启动 QEMU/KVM，16 vCPU、8 GiB RAM。真实分区通过 virtio-blk 接入；单设备队列、queue-size=64，QEMU `cache=none,aio=threads`。这是该虚拟化路径下的结果，不能等同裸机设备上限。
- mount显式 `delalloc,rnvpc=off,rnvpc_extend=off,rtio=off`；mke2fs显式 `has_journal`。为适配当前EXT4支持范围关闭metadata_csum、dir_index、64bit、flex_bg、inline_data、orphan_file。
- fio psync、buffered、iodepth=1/任务、thread=1、direct=0、fsync=0、fallocate=none；每项总量16 GiB，16任务时每任务1 GiB。每个配置一个样本。
- 顺序写是新建文件；随机写先顺序填满全部测试文件并sync，再做4KB随机覆盖写（固定随机种子20261017）。随机覆盖不主要测量新物理块分配收益。
- 采用生产默认缓存阈值：resident 512 MiB、dirty 128 MiB、background 64 MiB。分配器诊断统计关闭，内核日志级别error。

## 指标解释及结论

fio列取最终JSON的bw_bytes；包含最终sync列以实际写入16 GiB除以客体脚本从fio启动至sync返回的完整耗时，包含进程启动等开销。宿主分区写入列取宿主diskstats、排除fio最初5秒后的窗口均值，含元数据/journal I/O，因此随机写可略高于应用数据带宽。

写入量远大于缓存容量，且宿主持续落盘带宽接近fio带宽，结果不是仅由缓存吸收的短时峰值。最终sync仅0.11–0.27秒，未积压大量尾部数据。

本轮顺序写约285–288 MiB/s，随机覆盖约215–241 MiB/s；从1任务增至16任务没有增加总吞吐。未运行同版nodelalloc配对对照，也未做多轮统计，不能据此认定delalloc有性能提升或退化，不能仅凭吞吐定位瓶颈。此轮验证fio正常完成及文件系统结构检查，没有开启fio数据校验或做掉电测试。

## 可复现证据

证据目录：`/home/zn/asterinas-build-env/ext4-delalloc-journal-bw-20261017/`，含构建日志、ISO及SHA256、四项fio原始日志、每秒宿主diskstats、格式化日志、e2fsck日志/退出码、文件系统头、results.csv、运行脚本。source-head.txt记录HEAD，source-tracked.diff记录已有跟踪文件差异，source-modified-rust-sha256.json记录已修改和新增Rust文件校验和；测试版本包含尚未提交的delalloc等工作树改动。

ISO SHA256：`1d78529449a5ce2c2542b7696de1e6e0addf870eb7b16dcbf87485a61c6113ff`。

运行顺序：`bash build-release.sh`，随后 `python3 run-suite.py`。**后者将按历史授权清空专用测试分区，每次复用前仍须确认设备身份和授权适用。**
