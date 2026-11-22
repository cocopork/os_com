# Virtio 裸设备模块延迟分解

本实验对同一个采样 I/O 依次记录裸设备文件封装、通用块层和
virtio-blk 层的延迟，避免使用互不对应的独立采样结果相减。

## 配置

- 设备：`/dev/nvme1n1p2`
- 负载：4 KiB 随机写，`psync`、`direct=1`、每 job `iodepth=1`
- 并发：4、16、32 jobs
- 每个测试点：3 轮，每轮合计约 2 GiB
- 虚拟机：64 vCPU、8 GiB
- virtio：8 virtqueue、8 QEMU IOThread；IOThread 固定到 CPU 56--63
- 采样：每 1024 个裸设备写请求采样一个；每点累计 1536 个样本

裸设备没有 EXT4 等常规文件系统。本实验把 `/dev` 块设备文件的
`OpenBlockFile::write_at` 视为文件接口层，主要包含范围检查和进入块层前后的管理操作。

## 运行

诊断内核需要带 `virtio_blk_profile=on` 启动，并包含
`RAW_BLOCK_PROFILE`、`BLOCK_MODULE_PROFILE` 和 `VIRTIO_BLK_PROFILE` 探针。

```bash
ROOT_ART=/home/zn/asterinas-build-env/virtio-module-profile-20261122/results \
SOURCE=/home/zn/asterinas-build-env/virtio-contention-profile-20261122/source \
TARGET=/dev/nvme1n1p2 \
PARENT=/dev/nvme1n1 \
./run.sh
```

`SOURCE` 中的诊断镜像名为 `8q-module-profile.iso`。本轮镜像 SHA-256：

```text
fa8bce3081943f8f089ed11ccdfbf71d9f333896ef4d9ac7088b5d58a88c3e11  8q-module-profile.iso
```

缓存设备元数据后的消融镜像 SHA-256：

```text
737ea0f45a95bb3b6d1f39a152eeff60c36a6d5b89567d8e651c68f40ab219d6  8q-module-profile.iso
```

该镜像保存在
`/home/zn/asterinas-build-env/virtio-module-profile-cached-metadata-20261122/source/`，
对应结果目录为同级的 `results/`。

精简原始数据和汇总结果位于
[`../../test_results/fio_test/virtio-module-profile-20261122/`](../../test_results/fio_test/virtio-module-profile-20261122/README.md)。
