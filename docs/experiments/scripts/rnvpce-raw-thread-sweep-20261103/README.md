# rNVPCe 与裸 virtio-blk 4 KiB thread sweep

> **历史口径警告：**该脚本的rNVPCe准备阶段按job数并发创建独立文件，会使多job文件
> 高度碎片化，不再符合当前默认性能测试规范。它只用于复现2026-11-03至11-05的历史
> 数据，不能直接用于新的线程扩展性结论。新实验应使用一个预建低碎片文件的互不重叠
> 区间；必须测多inode时，应逐文件串行初始化并记录extent布局。原因和交叉验证见
> [文件布局实验](../../reports/RNVPCE-LAYOUT-CROSS-2026-11-05.md)。

该实验在同一个当前源码镜像和四 virtqueue 配置下比较：

- `rnvpce`：挂载 nojournal EXT4/rNVPCe，执行 buffered `psync` 随机写；
- `rnvpce-warm`：同一次挂载先完整覆盖并sync，再测量第二次覆盖；
- `raw`：不挂载文件系统，fio 直接对 `/dev/vdc` 执行 direct `psync` 随机写。
- `linux`：Linux guest使用相同QEMU多队列配置直接写 `/dev/vdc`。

线程数默认为 `1 2 4 8 16`，每点总计写入 2 GiB。raw 模式将设备前
2 GiB 划分为互不重叠的 per-job 区间。脚本复用
`rtio-virtio-multiqueue-20261102` 的设备身份、占用检查、四 virtqueue/IOThread
映射和宿主 NVMe hctx 跟踪。

运行会覆盖指定分区，必须使用已明确授权的测试分区：

```sh
ROOT_ART=/path/to/new-or-existing-artifact-root \
SOURCE_ART=/path/to/selected-iso-and-images \
TARGET=/dev/nvme1n1p2 \
bash run-sweep.sh
```

可以通过 `MODES=rnvpce`、`MODES=rnvpce-warm`、`MODES=raw`、
`MODES=linux` 和 `JOBS_LIST='1 4 16'` 选择子集。Linux模式的`SOURCE_ART`
需要包含`vmlinuz`、`initramfs.cpio.gz`、`ext2.img`和`exfat.img`。
raw 点不执行 e2fsck，因为测试会覆盖文件系统元数据；rNVPCe 点逐点重新格式化，
并要求 fio、sync、卸载和只读 e2fsck 全部成功。runner 状态3只表示低线程点
没有达到四个活跃hctx的机制门槛，不表示I/O失败。
