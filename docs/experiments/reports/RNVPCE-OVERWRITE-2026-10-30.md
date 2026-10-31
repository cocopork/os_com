# rNVPCe首次写与覆盖写对照（2026-10-30）

## 结论

预先分配完整SSD文件和extent后，rNVPCe不再分配SSD块，也不再修改GDT/bitmap、
发布或回收SSD分配记录。单线程128 KiB顺序写从226.8 MiB/s提高到579.0 MiB/s，
说明首次分配和建立SSD baseline是重要成本。

但overwrite没有消除主要的逐页NVM操作。每个128 KiB请求仍拆成32个4 KiB页；前台
规划、容量计算和existing-baseline fast path最多对每页执行三轮extent查询或校验，
之后批量发布32个redirect，并为每个用户请求发布一次NVM inode generation。16线程
overwrite只有290.9 MiB/s，是单线程的50%；诊断中前台累计时间从4.73 s上升到
109.30 s。瓶颈已经不是SSD分配，而是逐页映射查询、NVM redirect/descriptor事务及其
共享元数据路径的并发放大。

现有前期扫描中的`randwrite`本来就会先完整预填充文件，因此所有4 KiB随机写数据已经
是overwrite结果。本实验补充的是原扫描没有覆盖的128 KiB顺序overwrite。

## 测试配置

- 分支：`recovery/rnvpce-page-redirection`，提交
  `e4306c511819e5ae60f2cab155769d7c4a3595ea`加未提交kernel修复；kernel diff SHA256
  为`13881697baeccfeb6158a6f64c2b4cdb84d520c54cd7696ece46dd6006badb7b`。
- 设备：`/dev/nvme1n1p1`，每点重新格式化为nojournal EXT4。
- VM：64 vCPU、8 GiB；virtio-blk单队列、深度1024，既有CPU隔离配置。
- fio：buffered `psync`、128 KiB、`direct=0`、`fsync=0`、`iodepth=1`，总量2 GiB。
- jobs：1和16，每个job使用独立inode。
- create：在空文件系统上直接测量首次写。
- overwrite：先用原生EXT4、direct I/O顺序写满所有文件，`sync`并卸载，再挂载
  rNVPCe开始计时；准备阶段不计入结果。
- 预填充保证SSD inode、extent和data block已存在；NVM inode/extent authority仍在每个
  inode首次被测访问时bootstrap一次。该一次性成本每点最多16次，而每点共有16,384个
  128 KiB请求，不能解释持续的逐请求成本，但本实验没有把它从计时中扣除。
- 每点均完整写入2,147,483,648 B，最终`sync`、卸载和只读`e2fsck`成功。

## Release吞吐

| 语义 | Jobs | fio MiB/s | sync后 MiB/s | fio耗时 s | sync耗时 s |
|---|---:|---:|---:|---:|---:|
| 首次写 | 1 | 226.8 | 144.3 | 9.32 | 4.87 |
| 首次写 | 16 | 352.8 | 184.2 | 6.26 | 4.86 |
| overwrite | 1 | 579.0 | 492.3 | 3.91 | 0.25 |
| overwrite | 16 | 290.9 | 262.9 | 7.48 | 0.31 |

单线程overwrite的fio带宽是首次写的2.55倍，含sync带宽是3.41倍。16线程时，
overwrite的fio带宽反而只有首次写的0.82倍，但含sync带宽仍为1.43倍。首次写从
1线程到16线程提高1.56倍；overwrite从1线程到16线程下降到0.50倍。因此“EXT4和
rNVPCe曲线总体都不随线程单调上升”这个观察成立，但不能据此认为二者的扩展性瓶颈
相同；rNVPCe overwrite有独立而明确的前台并发放大。

## SSD分配是否消失

以下来自只开启pipeline计数器的诊断ISO。累计时间跨线程重叠，不能相加得到wall time。

| 语义 | Jobs | SSD分配页数 | 分配元数据页数 | 分配记录发布页数 | 分配记录回收页数 |
|---|---:|---:|---:|---:|---:|
| 首次写 | 1 | 524,288 | 524,288 | 524,288 | 524,288 |
| 首次写 | 16 | 524,288 | 524,288 | 524,288 | 524,288 |
| overwrite | 1 | 0 | 0 | 0 | 0 |
| overwrite | 16 | 0 | 0 | 0 | 0 |

overwrite仍调用16,384次`allocate_rnvpce_foreground_blocks(0, ...)`包装入口，但
`ssd_allocate units=0`且group reserve、block allocate、allocation metadata、
allocation-record publish/reclaim的count和units都为0。这是空操作入口，不是实际
SSD元数据分配。

## 前台时间分解

| 语义 | Jobs | 前台总累计 s | persist事务累计 s | SSD allocate累计 s | 其余规划/协调 s |
|---|---:|---:|---:|---:|---:|
| 首次写 | 1 | 7.255 | 1.800 | 0.099 | 5.357 |
| 首次写 | 16 | 84.462 | 41.117 | 0.378 | 42.967 |
| overwrite | 1 | 4.728 | 1.578 | 0.001 | 3.149 |
| overwrite | 16 | 109.303 | 33.230 | 0.001 | 76.072 |

“其余规划/协调”是`foreground total - persist - SSD allocate`的差值，不是一个独立
探针；它还包含范围构造、容量预留和少量外围逻辑。overwrite中这个差值从单线程平均
约192 us/request增加到16线程约4.64 ms/request，约24倍，是负扩展的主要证据。

细粒度探针进一步给出：overwrite fast-path body为89.5 us/request（1线程）和
2.04 ms/request（16线程）。inode publication guard的获取等待只有0.17和0.21 us，
所以不是同一inode发布锁的排队；每个job使用不同inode。guard持有时间分别为214.5 us
和6.52 ms，说明持锁范围内的事务本身在并发下变慢。

## 当前仍执行的操作

1. `write_rnvpce_at()`先对请求内32页逐页调用`current_page_mapping()`，后者每次读取
   NVM inode authority并路由extent tree。
2. `range_write_new_pages()`为了计算NVPage容量，再逐页调用一次
   `current_page_mapping()`。
3. existing-baseline fast path提交前，再对32页逐页调用`lookup_tree_extent()`校验
   SSD目标没有变化。一个128 KiB请求最多形成96次逐页树查询/校验。
4. fast path仍为32页分别取得staging NVPage、写入数据和descriptor，然后用一个
   `publish_redirect_batch()`批量发布；批量边界存在，但页内工作没有消失。
5. fast path不调用`replace_tree_extents()`，因此不会修改SSD extent baseline；但仍对
   每个128 KiB请求调用一次`publish_inode_state_with_generation()`，发布NVM中的inode
   authority generation。
6. migration completion对已有SSD baseline产生`new_baseline=None`，不会调用
   `publish_claimed_ssd_baseline_extents()`。当前`nv_extent_publish`探针名称已经过时，
   它包围的是redirect finalization/reclaim，不应解读为每批重新发布SSD extent。

因此，答案是：overwrite已经消除了SSD块和分配元数据操作，也没有重复发布SSD extent
baseline；但仍有大量逐页校验、NVPage/descriptor持久化、redirect发布和每请求一次的
NVM inode authority发布。下一步应把同一连续SSD extent上的范围查询、容量计算和
fast-path校验合并为一次range操作，而不是继续优化已经为0的SSD分配阶段。

## 证据

- 可复现脚本：`docs/experiments/scripts/rnvpce-overwrite-20261030/`。
- release原始数据：
  `/home/zn/asterinas-build-env/rnvpce-overwrite-20261030/release/`。
- pipeline诊断：
  `/home/zn/asterinas-build-env/rnvpce-overwrite-20261030/profile/`。
- 细粒度诊断：
  `/home/zn/asterinas-build-env/rnvpce-overwrite-20261030/detail/`。
- 三个ISO SHA256分别为：release
  `eb9c1cae0d30f75faebf85b4266c4c14ad3a22a544696b5ee14f34eb1dd06130`，pipeline
  `8c13129a5a39e9042adff2b4e95accc740dd4e2615b838e11fa351030d4e1fde`，细粒度
  `f2ad160be5746342aa52841a7cbc1c5f48b2278a1bb8dd4594c5454500ed9355`。
- 吞吐结论只使用关闭插桩的release ISO；两个诊断ISO仅用于阶段计数和相对归因。
