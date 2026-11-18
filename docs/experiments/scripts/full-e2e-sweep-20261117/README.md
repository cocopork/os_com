# EXT4 / rNVPCe 双设备端到端扫描

本目录执行 NVMe `/dev/nvme1n1p2` 与 SATA `/dev/sda4` 上的两模式、三负载、
thread sweep 和 block-size sweep。每点总数据约 2 GiB，重复三轮，共 648 次运行。

比较模式为 EXT4 和 rNVPCe-SU。负载为已有低碎片文件上的 4 KiB
随机覆盖写、新文件顺序 append write，以及重新挂载后的 cold buffered randread。
QEMU 为 64 vCPU/8 GiB，目标 virtio-blk 使用 4 个 virtqueue 和 4 个独立 IOThread；
rNVPCe 默认 4 个 rTIO worker。

运行前必须显式给出外部证据目录和冻结镜像目录：

```sh
ROOT_ART=/home/zn/asterinas-build-env/full-e2e-20261117/results \
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

图中实线是各轮均值，`X` 是每轮原始结果。每个负载和 sweep 轴分别输出带宽、
平均延迟和 P99 延迟图，图内以两个子图分隔 NVMe 与 SATA。

供后续 AI 直接执行同一实验的完整前置条件、构建参数、命令和验收标准记录在
[`test_results/full-e2e-20261117/README.md`](../../test_results/full-e2e-20261117/README.md)。

