# 013：原生 EXT4 extent 与独立持久化重定向表

日期：2026-10-20。状态：Superseded，用户已明确不采用本方案，未实现。

## 方向调整（2026-10-20）

当前设计与实施入口为 [014：FAST/FAIR 原地更新](../014-fast-fair-inplace-extents/README.md)，以该文档为准。

保留现有 ExtentRecordEntry 的 SSD/NV 混合映射语义，不引入独立重定向表。优先消除整节点 repair image：参考本地 FAST-FAIR 的 §3 FAST、§4 FAIR，把 entry 修改和节点结构更新设计为字段级原地持久化协议。下文保留为讨论历史，IMPLEMENTATION_PLAN.md 不再是待执行计划。

这不是直接按 logical_block/value/length 的任意顺序逐字段 flush：必须定义原子发布字段、每个中间状态的查询解释及崩溃恢复动作。FAST 的重复指针判定依赖其 key-pointer 布局，不能直接用于包含 allocation_seq、length、flags 的现有 entry；同样不能把双 u64 PersistentObjectRef 视为单次原子发布。保留 entry 语义不等于冻结其所有物理布局。

替代协议需覆盖：entry 插入/移动/删除；SSD 范围切成左右片段及 NV entry；SSD↔NV 类型转换；代数与指针一致性；兄弟/父/根发布；查询与恢复容忍中间状态；slot 回收。优先保留已有锁来控制并发，不以实现无锁读为本轮前提。每个持久化阶段必须说明旧映射或新映射为何仍完整可达，以及自然 cache eviction 提前落盘时为何安全。

论文 FAST 按跨 cache-line 的依赖顺序及最终发布点 flush/fence，不是要求每个字段都独立 fence。实现需先证明正确，再合并无依赖冲突的持久化操作。性能瓶颈程度及收益仍需独立测量，不能把修复镜像开销直接当作已证实的唯一瓶颈。

参考：[FAST-FAIR 原文提取](../../references/FAST-FAIR/analysis/full-text.md)，§3.1–3.5、§4.1。该目录提供论文及分析，不是已经可直接移植的 Rust 实现。

## 1. 目标与边界

用户确定的方案：extent 本体保持 EXT4 SSD 映射格式；NVM 驻留信息移到独立重定向表，表单独占用 NVM slot，由 per-extent descriptor 的持久引用访问。创建、发布、撤销与回收遵守 soft update。普通覆盖写不得因为 SSD→NVPage 切换而拆分 extent，也不得持久化整节点 repair image。

本方案针对 rNVPCe soft-update 路径；rNVPC WAL 版本不能因共享模块重构而失去其原有协议。代码目录 `rnvpc` 不代表所有路径都采用 WAL。

“逻辑 split 已解决”仅指已确定替代设计。当前 `ExtentRecordEntry`、`ExtentNodeRecord`、NodeRewrite/NodeSplit/RootSplit 以及 repair image 仍在代码中。新增 SSD 映射、unwritten 转换、truncate 等造成的真实 extent 分裂不由重定向自动消除。

## 2. 当前代码证据

- `kernel/src/fs/rnvpc/metadata.rs`：`ExtentRecordEntry` 使用 logical_block/value/allocation_seq/length/flags，混合 SSD、NVPage、child 三种引用；`ExtentNodeRecord` 是完整 4 KiB 自定义节点。
- `kernel/src/fs/rnvpc/extent_mutation.rs`：`replace_tree_extents` 批量生成叶节点最终布局，`rewrite_leaf_and_route` 经 repair image 发布；结构性 split 仍是独立事务；`refresh_leaf_reverse` 维护叶槽位及 entry 下标。
- `kernel/src/fs/rnvpc/extent_index.rs`：`lookup_current` 沿父子路由下降，没有 FAIR 所需的 sibling 横向补查。
- `kernel/src/fs/rnvpc/write_transaction.rs`：bootstrap、inode root、前台发布、迁移发布和树遍历依赖自定义节点。
- `kernel/src/fs/rnvpc/recover.rs`：需审计其节点扫描及所有权重建依赖后切换新格式。
- `kernel/src/fs/fs_impls/ext4/inode/extent_manager/node.rs`：EXT4 叶 entry 表示逻辑起点、长度、SSD 起点及 written/unwritten；内部索引另有类型。

## 3. 对象关系与格式

```text
持久 inode / descriptor 目录
    → ExtentDescriptor（稳定身份、逻辑范围、SSD 映射关联）
        → redirect_ref
            → RedirectTable（独立 slot）
                → page offset → NVPage 引用及版本
```

extent payload 使用原生 EXT4 编码，不把 NVM 指针写进 SSD block 字段。descriptor、sibling、锁、代数和发布状态均为外部辅助信息。内存解码结构无需与磁盘字节布局完全相同；NVM 地址与 SSD block ID 通过外部索引转换。内部节点的 SSD 子块号同样不能被偷偷替换为 NVM 指针。

每个 descriptor 的身份与叶 entry 数组下标解耦。重定向逻辑身份为 `(fs_id, inode, inode_generation, logical_block)`，slot 分配代数防止 ABA。descriptor 可按 extent 组织；split/merge 后不得因 entry 搬动丢失覆盖关系。

以下为建议实现约束，并非现有实现：

- 第一个表 slot 按需分配，存稀疏 page-offset 条目；有效项优先于 SSD 映射，无项则回退 SSD/unwritten/hole 语义。
- 单 slot 容量有限，长 extent 不等于一张固定大小表能覆盖全部页。首版采用多个表 slot 的分段链，每段声明逻辑范围；新段初始化持久化后发布 next 引用。DRAM 范围索引加速查询，不能每次从链头线性遍历。
- 具体槽位数由头部、entry 实际大小推导并静态校验，禁止凭空假定可以容纳任意 extent。
- descriptor 先允许无 SSD backing，覆盖追加写和 hole 写；后台分配后绑定 SSD 映射，前台只做容量预留。
- 有效性发布采用具有明确平台/模拟器保证的对齐原子提交字；现有双 u64 PersistentObjectRef 不能作为原子写假设。先写非有效状态下的身份和代数，再发布有效提交字。已发布 entry 换版本时，禁止先清旧引用造成短暂 SSD 回退；使用独立空 entry 准备新版本及明确的版本选择规则，旧版本保持有效至新版本发布。
- 新版本发布后，先持久化撤销同一逻辑页所有被替代的旧表项，才允许最新版本因迁移完成而失效，防止查询重新命中旧版本。恢复看到多个有效版本时先选择最高版本，并完成旧项撤销后才允许回收最新项；版本计数溢出必须拒绝更新。旧 NVPage 的释放还需等待旧迁移请求和读者退出。
- NVPage 内容在引用已有效时再次写入的掉电语义是独立问题；保留并验证既有数据更新保证，不能由表发布协议推导任意 4 KiB 写原子性。

## 4. 持久化协议

### 4.1 新建表和发布页

1. 分配并持久化可恢复的 slot 所有权，初始化表身份、范围及空有效项。
2. 初始化 NVPage；部分页写保留旧数据，整页覆盖不读旧 SSD。
3. 持久化 NVPage 数据及身份，再持久化表项准备字段，最后发布有效提交字。
4. 首次创建表时，表内容完成持久化后才发布 descriptor.redirect_ref；新 descriptor 必须先完整初始化再进入持久目录。
5. 首批多个页允许统一初始化、批量 persist 后发布表引用；已发布表按其 entry 协议更新。不得为了批量化省略依赖边界。

掉电恢复只跟随有效发布引用。分配完成但未发布的对象可能成为孤儿，必须通过 slot 所有权和可达性扫描安全回收，不能靠 DRAM 临时列表识别。

### 4.2 查询

先按逻辑范围查询 descriptor 和重定向（包括无 SSD backing 的范围），命中最高有效版本则读 NVPage；否则读取已确认的 SSD 映射或返回 hole/unwritten 的零数据。不得先因 SSD extent 不存在而返回零。

### 4.3 orderless migration 与页回收

1. 捕获迁移页身份/数据版本，记录 data 与全部必要 metadata 请求到 DRAM batch。
2. data/metadata 可无相互等待地提交；全部完成后进行批次设备 flush。
3. flush 成功后，若需要更改 SSD backing，先持久发布 descriptor 的已确认映射，再撤销相应重定向。
4. 撤销前在同一互斥协议中核对当前 page 引用、分配代数和数据版本仍匹配迁移快照。不匹配时保留最新重定向。
5. 持久化撤销引用后，等待运行时读者退出再回收 NVPage。

SSD 上提前落盘的 extent 不是自动可信的恢复来源；未完成迁移仍以 NVM 重定向和已确认 backing 为权威。保留 012 的目标归属保护、分配门控及恢复要求，禁止重挂载后直接用可能提前落盘的 SSD 元数据重建权威 descriptor。

flush 只证明批次已提交请求落盘，不允许较老写请求随后覆盖较新同块版本。已有同目标块请求排序/版本隔离同样必须保持；只核对 completion 版本不够。

### 4.4 空表和 descriptor 回收

在 descriptor 更新互斥下确认表无有效项、无未发布插入、无迁移持有者；持久清除父引用，等待读者退出后释放表 slot。多段表先持久化前驱绕过待回收段，再释放该段。空表不是删除 descriptor/SSD 映射的理由。

truncate/hole punch 必须先持久化可恢复的逻辑删除状态或文件可见范围，再撤销页引用和释放资源，防止掉电后旧 SSD 数据重新显露。具体范围删除协议归下述后续任务，不能仅清空表项。

## 5. 消除 ExtentNodeRecord 尚未解决的职责

| 现有职责 | 重定向方案是否覆盖 | 替代机制与未完成事项 |
| --- | --- | --- |
| NVPage 覆盖 SSD 的逐页映射 | 设计覆盖 | descriptor + 重定向表；尚未实现 |
| SSD 连续范围、hole/unwritten 语义 | 部分 | 原生 EXT4 payload；保持 NVM 已确认 backing 权威 |
| 内部节点路由、level、child 引用 | 否 | 原生索引 + 外部节点定位；统一查询/恢复 |
| 结构性 node/root split 与 root 发布 | 否 | FAIR 风格 sibling 协议、横向查找、延后 parent 更新、恢复补全及原子根切换 |
| entry 原地插入、移动、删除、真实 extent split/merge | 否 | FAST 类安全更新协议；32/12 字节记录均不可视为原子写 |
| 完整镜像修复和中间状态验证 | 否 | 字段级有效状态、范围边界、顺序不变量；旧校验要求无重叠及空槽全零不能直接照搬 |
| 对象身份、所属 inode、代数、分配与回收 | 部分 | 外部持久 descriptor/slot 元数据、恢复可达性扫描 |
| inode root、size、block_count 的一致性 | 否 | 根及 descriptor 目录发布、delalloc backing 绑定、截断协议 |
| 迁移反向定位、候选遍历与缓存失效 | 部分 | 从 leaf_slot/entry_index 改为稳定逻辑身份和版本；更新 dirty 索引与 lookup 缓存 |
| 启动导入、掉电扫描、旧持久格式兼容 | 否 | 格式版本及注册协议；旧格式明确拒绝或显式转换，禁止静默解释 |

删除 repair image 与删除 ExtentNodeRecord 类型是两个验收点。重定向可先去掉普通覆盖写的 NodeRewrite；只有上表所有消费者迁移完成，才能删除旧节点类型。不得将同样的整节点镜像逻辑换名后宣称完成。

FAIR 引用算法的第 2 行条件需按非空 sibling 解释；实现前必须给出 FAST 插入及范围 entry 更新的完整崩溃状态表，不能只实现 sibling split 的片段。

## 6. 验证与性能口径

- 覆盖已有 SSD extent 的中间 1 页、连续 32 页：extent payload/树结构不变；新数据正确；前台 repair-image、NodeRewrite、逻辑 split 计数为零。
- 整页/部分页、unwritten、hole、追加写、跨 extent、多段表、表满及 ENOSPC：已成功发布前缀可恢复，未发布资源可回收，无前台 SSD 元数据分配。
- 对每个发布/撤销持久化边界注入掉电，连续恢复两次验证幂等；新引用不指向未持久对象，旧引用不指向复用 slot。
- 版本 A→B 发布后在清理 A 的各阶段掉电，再迁移并撤销 B：必须回到已确认 SSD backing，绝不能重新读到 A；旧版本尚有迁移请求时禁止复用其页。
- 迁移与覆盖写、空表回收与插入、truncate 与 completion、活动读者与 slot 复用并发；较老 completion 不撤销新页。
- SSD data/metadata 任意完成次序、部分失败及 flush 失败：保留权威 NVPage；同块旧请求不得覆盖已确认新版本。
- descriptor/树结构变更时旧路由仍能定位最新页；未确认的 SSD 映射不能绕过 NVM 权威。
- 沿用现有隔离配置、2 GiB 数据、64 MiB 脏页上限、32 MiB 低水线；4 KiB randwrite 和 128 KiB write，1/16 线程，至少三轮，记录 fio 及含 drain 带宽。
- 独立诊断前台/flush/completion 阶段时间、锁等待、NVM persist 字节/次数、fence 次数、slot 分配次数、SSD flush/批次及峰值 metadata 占用。保持持久化模拟参数和设备不变。
- 验收不预设带宽提升；报告中位数及范围，失败样本不得丢弃。原始数据遵循 experiments 的现有忽略规则。

## 7. 实施顺序

见 [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)。先完成重定向生命周期和读写/迁移闭环，再替换结构性更新和恢复协议，最后删除 ExtentNodeRecord。本文没有批准格式破坏性升级、格式化设备或自动提交推送。
