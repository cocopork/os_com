# EXT4 writeback 与 rNVPCe 四 worker 固定布局扫描

本实验执行两种模式、随机/顺序覆盖写、thread/bs 两条扫描，每轮共 72 点。所有点先由
native EXT4 逐文件串行创建低碎片基线文件，再挂载被测模式；计时阶段不包含SSD数据块、
bitmap、GDT或extent节点分配。多inode用于让rNVPCe的四个inode-owned worker参与，
但禁止并发准备文件。

- thread sweep：4 KiB，jobs为`1,4,8,12,16,20,24,28,32`；
- bs sweep：1 job，块大小为`4,8,16,32,64,128,256,512,1024 KiB`；
- 每点约2 GiB，使用整数MiB的per-job文件，按实际总字节数校验；
- EXT4使用journal卷；rNVPCe使用nojournal卷和默认四worker；
- 两种模式均使用4个virtqueue/IOThread；
- 每点保存fio/sync/umount/e2fsck、hctx跟踪和`debugfs stat`布局证据。

正式扫描执行 5 个完整 round，并对每个测试点取算术平均；不要只重复部分异常点或
从多轮结果中挑选单个样本。

运行会清空指定测试分区：

```sh
ROOT_ART=/home/zn/asterinas-build-env/ext4-rnvpce-overwrite-sweep-20261105/results \
SOURCE_ART=/home/zn/asterinas-build-env/ext4-rnvpce-overwrite-sweep-20261105/source \
TARGET=/dev/nvme1n1p2 \
bash run-sweep.sh
```

开发烟雾测试可设置完整case名称，例如：

```sh
CASES='overwrite-ext4-threads-randwrite-1 overwrite-rnvpce-threads-randwrite-1' \
  bash run-sweep.sh
```

绘制 thread sweep 和 block-size sweep 对比图：

```sh
python3 plot_results.py \
  ../../test_result_images/fio_test/ext4-rnvpce-4worker-overwrite-sweep.csv \
  ../../test_result_images \
  --repeat-csv \
  ../../test_result_images/fio_test/ext4-rnvpce-4worker-overwrite-repeats-5x.csv
```

折线是五轮算术平均，`X` 是五轮原始 fio 带宽；图中不绘制 including-sync 带宽。
