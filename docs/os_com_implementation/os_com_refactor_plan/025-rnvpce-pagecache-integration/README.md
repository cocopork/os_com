# 025: rNVPCe PageCache 集成与 CLEAN NVPage 延迟回收

## 1. 问题

当前 rNVPCe 的每次读写都先查询持久化 extent/redirect 结构。即使同一逻辑页刚刚访问过，热路径仍会重复执行
文件树路由、redirect 校验和页身份确认。SSD 冷读也不保留 DRAM clean page；rTIO 完成迁移后立即清除 NVPage，
因而读热数据和容量回收都不能复用 Linux PageCache 已有的索引与冷热页管理能力。

本重构建立三层结构：

1. **PageCache 页索引**：运行期唯一的热数据索引，按 `(inode, page_index)` 定位 DRAM Page 或 NVPage；
2. **NVM 元数据层**：持久化 inode/extent 与 NV redirect，决定掉电恢复后的权威副本；
3. **SSD 基线 extent tree**：保存 SSD block 映射，承担冷启动和 SSD 权威数据的查找。

PageCache 是可丢弃的 DRAM 状态，不取代持久化 extent/redirect，也不参与掉电恢复判定。运行期间，PageCache
同时承担数据位置索引：只要有效数据仍位于 NVPage，对应页槽就必须是 `DirtyNv` 或 `CleanNv`。因此普通
PageCache miss 表示该页当前以 SSD 为权威副本，不再进入逐页 NV redirect 查询。

## 2. 页槽模型

每个 inode 拥有一个稀疏页索引。索引值是引用计数页槽，而不是直接裸指针：

```text
RnvpcePageSlot
  CleanDram(CachePage)       SSD authoritative
  DirtyNv(NvPageRef)         NVM authoritative, redirect durable
  CleanNv(NvPageRef)         SSD copy durable, NV redirect retained until reclaim
  Loading / Converting       transient, wait on this page only
```

索引读取使用 RCU/XArray 的无锁读能力；插入、删除和替换只短暂持有该 inode 的 XArray 写锁。页内容转换由页槽自身的
细粒度状态和等待队列协调。锁不得跨用户拷贝、SSD I/O、NVM persist、容量等待或页复制。

现有 `PageKey -> DescriptorHandle` 和 dirty extent 索引继续用于恢复、迁移候选和持久化 redirect 管理，但不再作为
普通热读的第一层数据索引。

## 3. 读取

```text
inode -> PageCache lookup
  hit CleanDram: copy to user
  hit DirtyNv/CleanNv: pin NVPage, copy to user
  miss:
    lookup NVPool-cached SSD extent metadata
      hit: resolve SSD block
      miss: traverse SSD extent tree and cache the extent metadata in NVPool
    batch read/readahead SSD blocks into DRAM pages
    install CleanDram slots
```

同一页只允许一个 `Loading` owner 发起冷读，其他线程等待该页槽，不重复提交 I/O。连续 miss 使用现有 PageCache
批量读接口与 extent 合并能力；完整页读取不逐页等待 BIO。NVPool 中的 extent 只缓存 SSD 路由，不判断数据是否
位于 NVPage。该判断由 PageCache 页槽完成。

## 4. 写入

- 命中 `DirtyNv`：取得页级写权限，原地覆盖并仅持久化修改范围；PageCache 映射不变。
- 命中 `CleanNv`：先把页状态转为转换中，再使 redirect/NVM 权威关系重新持久化，随后写入并发布 `DirtyNv`。
- 命中 `CleanDram`：分配 NVPage；部分页覆盖先复制 DRAM 旧数据，完整页覆盖不复制；持久化 NVPage 和 redirect 后，
  原子替换页槽为 `DirtyNv`。
- 未命中：先解析权威映射。完整页写不读取 SSD；部分页写从 SSD/NV 权威副本补齐未覆盖字节，再按上述顺序发布。

持久化顺序始终是 `初始化/修改 NVPage -> persist -> 发布 redirect -> PageCache 映射替换`。PageCache 替换不是持久化
提交点；崩溃后只从 extent/redirect 恢复。

## 5. rTIO completion 与 CLEAN NVPage

rTIO 写入 SSD 并持久化对应 SSD 数据与映射后，不立即解除 NV redirect 或释放 NVPage：

```text
DirtyNv -> migration in flight -> SSD copy durable -> CleanNv
```

completion 只发布页槽状态，不从 PageCache 删除该页。持久化 NV redirect 继续保证崩溃恢复可以选择 NVPage；后续回收
事务解除 redirect 后，SSD 才成为唯一持久副本。热读仍直接访问 clean NVPage，前台再次写入可恢复 `DirtyNv` 状态。

## 6. 容量压力回收

NVPage 紧张时，前台只负责触发/协助有界批量回收，不能按一次 4 KiB 写只回收一页。回收批次执行：

```text
claim a CleanNv batch
  -> reserve DRAM capacity; reclaim older CleanDram pages when needed
  -> copy NVPages to DRAM pages outside index locks
  -> acquire the inode PageCache writer once
  -> exact-owner replace CleanNv slots with CleanDram slots
  -> durably detach the old NV redirects
  -> free the NVPage range and notify capacity waiters
```

`DirtyNv`、迁移中或未完成 SSD 持久化的页面不可回收。回收范围保持细粒度 transition ownership，阻止同一范围
在转换期间被前台改写；其他页和其他 inode 继续并行。PageCache exact-owner 替换不得覆盖并发发布的新版本。

回收候选按 worker 和 allocation domain 分区，批量完成 selector/descriptor 清理、slot free 和容量通知。前台线程不得竞争
单一全局 reclaim 队列，也不得持有 PageCache XArray 写锁等待 NV 容量。

## 7. 并发与可扩展性约束

1. 不新增 `pool.state.lock` 一类覆盖所有 inode/page 的热路径锁；
2. PageCache 索引按 inode 隔离，页转换按 page slot 隔离；不同文件和不同页可并行；
3. NV slot 分配/释放继续按 allocation domain 分区，跨域批次先分组再操作；
4. redirect 更新按 inode/entry 批量处理，不在 PageCache 命中路径查询全局目录；
5. 唤醒目标页或目标 worker，避免 `wake_all` 和惊群；
6. XArray/页槽/分区锁内只做状态检查和指针交换，不做 I/O、persist、4 KiB 复制或睡眠；
7. 批量冷读、迁移和回收保留请求粒度，不退化为每 4 KiB 一次完整事务。
8. `CleanDram` 可按冷热和容量压力独立回收；`DirtyNv` 和 `CleanNv` 在 NVPage 释放前不得从 PageCache 独立淘汰。

## 8. 恢复边界

掉电后 PageCache、DRAM clean page、页槽锁和等待队列全部丢弃。恢复遍历持久化 inode/extent/redirect，并在开放
文件访问前为仍有效的 NVPage 重建 `DirtyNv` 页槽：

- redirect 选择 NV 时，NVPage 是权威副本并重建 PageCache 与迁移索引；
- redirect 已清除且 SSD publication 完成时，SSD 是权威副本；
- `CleanNv` 是运行时状态；若崩溃时 redirect 尚未解除，恢复将其作为 NV 权威页重建，不依赖易失的 clean 标记。

## 9. 分阶段实现与验收

1. 增加可保存 DRAM/NV backing 的 per-inode 页槽索引及并发单元测试；
2. 接入 rNVPCe 读路径，验证热读不再遍历 extent，冷读可批量填充 DRAM；
3. 接入写路径及 DRAM-to-NV、CleanNv-to-DirtyNv 转换，验证完整页覆盖跳过 SSD read；
4. 将 completion 改为发布 `CleanNv`，实现批量 `CleanNv -> CleanDram` 转换后再释放 NVPage；
5. 完成崩溃点、并发读写迁移、容量压力、sync/unmount 测试；
6. 在 NVMe 与 SATA 上复测 cold read、hot read、4 KiB thread sweep 和 bs sweep。

正确性门槛：不存在 stale PageCache 映射、悬空 NVPage 引用、双重释放或未持久化数据被标为 clean。性能门槛：PageCache
命中不再执行 extent tree 路由；32 线程不能因新增全局共享锁而出现新的串行断崖。

## 10. 2026-11-18 实现状态

已完成：

- 每 inode 一个 XArray，支持 `CleanDram`、`DirtyNv`、`CleanNv` 和 page-local `Loading`；
- 冷读按最多 32 页合并 SSD 请求，热读直接使用 PageCache 槽位；当前冷读仍逐页查询 NV authority extent，需改为
  PageCache miss 后只查询 NVPool-cached SSD extent 或 SSD extent tree；
- range write 提交后一次查询 redirect overlap，并用一次 inode XArray 写锁发布整个 DirtyNv 范围；
- rTIO 把 `IoComplete` 与 `Clean` 分开：先批量持久化 SSD 权威状态，再保留运行时 NV 数据和 allocation owner；
- completion 精确地把仍指向旧 entry 的 `DirtyNv` 槽改为 `CleanNv`，不会覆盖并发的新版本；
- 仅在容量/进度压力或 sync 排空时执行 worker-local 批量回收；下一步统一在锁外把整批 `CleanNv` 复制为
  `CleanDram`，每 inode 只获取一次 XArray 写锁，然后解除 redirect 并释放 NVPage；
- redirect directory、reclaim queue 和容量释放继续按 inode owner worker/allocation domain 分区，没有新增全局热路径锁。

尚未完成：

- `CleanNv` 再次写入目前先请求所属 worker 回收旧 clean entry，再重试正常写事务；直接原槽恢复 NVM 权威并转为
  `DirtyNv` 仍是后续优化；
- NVMe/SATA 完整性能矩阵和 32 线程验收属于阶段 5，不能仅凭正确性 ktest 宣称性能目标已经达到。
