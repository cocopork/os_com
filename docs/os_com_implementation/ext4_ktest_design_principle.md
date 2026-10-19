# EXT4 ktest 设计与开发验证原则

本文总结 `asterinas_ext4/ext4-research` 当前采用的内核测试方法，以及每轮代码实现完成后的验证顺序。它约束研究主线的日常开发；完整环境和资源隔离要求见仓库根目录的 `ASTERINAS_EXT4_TEST_ENVIRONMENT.md`。

## 0. 测试规模原则（精简优先，2026-09-18 起）

之后的功能实现，**ktest 不要太多**，不必穷举所有边界情况。验收的最低门槛是：

1. 能通过 fio 测试；
2. 能通过并发测试；
3. 每个模块提供的基础功能正确。

只为核心不变量与基础功能写最小确定性 ktest；边界、错误路径、故障矩阵等扩展覆盖按需补充，不作为每个功能实现的硬性要求。

## 1. ktest 的定位

ktest 是运行在 Asterinas 内核环境中的白盒测试，不等同于宿主机上的 Rust `cargo test`，也不等同于启动、用户态回归或性能测试。

- 测试模块使用 `#[cfg(ktest)]` 编译，测试函数使用 `#[ktest]` 注册。
- `cargo osdk check --ktests --package aster-core` 只检查 ktest 配置能否编译，不执行测试。
- `make ktest` 最终调用 `cargo osdk test`，启动测试内核并由 `[ktest runner]` 在 QEMU/KVM 中执行测试。
- ktest 负责验证内部数据结构、不变量、状态转换、锁与并发协议、错误传播、回滚和内存设备上的磁盘状态。
- syscall/VFS 用户语义、真实格式化和挂载、Linux 互操作、`e2fsck`、真实块设备 I/O 和性能不由 ktest 单独证明，必须由后续验证层覆盖。

当前 EXT4、NVPool 及其直接依赖的 PageCache 已形成大量 ktest。测试通常与实现放在同一模块的 `tests` 子模块中，使私有状态和中间转换可以被直接检查；公共 EXT4 测试夹具集中在 `kernel/core/src/fs/fs_impls/ext4/test_utils.rs`。

## 2. 测试夹具设计

### 2.1 内存块设备

`Ext4MemoryDisk` 使用零初始化的 frame segment 模拟块设备，并实现正常的异步 BIO 提交接口。它有三个用途：

1. 不依赖 initramfs、`mke2fs` 或真实磁盘，在内核测试中快速构造可重复的 EXT4 状态。
2. 统计 read、write、flush 次数以及最大 BIO segment 数和大小，用于验证 I/O 路径和 BIO 聚合结果。
3. 通过 `set_fail_writes()` 注入稳定的写失败，验证 metadata writeback、回滚、错误可见性和重试路径。

内存设备立即完成 BIO，因此适合验证文件系统状态机和提交边界，但不能证明真实 virtio 队列时序、设备延迟或掉电保持。

### 2.2 最小 EXT4 镜像

`Ext4FixtureBuilder` 直接在内存中构造最小 EXT4 镜像：superblock、GDT、block bitmap、inode bitmap 和 inode table 使用固定布局。构造器按测试需要启用以下状态：

- 正常或耗尽的 block/inode bitmap；
- 精确数量的剩余块，用于操作中途 `ENOSPC`；
- 128/256 字节 inode、预占 inode、不同 feature bits；
- extent 或 ext2 indirect 映射模式；
- EXT2/EXT4 mount flavor 和挂载选项；
- 普通 nojournal 或挂接 DRAM-backed NVPool 的文件系统。

夹具同时提供 raw inode、数据块和磁盘字节的读写入口。测试不仅检查 API 返回值，还应检查 bitmap、inode、extent、NV frame 或 SSD home block 是否处于协议要求的状态。直接修改 raw image 后必须使相关 inode/metadata cache 失效，避免测试误读旧缓存。

### 2.3 ktest 初始化

需要时间戳、inode generation 或线程调度的测试必须显式调用 `crate::time::clocks::init_for_ktest()`；公共 EXT4 fixture 在 `build()` 中统一完成该初始化。测试应优先复用公共夹具，不自行复制一套不完整磁盘布局。

## 3. 测试分层

### 3.1 数据结构与局部算法

直接测试 extent tree/path、extent status tree、block pointer tree、metadata cache 和统计结构。重点覆盖：

- 空树、边界块、相邻区间、重叠区间和最大长度；
- insert、split、merge、remove、truncate 后的不变量；
- cache hit/miss、状态转换和 reservation 计数；
- 成功路径与 `EINVAL`、`ENOSPC`、`EIO` 等负路径。

这类测试应短、确定、无外部设备，失败时能够直接定位到一个算法不变量。

### 3.2 组件状态机与并发协议

PageCache dirty/writeback、后台 writeback controller、completion engine、NVPool allocator 和 reclaim 使用白盒状态机测试。设计原则是：

- 检查每条合法转换和关键非法转换；
- 检查 credits、pin、引用、dirty page 等资源在成功与失败后都精确归还；
- 用原子变量、wait queue、显式 release 条件和线程 `join()` 建立确定的 happens-before；
- 不用真实时间延迟猜测线程是否到达某一步；必要时只用 `yield_now()` 等待可观察状态；
- 覆盖失败通知、等待者唤醒、shutdown 拒绝、重试和多错误消费顺序。

### 3.3 EXT4 纵向切片

纵向测试从挂载后的 inode 操作出发，贯穿 allocation、extent、PageCache/writeback、metadata cache 和块设备。它应同时检查：

- 用户可见结果，例如读回数据、文件大小和错误码；
- 内存状态，例如 ES tree、dirty/writeback 标记和 completion fence；
- 持久状态，例如 raw inode、bitmap、extent 和 home block；
- I/O 行为，例如 flush 次数、BIO 数量、segment 数和写入大小。

只检查“函数返回 `Ok`”不足以证明纵向协议正确。

### 3.4 故障与恢复模型

故障测试在确定的协议边界注入失败，例如分配后、引用发布前后、BIO 提交失败、post-I/O metadata 更新失败和 NV commit/clear 阶段。每个用例应回答：

1. 哪一步被认为已经持久？
2. 失败后哪个副本是有效状态？
3. 哪些资源必须保留，哪些资源允许回收？
4. 重试或恢复后是否幂等？

DRAM model backend 的 working/durable 双视图可以在同一 VM 中模拟崩溃和重建，但只能证明抽象协议，不能表述为真实 NVDIMM 掉电测试。

## 4. 单个功能的测试设计原则

1. **一个测试对应一个主要不变量。** 测试名应描述条件和期望结果，而不是复述函数名。
2. **先构造最小反例。** 修复 bug 时先保留能够稳定失败的 RED 用例，再实现修复并保存 GREEN 结果。
3. **检查状态而不只检查返回值。** 特别是 allocation、truncate、writeback 和恢复路径，要核对计数、bitmap、tree 和 raw disk。
4. **正负路径成对。** 至少覆盖正常完成、资源不足、I/O 失败、非法状态和重试；涉及持久状态时还要覆盖中断点。
5. **并发测试使用显式同步。** 测试必须能够证明线程交错已经发生，不能依赖 sleep 或偶然调度。
6. **不隐藏错误。** 除非协议明确把某个错误定义为可重试压力信号，否则不能在测试或实现中吞掉错误。
7. **测试保持确定性和隔离性。** 每个用例创建自己的 fixture、线程和状态，结束前排空 worker 并回收资源。
8. **测试范围随改动扩大。** 私有算法改动至少运行定向测试；共享 PageCache、块层、锁序和持久协议改动必须扩大到相关模块和系统级测试。

## 5. 每轮代码实现后的验证流程

下面的顺序是门禁，不应把后一步成功反推为前一步已经通过。命令应在固定的 `asterinas-ext4-research` 容器和 Research 源码树中执行，正式功能验证统一启用 KVM。

### 5.1 开发中的最短反馈环

1. 为新增行为或 bug 写一个能够失败的定向 ktest，并确认 RED 原因与预期一致。
2. 完成最小实现后，重新运行同一测试，确认至少 `1 passed; 0 failed`。
3. 运行同模块中与该状态机直接相关的测试，防止局部修复破坏相邻转换。

定向运行通常使用唯一的完整测试函数名作为 filter，例如：

```bash
make ktest ENABLE_KVM=1 \
  CARGO_OSDK_TEST_ARGS='group_zero_allocation_sync_changes_nv_bitmap_not_ssd_bitmap'
```

当前 runner 对某些模块路径过滤并不可靠，因此不能仅凭命令退出码判断筛选成功，必须检查 `[ktest runner]` 的实际用例名和通过数量。

### 5.2 每个 Task/提交前的基础门禁

```bash
cargo fmt --all -- --check
cargo osdk check --package aster-core
cargo osdk check --ktests --package aster-core
```

然后至少运行：

- 本 Task 新增或修改的全部定向 ktest；
- 被修改共享组件的相关 ktest；
- 若改动非默认 workspace crate 或通用库，运行 `make test` 覆盖其宿主机 Rust 测试；
- 对失败修复保留 RED/GREEN 日志。

这一级证明格式、普通内核配置、ktest 配置和目标行为均可工作。只有 `cargo osdk check --ktests` 通过时，不能声称 ktest 已执行。

### 5.3 里程碑门禁

完成一个实现里程碑后，在基础门禁之外继续执行：

1. 未筛选的完整 `make ktest ENABLE_KVM=1`，或明确记录阻止全量通过的既有失败；
2. KVM boot smoke，确认普通内核能够进入启动/测试入口；
3. 若改动 EXT4 用户语义、挂载、目录、read/write/fsync/truncate，则运行 EXT4 用户态 regression；
4. 若改动盘上 metadata、分配和持久图，则运行对应 fault/crash matrix，并在适用时增加重挂载、Linux 互挂或 `e2fsck`；
5. 若改动 PageCache 或块层共享接口，则运行其自身 ktest，并回归普通 nojournal EXT4，不能只测 NVPool 分支。

### 5.4 性能验证

fio、Filebench、LevelDB、BIO/QD 追踪和裸设备测试位于正确性门禁之后。性能变好不能替代 ktest 或 regression；性能结果也不能证明掉电一致性。

## 6. 按改动类型选择验证范围

| 改动类型 | 最低 ktest | 追加验证 |
| --- | --- | --- |
| extent/ES tree 局部算法 | 新增定向用例 + 相关 tree/status suite | EXT4 纵向 allocation/truncate 测试 |
| allocator、bitmap、inode metadata | 资源不足、回滚、raw metadata 检查 | regression、重挂载/`e2fsck` |
| PageCache dirty/writeback | PageCache 状态、重脏、并发与错误测试 | nojournal buffered write/fsync、fio smoke |
| completion/background worker | credits、fence、失败通知、shutdown、并发交错 | 全量 ktest、KVM boot、writeback smoke |
| NVPool frame/state/reclaim | 容量、pin、状态机、并发和回收测试 | model recovery/fault matrix |
| EXT4-NVPool 纵向协议 | NV/SSD 双侧状态和发布顺序测试 | 重建、drain、普通 nojournal 回归 |
| VFS/syscall 可见语义 | inode/fixture 纵向 ktest | 用户态 EXT4 regression/xfstests |
| 块层或 virtio | 组件 ktest 和 EXT4 I/O 统计测试 | raw-device 与文件系统 QEMU smoke |

## 7. 结果判定与证据纪律

测试报告必须记录源码 commit/dirty 状态、完整命令、KVM/内存等关键参数、日志路径和 runner 汇总。

- `1 passed; 0 failed; N filtered out`：可作为该定向测试通过证据。
- `0 passed; 0 failed; N filtered out`：过滤器未命中，不是通过。
- 只有编译日志，没有 `[ktest runner]`：只能记为 ktest 编译通过。
- QEMU 未启动或未进入 runner：记为环境/启动阻塞，不能记为测试通过，也不能直接归因于 EXT4。
- 全量运行中存在失败或超时：不能宣称全量全绿；可以单独陈述已完成的定向测试，并明确剩余测试债务。
- 已知无关失败必须给出具体测试名和复现结果，不能用“环境问题”笼统跳过。

日志建议保存在 `research1/implementation/testing/results/research/<timestamp>-task<id>-<scope>/`，文件名包含 test scope 和 `red`/`green`。README 汇总有效证据、无效尝试、未覆盖范围和下一步补跑项；原始日志不覆盖。

## 8. 完成判定

一个 Task 只有在以下条件同时满足时才可提交和验收：

1. 新行为有能够命中并通过的 ktest，bug 修复保留 RED/GREEN 证据；
2. 格式、普通配置和 ktest 配置编译通过；
3. 所有受影响的定向/相关 ktest 实际执行且通过；
4. 未执行或未通过的扩大验证已明确记录，且没有被错误表述为成功；
5. 按改动影响面完成对应的 boot、regression、fault matrix 或系统 smoke。

全量 ktest、用户态 regression 和性能实验解决的问题不同。当前开发流程的核心不是“每轮把所有测试都跑一遍”，而是先用最小确定性 ktest 锁定不变量，再按共享范围和持久化风险逐层扩大验证，直到覆盖该 Task 实际可能破坏的系统边界。
