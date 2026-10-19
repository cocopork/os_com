# 恢复内核功能完备性清单

> 后续更新：原生 EXT4 的 journal/nojournal delalloc 已在工作树实现并通过299项 kernel ktest。下表保留恢复调查时的历史状态，当前实现边界、预留限制及验证见 [EXT4 delalloc 实验报告](../experiments/reports/EXT4-DELALLOC-2026-10-17.md)。

核对日期：2026-10-17（北京时间）。

核对对象：`/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild`，分支 `recovery/rnvpc-rebuild`，基于 `49e7829d6f6718f0def92231fcdbb5691c883629`。

依据：找回的 Codex session、历史设计目录 `/home/zn/zn/Rust_EXT4/os_com_implementation`、当前源码及当前构建/测试记录。本文记录核对时的状态，不代表历史检查点已逐字节完整恢复。

**结论：研究模块主要读写链路已经恢复，但完整实验基线尚未恢复。突出缺口是原生 EXT4 后台 writeback、delalloc、PageCache dirty-only 索引，以及迁移 ownership 和跨真实重启的恢复接线。**

状态含义：

- **已验证**：当前 rebuild 有针对性测试；不代表所有边界均已覆盖。
- **已有实现**：源码和调用链存在，运行时覆盖仍有限。
- **部分/缺失**：存在明确差距。
- 历史文档中的 PASS 不计作当前 rebuild 的验证；历史计划不等于历史已实现。

## 一、研究部分：rNVPC

依据：[前台 checkpoint 设计](../os_com_implementation/rNVPC-rTIO/rnvpc-foreground-checkpoint-design.md)、[会话来源说明](persistent-wal.md)。

| 应支持的功能 | 当前 rebuild | 缺口或限制 |
|---|---|---|
| 每 inode 持久 WAL；数据先持久化，再提交日志并发布映射 | 已实现、已有测试 | 创建失败等故障边界尚未全部覆盖 |
| 固定 1024 条 WAL，第 1025 次写前台 checkpoint | 已验证 | 按 LSN 重放全部记录，不合并重复键 |
| 持久 B-link B+tree、双镜像节点与根发布 | 已实现、已有测试 | 已测分裂及部分故障点，不是完整故障矩阵 |
| checkpoint 成功后推进 WAL head | 已验证 | checkpoint 失败保留日志，支持重试 |
| WAL 与 SSD completion 解耦；wal-pinned 页禁止迁移 | 已验证 | SSD 完成不能用于退休 WAL |
| 重启后重放日志、重建易失映射 | 模型内已验证 | 普通启动没有接入保留旧 NVM 镜像的恢复流程 |
| EXT4 读写接入、迁移忙页等待、部分页覆盖 | 已验证部分路径 | 并非完整用户态并发测试 |
| 与历史研究对象保持相同的前台开销 | 尚未对齐 | 当前为保证分配和文件大小持久性，增加前台 SSD 同步；不能直接复现原性能结果 |

WAL 属于 **rNVPC**；rNVPCe 不复用这套日志。

## 二、研究部分：rNVPCe

依据：[总设计](../os_com_implementation/rNVPC-rTIO/design.md)、[当前事务实现](../../kernel/src/fs/rnvpc/write_transaction.rs)。

| 应支持的功能 | 当前 rebuild | 缺口或限制 |
|---|---|---|
| 无 WAL；fixed inode 作为持久恢复根 | 已有实现 | 已测固定根及部分恢复路径 |
| NV 命中直接读写，不经过 PageCache 数据缓存 | 已接入、已有测试 | SSD 未命中读使用临时 BIO 缓冲，不建立干净 PageCache 页 |
| 已驻留 NVPage 原地更新；新页初始化后发布引用 | 已有实现 | 不承诺中断写入整体原子性 |
| 部分页写、补零、SSD→NV 初始化、成功前缀持久化 | 已有实现、部分验证 | 短 reader 已测；真实用户页故障场景未全面验证 |
| 数据先于 extent 引用，inode 可见字段最后发布 | 已有实现 | 需要更完整的故障切点覆盖 |
| split/root growth 的 pending mark、repair image、局部冻结与修复 | 已有实现 | 尚未逐项复跑全部历史恢复测试 |
| 8 字节 reverse locator、generation 校验 | 已有实现、部分验证 | 已测部分并发、过期引用与回收行为 |
| count-only SSD reservation；迁移时才选择 SSD 地址 | 已有实现 | 分配落盘到 target 发布之间仍有崩溃泄漏窗口 |
| leaf-level completion、独立 reservation guard、64 分片 pending index | 已有实现、部分验证 | 性能及高并发效果未复现 |
| 有界 repair workspace 复用 | 源码存在 | 不在本轮全部定向验证范围内 |
| 文件 fsync 保持 NV 持久性；文件系统 sync/unmount 导出原生 EXT4 | 已接入，sync 路径有测试 | 完整卸载/重新挂载场景仍需验证 |
| truncate、fallocate、unlink、rename-overwrite 的持久图更新 | 缺失，当前明确拒绝 | 历史文档也承认未完成，不全是恢复丢失 |
| 全部 NV 元数据迁出、只靠 SSD 恢复 | 未实现 | 历史版本同样未宣称完成 |

## 三、研究部分：rTIO

依据：[设计中的迁移流程](../os_com_implementation/rNVPC-rTIO/design.md)、[当前迁移代码](../../kernel/src/fs/fs_impls/ext4/migration.rs)。

| 应支持的功能 | 当前 rebuild | 缺口或限制 |
|---|---|---|
| 全局去重脏 NVPage 队列、精确 generation admission | rNVPCe 已实现 | rNVPC 仍聚合各 inode 的上下文候选，未统一成全局队列 |
| 高水位启动、提交与完成两个 worker | 已接入 | 大于 NV 容量的持续负载及背压尚未验证 |
| NV 物理页直接 DMA，连续目标合并 BIO | 已实现 | 不等于底层 virtio descriptor 已合并 |
| 批量选择连续 SSD 空间 | 部分 | 当前逐页分配，再合并恰好连续的目标；缺少批量连续 run 分配 |
| IRQ 回调仅入队/唤醒，进程上下文持久化与回收 | 已实现并做源码复查 | 完整设备错误矩阵未覆盖 |
| 有界 in-flight/完成队列，完成排空后返还 credit | 已有实现 | 当前上限 128 页 |
| SSD 数据持久化→更新引用→回收精确 NV generation | 已有实现、部分验证 | 跨 EXT4 分配与 NV target 的持久 ownership 协议仍不完整 |
| 迁移失败可重试且不重复消耗 credit | 部分已验证 | 歧义 target 失败锁存错误，只能限制运行期泄漏，不能修复崩溃窗口 |
| 历史实验中的 CPU 隔离选项 | 未恢复 | 会话保留了 rtio_cpu_isolation 补丁，当前未接入；属于实验配置功能 |

## 四、基线部分：EXT4、writeback thread、PageCache

不能直接沿用旧文档的“支持”结论。会话片段出现过独立 writeback worker、脏页加在飞页计数和 delalloc reservation，而当前 rebuild 没有对应完整实现。

证据：[历史会话提取片段](/home/zn/zn/recovery_Rust_EXT4/research_recovery_20261016/positioned-views/kernel/src/fs/fs_impls/ext4/fs.rs/unversioned_sed_evidence.rs.partial)、[PageCache 当前写回路径](../../kernel/src/vm/page_cache/vmo/mod.rs)、[历史功能核对](../os_com_implementation/ext4-function-check.md)。提取片段存在缺口和版本混杂，不能直接整段覆盖当前源码。

| 应支持的功能 | 当前 rebuild | 缺口或限制 |
|---|---|---|
| 原生文件、目录、普通/直接读写、文件生命周期操作 | 基线代码存在 | 本轮仅少量原生路径回归，不能称完整 EXT4 测试通过 |
| JBD2 journal、commit/checkpoint/recovery；nojournal 分支 | 已有实现 | 本轮未完整验证两种模式的 NV 集成 |
| fsync/fdatasync、O_SYNC/O_DSYNC 持久性分流 | 已有实现 | 完整并发及故障矩阵待验证 |
| 常用 extent insert/truncate 局部更新，ES cache | 已有实现 | ES cache 不是历史完整的 delayed-allocation 索引；部分范围变换仍 flatten/rebuild |
| 文件系统层连续物理 run 合并 BIO | 已有实现 | 普通 EXT4 后端有多 segment 提交 |
| 原生 EXT4 独立后台 writeback thread | 缺失 | journal commit thread 和 rTIO worker 都不能代替它 |
| dirty inode 集合、周期/压力唤醒、干净卸载排空 | 未完整恢复 | 缺少原生后台写回的整套接线 |
| dirty + in-flight 计数及前台 throttle | 未恢复历史机制 | 不能保证 buffer-write 实验具有原设计的背压语义 |
| delalloc：前台预留、writeback 时分配 SSD 块 | 缺失 | 当前原生写路径仍先分配，再写 PageCache |
| delalloc 数据/元数据 reservation、ENOSPC 回滚与完成结算 | 缺失 | rNVPCe 的 reservation 不能替代原生 delalloc |
| 普通 PageCache 缓存、预读、脏页写回 | 已有实现 | 当前仍是旧基线机制 |
| dirty-only 稀疏索引、范围选取 | 缺失接线 | XArray 有 mark 能力，但 PageCache 仍遍历 present 页再检查 dirty |
| owner/identity、redirty、writeback generation、detached completion | 未恢复完整协议 | 历史有片段，不等于当前生产路径已经具备 |
| Buddy Cache 块分配 | 没有 | 当前仍位图扫描；历史计划也不能证明它已完成 |
| virtio-blk 多队列、连续物理 segment descriptor 合并 | 没有 | 当前使用首个队列，逐 segment 建 descriptor；属于尚未具备的优化能力 |

“能写回”与“恢复了 dirty-only 后台写回基线”是两个不同状态。

## 五、硬件模拟：NVDIMM、persist、fence

依据：[设计中的模拟边界](../os_com_implementation/rNVPC-rTIO/design.md)、[当前 DRAM backend](../../kernel/src/fs/rnvpc/dram.rs)、[持久化协议](../../kernel/src/fs/rnvpc/persist.rs)。

| 应支持的功能 | 当前 rebuild | 缺口或边界 |
|---|---|---|
| 有界 DRAM NVPool、数据/元数据容量划分 | 已实现、已有测试 | 不是真实 NVDIMM |
| 数据区与控制区分别 persist，descriptor commit/clear 顺序 | 已实现 | 供上层持久协议使用 |
| fast 模式 | 已实现 | 工作镜像、范围检查、compiler fence、统计；不产生真实持久存储 |
| fast-no-persist 消融模式 | 已实现、已有测试 | 跳过模拟 persist/fence，不代表真实硬件零成本 |
| model 工作/持久双镜像、模拟 crash、epoch 失效 | ktest 中已实现、已有测试 | 普通内核明确拒绝 model |
| aligned 64-bit 控制字段访问 | 已有实现 | 不能据此推导真实硬件掉电原子性 |
| 真正 clwb/clflushopt 与硬件 sfence | 没有 NVDIMM backend 接线 | compiler_fence 不是硬件持久屏障；真实 PM 验证在历史设计中也属范围外 |
| NVDIMM 延迟、带宽、缓存行写回成本模拟 | 未见对应模型 | 当前不能推导真实 NVDIMM 性能；不是已证实丢失的历史功能 |
| 跨系统重启保留镜像并自动恢复 | 未接入 | 普通初始化重新分配并 format；当前恢复测试在受控模型中完成 |

## 六、验证范围与补齐顺序

当前源码与上一轮已测试的 [32 个源码文件哈希清单](integration-source-manifest.json) 一致。整核构建成功，**56 项定向测试通过**；并非全部内核测试，也不覆盖完整基线、长期超容量负载或真实掉电。最终证据以 [WAL/EXT4 集成验证报告](WAL-EXT4-INTEGRATION.md) 为准，早期子任务文档中的“待执行”描述不覆盖最终结果。

按影响排序：

1. **实验基线**：PageCache dirty/writeback 协议、原生 writeback worker、delalloc 及 reservation。
2. **持久迁移闭环**：SSD 分配与 NV target 的 ownership 缺口，以及真实启动恢复入口。
3. **研究对象对齐**：rNVPC 新增前台同步开销、rTIO 批量分配与候选调度差异。
4. **验证覆盖**：journal/nojournal、超容量背压、sync/unmount/remount、并发与故障矩阵。

以上源码存在/缺失结论置信度高；历史片段所代表的最终版本完整性与未复跑的运行时行为仍有不确定性。清单不是新功能已获实现或测试通过的声明。
