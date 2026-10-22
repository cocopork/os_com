# 015：连续 NV extent 与整段迁移

日期：2026-10-21。状态：Implemented，性能仍待继续优化。

## 1. 目标

当前 rNVPCe 前台把一次 128 KiB 写拆成 32 个 4 KiB NVPage，并为每页发布一个
`ExtentRecordEntry`。后台 completion 随后又逐页验证和修改同一 leaf。这使大块写的
extent 操作、反向索引和 NVDIMM 持久化次数随页数线性增长。

本需求把逻辑连续且 NVDIMM 槽位连续的一组 NVPage 表示为一个 NV extent：

```text
ExtentRecordEntry {
    logical_block = L,
    length        = N,
    target        = NvPage(base_slot + 1),
}
```

页 `L + i` 解析为 `base_slot + i`。每个 NVPage 仍有独立描述符和 allocation sequence，
用于页身份、恢复和防止 slot ABA；allocation sequence 不重新放回 extent entry。

## 2. 第一版边界

1. 预留器优先从空闲池取得连续槽。若池已碎片化，提交端按“逻辑地址连续且 NV 槽连续”
   合并为最少的 maximal runs；每个 run 发布一个 entry。
2. 前台先写完一个批次的数据和所有页描述符，再按已有双描述符协议批量发布 selector；
   所有页 ready 后才发布 leaf entry。数据 cache line 仍需逐一 flush，但依赖阶段共享 fence。
3. completion 只迁移完整 NV extent。只有 entry 内所有页均被同一批次持有、身份与版本未变、
   且 SSD 目标逻辑和物理连续时，才用一次 8B target 更新把整段从 NV 切到 SSD。
4. 任一页忙、被前台重写、缺少完成结果或目标不连续时，整段推迟；第一版不拆分活跃 NV extent。
5. target 切换持久后，批量清除全部页描述符，共享阶段 fence，再在一个分配临界区中归还槽位。
6. 候选发现继续使用现有逐页 dirty queue。按 dirty extent 建立的 DRAM 索引是后续独立优化，
   不作为本需求正确性或性能成立的前提。
7. 候选收集后在短 inode publication 临界区内二次校验完整 extent 并发布 SSD target 身份；
   data BIO 和设备 flush 期间不持有 publication guard。completion 在另一个短临界区内按当前
   forward edge 重新解析 locator，以处理并发 leaf rewrite 导致的 entry index 移动。
8. EXT4 migration round 可以达到 1024 页，descriptor/reclaim 事务保持 128 页上限。completion
   先按当前完整 extent 分组，再把若干 extent 装入不超过 128 页的事务，绝不从 extent 中间切批。

## 3. Soft-update 顺序

### 3.1 前台 SSD 到 NV

| 阶段 | 操作 | 掉电后的权威状态 |
| --- | --- | --- |
| A0 | 在 DRAM 中预留连续 NV slots；extent 尚未引用 | 原 SSD 映射 |
| A1 | 填写 N 个 NVPage 数据和 inactive descriptors，flush 全部相关 cache lines，fence | 原 SSD 映射；孤立 NV 对象可回收 |
| A2 | 发布 N 个 descriptor selectors，flush 后 fence | 原 SSD 映射；NV 对象已可恢复但不可达 |
| A3 | 准备范围字段，原子发布一个 NV target 并持久化 | 新 NV extent |
| A4 | 更新 DRAM 反向索引和 inode 可见状态 | 由 A3/A4 已持久字段判定 |

一个用户 write 可以产生多个连续 runs；每个 entry 是独立提交点，沿用现有短写语义，不承诺
整个 syscall 原子。

### 3.2 后台 NV 到 SSD

| 阶段 | 操作 | 掉电后的权威状态 |
| --- | --- | --- |
| C0 | 冻结并 pin 整个 NV extent，分配连续 SSD blocks | NV extent |
| C1 | orderless 提交 data 与 EXT4 metadata | NV extent；SSD 中间内容不权威 |
| C2 | 批次请求全部完成后发一次设备 flush | NV extent |
| C3 | 验证 N 页仍属于同一个 entry，原子切换 target 到 SSD base，persist + fence | SSD extent |
| C4 | 批量清除 N 个 descriptors，persist + fence | SSD extent；残留 NV 只会泄漏 |
| C5 | 解除 pins，并在一个分配临界区归还连续 slots | SSD extent |

C3 是权威副本切换点。它只改变对齐的 8B `ExtentTarget`；`logical_block` 和 `length` 保持不变。
因此掉电只会看到完整的旧 NV target 或完整的新 SSD target，不会看到逐页混合的 extent。

## 4. 查找、反向索引与恢复

- 查询命中 NV extent 后按 `logical_block - entry.logical_block` 计算 slot 偏移，并校验目标页描述符
  的 `(fs_id, inode, logical_block)`。
- 每个页都保留一个 DRAM `ReverseExtentRef`，可以共同指向同一 leaf entry。completion 必须同时
  验证 entry 范围、base slot、每页 allocation sequence 和连续 SSD target。
- recovery 扫描一个 NV entry 时展开其全部页，逐页验证 descriptor 后重建页索引和反向索引。
  任一成员缺失或身份不符都不能发布悬空的部分范围；挂载应报告损坏或按既有修复规则保守处理。
- 逻辑 extent split 必须能切分 NV range，并给右片段的 base slot 加相同偏移。普通覆盖写可以因此
  保留未覆盖的 NV 前后片段。

## 5. 验收

- 构造 32 个逻辑连续、物理连续 NVPages 后，树中只有一个 `length=32` 的 NV entry。
- 命中该范围的首、中、尾页均解析到正确 slot；恢复后结果相同。
- completion 缺少任一页时不修改 entry；完整 32 页且 SSD blocks 连续时只执行一次 target persist。
- 在 descriptor ready、entry publish、SSD target switch、descriptor clear 后模拟崩溃，读取只得到
  完整 NV 版本或完整 SSD 版本，不出现悬空或混合范围。
- 4 KiB 随机写不退化；128 KiB 顺序写的 entry 数、leaf target persist 次数和 fence 次数显著下降。
  最终按 2 GiB、64 MiB 高水线、32 MiB 低水线和既有 CPU 隔离配置对比 fio。
