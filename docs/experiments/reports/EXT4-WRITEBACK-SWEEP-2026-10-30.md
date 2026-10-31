# 当前 EXT4 writeback thread/bs sweep（2026-10-30）

## 历史数据范围

历史文档中没有同时覆盖 thread sweep 和 block-size sweep 的 buffered EXT4
writeback 实验。完整的双 sweep 是
`DEVICE-DIRECT-SWEEP-2026-10-18.md`，但其 `direct=1` 绕过 PageCache，不能作为
writeback 对照。

可用的历史 writeback 数据只覆盖代表点：4 KiB 随机覆盖写、128 KiB 顺序写，
各 1/16 线程。与本轮条件最接近的是
`RNVPCE-BUFFER-FIX-2026-10-18.md` 中的 EXT4 对照：同为 2 GiB、64 vCPU、
virtio 单队列 1024、`psync/direct=0/iodepth=1`、每点 fresh journal EXT4。

## 本轮条件

- 当前分支 `recovery/rnvpce-page-redirection`，提交 `e4306c511819e5ae60f2cab155769d7c4a3595ea`；
- release ISO SHA256：`82d8c9c553cea8b775a990dace6a6a23d045a980497af2513df053e65fa65e0e`；
- `/dev/nvme1n1p1`，每点重新格式化为 4 KiB-block journal EXT4；
- mount：`delalloc,rnvpc=off,rnvpc_extend=off,rtio=off`；
- fio 3.39，`psync/direct=0/fsync=0/iodepth=1`，每线程独立 inode；
- 每点总量约 2 GiB，随机写先用 direct 顺序写预填充并 sync；
- thread sweep：4 KiB `randwrite`，线程数 1,4,8,12,16,20,24,28,32；
- bs sweep：单线程顺序 `write`，4,8,16,32,64,128,256,512,1024 KiB；
- QEMU/KVM 64 vCPU、8 GiB，fio CPU0–31、writeback CPU60、virtio CPU62、
  journal CPU63；virtio-blk 单队列1024，`cache=none,aio=threads`；
- 每点均完成 fio、最终 sync、卸载，随后 `e2fsck -fn`；18/18 点通过。

`含sync` 带宽从 fio 启动计时到最终 sync 返回，因而比 fio 自报值更接近完整
writeback/排空吞吐，但不包含随机写的预填充及卸载。

## Thread sweep：4 KiB 随机覆盖写

| 线程 | fio MiB/s | 含sync MiB/s | 平均延迟 us | P99 us |
|---:|---:|---:|---:|---:|
| 1 | 103.4 | 101.2 | 37.4 | 22.4 |
| 4 | 145.3 | 140.5 | 95.9 | 35.1 |
| 8 | 201.5 | 190.3 | 145.0 | 929.8 |
| 12 | 294.8 | 276.8 | 146.0 | 1515.5 |
| 16 | 344.9 | 320.0 | 175.7 | 5013.5 |
| 20 | 341.1 | 321.3 | 222.7 | 3194.9 |
| 24 | 362.4 | 330.6 | 255.0 | 1056.8 |
| 28 | 325.3 | 297.1 | 319.1 | 1368.1 |
| 32 | 298.4 | 283.3 | 399.2 | 1744.9 |

本轮吞吐随线程数扩展到24线程达到峰值，之后回落。P99不是单调变化，且每点
只有一个样本，不能仅据此定位锁竞争或调度原因。

## Block-size sweep：单线程顺序写

| 块大小 KiB | fio MiB/s | 含sync MiB/s | 平均延迟 us | P99 us |
|---:|---:|---:|---:|---:|
| 4 | 696.1 | 604.1 | 5.4 | 9.0 |
| 8 | 658.7 | 572.1 | 11.5 | 11.5 |
| 16 | 945.5 | 796.9 | 16.2 | 602.1 |
| 32 | 1229.3 | 948.1 | 25.0 | 387.1 |
| 64 | 1282.4 | 1050.3 | 48.1 | 1269.8 |
| 128 | 1219.8 | 939.4 | 101.1 | 2900.0 |
| 256 | 987.5 | 846.3 | 250.4 | 1908.7 |
| 512 | 995.6 | 839.3 | 496.5 | 2605.1 |
| 1024 | 1042.8 | 894.3 | 944.7 | 2965.5 |

本轮 fio 峰值位于64 KiB；32–128 KiB 均超过1.2 GiB/s。256 KiB以上没有继续
提高，可能同时受 guest writeback 批处理、virtio/宿主128 KiB请求上限和短样本
波动影响；本实验没有阶段探针，不能在这些因素间归因。

## 与最接近历史点比较

| 工作负载 | 线程 | 2026-10-18 fio | 当前 fio | 变化 | 2026-10-18 含sync | 当前含sync | 变化 |
|---|---:|---:|---:|---:|---:|---:|---:|
| 4 KiB随机写 | 1 | 303.7 | 103.4 | -65.9% | 288.9 | 101.2 | -65.0% |
| 4 KiB随机写 | 16 | 354.7 | 344.9 | -2.8% | 328.7 | 320.0 | -2.6% |
| 128 KiB顺序写 | 1 | 1423.2 | 1219.8 | -14.3% | 1137.8 | 939.4 | -17.4% |

16线程随机写基本复现历史水平；128 KiB单线程低约14–17%。4 KiB单线程明显低于
2026-10-18 的单点，但历史报告也明确记录同类测试跨轮波动较大；本轮只有一个
样本，不能直接判定为代码回退。若该点影响后续结论，应对1线程4 KiB随机写与
128 KiB顺序写做至少3轮交错复测，并同时记录 writeback 阶段计数。

## 证据与复现

- 原始日志、18份e2fsck输出及CSV：
  `/home/zn/asterinas-build-env/ext4-writeback-sweep-20261030-current/`；
- 运行脚本：`docs/experiments/scripts/ext4-writeback-sweep-20261030/run-sweep.sh`；
- 汇总脚本：`docs/experiments/scripts/ext4-writeback-sweep-20261030/summarize.py`。

本轮没有启用 fio 内容校验、宿主 diskstats 或内核阶段探针。e2fsck 通过只说明
卸载后的 SSD 文件系统结构检查通过，不替代数据校验或掉电一致性测试。
