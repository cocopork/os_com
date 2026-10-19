# EXT4 journal / nojournal Delalloc Implementation Plan

> 执行技能：executing-plans；用户2026-10-17已授权实现，直接推进，不提交推送。

**Goal:** Native regular-file buffered write 默认延迟物理分配，journal/nojournal writeback完成分配和元数据持久化。
**Architecture:** ES独立的不可丢弃delayed层＋统一容量账本中的持有者预留＋writeback有界事务。区分buffered size与可捕获size；保留nodelalloc对照。
**Tech Stack:** 当前Rust/no_std、OSDK、Docker/QEMU、EXT4/JBD2。
**Spec:** rebuild `docs/superpowers/specs/2026-10-17-ext4-delalloc-design.md`；本目录007/008以及 `../../ext4_performance.md` §3/4。

## 约束及审查重点

保留全部已有修改。延迟状态不能被ES事实缓存淘汰/无效化带走；不另建平行PendingAlloc树。预留含数据与extent插入/转换元数据，转inflight必须原子且不能被其他分配意图偷用。所有事务切分先登记ordered数据并捕获一致inode。属性捕获不能提前暴露pending size；nojournal fsync必须刷bitmap/GDT/extent/inode。短写、故障、truncate、punch、删除、direct I/O和研究模式交互均须测试。

## 任务

- [x] 1. 先运行两模式前台不分配测试，确认RED；补充状态协议及锁序台账。
- [x] 2. 增加持有者容量预留、原子转换和ES delayed层；先测重叠、释放、并发额度及错误路径。
- [x] 3. 接入默认delalloc/nodelalloc解析和前台缓存写入；处理短写与partial-page读取，旧精确分配测试显式使用nodelalloc。
- [x] 4. 接入有界分配writeback和ordered journal捕获；fsync/sync/卸载统一使用；补充nojournal分配元数据批量持久化。
- [x] 5. 处理truncate/punch/fallocate/direct/属性/删除的pending状态及safe size；故障不能泄漏额度或提交不完整映射。
- [x] 6. 运行针对性、完整kernel回归和remount/recovery测试，独立最终审查；记录实际证据后更新007/008状态。

## 执行证据

`/home/zn/asterinas-build-env/ext4-delalloc-20261017/`；任务起点源码/工作树diff已保存。现有linked worktree继续使用。真实设备仅允许既有授权的nvme1n1p1，功能验证优先内存盘，不新增未授权设备操作。


## 完成记录（2026-10-17）

当前工作树实现完成，未提交推送。完整 kernel 299/299；补充 committed-but-uncheckpointed delalloc 前缀崩溃/replay 1/1。570个原场景体保留且全部通过一次，新增状态/容量场景2个、功能入口10个。独立最终审查发现均修复并验证，无延后审查项。

预留预算采用保守27块/未知页；已知 written 的 EOF 追加保留 pending coverage 但不增加物理预留。非零 truncate/范围变更/direct 先 drain；零截断和删除取消。旧精确分配 fixture 显式 nodelalloc；真实挂载默认 delalloc。台账完整记录取舍与验证失败，不把空筛选运行或编译失败计为通过。
