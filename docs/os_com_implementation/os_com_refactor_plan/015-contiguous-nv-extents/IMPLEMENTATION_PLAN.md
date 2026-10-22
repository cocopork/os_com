# 连续 NV extent 与整段迁移实施计划

日期：2026-10-21。对应 [README.md](README.md)。

## Task 1：范围表示和解析

- [x] 为 `ExtentRecordEntry::nv_leaf` 增加 length，并允许合法的多页 NV range。
- [x] 增加 `nv_slot_at(logical_block)`，集中处理范围检查、偏移和溢出。
- [x] 修改 lookup、切片和恢复遍历，消除 `NV length == 1` 假设。
- [x] 增加首/中/尾解析及 NV range 切片 ktest。

## Task 2：连续预留与前台合并

- [x] 空闲槽预留优先选择请求大小的连续 run，保持一次 allocation lock。
- [x] 让 reservation 按确定的槽位顺序消费，碎片化时仍能完整预留。
- [x] 将新页按逻辑地址和 NV slot 合并成 maximal runs，再调用一次树替换。
- [x] 增加批量写只生成一个 NV entry 的定向 ktest，并保留碎片化 fallback。

## Task 3：反向索引和恢复

- [x] leaf reverse refresh 为 NV range 内每页安装同一个 entry locator。
- [x] recovery 扫描展开 NV range并逐页验证 descriptor identity。
- [x] 增加重建后首/中/尾页可查和缺失 descriptor 被拒绝的测试。

## Task 4：完整 extent completion

- [x] 候选仍按页收集，但按 `ReverseExtentRef` 分组并要求覆盖 entry 的完整范围。
- [x] 要求 migration SSD targets 与逻辑页一一对应且物理连续。
- [x] leaf 锁内一次验证整个范围，只持久化一次 8B target。
- [x] target 发布后批量释放 credits、清 descriptors、归还 NV slots；部分或过期组整体推迟。
- [x] completion 在短 publication 临界区按当前 locator 重分组，避免 leaf rewrite 后使用旧 index。
- [x] 按完整 extent 装箱到 128 页事务，兼容 1024 页 EXT4 writeback round。
- [x] 增加“缺一页不发布”和“完整范围一次发布”的 ktest及故障点验证。

## Task 5：验证和测量

- [x] 运行生产与 ktest 配置编译、相关 rNVPCe ktests和崩溃恢复测试。
- [x] 记录 4 KiB randwrite、128 KiB write 的 fio 结果。
- [x] 检查连续写只生成一个 NV entry，completion 缺页不发布且完整范围一次切换。
- [x] 将结果写入 `docs/experiments/reports/`；原始数据保持在未跟踪目录。
