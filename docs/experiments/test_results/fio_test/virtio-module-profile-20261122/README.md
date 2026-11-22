# Virtio 裸设备模块延迟分解结果

实验方法见
[`../../../scripts/virtio-module-profile-20261122/README.md`](../../../scripts/virtio-module-profile-20261122/README.md)，
详细分析见
[`../../../reports/VIRTIO-MODULE-LATENCY-2026-11-22.md`](../../../reports/VIRTIO-MODULE-LATENCY-2026-11-22.md)。

目录内容：

- `summary.csv`：4/16/32 jobs 的模块延迟、占比和 fio 汇总。
- `raw/<case>/result.json`：三轮 fio 结果。
- `raw/<case>/module.json`：同一采样请求的模块边界累计值。
- `raw/<case>/profile.json`：各 virtqueue 的提交和完成子阶段。
- `raw/<case>/case.cfg`、QMP 和亲和性文件：运行配置证据。
- `cached-metadata/comparison.csv`：缓存前后的逐点对照。
- `cached-metadata/raw/<case>/`：缓存设备元数据后的三轮原始结果。

未保存体积较大的启动镜像、磁盘镜像和完整串口日志。
