# EXT4 writeback 与 rNVPCe 四 worker 首次追加写扫描

本实验比较新建空文件的顺序追加写，计时包含文件创建、数据块、bitmap/GDT 和 extent
等首次分配与更新开销。矩阵共 36 点：

- thread sweep：4 KiB，jobs 为 `1,4,8,12,16,20,24,28,32`；
- bs sweep：1 job，块大小为 `4,8,16,32,64,128,256,512,1024 KiB`；
- EXT4 使用 journal，rNVPCe 使用 nojournal 和默认四 worker；
- 两种模式均使用四个 virtqueue/IOThread，每点约写入 2 GiB。

运行会清空指定测试分区：

```sh
ROOT_ART=/home/zn/asterinas-build-env/ext4-rnvpce-append-sweep-20261106/results \
SOURCE_ART=/home/zn/asterinas-build-env/ext4-rnvpce-append-sweep-20261106/source \
TARGET=/dev/nvme1n1p2 \
bash run-sweep.sh
```

绘图：

```sh
python3 plot_results.py \
  /home/zn/asterinas-build-env/ext4-rnvpce-append-sweep-20261106/results/results.csv \
  ../../../test_result_images
```
