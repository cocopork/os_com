# FAST/FAIR In-place Extent Implementation Plan

> **For agentic workers:** 使用 executing-plans 按任务推进；先读仓库 AGENTS.md 与本目录 README.md。所有任务尚未执行，不绕过协议验证关卡。

**Goal:** 保留 SSD/NV 混合映射，以原子目标字与字段级 soft update 替换整节点 repair image，并安全移除 entry 分配代数。

**Architecture:** 先确定持久格式、slot 生命周期及中间状态解释，再实现 target 替换、FAST 类 entry 修改与 FAIR 类结构分裂。生产查询、迁移和恢复使用相同解释规则。

**Tech Stack:** safe Rust、Asterinas NVM/persist、OSDK ktest、QEMU/KVM、fio；架构原语仅允许在 OSTD 层实现。

**Spec:** [README.md](README.md)，2026-10-20；参考 FAST-FAIR 原文 §3–4。

## Global Constraints

- 不实施 013 重定向表，不改变 SSD EXT4 编码，不向 entry 重新塞入 allocation_seq。
- 8B 原子性和持久顺序需证明；cache line 和 16B 引用不视为原子。
- 不先删除所有对象代数；必须先证明无旧引用存活再复用 slot。
- 不削弱 rNVPC WAL、NVPage 数据更新或 orderless migration 的保证。
- 不引入完整 repair image 的替代名称；新 sibling 初始化允许完整节点持久化。
- 保留工作树既有更改，未获请求不提交/推送；运行测试不得默认为允许格式化任意设备。

## Review Focus

1. 自然 eviction 导致准备字段先落盘、类型与范围混搭：Task 1/3/4。
2. SSD 相同 target 的合法区间被 FAST 重复规则误判：Task 1/4。
3. 旧缓存/BIO/completion 在 slot 复用后访问新对象：Task 2/6。
4. parent/root 未完成、跨层 sibling 路由与恢复扫描遗漏：Task 5。
5. 截断后的 SSD 旧数据显露、旧格式误解码及恢复二次执行：Task 6/7。

## 拟新增模块与接口约束

以下是计划接口，不声称已存在。编码在 Task 1 冻结后才可落入生产结构。

| 模块 | 职责和接口 |
| --- | --- |
| `kernel/src/fs/rnvpc/extent_target.rs`（新） | `ExtentTarget`，checked SSD/NV/child 构造及解码，无分配代数 |
| `extent_inplace.rs`（新） | `replace_target(expected, durable_new)`、`insert_entry`、`remove_entry`；均绑定节点 guard，接受已持久目标令牌 |
| `extent_state.rs`（新） | `decode_node_state`，输出稳定/可解释中间/损坏分类及有效逻辑记录，查询/恢复共用 |
| `extent_inplace_tests.rs`（新） | 小容量状态模型、故障注入和运行时协议测试 |
| `frame.rs`、`frame/migration.rs` | pin/退役、可达性和复用门控；提供强持有而非裸 slot 生命周期 |
| `persist.rs` | 对齐字存储、范围 persist、依赖屏障及故障点的安全封装 |

## Task 1：格式、状态模型和持久化契约

**Files:** 上述新 target/state/tests 模块，`metadata.rs`、`layout.rs`、`persist.rs`、`mod.rs`；必要的 OSTD 原语由调用链确定后列入协议文档。

- [ ] 审计逻辑块最大值、真实 SSD 位宽、slot 数量、引用 API、当前元数据落盘方式；决定 range 编码并记录不兼容边界。超范围用例必须明确拒绝。
- [ ] 编写 `PROTOCOL.md`：确定 target 位域、节点头/sentinel/count、重复判定、操作提交点及所有中间状态。逐项列出 FAST 原假设是否成立。
- [ ] 先构建小容量纯状态模型测试：插入/shift/删除、合法相同 SSD target、范围重叠、首尾位置和每个字写中断；枚举允许的提前落盘顺序，失败则修改协议。
- [ ] 明确后端持久写是否仍暗中写整页/切换完整副本。若是，先补范围级落盘语义，计数反映实际字节，不能只缩小上层 API 参数。
- [ ] 原子发布/所有权保护/平台语义通过独立审阅后冻结格式。此任务不通过则禁止 Task 3–5 的生产更新。

## Task 2：无代数 entry 的安全回收

**Files:** `frame.rs`、`frame/migration.rs`、`recover.rs`、`index.rs` 及 dirty/迁移引用消费者。

**Interfaces:** 新持有令牌 `PinnedNvObject`，负责存活期禁止 slot 复用；退役操作接收“持久引用已撤销”的证明，最后持有者退出后才归还分配器。DRAM 队列可继续使用代数过滤。

- [ ] 添加旧读者、dirty 候选、旧 locator、BIO 和 completion 持有期间复用失败的测试，及掉电恢复前禁止开放分配的测试。
- [ ] 实现持有/退役门控和恢复可达性保护；包含 FAST 临时重复与 sibling。
- [ ] 测试同 slot 跨 inode、同 inode 不同逻辑页复用；旧缓存不能读到新对象。通过后才移除新持久 entry 的 allocation_seq 依赖。

## Task 3：稳定单页 target 替换

**Files:** `extent_inplace.rs`、`write_transaction.rs`、`extent_mutation.rs`、`persist.rs`。

**Interfaces:** `replace_target` 需 expected target、节点 guard 和 `DurableTarget` 令牌；令牌由 NV 数据持久化或 SSD batch flush 完成路径生成，普通调用者不能伪造。

- [ ] 添加 SSD→NV、NV→SSD、unwritten→NV、expected 不符、数据持久化失败与目标发布后故障测试。
- [ ] 实现 README §6.1，不复制或持久化整节点；保留旧资源直到 Task 2 退役条件成立。
- [ ] 断言实际 repair-image 字节/次数为零，读及二次恢复看到完整旧或新 target。与范围变化路径分开验收。

## Task 4：FAST 类修改及逻辑 split

**Files:** `extent_inplace.rs`、`extent_state.rs`、`extent_mutation.rs`、`extent_index.rs`、`extent_inplace_tests.rs`。

- [ ] 用冻结协议实现节点内移位和插入，首版保留读写互斥；查询/恢复调用同一解码器。
- [ ] 测试 SSD `[a,b)` 覆盖首/中/尾页、unwritten、连续 32 页、跨 cache line、接近满节点和 ENOSPC；逐个持久切点验证无错误 target/缺口。
- [ ] 实现映射等价 SSD 片段准备→原范围收缩→单页 target 发布；所有中间状态均须模型通过，禁止以无限制范围重叠掩盖错误。
- [ ] 先在足够空位且不跨节点的条件下验收，再接入 Task 5 预分裂和批量 replacement；短写保留已提交前缀。
- [ ] 保留现有批次入口，统计必要 cache-line persist；没有依赖关系的字段可合并持久化，有依赖的阶段不能合并越过提交点。

## Task 5：FAIR 分裂与完整 NVM 路径

**Files:** `extent_mutation.rs`、`extent_index.rs`、`write_transaction.rs`、`metadata.rs`、`recover.rs`、`extent_state.rs`。

- [ ] 添加根为叶/多层根、parent 未补插、分裂后立即再分裂、重复恢复、未加载子树和加载中断测试。
- [ ] 实现按需持久注册完整路径；旧 SSD backing 与 NVM child 引用明确区分，新 NV 数据发布前完成路径可达性。
- [ ] 按 README S0–S4 实现 split；支持所有层 sibling 查找和幂等 parent 补插，原子发布根；分裂前保证目标节点容量和保留项空间。
- [ ] 范围查询、反向定位、迁移扫描、恢复和缓存失效均使用新路由；仅点查通过不能验收。
- [ ] 不强行实现节点 merge；对未满节点保留、元数据占用和 ENOSPC 明确报告。

## Task 6：migration、范围删除与恢复闭环

**Files:** `write_transaction.rs`、`recover.rs`、`frame/migration.rs`，EXT4 `migration.rs`、`migration/batch.rs`、inode/extent 管理入口，`rnvpce_integrity_tests.rs`、`nv_tests.rs`。

- [ ] 测试迁移时覆盖写、flush 失败、data/metadata 部分完成、同 SSD block 旧请求迟到和 entry 移位后旧 locator。
- [ ] 接入新 target 发布和 pin 退役，保留数据版本检查；旧 completion 不改变新数据权威性。
- [ ] 定义并测试 truncate/hole punch 可恢复删除状态，先限制可见范围再释放资源；重挂载不能暴露旧 SSD 内容。
- [ ] 验证 orphan 扫描、部分注册、新根发布中断与恢复重复执行；恢复解释完成前不开放 allocator 复用。

## Task 7：切换、清理及对照实验

**Files:** `layout.rs`、`metadata.rs`、`extent_mutation.rs`、相关测试；`docs/experiments/scripts/`、`docs/experiments/reports/`。

- [ ] 格式版本检查测试：旧格式明确拒绝，无静默转换/清盘。rNVPC WAL 的旧公共类型按需隔离保留。
- [ ] 新生产路径完整接入后删除其 NodeRewrite repair-image 及旧恢复分支；保留真实 sibling 分配。通过调用审计证明没有 fallback 回到整节点镜像。
- [ ] 在项目容器编译并运行定向及相关恢复 ktest；准确命令从当前 harness 帮助/已有脚本取得并记录，不杜撰 filter 参数。
- [ ] 按 README 验收矩阵运行相同配置三轮 A/B；记录代码状态、镜像、设备身份和完整命令，数据与图遵循现有 git 忽略规则。
- [ ] 报告各阶段延迟、persist 实际字节、fence/调用次数、内存峰值、fio 与含 drain 带宽中位数及范围。没有收益也如实记录。

## 完成判定

协议证明/故障枚举、并发回收、功能及恢复测试全部通过，实际生产路径不再生成 repair image 后才能标记实现完成。性能提升是单独实测结论；本计划既不预设数值，也不把 013 的已讨论设计当成实现基础。
