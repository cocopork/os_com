# EXT4 / rNVPCe 双设备完整扫描复现说明

阅读本文件后，应能够在相同硬件和代码上重复 2026-11-17 的完整性能扫描。正式
比较对象只有 EXT4 writeback 和 rNVPCe-SU + rTIO，不包含 rNVPC-WAL。

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
| 块设备 | 4 virtqueue、4 IOThread，IOThread 固定到 CPU 60--63 |
| rTIO | 4 worker，CPU isolation 开启 |
| NVM | 128 MiB pool、64 MiB metadata reserve、64 MiB data limit、fast 模拟 |

矩阵为 2 设备 x 2 模式 x 3 负载 x 2 sweep x 9 坐标 x 3 轮，共
648 次 VM 运行。overwrite 和 cold randread 会先在原生 EXT4 上用 128 KiB direct
write 创建低碎片文件，再卸载并按被测模式重新挂载。append 从空文件开始，包含
首次数据块和元数据分配。写测试在 fio 后执行 `sync`，但图中带宽只使用 fio 测量期；
含 sync 带宽仍保存在 CSV 中。

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

执行脚本位于：

```text
docs/experiments/scripts/full-e2e-sweep-20261117/
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
bash docs/experiments/scripts/full-e2e-sweep-20261117/run-sweep.sh
```

完整运行耗时较长。脚本支持安全续跑：已有且能重新解析的 `result.json` 会跳过；
遇到不完整点时会拒绝覆盖，应先检查并将其移动到单独的 `failed-*` 证据目录。不要
直接删除失败证据。

只运行少量坐标进行预检时，使用空格分隔的精确 case 名：

```sh
CASES='e2e-nvme-ext4-overwrite-randwrite-threads-1-r1 e2e-nvme-rnvpce-overwrite-randwrite-threads-1-r1' \
ROOT_ART="$RUN_ROOT/results" \
SOURCE_ART=/home/zn/asterinas-build-env/full-e2e-20261117/source \
bash docs/experiments/scripts/full-e2e-sweep-20261117/run-sweep.sh
```

## 6. 聚合、绘图与归档

完整 runner 结束时会自动聚合；也可单独重跑：

```sh
SCRIPT_DIR=docs/experiments/scripts/full-e2e-sweep-20261117
python3 "$SCRIPT_DIR/summarize.py" "$RUN_ROOT/results"
python3 "$SCRIPT_DIR/plot_results.py" \
  "$RUN_ROOT/results/results.csv" \
  docs/experiments/test_results/full-e2e-20261117
```

归档绘图输入数据：

```sh
cp "$RUN_ROOT/results/results.csv" \
  docs/experiments/test_results/full-e2e-20261117/results.csv
cp "$RUN_ROOT/results/aggregate.csv" \
  docs/experiments/test_results/full-e2e-20261117/aggregate.csv
```

`results.csv` 保存 648 次逐轮结果，是绘图的直接输入；`aggregate.csv` 保存 216 个
三轮均值、标准差、最小值和最大值。图片实线是均值，`X` 是逐轮原始点。

## 7. 验收

完整实验必须同时满足：

1. `summarize.py` 输出 `validated 648/648 runs, aggregated 216/216 points`；
2. `results.csv` 为 649 行（含表头），`aggregate.csv` 为 217 行（含表头）；
3. 生成 18 张 PNG：3 个负载 x 2 个 sweep x 3 个指标；
4. 每个点的 fio error、`sync`、卸载状态均为 0；
5. 每个点后置 `e2fsck -fn` 状态为 0；
6. 测试结束后两块目标分区均未挂载，且没有残留 `full-e2e-qemu-*` 容器。

当前目录中的 `results.csv`、`aggregate.csv` 和 18 张 PNG 是 2026-11-17 历史
实验的归档副本。完整逐点日志与 QEMU 证据仍保存在：

```text
/home/zn/asterinas-build-env/full-e2e-20261117/results/
```
