# rNVPCe EST 路由缓存设计

## 目标

降低 rNVPCe 覆盖写的 write plan 开销，使重复写已映射范围时不再逐次读取和校验
NVM extent tree 节点。缓存必须扩展现有 EXT4 extent status tree（EST），不能新建
第二套 inode 路由缓存。

## 权威边界

EXT4 原生 extent tree 与 rNVPCe 的 NVM 权威 extent tree 在运行期间可能不同步。
因此，现有 EST `map` 继续只表示原生 EXT4 tree 的事实；同一个 `EsCache` 内新增
独立的 rNVPCe SSD-route 层，不能把 NVM 权威映射混入原有 `map`。

rNVPCe route 层是 DRAM 加速状态，不参与恢复。每组事实绑定 NVM inode
generation；查询 generation 不同时整层清空并 miss。rNVPCe extent tree 发生权威
修改时必须发布新的 inode generation；redirect 新建、扩展、迁移和回收不改变 SSD
baseline，因此在 generation 不变时缓存事实继续有效。

## 缓存内容

- 只缓存 `logical range -> SSD physical range + written state` 的完整映射事实。
- 不缓存 hole。查询范围存在任何未覆盖块时返回 miss，仍由原 NVM tree walker 判断
  hole 和 allocation goal。
- tree miss 后回填遍历过程中已经验证的完整 SSD extent，而非只回填请求裁剪后的
  4 KiB，以便随机覆盖同一大 extent 的后续请求命中。
- 复用 EST 的容量上限、重叠替换和整表淘汰策略；rNVPCe route 层使用同一个 EST
  内部细粒度锁，不扩大 inode write lock 的范围。

## write plan 流程

1. 查询 DRAM redirect index。
2. 若 redirect 完整覆盖请求，跳过 NVM extent tree 和 EST SSD-route 查询。
3. 否则按当前 NVM inode generation 查询 EST rNVPCe route 层。
4. 完整命中时直接构造 SSD route；miss 时遍历 NVM extent tree，并把验证过的完整
   SSD extents 回填 EST。
5. redirect 继续覆盖 SSD baseline，最终按现有规则构造 `ExistingNv`、
   `ExistingSsd` 和 `NewAllocation`。

## 一致性与非目标

- generation mismatch 只能造成 cache miss，不能返回旧映射。
- cache miss、容量淘汰和重启后空缓存均不改变行为。
- 本阶段不缓存 hole、不改变持久化协议、不改变 allocation goal 策略，也不为 EST
  增加新的全局锁或 inode 级锁。
- debug ktest 必须比较缓存 payload 与 NVM tree walker 的 logical range、SSD block 和
  written 状态，补齐现有 EST 仅校验 coverage 的不足。
