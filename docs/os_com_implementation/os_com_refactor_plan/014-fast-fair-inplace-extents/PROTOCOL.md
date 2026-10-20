# FAST/FAIR In-place Extent Protocol

日期：2026-10-20。本文冻结第一版实现协议；README 描述目标，本文描述持久状态机。

## 1. 平台和格式边界

- EXT4 `Iblock` 是 `u32`，当前 rNVPC API 使用 `u64 logical_block`。第一版保留 u64，拒绝加法溢出。
- EXT4 物理块 ID 是 u64，磁盘格式实际使用 48 位。target 提供 61 位 payload，构造时拒绝超限。
- pool 容量由配置限制为不超过 u32 slot；NVPage/child 的 payload 保存 `slot_plus_one`，0 非法。
- `NvMemoryBackend::store_page_u64` 要求 8B 对齐；DRAM/model 后端用单次 machine-word store。`persist(offset, 8)` 加 `fence()` 建立持久顺序。
- cache line 不视为原子，16B `PersistentObjectRef` 不视为原子。entry 不保存 allocation sequence。
- 新 extent 持久格式与旧格式不兼容。生产切换时提高格式版本并拒绝旧格式；不自动转换或格式化设备。

## 2. Entry 布局

稳定布局为 24B，对齐 8B：

```text
offset +0: logical_block: u64
offset +8: range_tail:    u64  // low 32 bits length, high 32 bits reserved=0
offset +16: target:       u64  // 0 means unpublished/free
```

`target` 位 63..61 是 kind，位 60..0 是 payload：

```text
1 SSD-written   payload=physical block
2 SSD-unwritten payload=physical block
3 NVPage        payload=slot_plus_one, length=1
4 child         payload=slot_plus_one, length=0
0 unpublished
5..7 corrupt/reserved
```

`logical_block` 和 `range_tail` 只在 target=0 时任意准备。对于保持起点不变的 SSD extent 收缩，允许一次对齐 8B range_tail store；其依赖片段必须先持久化。target 是 entry 插入、删除及 SSD/NV 类型替换的提交字。

第一版运行时使用稠密有效前缀，并允许前缀后的 slot 留有一次未完成准备产生的字节。插入先准备 slot，最后用包含 `level/entry_count/flags` 的对齐 8B header word 扩大有效前缀；因此 target 已写而 count 未提交时仍忽略该 slot。删除和 FAIR 清理将在后续切换到 target=0 的稀疏扫描后启用。合法的两个 SSD entry 可以具有相同 target payload，不能以“相同指针”判定无效。

## 3. Insert/Delete/Replace

插入：

1. 在节点写锁下选择 target=0 的槽。
2. 分别写 logical_block 和 range_tail；每个 8B 字 persist/fence。
3. 原子写 target；persist 8B 并 fence。
4. 原子写包含新 entry_count 的对齐 8B header word；persist/fence。此步提交。第 3、4 步之间崩溃时旧有效前缀不变。

删除：

1. 原子写 target=0；persist 8B 并 fence。此步撤销可达性。
2. 等待所有 pin、查询、迁移快照及 I/O 退出后，槽可复用；旧对象 slot 还须经过对象退役协议。

范围不变的 target 替换：先持久化新资源，再比较 expected target，随后原子 store/persist/fence 新 target。store 前崩溃读旧映射，store 落盘后崩溃读新映射。

运行时首版保留节点读写互斥。恢复不持有旧锁，因此仍必须按上述中间状态正确解析。

## 4. SSD Range → NVPage

原范围 `[a,b)`，目标页 p：

- length=1：直接 target 替换。
- p=a 且 length>1：插入并持久发布右片段 `[a+1,b)`；原子持久收缩原 entry 为 `[a,a+1)`；替换原 entry target 为 NV。
- p=b-1 且 length>1：插入并持久发布单页 SSD `[p,b)`；原子持久收缩原 entry 为 `[a,p)`；替换单页 target 为 NV。
- a<p<b-1：依次插入并持久发布单页 SSD `[p,p+1)`、右片段 `[p+1,b)`；原子持久收缩原 entry 为 `[a,p)`；替换单页 target 为 NV。这个顺序使最终有效前缀保持按逻辑块排序。

在收缩前的重叠必须映射同一逻辑块到同一 SSD 物理块，且 written/unwritten 相同。查询/恢复枚举所有匹配项：等价时返回该映射，冲突时返回 EUCLEAN。NV target 只在旧范围已经持久收缩后发布，因此合法状态没有 SSD/NV 权威冲突。

每个替换最多需要两个空槽。空间不足时先执行 FAIR split；ENOSPC 发生在发布 NV target 前时，原 SSD 范围继续可读，未引用 NVPage按对象协议回收。

批量写由多个单页提交组成，短写保留已提交前缀；本协议不承诺整个 write 原子。

## 5. FAIR Node Split

1. 分配 sibling，复制右半有效 entry，设置其旧 sibling；完整持久化新节点和对象所有权。此时新节点不可达。
2. old node 的 sibling 是单个 8B `slot_plus_one` 字。原子发布并 persist/fence；查询仍可先使用 old 中的重复映射。
3. 原子缩小 old 的 `entry_count`，隐藏已复制的右半前缀后缀。若 old 未命中，查询根据 sibling 首有效边界横向继续；被隐藏 slot 暂不复用。
4. 使用本协议的 entry 插入向 parent 发布 `(boundary,sibling)`。parent route 允许物理 slot 无序，查询选择不大于 key 的最大 boundary；parent 尚未更新时 sibling 链保持可达。
5. 根分裂先持久化新根和 children；当前 inode root 仍是带 allocation sequence 的 16B 引用，继续使用既有 pending mutation/repairable publication 协议切换。8B 原子发布只适用于 extent entry target 和 node sibling slot，不能把 inode root 误当作原子字。

所有层都支持 sibling 横向查找。恢复从 inode root 出发遍历 child 与合法 sibling；重复映射只计一次资源所有权，并幂等补全 parent route。循环、悬空 sibling、冲突映射或非法 kind 返回 EUCLEAN。

节点 merge 不在第一版范围。允许低占用节点长期存在。

## 6. Slot 生命周期

entry 不保存 allocation sequence。复用顺序固定为：

```text
持久撤销所有权威/临时引用
  → 恢复扫描确认不可达
  → 等待 DRAM cache、pin、dirty queue、迁移、BIO 和延迟任务退出
  → 持久清除对象 descriptor
  → allocator 允许复用
```

DRAM 队列可继续用 allocation sequence 过滤旧候选。数据版本另行保护旧 completion，不能用 slot 生命周期替代。

## 7. 故障验证

模型和内核测试在每个字段 store、persist、fence 后模拟退出，以持久镜像重建树并读取所有受影响逻辑块。至少覆盖：首/中/尾页 split、合法等价重叠、冲突重叠、未发布 entry、sibling 发布前后、旧 entry 删除的每一步、parent/root 未完成及二次恢复。

成功标准是返回旧或新合法内容；允许映射等价重复，不允许缺口、错误 target、悬空引用、循环或访问复用 slot。
