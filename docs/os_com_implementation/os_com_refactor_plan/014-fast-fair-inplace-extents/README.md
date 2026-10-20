# 014：FAST/FAIR 风格的 NVM extent 原地更新

日期：2026-10-20。状态：Planned；设计约束和分阶段实施计划，未修改内核、未验证性能。§6 的 FAST 区间适配必须通过状态枚举后才能实现生产更新，本文不将论文点键算法视作现成的 extent 正确性证明。

## 1. 已确定的方向

1. 保留 `ExtentRecordEntry` 混合表达 SSD 映射、NVPage 映射和子节点引用的设计语义，不实施 013 的独立重定向表，不要求 NVM entry 与 EXT4 磁盘 entry 逐字节一致。
2. 去掉 extent entry 内的 `allocation_seq`；通过先持久撤销引用、等待运行时持有者退出、最后复用 slot 来保证引用有效。不要同时删除所有分配器或 DRAM 队列代数。
3. 将地址与地址类型封装为一个对齐 8B 原子发布字段，避免地址与类型分别生效。普通更新采用字段级 soft update，不持久化整节点 repair image。
4. 首次访问相关文件页时，inode 到目标 leaf 的整条路径按需加载并持久注册到 NVM。extent 修改、分裂、父路由和根更新在 NVM 完成；无需一次加载整棵树。
5. FAST 负责节点内插入、移动、删除的阶段化更新，FAIR 负责 sibling 可达性、节点分裂及父节点补更新。查询和恢复必须认识允许的中间状态。
6. 保留 rTIO 的 orderless data/metadata 提交、全部完成后批次 SSD flush、随后 NVM 发布/回收的顺序。

本轮作用于 rNVPCe soft-update 路径，不能误将共享目录 `rnvpc` 中 WAL 版本切换成无 WAL。用户讨论中的“rNVPC”泛称在本文按这两个版本明确区分。

## 2. 问题与代码入口

当前 `insert_leaf_extent → replace_leaf → publish_repairable_node_rewrite` 先复制 entries、构造完整 4 KiB `ExtentNodeRecord`、持久化 repair image 和 pending mark，再覆盖原节点。`replace_tree_extents` 虽能批量合并修改，仍依赖这套镜像机制。节点引用不变不等于字段级原地更新。

| 代码 | 当前职责 / 重构入口 |
| --- | --- |
| `kernel/src/fs/rnvpc/metadata.rs` | entry、node、inode、PersistentObjectRef 编码和严格校验 |
| `kernel/src/fs/rnvpc/extent_mutation.rs` | 插入、范围替换、NodeRewrite、NodeSplit、RootSplit、反向索引刷新 |
| `kernel/src/fs/rnvpc/extent_index.rs` | 查询和 DRAM 缓存；当前缺少 FAIR 的 sibling 横向补查 |
| `kernel/src/fs/rnvpc/write_transaction.rs` | bootstrap、前台发布、inode root、迁移完成及树遍历 |
| `kernel/src/fs/rnvpc/frame.rs`、`frame/migration.rs` | slot 分配/持有/回收、对象身份校验、迁移状态 |
| `kernel/src/fs/rnvpc/recover.rs`、`layout.rs`、`persist.rs` | 持久格式、恢复、持久化抽象 |
| `kernel/src/fs/fs_impls/ext4/migration.rs`、`migration/batch.rs` | orderless 请求批次及完成发布 |

repair image 是已确认存在的开销来源，不等于已证明的唯一瓶颈。收益须以同条件阶段诊断与 fio 对照验证。

## 3. FAST/FAIR 参考边界

本地依据：[论文原文](../../references/FAST-FAIR/analysis/full-text.md)，§3.1–3.5、§4.1–4.2。该目录是论文和提取分析，不是现成 Rust 实现。

- FAST 用有序 shift 和重复指针使未完成记录可被忽略，最后指针写入作为插入提交点；删除先使记录失效再移动。
- FAIR 先初始化并持久化 sibling，发布 sibling 指针，再截断原节点，最后更新 parent。所有层都需要横向路由，根分裂另需持久根切换。
- cache line 不是掉电原子单位；对齐 8B 存储的保证需要在目标架构和模拟后端落实。
- FAST 按跨 cache-line 依赖和最终发布点 flush/fence，不要求每个普通字段独立 fence。自然 eviction 也可能提前落盘，证明不能只考虑显式 flush 处。
- 论文点键记录的唯一指针假设不能直接应用于 SSD 范围、unwritten、重复映射片段和 slot 复用。区间查询以及恢复的解释规则必须补全。
- 本轮先保留写锁和必要读锁，不将无锁读作为性能前提；锁不能代替持久化依赖。

## 4. 格式设计

### 4.1 目标字

建议的新内部类型 `ExtentTarget(u64)`（尚未存在）：

```text
bits 63..61: kind
bits 60..0 : payload

kind 0: Invalid，payload 必须为 0
kind 1: SSD Written，payload 为物理块号
kind 2: SSD Unwritten，payload 为物理块号
kind 3: NVPage，payload 为 slot_plus_one
kind 4: NVM child，payload 为 slot_plus_one
其余: 保留，解码拒绝
```

这是本文的建议编码，不是用户指定的位分配。它给 SSD/NVM 足够地址空间，但仍须检查溢出和非法零 slot；不把 slot_plus_one=0 当有效 NV 地址。节点层级约束允许的 kind。SSD written/unwritten 纳入同一原子字；NV 未初始化数据不发布，零填充等数据语义在发布前完成。

不在该字或 extent entry 中重新嵌入 allocation_seq。稳定字节地址/slot 的身份有效性由 §8 生命周期保证。inode 身份代数、数据版本和后台队列代数属于不同机制，按用途保留。

### 4.2 范围字与布局选择

保留 logical_block、length 的语义；是否压缩物理字段是实现选择。推荐首版研究布局为 16B entry：`range:u64 + target:u64`，range 低 32 位 logical_block、高 32 位 length，NV length=1，内部 entry length=0。该选择与原生 EXT4 的 32 位逻辑起点一致，但比现有自定义 u64 起点更窄。

必须先审计现有持久数据/调用边界：超出编码范围返回明确错误，禁止截断。若现有契约必须支持 64 位逻辑起点，采用保留起点宽度的布局，并重新证明范围更新；不得为了 16B 静默缩小支持范围。实际布局及 API 范围检查由 Task 1 形成定稿，生产实现不得同时模糊支持两种未标版本布局。

即使 range 和 target 各是原子字，两字组合也不原子。修改 target 前必须确保其 range 已准备完毕且中间 entry 不会被错误使用。打包不能替代 §6 的协议。

### 4.3 节点和根

保留节点概念以及 owner、level、sibling、边界和有效范围的职责；`ExtentNodeRecord` 类型可以改造，不以删除类型名为目标。新 sibling/root 持久引用也必须使用原子目标字协议，不能仍原子假设现有 16B PersistentObjectRef。

`entry_count` 不得成为与 sentinel/有效状态冲突的唯一恢复真相；应由新协议定义其为提示还是权威字段。旧校验中“全局无重叠、未用槽全零”的条件不能直接用于合法中间状态。稳定状态校验与恢复状态解码分开。

元数据页的校验方式不得重新要求每次更新先持久化完整节点副本。任何 checksum/generation 更新都必须与字段级协议兼容，不能因持久化次序正常而错误拒绝可恢复节点。

新格式独立版本；旧格式首版明确拒绝，离线转换另行设计，不静默解码或自动格式化。

## 5. 完整 NVM 路径及权威性

按需加载 inode→root→internal→leaf；每个子节点内容和归属先持久化，再发布 NVM 路由，完整路径可恢复后才发布依赖它的新 NV 数据映射。未加载子树可使用明确的 SSD backing 标识，不能把 SSD block 当 NV slot 解释。该表示必须与 Task 1 的 kind/外部 backing 元数据统一，若需要扩充 kind 应更新格式文档和测试。

加载 SSD 元数据只能使用已确认的映射或已有恢复保护基线。orderless 写回中提前落盘的 SSD extent/bitmap 不能作为新权威来源。未完成注册的 inode 不进入新协议；分配中断后的节点作为孤儿处理。

NVM 新分裂节点不必立即分配 SSD backing；后台负责 SSD 元数据分配。释放路径节点前须证明其修改和权威映射已可靠转移、没有活跃引用；首版可保留已加载元数据节点，但必须报告内存占用和 ENOSPC，不能声称已有元数据 eviction。

## 6. 原地更新协议与证明关卡

### 6.1 普通单页 target 替换

前置条件：逻辑范围不变、entry 当前处于稳定有效状态、新旧 target 类型满足该范围语义。

| 阶段 | 修改 | 持久/恢复要求 |
| --- | --- | --- |
| P0 | 分配并初始化新目标 | 持久所有权和内容；旧 target 仍权威 |
| P1 | 验证旧 target 与预期一致 | 在节点修改互斥内进行，防止覆盖他人发布 |
| P2 | 原子写入新 target 并持久化 | 提交点；恢复看到旧或新完整 target |
| P3 | 清理旧资源 | 先确认无有效引用，再等待 pin/读者/I/O 退出后复用 |

SSD→NV 先持久化数据；NV→SSD 必须先有 batch data/metadata 完成与设备 flush 证明。P2 之后任何错误均不能回滚成释放新权威页。

### 6.2 节点内插入、移动、删除

采用 FAST 风格逐步移动，不产生整节点 repair image。需在实现前交付一个精确的 entry 状态判定器，供正常查询、范围遍历和恢复共用：

1. 明确哪个原子字/重复关系使 entry 暂时无效，以及其原映射在哪里仍有效。
2. 每次 shift 先建立可解释的目标位置，再覆盖来源；跨 cache line 的持久化顺序显式记录。
3. 新 entry 的 payload/range 准备完成后才发布有效 target；删除先逻辑失效，后清理或移动。
4. 原有有效映射不能因为准备新记录临时失效。相邻 target 相等但属于合法 SSD 范围的情况不得误判为重复记录。
5. 在每个 8B store、flush、fence 以及可能提前 eviction 的状态下，查询返回旧或新合法语义，不返回拼接的错误目标。

**未解决且必须先验证的适配点：** FAST 原始重复指针规则如何在本项目的区间 entry 上成立；是否需要少量节点内状态字；多字段 entry 的移动判定。允许小型发布状态，但禁止把整节点 repair image 换名引入，也不能把它隐式实现为通用 WAL。若需要额外状态，必须在协议定稿写清编码与回收规则。

### 6.3 SSD 范围变为含 NV 页的范围

设原映射为 SSD `[a,b)`，覆盖单页 p。以下是待由 §6.2 实现的语义分阶段方案，不是可直接 memcpy 的算法：

1. 在稳定树上预留最多两个 entry 空位，必要时先做结构性 split；中途不得因 ENOSPC 丢失原范围。
2. 持久化新 NVPage，暂不发布 NV 映射。
3. 在同一节点内先安装右 SSD 片段 `[p+1,b)`（非空时）；p>a 时再安装独立的 SSD 单页 `[p,p+1)`。它们与旧范围重叠，但映射到相同 SSD 块，unwritten 状态也一致。p=a 时复用原 entry，不插入相同起点的重复记录。
4. 对新片段的可恢复性确认后，p>a 时收缩原范围到 `[a,p)`；p=a 时先持久右片段，再收缩原 entry 为 `[a,a+1)`，避免制造零长有效 extent。p=b-1 时无右片段；原范围 length=1 时直接执行 §6.1，不进行范围准备。
5. 原子将 p 的单页 target 从 SSD 改为 NV，作为此次映射替换提交点。
6. 清理协议允许的冗余状态，更新 DRAM 索引。

步骤 3–4 的重叠只能出现在声明的更新状态中，不能将任意重叠视为合法。查询必须保证在步骤 5 前读旧 SSD 语义，之后命中 NV 页。恢复步骤 5 之前可以完成映射等价的 SSD 规范化并回收未引用 NVPage；之后必须保留新 NVPage。

范围逻辑 split 与 FAIR 结构性 split 是两件事：不因 FAIR 论文允许节点重复就声称以上区间协议已被证明。跨节点、批量 replacement、短写须分别验证；第一版可按页推进多个独立提交点，不承诺整个 write 原子。

### 6.4 结构性 split、父路由与根

S0：初始化新 sibling（含旧后继），持久化其内容和分配所有权。

S1：原子发布旧节点的 sibling 引用并持久化；此前旧节点仍完整，之后右侧已有可恢复路径。

S2：按新节点协议发布截断边界/sentinel，持久化；不得直接沿用旧 entry_count 更新代替完整协议。

S3：用 FAST 类插入发布 parent 路由；parent 未完成时查询可横向进入 sibling。所有层的查找、范围遍历、迁移定位和恢复都支持此状态。

S4：根分裂先初始化可达新根，再原子切换持久 inode root；旧根变成子节点，不可当孤儿释放。未完成 parent 补插可幂等重做，不重复注册 sibling。

split 的重复条目不等于两份资源所有权，不能因两条路径计数重复而错误释放。节点 merge/删除先延后作为单独任务，不能把 split 反向执行就认定正确；允许未满节点保留，逻辑删除与 truncate 的数据可见性仍须正确。

## 7. 与 rTIO 和数据更新的关系

- data 与必要 SSD metadata 异步提交；完成且 flush 成功后，才通过目标字切换为 SSD 映射，再撤销旧 NV 引用和回收。
- completion 持有页对象/版本快照。若前台改变映射或数据版本，旧 completion 不得清除最新权威数据。
- 同一 SSD block 的旧写请求不能在新版本确认后迟到覆盖；现有请求排序/隔离要求保留。
- `allocation_seq` 是分配实例，不是数据版本。移出 entry 后仍需保留脏页版本或迁移期间冻结机制。
- 已发布 NVPage 的再次原地数据写入，是否允许 torn page 属于既有数据持久语义；本方案不声称 4 KiB 数据天然原子，也不悄悄降低已有保证。
- 反向定位从易移动 entry 下标扩展为可验证的逻辑身份，或在节点锁/移动时同步更新；不能让旧 locator 指向另一页后继续 completion。

## 8. 无 entry 分配代数的生命周期

顺序：持久撤销所有有效引用 → 排除恢复可认出的临时副本 → 等待运行时持有者退出 → 持久更新分配状态 → 允许 slot 复用。

运行时持有者包括读者、页缓存/extent 缓存、dirty 候选、迁移快照、BIO、节点路由和延迟任务。可保留 DRAM 候选中的 alloc_seq；或者改为强持有 pin，不能只从 entry 删字段却保留无保护裸 slot。

恢复必须先扫描权威根和合法 sibling/重复状态，建立可达集合，再核对分配元数据并开放 slot 复用。不可达对象在证明无恢复依赖后回收；解析不明对象隔离或报错，不能直接当空闲。中断后重复恢复结果相同。

纯崩溃一致性保证不覆盖任意介质损坏；owner/range 校验仍保留。跨 inode 复用不能让旧 DRAM 缓存误引用新对象，inode generation 或完整缓存生命周期仍需保证。

## 9. 删除、截断和恢复

逻辑删除先持久化文件可见范围/删除状态，再撤销 entry 和释放资源；否则掉电后会从 SSD backing 重新读到已删除内容。Task 6 必须覆盖 truncate、hole punch、unwritten 转换及恢复，不能以暂不实现节点 merge 免除这些语义。

旧 `PendingExtentMutationRecord` 与新协议不可对同一节点同时生效。切换前拒绝旧持久格式，新的恢复解码先解释中间状态，再规范化和开放写入。正常查询与恢复共用状态解释，不分别猜测。

## 10. 验收标准

- 目标路径不再生成整节点 repair image；保留真实 sibling 初始化所需的写入并单独计数。类型命名是否保留不是指标。
- 每个状态模型切点掉电和二次恢复都保持已确认映射；无 dangling 引用、错误区间、旧 completion 覆盖新页或 slot ABA。
- 模拟器不仅在 API 之间中断，还覆盖字写和 cache-line 落盘顺序；不能仅用“函数调用成功”测试替代掉电状态枚举。
- 编译和定向 ktest 覆盖 native EXT4、rNVPC WAL、rNVPCe，特别验证 root/sibling 和恢复变化的共享影响。
- 固定 2 GiB 工作量、64 MiB 脏页上限、32 MiB 低水线和历史 CPU 隔离，4 KiB randwrite/128 KiB write、1/16 线程，至少三轮，同镜像环境和持久化模拟参数。
- 记录前台/flush/completion 时间、repair image 字节/次数、字段写字节、cache-line persist/fence 次数、结构 split 次数、锁等待、slot 占用、fio 及含 drain 带宽。不得通过减少持久性、改变窗口或延迟参数制造提升。

## 11. 实施与未决事项

见 [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)。确定项是混合 entry、8B target、移出 allocation_seq、无整节点镜像和严格回收；range 编码、FAST 区间状态判定、持久化后端原子语义需先完成设计验证。它们是明确的前置交付，不是实施者可以跳过的细节。

不执行 013 重定向方案，不新增 WAL 替代 soft update，不在本文任务中自动提交、推送、格式化或进行持久格式转换。
