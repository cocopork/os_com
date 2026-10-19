# Orderless Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement remaining unchecked tasks. 核心批次协议已经实现；实际状态以 README.md 的“实施状态”为准。实现时读取 README.md 及仓库 AGENTS.md。

**Goal:** 以 DRAM 迁移批次异步提交 SSD data/metadata，完成后一次 flush，再安全发布和回收，恢复不信任未完成的 SSD 映射。

**Architecture:** 持久 NVM 图决定数据归属，DRAM batch 只记录请求进度；块版本化 metadata 快照解决共享写出。挂载恢复先建立保护集合与分配门控，后台只回收已核对垃圾。

**Tech Stack:** safe Rust、Asterinas block/BIO、NVM 双副本与 soft update、Docker/OSDK ktest、QEMU/KVM、fio。

**Spec:** [README.md](README.md)，日期2026-10-19。

## Global Constraints

- 首版只切换 rNVPCe nojournal；native/rNVPC WAL 保持原路径。已有文件先完成持久注册，禁止 namespace/truncate/native 并发修改。
- 全局128页在途；批次最多128页、256个唯一metadata块；超限提交前拆批。
- 正常非空批次一个SSD flush，分配阶段无SSD flush；NVM持久化顺序保持。
- DRAM事务不持久化请求进度；NVM恢复基线、完整权威映射和目标归属不能省略。
- 禁止锁内阻塞I/O；回调不分配、不发布NVM、不回收页。
- rNVPCe rTIO 只有 orderless migration 路径，不得恢复运行时开关或 010 ordered fallback。历史 A/B 镜像只用于性能归因。不得以无 flush、增大在途窗口、降低持久性模拟成本冒充收益。
- 当前工作树含既有改动，执行前保存状态，禁止reset/覆盖；未获提交请求不自动commit/push。

## Review Focus

1. 请求立即完成和部分提交失败导致的计数穿零/双重释放：Task2。
2. 同一metadata块或data目标版本倒序覆盖：Task3/4。
3. SSD位图、extent、inode部分落盘或撕裂，而NVM仍权威：Task1/5。
4. flush成功后部分NVM发布，重复恢复或新版本并发覆盖：Task4/5。
5. 后台扫描与分配、共享inode表邻居、注册不完整：Task1/5。

## 文件与接口分工

以下为拟新增接口，不声称代码已有。类型位于所列模块，具体容器可按现有风格实现，状态和所有权契约不可省略。

| 文件（仓库相对路径） | 职责 |
| --- | --- |
| `kernel/src/fs/fs_impls/ext4/migration/batch.rs`（新） | 批次状态机、请求票据、完成/flush门控 |
| `kernel/src/fs/fs_impls/ext4/migration/metadata.rs`（新） | 不可变块快照、版本与跨批次依赖 |
| `kernel/src/fs/fs_impls/ext4/migration/recovery.rs`（新） | 挂载核对、保护集合及垃圾回收 |
| `kernel/src/fs/rnvpc/migration_authority.rs`（新） | 注册基线、完整NVM权威图与恢复校验 |
| `kernel/src/fs/fs_impls/ext4/migration.rs` | 准备/提交/完成编排，删除新路径前置SSD同步 |
| `kernel/src/fs/fs_impls/ext4/fs.rs`、`block_group.rs` | 挂载门控、局部metadata收集、分配/释放接入保护集合 |
| `kernel/src/fs/fs_impls/ext4/inode/mod.rs`、`inode/extent_manager/mod.rs` | native导出快照、权威读路径、共享inode表保护 |
| `kernel/src/fs/rnvpc/{layout.rs,persist.rs,write_transaction.rs,frame/migration.rs}` | 格式扩展（若需要）、批次持久发布、代际验证和回收 |
| `kernel/src/fs/fs_impls/ext4/orderless_tests.rs`（新）、`rnvpc/rnvpce_integrity_tests.rs` | 联合故障模型与功能回归；在各mod.rs声明 |
| `docs/os_com_implementation/test/rnvpce-buffer-compare/` | A/B镜像绑定、实验参数和证据 |

### Task 1：恢复权威与启用条件

**Interfaces:**
- `MigrationAuthority::register_existing_inode(pool: &Arc<Rnvpc>, fs: &Ext4, inode: u64) -> Result<RegisteredInode>`：在写出/迁移前持久保存完整映射、大小、必要属性及共享inode块恢复基线；失败不能启用。
- `MigrationAuthority::recover(pool: &Arc<Rnvpc>, fs: &Ext4) -> Result<AuthoritySnapshot>`：只认NVM有效版本；snapshot含注册inode、NV/SSD/hole映射、pending targets和恢复基线。
- `RecoveryGate::reconcile(fs: &Ext4, snapshot: &AuthoritySnapshot) -> Result<ProtectedAllocationSet>`：完成保护与核对才开放已证明安全的空闲范围。

`RegisteredInode`是绑定fs_id/inode/持久注册代际的凭据；`AuthoritySnapshot`是有效注册文件完整映射、pending targets及恢复基线的只读集合；`ProtectedAllocationSet`维护块范围、保护原因与分配代际。三个类型分别由本任务在authority/recovery模块定义；fs提供绑定设备，不得替换成未校验的全局设备。

- [ ] 写测试 `orderless_registration_authority`：未注册/注册中断拒绝启用，NV覆盖SSD提前发布映射，完整文件映射与共享inode邻居恢复；pool/device不匹配拒绝。
- [ ] 用现有ktest入口运行并确认新断言RED，记录失败原因。
- [ ] 实现注册/恢复及显式模式选择；必要格式升级与旧池拒绝，新快路径保持关闭。
- [ ] 测试SSD位图标空闲但NVM引用的块不可重新分配；不可解释重叠/损坏拒绝可写挂载；GREEN后记录证据。

### Task 2：DRAM批次与flush协调

**Interfaces**（`migration/batch.rs`）：
- `MigrationBatchId { boot_epoch: u64, sequence: u64 }`；`RequestKind::{Data, Metadata}`。
- `MigrationBatch::new(id, page_limit: usize, metadata_limit: usize) -> Result<Arc<MigrationBatch>>`。
- `register_request(self: &Arc<Self>, kind: RequestKind) -> Result<RequestTicket>`；ticket完成/提交失败二选一、恰好一次，不可复制。
- `seal(&self) -> Result<()>`；`RequestTicket::complete(self, status: BioStatus)`；提交拒绝由持票方消费ticket并记录失败。
- `try_start_flush(&self) -> bool`：仅封口、非空、全部成功完成且尚未flush时返回true；原子获得唯一资格。
- `finish_flush(&self, status: BioStatus) -> Result<()>`；成功后产生仅能由完成工作者消费的 `DurableBatch` 凭据，回收接口只接受此阶段的批次。

- [ ] 测试 `orderless_batch_lifecycle` 一项内覆盖即时回调、先完成后seal、空批次、重复flush竞争、失败后仍排空DMA、唤醒不丢失。
- [ ] RED后实现状态机和预分配回调资源；明确block提交返回Err是否仍可能回调的实际契约，并用适配器确保单一所有权。
- [ ] 增加并发测试，断言无提前Durable、无双重ticket完成、无失败页回收；GREEN后记录。

### Task 3：局部metadata快照与异步导出

**Interfaces**（`migration/metadata.rs`）：
- `MetadataVersion { block: u64, generation: u64 }`。
- `MetadataSnapshot`：唯一版本、不可变块内容、包含的修改及依赖批次；`MetadataDependency`标识批次所需版本下界。
- `MetadataWriteback::capture(&self, batch: &Arc<MigrationBatch>, images: Vec<MetadataImage>) -> Result<Vec<MetadataDependency>>`：收集本次分配/native extent/inode改变，超过256块提交前拆批。
- `MetadataWriteback::submit_ready(&self, fs: &Ext4) -> Result<usize>`：同块最多一版在途，完整新快照覆盖之前有效修改；回调满足所覆盖依赖。

`MetadataImage { version: MetadataVersion, bytes: Vec<u8> }`由元数据修改路径生成，bytes必须等于一个文件系统块大小；capture接管缓冲并生成依赖。提交接口不得绕过Task2请求票据。多批次依赖与覆盖关系在调度器内维护，不要求调用者猜测完成状态。

- [ ] 测试 `orderless_metadata_versions`：两批次共享位图/extent/inode块，反向准备和即时回调，旧版完成不清新版dirty，新版不能先于旧版写出，邻居字段保持。
- [ ] 测试data未完成也能提交metadata；所有需要的metadata依赖均完成才允许flush；一个快照被多批次引用不丢通知。
- [ ] 将现有extent导出拆为“锁内准备快照 / 锁外异步提交”，按块收集，不能在热路径调用全文件系统`sync_metadata()`；不在自旋锁内做I/O。
- [ ] RED→GREEN，记录受影响元数据种类；任何未覆盖种类阻止启用。

### Task 4：接入rTIO并发布/回收

**Consumes:** Task1保护集合、Task2 batch/DurableBatch、Task3依赖与快照；010批量目标/额度/回收接口。
**Produces:** 新模式下的 `prepare_nv_targets` / 完成工作者；一个批次的完整结果及可解释错误。

- [ ] 测试 `rnvpce_batch_pipeline`：128页跨多个inode；data/metadata乱序完成；准备阶段flush=0，成功批次flush=1；flush前NV引用和容量不变。
- [ ] 先持久记录目标身份，再异步提交；在同一逻辑页/SSD目标上实施版本排他，保存完整NV页直到flush与发布结束。
- [ ] completion收集所有依赖后交给唯一flush协调者；等待期间不持前台锁；保持128页总预算并允许另一批次准备/提交。
- [ ] `DurableBatch`门控NVM叶发布、额度和描述符清除；ESTALE/部分叶失败保持独立结果，不能因批次flush成功清掉新NV版本。
- [ ] 在同一功能测试中注入data/metadata/flush错误，确认排空、错误传播和容量等待者唤醒；native/rNVPC保持原路径，GREEN。

### Task 5：联合掉电模型、重迁移与后台空间核对

**Interfaces:**
- 可重排SSD测试设备：区分已提交/已完成/持久介质，允许completion不持久；flush成功持久化此前已完成写入，支持data/metadata任意子集落盘及扇区撕裂。
- `RecoveryGate::reconcile`消费恢复后的NVM与SSD介质；旧运行请求被终止，不能混入重启后完成。
- 后台回收：扫描候选，组锁下按分配代际和最新保护集合复核；只有无引用、无lease、无在途的块可释放。

- [ ] `orderless_crash_recovery` 覆盖README崩溃矩阵，每个切点连续恢复两次；逐页不同内容CRC/逐字节断言、权威来源、无重复分配、额度守恒。
- [ ] 覆盖metadata先落盘、data后落盘两种极端；inode表/位图/GDT撕裂从基线重建或拒绝挂载，不允许默默native fallback。
- [ ] `orderless_gc_races`：孤儿data/extent节点、共享节点、scanner快照后新分配、free后再掉电；有效块永不被回收，未完成扫描区永不提前分配。
- [ ] 实现完整核对和后台回收后再打开实验配置入口；未满足Task1注册范围的请求明确报错。
- [ ] 组合模型GREEN后执行完整内核回归、原生EXT4/rNVPC WAL回归与独立只读审查；先解决正确性问题再做性能。

### Task 6：同步边界、实验与文档

- [ ] 对drain/sync/unmount补充覆盖范围、flush完成与最终native导出测试；clean卸载后原生只读挂载逐文件校验，不能仅看e2fsck。
- [ ] 构建关闭诊断的010对照与新版镜像，保存完整源码差异（含新增文件）、ISO hash、命令行及设备缓存/flush配置。
- [ ] 独立诊断构建分开测量metadata准备/提交/完成、data完成、flush排队/服务、NVM发布与回收；记录批次页数分布、in-flight、实际flush次数，避免全局计数混淆来源。
- [ ] 沿用2GiB、64MiB、128页窗口、1/16线程、4KiB随机/128KiB顺序、CPU隔离；前后各三轮，交替顺序，禁止测试时并行编译。
- [ ] 每点确认完整写入、fio错误0、drain和卸载成功、e2fsck；单独报告恢复模型边界、native读回和真实掉电尚未验证项。
- [ ] 保存 `docs/experiments/reports/ORDERLESS-MIGRATION-<date>.md`，诚实报告回退或无提升；更新需求状态与实现清单。未授权不提交推送。

## 执行检查命令

统一在现有Docker开发容器内构建/ktest，使用仓库Makefile既有入口（先查明现有测试筛选参数，禁止编造过滤开关）：

```bash
sg docker -c 'docker exec -w /root/asterinas os-com-rnvpc-rebuild make ktest'
sg docker -c 'docker exec -w /root/asterinas os-com-rnvpc-rebuild make kernel'
git diff --check
```

每个任务先跑相应功能测试；最后才跑完整suite。固定SMP/内存及镜像配置并归档实际命令，结果必须0 failed；历史SMP问题不能通过减少用例掩盖。文档阶段只验证链接、内容一致性和diff，不运行破坏性设备测试。

## 开工前固定的实施细节

上述新类型的字段布局、所有权索引和磁盘恢复扫描算法由实现者依据现有代码具体化；先核对新恢复信息是否需要格式升级。接口变更必须同步更新消费者与计划，不得借此改变权威边界、模式范围或故障契约。所有任务完成前不能宣称支持完整AchillesFS命名空间掉电一致性。
