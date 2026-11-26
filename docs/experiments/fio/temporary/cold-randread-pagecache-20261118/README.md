# rNVPCe PageCache 冷读回归实验（2026-11-18）

该临时实验验证 PageCache 集成后 rNVPCe 在 NVMe 与 SATA 上的 cold randread
block-size sweep 和 thread sweep。它是问题定位数据，不作为当前正式 full-e2e 结论；
正式对比以 `fio/formal/full-e2e-20261123/` 为准。

- `results.csv`：逐轮结果；
- `aggregate.csv`：聚合结果；
- `raw/`：保留 case 配置、文件布局、e2fsck 状态和最终 `result.json`。

QMP、容器、QEMU inspect、格式化日志、重复 e2fsck 日志及已被正式 full-e2e 总图
替代的单项预览图已清理。这些过程文件不影响本实验保留的性能结论，也不再作为
正式证据使用。
