# 冷读扩展性定位与优化实验

日期：2026-11-26

## 配置

- NVMe：`/dev/nvme1n1p2`
- QEMU：40 vCPU、8 virtqueue、8 IOThread；vCPU 与 IOThread 分离绑核
- I/O：`psync`、buffered、`iodepth=1`、`randread`、固定随机种子
- 缓存：128 MiB；数据集约 2 GiB
- 每个点 3 轮，表格使用算术均值；每轮结束执行 `e2fsck -fn`

完整验收输入见 `.omx/specs/autoresearch-cold-read-scaling/final-results.csv`，
本目录的 `summary.csv` 是其均值汇总。原始 QEMU/FIO 日志保存在：

```text
/home/zn/asterinas-build-env/cold-read-scaling-20261126/
```

## 结论

1. Achilles 原冷读 miss 维护第二套 per-page loading/SSD-route/CleanDram
   索引。32 线程 4 KiB 三轮均值仅 406.24 MiB/s，平均延迟
   242.31 us。SSD 权威 clean page 改走通用 PageCache 后，最终达到
   1051.72 MiB/s、114.64 us。
2. 通用 PageCache 的 clean reclaim 原来每页重新获取 `commit_lock` 和
   XArray 锁。改成每批单次持锁扫描和移除后，EXT4 32 线程从
   809.01 MiB/s、132.00 us 提高到 919.28 MiB/s、120.67 us。
3. 多页冷读在 detector 尚未升窗时会逐页 demand fault。EXT4 与
   Achilles 现在都精确预取当前请求范围；256/512/1024 KiB 带宽随
   块大小增长，不再在 256 KiB 后下降。
4. 最差必测点为 EXT4 32 线程，仍达到 Linux 的 90.52%；Achilles
   所有点均不低于 EXT4 的 105.66%。性能门槛与文件系统检查全部通过。

控制组也未回退：最终 4 KiB 单线程 EXT4/Achilles 均值分别为
43.26/43.33 MiB/s，16 线程分别为 570.68/570.31 MiB/s。
