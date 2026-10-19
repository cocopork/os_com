# 009 rNVPCe nojournal 性能复测（2026-10-18）

当前009实现下，rNVPCe仍明显低于原生EXT4 nojournal；1→16线程没有带来rNVPCe带宽提升。本轮未修改内核或提交代码。

## 配置与口径

- 真实NVMe分区 `/dev/nvme1n1p1`（用户已授权清空），每点重新格式化。EXT4和rNVPCe都使用 `--journal off`，逐点dumpe2fs确认无HAS_JOURNAL。
- 每组3轮，总计24点；第二轮交换两种模式的执行顺序；不同点串行执行。每点总量2GiB，psync、buffered/direct=0、fsync=0、iodepth=1，每线程独立文件。随机写先通过原生EXT4 direct写满文件，sync并卸载，再重新挂载测试。
- Docker/QEMU/KVM：64vCPU、8GiB内存、virtio单队列深度1024。fio限定CPU0–31，virtio转发CPU62；原生writeback CPU60；rTIO工作线程沿用Kernel角色CPU32–59。
- EXT4缓存/脏页高水线64MiB、低水线32MiB；NVPool128MiB、metadata reserve64MiB、数据容量上限64MiB。rNVPCe使用fast DRAM模拟后端，未选择fast-no-persist。
- fio带宽取fio输出；含sync带宽=实际2GiB字节数/(启动fio到最终sync返回的时间)，不包含卸载。下表均为3次中位数，单位MiB/s；百分比为两个中位数之比。

## 带宽

| 工作负载 | 线程 | EXT4 fio | rNVPCe fio | rNVPCe/EXT4 | EXT4含sync | rNVPCe含sync | rNVPCe/EXT4（含sync） |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4KiB随机写 | 1 | 367.0 | 132.0 | 36.0% | 340.8 | 125.4 | 36.8% |
| 4KiB随机写 | 16 | 395.8 | 124.9 | 31.6% | 359.9 | 119.7 | 33.3% |
| 128KiB顺序写 | 1 | 531.4 | 154.6 | 29.1% | 487.6 | 116.4 | 23.9% |
| 128KiB顺序写 | 16 | 598.7 | 105.8 | 17.7% | 517.2 | 84.6 | 16.4% |

## 三轮范围与延迟

最小/最大值只描述本轮观测波动，3次测量不足以推导统计显著性。延迟列是每次fio汇总平均/p99延迟再取三轮中位数。

| 工作负载 | 线程 | 模式 | fio范围 MiB/s | 含sync范围 MiB/s | 平均延迟 μs | p99延迟 μs |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| 4KiB随机写 | 1 | ext4 | 338.3–370.2 | 316.0–343.6 | 10.28 | 6.75 |
| 4KiB随机写 | 1 | rnvpce | 126.7–132.8 | 120.3–126.6 | 29.18 | 65.28 |
| 4KiB随机写 | 16 | ext4 | 393.4–431.6 | 356.8–392.3 | 156.57 | 2703.36 |
| 4KiB随机写 | 16 | rnvpce | 120.7–125.3 | 114.5–119.8 | 494.22 | 2801.66 |
| 128KiB顺序写 | 1 | ext4 | 413.2–602.5 | 384.2–537.5 | 232.78 | 6651.90 |
| 128KiB顺序写 | 1 | rnvpce | 135.5–155.6 | 107.2–117.8 | 807.02 | 2113.54 |
| 128KiB顺序写 | 16 | ext4 | 558.5–602.5 | 486.5–517.2 | 3147.95 | 14745.60 |
| 128KiB顺序写 | 16 | rnvpce | 99.8–108.3 | 80.7–86.4 | 17750.96 | 143654.91 |

## 结论与边界

- 关闭JBD2之后，rNVPCe与EXT4 buffered写仍有明显差距，差距在fio运行阶段已存在，并非仅由最终sync引入。单独去掉journal尚未达到接近原生buffered带宽的目标。
- rNVPCe的16线程带宽低于单线程。本轮只测总带宽与fio延迟，没有新增锁/逐阶段计时，不能据此确定是哪把锁或哪个持久化步骤导致；010中的NVPage批量事务尚未实施。
- rNVPCe前台写沿用NVM提交协议，而原生buffered写主要提交到DRAM；二者前台持久性语义不同。含sync指标补充反映排空成本。fast模式也不代表真实NVDIMM延迟。
- 24/24点完整写入2GiB，fio error=0，sync返回、umount成功，后置e2fsck exit=0。本轮没有CRC/逐文件重启读回、掉电实验或逐阶段性能分解，不能据此证明009尚未完成的完整元数据掉电一致性。

## 可复现证据

- [逐点CSV](/home/zn/asterinas-build-env/rnvpce-nojournal-perf-20261018/all-results.csv)、[三轮统计JSON](/home/zn/asterinas-build-env/rnvpce-nojournal-perf-20261018/summary.json)、[验证摘要](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/experiment-runs/rnvpce-nojournal-perf-20261018/validation.json)、[运行顺序](/home/zn/asterinas-build-env/rnvpce-nojournal-perf-20261018/order.json)。
- raw目录按round-1/2/3保存每点guest日志、manifest、磁盘特性、e2fsck和status；harness目录为本轮脚本快照。大型镜像保留在 `/home/zn/asterinas-build-env/rnvpce-nojournal-perf-20261018/`。
- 基础提交 `93c2b45dd4ac49b14daab7e5c4ecaab7af61d286` 加当前未提交kernel补丁；复用已构建release ISO，逐点核验镜像与源码指纹。没有重复运行内核回归；此前009回归证据见[实现报告](RNVPCE-NOJOURNAL-2026-10-18.md)。
- ISO SHA256 `f7abf7d14f71b9527e3432acb5af3c6e0e5a2ee84e4784a10065804b9346f9cb`。
- kernel diff SHA256 `be221817a32b8e1d289850df15a4febc0aecd539a752b777339e0faca52447a8`；[对应冻结补丁](/home/zn/asterinas-build-env/rnvpce-nojournal-20261018/final-export/kernel-source.diff)。

单点复现（先准备所需selected.iso/ext2.img/exfat.img到新的artifacts目录，勿覆盖既有日志）：

```bash
python3 docs/os_com_implementation/test/rnvpce-buffer-compare/run.py nvme \
  --variant rnvpce --rw randwrite --jobs 1 --total-mib 2048 \
  --runtime 180 --journal off --format-authorized --artifacts /path/to/fresh-artifacts
```
