# EXT4 / rNVPCe 双设备端到端扫描

本目录执行 NVMe `/dev/nvme1n1p2` 与 SATA `/dev/sda4` 上的两模式、三负载、
thread sweep 和 block-size sweep。每点总数据约 2 GiB，重复三轮，共 648 次运行。

比较模式为 EXT4 和 rNVPCe-SU。负载为已有低碎片文件上的 4 KiB
随机覆盖写、新文件顺序 append write，以及重新挂载后的 cold buffered randread。
QEMU 为 64 vCPU/8 GiB。正式复测中 NVMe 使用 8 个 virtqueue/8 个 IOThread，
SATA 使用 1 个 virtqueue/1 个 IOThread；rNVPCe 在 NVMe 使用 4 个 rTIO worker，
在 SATA 使用 1 个 rTIO worker。

当前写入复测中，rNVPCe-SU 和 NVLog 的 append/overwrite 使用 `fsync=1`，EXT4
保留已有 `fsync=0` 数据作为异步对照；所有 cold-randread 均为 `fsync=0`。

运行前必须显式给出外部证据目录和冻结镜像目录：

```sh
ROOT_ART=/home/zn/asterinas-build-env/full-e2e-20261123/results \
SOURCE_ART=/home/zn/asterinas-build-env/full-e2e-20261117/source \
bash run-sweep.sh
```

使用 `CASES='case1 case2'` 可运行指定坐标。已有且能重新解析的 `result.json` 会跳过；
不完整目录会拒绝覆盖，必须先人工检查并移到失败证据目录。脚本逐点重新格式化目标
分区，并在 fio、卸载及只读 e2fsck 全部成功后才生成 `result.json`。

聚合和绘图：

```sh
python3 summarize.py "$ROOT_ART"
python3 plot_results.py "$ROOT_ART/results.csv" /path/to/output
```

仅输出 block-size sweep 和 thread sweep 两张 2x3 带宽总图。图中实线是各轮
均值，`X` 是每轮原始结果；CSV 仍保留平均延迟和 P99 延迟数据。

将另行采集并验证的 NVLog 324 点合入归档、重新聚合并绘制三对象曲线：

```sh
python3 integrate_nvlog.py \
  /path/to/archive/results.csv \
  /path/to/archive/raw/nvlog \
  /path/to/archive
```

该命令会保留 EXT4/rNVPCe 的 648 轮结果，加入 NVLog 的 324 轮 `result.json`，
生成 972 轮 `results.csv`、单独的 `nvlog-results.csv`、324 点 `aggregate.csv`，
以及 2 张 2x3 带宽总图（block-size、thread）。重复执行时会先清除输出目录中的
旧 PNG，并替换已有 NVLog 行，不会重复追加。

供后续 AI 直接执行同一实验的完整前置条件、构建参数、命令和验收标准记录在
[`../protocol.md`](../protocol.md)。其中 append thread sweep 的后续测试采用每 job
独立文件、每文件 1 GiB 上限和固定 30 秒计量窗口，不再按 job 平分固定总数据量。

