# Extent Redirection Implementation Plan

> **Superseded（2026-10-20）：用户已明确保留 ExtentRecordEntry，改为 FAST/FAIR 风格字段级原地持久化。本文件仅作历史记录，不执行以下任务。最新方向见 README.md 的“方向调整”。**

> **For agentic workers:** 使用 executing-plans 按任务实现，先读取仓库 AGENTS.md 和本目录 README.md。当前仅文档，不得将任务勾选为已实现。

**Goal:** 普通 SSD→NVPage 覆盖通过独立持久重定向完成，逐步移除自定义 extent 节点及整节点 repair image。

**Architecture:** 原生 EXT4 extent payload 记录 SSD backing；持久 per-extent descriptor 引用独立表 slot，表项发布和撤销遵守 soft update。树结构更新另行实现字段级协议，保持 orderless migration 的 NVM 权威。

**Tech Stack:** safe Rust、Asterinas NVM slot/persist、EXT4、OSDK ktest、QEMU、fio。

**Spec:** [README.md](README.md)。日期：2026-10-20。

## Global Constraints

- 不修改 EXT4 原生 extent payload 编码来携带 NVPage 引用；辅助字段外置。
- 不以双 u64 引用、整个 entry 或 4 KiB 页天然原子为前提。
- 不恢复 ordered migration，不让前台为新文件分配 SSD 元数据，不削弱 WAL 版本。
- 保留工作树已有修改；当前任务不包括提交、推送或运行磁盘实验。
- 必须先定义持久格式/原子发布契约；尚未定义的 FAST 类协议是删除旧类型的阻塞项，不能由实施者用 memcpy+persist 代替。

## Review Focus

1. 新表已分配未发布、发布字撕裂、slot ABA：Task 1。
2. 无 SSD extent 的追加写以及部分写失败：Task 2。
3. migration 捕获后发生覆盖写、同块旧请求迟到：Task 3。
4. 表回收与插入/读者竞态、跨段表查找：Task 1/3。
5. 结构变更及 truncate 后旧 backing 重新显露、旧格式错误挂载：Task 4/5。

## Task 1：格式与表生命周期

**Files:** 新建 `kernel/src/fs/rnvpc/extent_descriptor.rs`、`redirect.rs`、`redirect_tests.rs`；修改 `mod.rs`、`metadata.rs`、`frame.rs`、`persist.rs`、`recover.rs`。

**Interfaces:** 拟新增 `DescriptorId`、`RedirectVersion`、`RedirectSnapshot`、`DurablePageRef`、`DurableBacking` 类型，由新模块定义并隐藏编码。公开内部契约：`lookup(logical_block) -> Result<Option<RedirectSnapshot>>`；`publish(logical_block, DurablePageRef) -> Result<RedirectVersion>`；原子提交字的落盘保证需先由 persist 层证明。所有方法绑定 descriptor 实例，不以全局裸指针传参。

- [ ] 写生命周期 ktest：未发布 slot 孤儿、首次发布、分段扩容、两次恢复、表满、分配失败、ABA、读者占用时禁止复用。
- [ ] 运行定向 ktest，记录失败后实现持久目录、descriptor 身份、slot 分配及提交字协议，再运行通过。
- [ ] 输出实际 layout、容量计算及每个掉电边界的状态表；没有这个工件不进入 Task 2。

## Task 2：前台写与查询

**Files:** `write_transaction.rs`、`extent_index.rs`、新 `redirect.rs`，EXT4 `inode/mod.rs` 和 `nv_tests.rs`，`rnvpc/rnvpce_integrity_tests.rs`。

**Interfaces:** 使用 Task 1 的 lookup/publish，返回最新有效 NVPage 或已确认 SSD backing；前台持久化引用发布不调用 replace_tree_extents。保留既有 NVPage 数据更新语义。

- [ ] 先加中间 1 页/32 页覆盖、部分页、hole、unwritten、追加、短写和 ENOSPC 测试，断言无权威数据丢失且原 SSD extent 不变。
- [ ] 实现 descriptor 按需注册和无 backing 范围；查询先查重定向，再执行 SSD/hole 语义。
- [ ] 测试通过并验证普通覆盖路径 NodeRewrite 和 repair image 为零；旧类型此时仍保留给其他职责。
- [ ] 验证 A→B 版本轮换每个持久化边界：B 发布后先持久撤销 A，恢复完成该清理后才允许撤销 B，禁止旧版本复活；旧页等待迁移和读者释放。

## Task 3：迁移与回收

**Files:** `rnvpc/write_transaction.rs`、`index.rs`、`frame/migration.rs`、`recover.rs`，EXT4 `migration.rs`、`migration/batch.rs`。

**Interfaces:** completion 持有 Task 1 的 `RedirectSnapshot`；新增 `retire_if_matches(snapshot) -> Result<bool>`，在互斥内核对引用/代数/数据版本并持久化撤销，失败表示保留新版本。backing 更新必须持有 flush 完成证明，不能仅凭布尔值发布。

- [ ] 先测试旧 completion、新前台写、flush 失败、部分请求失败、旧同块请求迟到及空表并发回收。
- [ ] 替换叶槽位反向定位；实现 SSD backing 持久发布→重定向撤销→读者退出→页回收。
- [ ] 验证所有失败状态仍可从 NVM 权威恢复，批次设备 flush 不退化为逐页 flush。

## Task 4：替换树结构、范围操作和根发布

**Files:** `extent_mutation.rs`、`extent_index.rs`、`metadata.rs`、`extent_descriptor.rs`、EXT4 `inode/extent_manager/`。

**Interfaces:** 本任务开始前必须交付字段级节点协议文档：原生 payload 与外部 sibling/boundary 的关系、原子发布字、FAST 类插入移动规则、root 切换、split/merge、truncate 删除与 descriptor 重绑定。由该协议定义接口后再实现；这是明确设计关卡，当前文档不声称已经解决。

- [ ] 为每个字段持久化边界建立故障用例，包含 parent 未更新时的 sibling 查询、完整 root 分裂、entry 搬动和截断恢复。
- [ ] 实现外部节点定位和范围协议，禁止出现无映射窗口、错误重叠选择或恢复后已删除数据复活。
- [ ] 核对 descriptor 稳定身份，所有查找/遍历路径支持中间树状态后才移除对应 repair-image 路径。

## Task 5：恢复格式切换与旧类型删除

**Files:** `recover.rs`、`layout.rs`、`metadata.rs`、`write_transaction.rs`、`frame.rs`、`extent_mutation.rs`、`rnvpce_integrity_tests.rs` 及所有编译引用点。

- [ ] 增加新/旧格式识别测试、注册未完成、重挂载、不可达对象、inode 重用和恢复幂等测试；首版旧格式明确拒绝，转换另行实施。
- [ ] 删除 ExtentNodeRecord 前逐项核对 README 的职责表，不以删除符号掩盖恢复能力缺失。
- [ ] 构建并运行相关 ktest；检查 WAL/native EXT4 路径不退化。`rg -n 'ExtentNodeRecord|BoxedExtentNodeRecord' kernel/src` 应无生产依赖；残余历史说明明确标注。

## Task 6：对照测试与报告

**Files:** `docs/experiments/scripts/` 新实验入口；`docs/experiments/reports/` 新报告。

- [ ] 复用已有 Docker/OSDK harness：先确认当前可执行入口及参数并把准确命令写入报告，不杜撰新的 make filter。
- [ ] 固定镜像、代码状态、CPU 隔离及 NVM 模拟参数，以 2 GiB、64/32 MiB 水线执行 README 测试矩阵。
- [ ] 对照阶段时间、锁等待、持久化次数/字节、slot 占用与含 drain 带宽，报告三轮中位数及范围。
- [ ] 仅依据通过的功能/恢复测试和实际测量更新实施状态；没有测试结果时保持 Planned。
