# 混合 `write()+fsync()` 比例实验复现说明

本目录用于归档 EXT2、EXT4 writeback 和 rNVPCe-SU 在 4 KiB 随机混合读写下的
`fsync` 注入比例扫描。当前先保存测试方法；正式数据只有在完整 432 次运行全部通过
校验后才写入本目录。

## 破坏性设备

实验会反复格式化并清空两个真实分区：

- NVMe：`/dev/nvme1n1p2`，精确容量 `274877906944` 字节；
- SATA：`/dev/sda4`，精确容量 `274877890560` 字节。

每次启动完整实验前必须重新确认用户授权。runner 还会检查规范路径、父设备、容量、
挂载、swap、holder 和打开用户；任何检查失败均停止，不自动替换设备。

## 实验契约

| 项目 | 设置 |
|---|---|
| 请求 | 4 KiB、buffered、页对齐随机访问 |
| 并发 | 8 线程；每线程独占一个文件和普通 FD |
| 读写比例 | 0:10、3:7、5:5、7:3 |
| `fsync` 注入比例 | 写请求后的 0%、20%、40%、60%、80%、100% |
| 时间 | 10 秒预热、30 秒正式测量 |
| 重复 | 每坐标 3 轮 |
| 数据集 | 8 × 320 MiB = 2560 MiB |
| 预分配 | 每轮测量前顺序写满全部文件并同步，正式写均为覆盖写 |
| NVMe | 8 virtqueue、8 IOThread，宿主 CPU 56--63 |
| SATA | 1 virtqueue、1 IOThread，宿主 CPU 63 |
| 前台线程 | 来宾 CPU 0--7 |

固定种子为 `20261119`。读写类型、页面地址和同步选择使用相互独立的计数器哈希；
不同同步比例共享相同访问序列，且低同步比例集合是高同步比例集合的子集。

## 缓存口径

- EXT4：`pagecache_capacity_mb=256`，DRAM PageCache 预算 256 MiB。
- rNVPCe：`pagecache_capacity_mb=128`，加 `rnvpc_data_limit=128M`，总数据缓存
  预算 256 MiB；NVM metadata reserve 不计入数据缓存。
- EXT2：当前代码没有文件系统级 PageCache 容量限制。结果必须标记
  `cache_control=uncontrolled`，只记录 256 MiB 目标预算和 `/proc/meminfo` 观测，不能
  把 EXT2 数据称为严格同缓存容量对比。

工作集约为配置数据缓存预算的十倍。报告中的“配置预算”和“运行时观测”必须分列。

## 构建无插桩镜像

正式测试要求以下源码常量均为 `false`：

```text
kernel/src/fs/fs_impls/ext4/pipeline_profile.rs: ENABLED
kernel/src/fs/fs_impls/ext4/stats.rs: LATENCY_ENABLED
```

构建参数使用 `rtio_cpu_trace=off`：

```sh
cd /mnt/data/zn/Rust_EXT4/os_com_rnvpc_rebuild/kernel
CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=16 SMP=64 MEM=8G CONSOLE=ttyS0 \
cargo osdk build --release \
  --init-args="/ext2/mixed-sync-guest.sh" \
  --kcmd-args="console=ttyS0 ostd.log_level=error rnvpc=off rnvpc_extend=off rtio=off rnvpc_size=256M rnvpc_meta_reserve=128M rnvpc_data_limit=128M rnvpc_sim_mode=fast rtio_workers=4 rtio_cpu_isolation=on rtio_cpu_trace=off"
```

将 ISO、`ext2.img`、`exfat.img` 放入独立 `SOURCE_ART`，记录 SHA-256，并创建
`build-metadata.json`：

```json
{
  "pipeline_profile": false,
  "latency_profile": false,
  "rtio_cpu_trace": false,
  "rnvpc_data_limit_mib": 128,
  "init_args": "/ext2/mixed-sync-guest.sh"
}
```

runner 会再次检查源码常量和该文件，缺失或不匹配时拒绝测试。

## 执行

```sh
cd /mnt/data/zn/Rust_EXT4/os_com_rnvpc_rebuild
RUN_ROOT=/home/zn/asterinas-build-env/mixed-sync-$(date +%Y%m%d-%H%M%S)
mkdir -p "$RUN_ROOT/results"

ROOT_ART="$RUN_ROOT/results" \
SOURCE_ART=/home/zn/asterinas-build-env/mixed-sync-source \
bash docs/experiments/scripts/mixed-sync-ratio-20261119/run-sweep.sh
```

用 `CASES='case-name ...'` 可执行精确子集。已有 `result.json` 只有在重新解析通过时
才跳过；不完整目录会保留并导致停止，runner 不自动删除或覆盖失败证据。

## 聚合与绘图

```sh
SCRIPT_DIR=docs/experiments/scripts/mixed-sync-ratio-20261119
python3 "$SCRIPT_DIR/summarize.py" "$RUN_ROOT/results"
python3 "$SCRIPT_DIR/plot_results.py" \
  "$RUN_ROOT/results/results.csv" \
  docs/experiments/test_results/mixed-sync-ratio-20261119
```

归档时将 `results.csv` 和 `aggregate.csv` 一并复制到本目录。`plotting-source.csv` 是
绘图直接输入；折线表示三轮算术平均，`X` 表示每轮原始点。

## 验收

1. `summarize.py` 输出 `validated 432/432 runs, aggregated 144/144 points`；
2. `results.csv` 433 行，`aggregate.csv` 145 行，`plotting-source.csv` 433 行；
3. 生成 16 张 PNG：2 设备 × 4 读写比例 × 2 指标；
4. 每轮的基准、后置 sync、卸载和 e2fsck 状态均为 0；
5. 8 个文件均有完整 320 MiB 预写证据；
6. 固定种子、实际读写比例和实际同步比例通过计数校验；
7. QMP、IOThread affinity、镜像/二进制哈希和构建元数据完整；
8. 测试结束后目标分区未挂载，且没有残留 `mixed-sync-qemu-*` 容器。
