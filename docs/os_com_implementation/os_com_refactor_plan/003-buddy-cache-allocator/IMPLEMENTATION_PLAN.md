# EXT4 组级分配器 Implementation Plan

> **For agentic workers:** 执行时使用 executing-plans，按任务推进；本文件只记录计划，不授权现在开始修改内核。检查框仅在取得实际证据后勾选。

**Goal:** 实现组级分配锁、内存空闲空间索引与容量预留计数，解除现有超级块大锁对整个块分配过程的串行化。

**Architecture:** 组内维护可重建的空闲索引及 pending-free 排除状态。独立容量账本授权请求，通过组级状态与 journal 发布协议完成分配和释放；暂不引入 PA。

**Tech Stack:** 当前 rebuild 的 Rust/no_std、EXT4 journal、OSDK ktest、Docker/QEMU、fio、e2fsck。

**Spec:** [README.md](README.md)，2026-10-17 用户确认的功能范围；其中技术细节是实现建议，编码前须通过任务 1 的一致性审计。

## Global Constraints

- 代码目标：`/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild`。
- 不实现 inode/locality-group PA，不依赖 delalloc，不修改磁盘格式。
- 保留 metadata reserve、rNVPCe reservation、journal pin/revoke、校验和和错误中止语义。
- 当前工作树已有缓存压力等未提交改动，实施前保存基线清单，不能覆盖或混入无关提交。
- 真实设备实验只能使用既有明确授权的专用分区 `/dev/nvme1n1p1`，不得操作整盘或其他分区。

## Review Focus

- 普通分配与 rNVPCe 预留同时发生：不能超额授权（任务 3）。
- 同一空闲区间并发争抢：只能有一个所有者（任务 2、4）。
- bitmap 已释放但事务尚未提交：不得进入可分配索引（任务 5）。
- 两个组共享一个 GDT block：字段更新不能互相覆盖（任务 4、6）。
- 部分分配后错误、abort 或掉电：额度、索引及恢复后的磁盘状态不能分歧（任务 3—6）。

## 文件分工

下列路径相对 rebuild 根目录，新增模块名为建议，不是已有接口。

| 文件 | 责任 |
| --- | --- |
| `kernel/src/fs/fs_impls/ext4/allocator/mod.rs`（新增） | 容量账本、RAII reservation、组分配协调 |
| `kernel/src/fs/fs_impls/ext4/allocator/free_space.rs`（新增） | 每组 buddy/分层空闲索引及纯算法测试 |
| `kernel/src/fs/fs_impls/ext4/mod.rs` | 模块声明 |
| `kernel/src/fs/fs_impls/ext4/block_group.rs` | 组级锁、索引构建、分配和释放的原子状态变换 |
| `kernel/src/fs/fs_impls/ext4/fs.rs` | 分配/释放入口、容量初始化、rNVPCe 授权和统计/同步接入 |
| `kernel/src/fs/fs_impls/ext4/journal/{mod.rs,transaction.rs,commit.rs,checkpoint.rs}` | pin 完成、计数与镜像发布协议 |
| `kernel/src/fs/fs_impls/ext4/test_utils.rs` 及相关模块的测试 | 多组、近满盘、事务故障与并发夹具 |

## Task 1：固定状态协议和锁序

- [x] 保存实施起点 commit、工作树 diff、构建配置；核对上述路径及全部 alloc/free/reserve 调用方。
- [x] 列出每个计数的单位、初始化来源、增减时点和权威状态；明确 pending-free 与物理空闲统计的关系。
- [x] 写出 allocate、free、commit、abort 的状态转换表，给出超级块计数在事务间发布的协议。
- [x] 画出包含 inode、superblock、group、journal、GDT buffer 的锁序；确认所有旧入口一起迁移，不保留反向嵌套边。
- [x] 审查通过后固定后续任务的 Rust 接口。尤其明确 reservation 的 claim/consume/drop，以及 pending-free 完成批次的幂等身份；未解决事务镜像归属之前不接入生产路径。

## Task 2：实现组内空闲索引

- [ ] 先写失败测试：空组/满组、不同长度对齐、碎片合并、组尾、系统区、BLOCK_UNINIT、pending-free 排除。
- [x] 从权威 bitmap 构建索引，实现 goal 搜索、精确占用、释放合并和组摘要。
- [x] 用确定性分配/释放序列对照简单 bitmap 模型，逐步检查空闲数及区间所有权。
- [x] 跑索引 ktest，确认通过；单独审阅该模块后进入集成。

## Task 3：实现容量账本及 RAII reservation

- [ ] 先写失败测试：普通预留竞争、metadata floor、rNVPCe 与普通请求竞争、部分消费、drop、错误和整数边界。
- [x] 实现统一授权入口；多字段条件必须由一个一致性协议保护，不能用无协调的原子快照判定。
- [ ] 在未接入组内修改前完成账本测试，断言所有正常/错误出口无额度泄漏，无重复消费。
- [x] 对照现有 statfs 与 rNVPCe 接口明确哪些返回物理空闲、哪些返回可授权容量。

## Task 4：接入分配路径

- [ ] 先写不同组并行推进、同组同区间竞争、GDT 共块更新和冷 metadata capture 故障测试。
- [x] 将 `alloc_blocks_with_intent` 改为容量授权 → 组级索引分配 → 事务发布 → 消费实际额度。
- [x] 移除跨组搜索和整个组内分配外围的 `super_block.write()`；按任务 1 协议更新磁盘计数。
- [x] 接入所有数据块与 extent 元数据块调用方，保留部分分配、goal、journal credits 和系统区检查。
- [x] 验证不能回滚的局部修改正确 abort，不能把不确定块重新放回空闲池。

## Task 5：接入释放、提交与恢复

- [ ] 先写 free 与 alloc 交错、提交前禁止复用、提交后可用、重复完成及 abort 测试。
- [x] 在组保护范围内发布 pending-free 和位图变化；提交完成后幂等更新索引与容量。
- [x] 消除提交回调中的 journal/group 锁环；跨组批次逐组应用。
- [x] 恢复后从磁盘权威状态重建索引与容量，验证非 journal 路径不会永久保留 pin。

## Task 6：集成与性能验收

- [x] 执行现有 EXT4 回归和新增 allocator 测试；编译通过不能替代运行结果。
- [ ] 多文件并发扩展、释放、近满盘重试后检查数据与计数；增加事务关键阶段的故障/恢复测试。
- [x] 同步卸载并运行 e2fsck；结构检查与数据正确性检查分别记录。
- [x] 冻结前后 ISO、源码清单及 fio 参数；运行 128KB 顺序扩展写 1/16 线程、预填充 4KB 随机覆盖写对照。
- [x] 记录锁等待/持有、搜索与预留等待指标；报告仍存在的 journal/设备瓶颈，不预设吞吐提升。
- [x] 完成代码审阅并把验证证据写入本目录及项目 STATUS；仅按实际通过的范围调整状态，不自动推送。

## 执行与验证命令约束

使用当前可用 Docker 开发容器和仓库规定的 `cargo osdk test`/release 构建流程；先验证容器、工具链和测试选择器，不复用未经核实的历史命令。ktest 与性能测试的 SMP 配置分别记录；不得让并行构建覆盖对照实验的冻结 ISO。

本计划是阶段级实施记录。任务 1 是必须产出的协议设计交付，不允许把未知的 journal 计数发布细节交给实现过程临时猜测。

## 2026-10-17 实施结果

核心实现已完成。Task 1 协议见 `/home/zn/asterinas-build-env/ext4-group-allocator-20261017/protocol.md`，完整执行记录见 `progress.md`；实现和测试证据见 [报告](../../../experiments/reports/EXT4-GROUP-ALLOCATOR-2026-10-17.md)。本次采用分层连续区间摘要和新鲜 pin 快照的惰性发布方案；Task 5 的“提交更新索引/容量”由提交退休 pin、下次授权/搜索重新计算有效可用集合完成，不新增回调。

- 已完成：组 gate、索引、容量授权/消费/drop、rNVPCe 统一 promise、组外超级块发布、释放 pin 与恢复重建。
- 回归：593 passed / 0 failed / 158 filtered；新增 8 项测试。代码审查发现的临时 claim 误报 rNVPCe ENOSPC 已经 RED -> GREEN 修复。
- 性能：12 个 16GiB fio 样本完成，无稳定加速；顺序单线程均值 -12.8%，16 线程 -1.5%。单独记录锁计时，未用带探针运行替代吞吐样本。
- 数据及结构：重新挂载后完整 4GiB CRC32C 通过；累计 15 次 e2fsck 全部 exit 0。
- 默认关闭探针的最终 ELF 与实际吞吐测试 ELF 按 SHA256 完全相同；原有无关工作树文件哈希保持不变。

保留未勾选项表示验证矩阵没有全部完成，不表示相关核心功能未实现。已有回归覆盖 BLOCK_UNINIT、系统区、pin/revoke、metadata floor、部分分配/错误、abort 和崩溃重放；但尚未为以上每一项单独增加新的 RED 测试，也未穷举整数边界、所有 reservation 错误出口、冷 capture 关键阶段的新故障组合。后续应补这些专项测试及 pin-heavy 释放压力，不能将当前结果称为全故障矩阵验证通过。

后续性能分析先拆开组内索引维护、journal capture 和每组请求分布；不以继续缩短超级块锁作为已证实的性能方案。inode/locality-group PA 仍不在本次范围。
