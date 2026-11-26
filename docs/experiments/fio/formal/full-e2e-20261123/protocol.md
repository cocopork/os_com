# EXT4 / rNVPCe-SU / NVLog 双设备完整扫描协议

阅读本文件后，应能够在相同硬件和代码上重复 2026-11-17 的完整性能扫描。正式
比较对象为 EXT4 writeback、rNVPCe-SU + rTIO 和 NVLog，不包含 rNVPC-WAL。
EXT4/rNVPCe 于 2026-11-17 采集，NVLog 对齐矩阵于 2026-11-20 完成；三者已合并到
统一 CSV 和图片。NVLog 每点只保留最终 `result.json`，不再保留 QEMU、格式化、
e2fsck 和临时 NVDIMM 日志。

## 1. 安全约束

本实验会反复格式化并清空以下两个真实分区：

- NVMe：`/dev/nvme1n1p2`，必须精确为 `274877906944` 字节；
- SATA：`/dev/sda4`，必须精确为 `274877890560` 字节。

开始前必须重新获得用户对这两个精确分区的破坏性测试授权。不得根据盘符相似性
替换设备，不得在分区已挂载、作为 swap、存在 holder 或被进程打开时继续。runner
会再次检查设备类型、父设备、容量、挂载和打开用户，但不能替代用户授权。

## 2. 固定实验契约

| 项目 | 配置 |
|---|---|
| 模式 | EXT4 journal；rNVPCe nojournal + soft update + rTIO |
| 负载 | 低碎片文件 4 KiB randwrite overwrite；新文件 sequential append；cold buffered randread |
| thread sweep | 4 KiB，1/4/8/12/16/20/24/28/32 jobs |
| bs sweep | 1 job，4/8/16/32/64/128/256/512/1024 KiB |
| 数据量 | 每点总计约 2 GiB，各 job 平分 |
| fio | `psync`、buffered I/O、iodepth=1、固定随机种子 |
| 重复 | 每个坐标 3 轮 |
| VM | 64 vCPU、8 GiB，fio CPU 为 0--31 |
| 块设备 | NVMe 8 virtqueue/8 IOThread；SATA 1 virtqueue/1 IOThread |
| rTIO | 4 worker，CPU isolation 开启 |
| NVM | 128 MiB pool、64 MiB metadata reserve、64 MiB data limit、fast 模拟 |

EXT4/rNVPCe 矩阵为 2 设备 x 2 模式 x 3 负载 x 2 sweep x 9 坐标 x 3 轮，共
648 次 VM 运行；NVLog 使用相同坐标，另有 324 次运行，合并后共 972 轮、324 个
聚合点。overwrite 和 cold randread 会先在原生 EXT4 上用 128 KiB direct
write 创建低碎片文件，再卸载并按被测模式重新挂载。append 从空文件开始，包含
首次数据块和元数据分配。写测试在 fio 后执行 `sync`，但图中带宽只使用 fio 测量期；
含 sync 带宽仍保存在 CSV 中。

### 2.1 后续 append thread sweep 修订契约

上表中的“每点总计约 2 GiB，各 job 平分”只用于复现 2026-11-17 历史矩阵。
后续测试 append 的多线程扩展性时，不再固定所有 job 的总写入量，而采用以下配置：

| 项目 | 修订配置 |
|---|---|
| 负载 | 4 KiB sequential append，从空文件开始，包含数据块和元数据分配 |
| jobs | 1/4/8/12/16/20/24/28/32 |
| 文件关系 | 每个 job 独占一个文件，不共享 inode |
| 单文件容量 | 每个 job 固定 1 GiB；不得再用总量除以 jobs |
| 计量方式 | 每个 job 单遍 append 1 GiB；以 fio 计量期的聚合带宽作为 thread-sweep 主指标 |
| I/O | `psync`、buffered I/O、`iodepth=1`；EXT4 为 `fsync=0`，rNVPCe-SU/NVLog 为 `fsync=1` |
| 后置同步 | fio 结束后单独执行 `sync`，记录耗时和 including-sync 带宽，不计入主曲线 |
| 重复 | 每个坐标至少 3 轮；保留逐轮结果、均值和标准差 |

该配置参考 P2CACHE 和 NVLog 的扩展性实验：线程访问各自的独立文件，每线程
保持相同的文件或工作集上限，线程增加时总数据集随之增长。其目标是测量文件系统
在相同单线程工作负载下增加并发执行单元后的扩展性。固定总量再按 jobs 平分会同时
改变单文件大小、extent 布局和每个 inode 的元数据分配量，不能再用于 append
thread-sweep 的可扩展性结论。

这里的 1 GiB 是每个 job 的实际写入量，线程数增加时总写入量按比例增长。不使用
`time_based` 循环，避免顺序写到达有限文件边界后回绕并变成 overwrite。block-size
sweep、overwrite thread sweep 和 cold-randread thread sweep 仍按
各自实验契约执行，不受本节覆盖。

### 2.2 后续 cold buffered randread 修订契约

cold randread 的目标是测量文件系统的实际冷读路径，包括 inode/extent
查找、块设备读取、DRAM Page 分配和 PageCache 索引构建。它使用 buffered
I/O，但不得在同一轮内重复读取已缓存的数据块。

| 项目 | 修订配置 |
|---|---|
| I/O | `psync`、`direct=0`、`iodepth=1`、`fsync=0` |
| 访问模式 | `rw=randread`、`norandommap=0`，一个计量周期内每个数据块最多读取一次 |
| 计量方式 | 单遍读取，`time_based=0`；不得使用固定时长循环重读 |
| 数据集 | 总容量至少为 DRAM PageCache + NVM data cache 配额的 10 倍 |
| 默认容量 | 总缓存 256 MiB 时，每个坐标使用至少 2560 MiB 总数据集 |
| thread sweep | 4 KiB；各 job 独占文件或不重叠区间，平分固定总数据集 |
| bs sweep | 1 job；4/8/16/32/64/128/256/512/1024 KiB；每点单遍读取同样大小的数据集 |
| 重复 | 每个坐标至少 3 轮，每轮重新建立冷缓存状态 |

每轮先以准备模式挂载目标分区，预生成低碎片文件并完成 `sync`，再卸载并按
被测模式重新挂载。计量前必须通过重启 VM，或等价的卸载、清空 DRAM PageCache 和
rNVPC clean-page cache、重新挂载流程建立冷状态。`invalidate=1` 只可作为
辅助手段，不能替代上述冷状态准备。

`direct=1` 读取另归入 `raw-device` 类别，用于建立块层、virtio 驱动和设备带宽
上限。该数据不包含 PageCache 构建开销，不得替代 full-e2e 的 buffered cold-read
曲线；两者的差值用于观察文件系统冷读路径额外开销。本节是后续实验契约，
不追溯改变本目录已归档的 2026-11-23 历史数据。

### 2.3 后续 overwrite thread sweep 修订契约

overwrite 的多线程扩展性测试采用固定每 job 工作集，避免历史矩阵中“固定总量再按
jobs 平分”同时改变单文件大小、extent 布局和 inode 数量。

| 项目 | 修订配置 |
|---|---|
| 负载 | 4 KiB random overwrite；计量期不分配新的数据块 |
| jobs | 1/4/8/12/16/20/24/28/32 |
| 文件关系 | 每个 job 独占一个文件，不共享 inode |
| 单文件容量 | 每个 job 固定 1 GiB；总工作集随 jobs 线性增加 |
| 准备 | 先用 128 KiB direct sequential write 预建低碎片文件并完成 `sync` |
| 计量方式 | 每个 job 对自己的 1 GiB 文件执行一遍随机覆盖；`time_based=0` |
| I/O | `psync`、buffered I/O、`iodepth=1`、固定随机种子；EXT4 为 `fsync=0`，rNVPCe-SU/NVLog 为 `fsync=1` |
| 后置同步 | fio 结束后单独执行 `sync`；主曲线只使用 fio 计量期带宽 |
| 重复 | 每个坐标 3 轮；保留逐轮结果、均值和标准差 |

该配置将不同 jobs 下每个执行单元的文件容量、extent 布局目标和覆盖写工作量保持
一致，线程增加时总工作集由 1 GiB 增长到 32 GiB。它用于观察并发覆盖写的扩展性，
不能与固定 2 GiB 总量的 2026-11-23 历史 thread sweep 直接拼接。

### 2.4 rNVPCe-SU / NVLog 同步写复测（2026-11-25）

rNVPCe-SU 与 NVLog 的 append-write 和 overwrite-randwrite 改为 `psync`、buffered
I/O、`iodepth=1`、`fsync=1`。fio 每完成一个 block 写便调用一次 `fsync()`，分别
触发 rNVPCe 持久化写路径和 NVLog 同步日志路径。cold-randread 继续使用
`fsync=0`；EXT4 保留已归档的 `fsync=0` 数据，作为异步 buffered-write 对照。

复测范围如下：

| 扫描 | 配置 |
|---|---|
| bs sweep | 1 job，2 GiB；4/8/16/32/64/128/256/512/1024 KiB |
| thread sweep | 每 job 独占 1 GiB 文件；4 KiB；1/4/8/12/16/20/24/28/32 jobs |
| 设备队列 | NVMe 8 virtqueue；SATA 1 virtqueue |
| 重复次数 | 每坐标 3 轮 |

每个写测试点必须同时满足 fio、卸载和 e2fsck 成功。两种同步写结果 JSON 均记录
`fio_fsync_blocks=1`；NVLog 还要求 fio 运行期间采样到的 `nvpcctl usage` 峰值
`used pages` 大于 0，并记录为 `nvlog_used_pages`，避免 post-fio `sync` 清空日志后
误判，也避免把未进入 NVLog 的异步写结果收入。

该矩阵有意保留两种持久化语义：rNVPCe-SU 与 NVLog 为逐 block `fsync()`，EXT4 为
`fsync=0`。因此 EXT4 曲线是异步上限对照，不代表三者在相同 durability 语义下的
直接性能排名。

## 3. 代码与工具

仓库根目录：

```text
/mnt/data/zn/Rust_EXT4/os_com_rnvpc_rebuild
```

历史被测内核基于 `cce2180a2bdf324f9032633800a5f0eb6cd6e186`，叠加了当时
未提交差异；该差异的 SHA-256 为
`02d20245a177133b818a9ae5fcd5a462003fdc73b183c511c334737cfe2a5fbf`。精确重放历史
实验应使用下文已冻结且校验哈希的 `selected.iso`，不应仅检出基础
提交后重新编译。若测试后续代码，应记录实际 `git rev-parse HEAD`和新
镜像哈希，不能把新结果标成历史结果的完全复现。

当前归档的执行脚本位于：

```text
docs/experiments/fio/formal/full-e2e-20261123/scripts/
```

需要 Docker、`/dev/kvm`、`debugfs`，以及镜像
`os-com-recovery-tools:workspace-20261018`。runner 使用其中的 QEMU、OVMF、fio、
e2fsprogs 和 host PID namespace 工具。

## 4. 准备启动镜像

精确重复历史实验时，优先复用：

```text
/home/zn/asterinas-build-env/full-e2e-20261117/source/
```

该目录必须包含以下文件，并核对哈希：

```text
d4ce0ac132c2031b8ed31b2449a80d377791dc385b804fe14789d3612441784b  selected.iso
e458f983d1565ab08710ee3030c0d3d50fcd379f4157faaf2b78cd7d6c8dcef8  ext2.img
7bc88244039f0409bad103316881407768cdc6faf44837963f27a8cc68ea10d8  exfat.img
```

测试新代码时需要重新构建 `selected.iso`。在已经挂载仓库且具有 `cargo-osdk` 的
开发容器中，从仓库 `kernel/` 目录执行：

```sh
CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=16 SMP=64 MEM=8G CONSOLE=ttyS0 \
cargo osdk build --release \
  --init-args="/ext2/rebuild-fio.sh" \
  --kcmd-args="console=ttyS0 ostd.log_level=error rnvpc=off rnvpc_extend=off rtio=off rnvpc_size=128M rnvpc_meta_reserve=64M rnvpc_data_limit=64M rnvpc_sim_mode=fast rtio_workers=4 rtio_cpu_isolation=on rtio_cpu_trace=off"
```

将 `target/osdk/aster-kernel-osdk-bin.iso` 复制为新 `SOURCE_ART/selected.iso`；
`ext2.img` 和 `exfat.img` 可从上述冻结目录复制。记录三个文件的新 SHA-256，测试
期间不得重新编译或替换镜像。

## 5. 执行完整矩阵

先确认目标设备存在且未挂载，并再次核对容量：

```sh
lsblk -dnbo NAME,SIZE,TYPE,PKNAME /dev/nvme1n1p2 /dev/sda4
findmnt -rn -S /dev/nvme1n1p2
findmnt -rn -S /dev/sda4
```

创建新的外部证据目录。不要覆盖历史目录：

```sh
cd /mnt/data/zn/Rust_EXT4/os_com_rnvpc_rebuild
RUN_ROOT=/home/zn/asterinas-build-env/full-e2e-$(date +%Y%m%d-%H%M%S)
mkdir -p "$RUN_ROOT/results"

ROOT_ART="$RUN_ROOT/results" \
SOURCE_ART=/home/zn/asterinas-build-env/full-e2e-20261117/source \
bash docs/experiments/fio/formal/full-e2e-20261123/scripts/run-sweep.sh
```

完整运行耗时较长。脚本支持安全续跑：已有且能重新解析的 `result.json` 会跳过；
遇到不完整点时会拒绝覆盖，应先检查并将其移动到单独的 `failed-*` 证据目录。不要
直接删除失败证据。

只运行少量坐标进行预检时，使用空格分隔的精确 case 名：

```sh
CASES='e2e-nvme-ext4-overwrite-randwrite-threads-1-r1 e2e-nvme-rnvpce-overwrite-randwrite-threads-1-r1' \
ROOT_ART="$RUN_ROOT/results" \
SOURCE_ART=/home/zn/asterinas-build-env/full-e2e-20261117/source \
bash docs/experiments/fio/formal/full-e2e-20261123/scripts/run-sweep.sh
```

## 6. 聚合、绘图与归档

EXT4/rNVPCe runner 结束时会自动聚合；也可单独重跑：

```sh
SCRIPT_DIR=docs/experiments/fio/formal/full-e2e-20261123/scripts
python3 "$SCRIPT_DIR/summarize.py" "$RUN_ROOT/results"
python3 "$SCRIPT_DIR/plot_results.py" \
  "$RUN_ROOT/results/results.csv" \
  docs/experiments/fio/formal/full-e2e-20261123/plots
```

归档绘图输入数据：

```sh
cp "$RUN_ROOT/results/results.csv" \
  docs/experiments/fio/formal/full-e2e-20261123/results.csv
cp "$RUN_ROOT/results/aggregate.csv" \
  docs/experiments/fio/formal/full-e2e-20261123/aggregate.csv
```

合入本目录的 NVLog 原始结果并重新绘图：

```sh
ARCHIVE=docs/experiments/fio/formal/full-e2e-20261123
python3 "$SCRIPT_DIR/integrate_nvlog.py" \
  "$ARCHIVE/results.csv" "$ARCHIVE/raw/nvlog" "$ARCHIVE"
```

`results.csv` 保存三种系统的 972 次逐轮结果，是绘图的直接输入；
`nvlog-results.csv` 单独保存 NVLog 的 324 次结果；`aggregate.csv` 保存 324 个三轮
均值、标准差、最小值和最大值。图片实线是均值，`X` 是逐轮原始点。整合命令可
重复执行，不会重复追加 NVLog 行。

## 7. 验收

完整实验必须同时满足：

1. `integrate_nvlog.py` 输出 `integrated 972/972 runs, aggregated 324/324 points`；
2. `results.csv` 为 973 行、`nvlog-results.csv` 和 `aggregate.csv` 均为 325 行；
3. 只生成 `bandwidth-bs-summary.png` 和 `bandwidth-thread-summary.png`；
4. 每个点的 fio error、`sync`、卸载状态均为 0；
5. 每个点后置 `e2fsck -fn` 状态为 0；
6. 测试结束后两块目标分区均未挂载，且没有残留 `full-e2e-qemu-*` 容器。

当前目录中的 `results.csv`、`aggregate.csv` 和 `plots/` 下两张带宽总图是三对象
归档结果。EXT4/rNVPCe 的完整逐点日志与 QEMU 证据仍保存在外部构建目录。NVLog
的 324 个点已在清理前全部通过正式解析器复验；仓库归档只保留每点最终
`result.json`，并在 `nvlog-results.csv` 中提供统一绘图输入：

```text
/home/zn/asterinas-build-env/full-e2e-20261117/results/
docs/experiments/fio/formal/full-e2e-20261123/raw/nvlog/
```
