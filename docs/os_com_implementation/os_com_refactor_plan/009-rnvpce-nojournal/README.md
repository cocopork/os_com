# 需求：rNVPCe nojournal

日期：2026-10-18。用户已指定优先实现；与 NVPage 批量事务拆分维护。

## 目标与范围

rNVPCe 使用既有 WAL-free soft-update 数据与 extent 协议，不再启动或依赖 JBD2。原生 EXT4 和 rNVPC/WAL 模式的 journal 行为保持原有语义。NVPage 批量事务另见 ../010-nvpage-batch-transactions/README.md，本需求不调整其逐页分配或持久化粒度。

## 挂载和运行需求

1. rNVPCe 挂载不创建 Journal、不启动提交线程、不创建事务 handle、不设置 INCOMPAT_RECOVER；不永久清除磁盘 HAS_JOURNAL 特性。
2. 无 journal 文件系统直接进入已有 nojournal 路径。带干净内部 journal 的文件系统验证日志后，允许以 rNVPCe nojournal 运行；原生模式重新挂载仍可使用保留的 journal。
3. 文件系统 RECOVER 标记、待处理 orphan 或 journal s_start 表明需要恢复时，rNVPCe 挂载必须在任何写入和后台线程启动前失败，提示先通过原生 EXT4 恢复。不能静默忽略日志，不能自动清空磁盘日志。
4. rTIO 的 SSD 分配持久化、数据 BIO 后设备 flush、NVM extent 发布和 NVPage 回收顺序保持。无 JBD2 不等于无 flush。
5. foreground write/fsync 继续依赖现有 NVM 提交协议；sync/unmount 继续完成 native extent 导出及必要 SSD 元数据同步。
6. nojournal 的 native extent 导出按最多64个逻辑块保留并合并节点 after-image，每个区间结束后写一次 inode；失败 after-image 保留并可重试。此处是 SSD 元数据导出，不改变 NVPage 迁出事务协议。
7. 错误传播保留：分配、元数据写入、flush 或导出失败不得报告为成功。原生 EXT4 的日志恢复与提交回归必须继续通过。

## 一致性范围与完整替代要求

挂载策略切换不是整个 EXT4 元数据 soft-update 的证明。已有 NVM 数据/extent 恢复与 SSD 元数据直接写回的边界必须如实报告，不能把 fio 完成或 e2fsck 通过等同于掉电一致性。

完整替代仍需覆盖研究模式支持的创建、目录/链接、写入、截断、删除及同步导出；当前已返回 EOPNOTSUPP 的操作不得偷偷启用。需要验证：

- SSD 位图已写但目标引用尚未发布时的孤儿分配；引用与位图之间的所有权恢复及防重复分配。
- 创建后目录项、inode 位图和 NVM 固定 inode 根的持久化关系。
- 删除/截断先断引用后释放资源，以及 native extent 导出中断后的权威状态。
- NVM 恢复完成后、前台分配及 rTIO 启动前的空间状态重建。

这些恢复覆盖项单独记录完成证据；本次挂载实现不能把未证明项勾选为完成。

## 验收

- 干净 HAS_JOURNAL 镜像以 rNVPCe 挂载后 journal() 为 None，begin_op().get() 为 None，RECOVER 未设置；原生/rNVPC 的 journal 仍有效。
- 有 RECOVER、orphan 或非零 journal start 的镜像拒绝 rNVPCe，磁盘内容不变；先原生恢复后可重新使用研究模式。
- 部分页写、稀疏写、迁出与导出读回正确；迁出分配及完成阶段仍有必要 flush，注入错误保留可恢复 NV 副本。
- 内核回归、release 构建通过；真实盘2GiB写入、sync、unmount和后置e2fsck通过。结果必须说明这只覆盖哪些一致性边界。

## 当前进度

- [x] 挂载默认 nojournal 与脏 journal 拒绝。
- [x] 模式隔离、数据迁出、错误路径测试。
- [x] 构建、回归与真实盘功能验证。
- [ ] 全部 SSD/命名空间元数据的掉电一致性替代证明（不能由前三项推导）。

## 决策记录

用户于2026-10-18要求拆分两项需求并优先实现nojournal。实现直接沿用当前恢复分支工作树，保留先前尚未提交改动，不提交或推送；不在本轮混入NVPage批量事务优化。

验证记录：[RNVPCE-NOJOURNAL-2026-10-18.md](../../../experiments/reports/RNVPCE-NOJOURNAL-2026-10-18.md)。完整元数据掉电一致性项仍未完成，不能由功能测试通过推导。

性能复测：[三轮 EXT4 nojournal 对照](../../../experiments/reports/RNVPCE-NOJOURNAL-PERFORMANCE-2026-10-18.md)，共24点，涵盖4KiB随机写、128KiB顺序写及1/16线程，分别报告fio与包含最终sync的带宽。

## 后续设计：012 orderless migration

[012设计](../012-orderless-migration/README.md)提出取消分配阶段SSD同步，异步提交本批次data与metadata，全部完成后统一flush，再持久发布NVM映射并回收。它以恢复权威及分配核对为前置，尚未实现；本文描述的现有同步顺序和历史验证结论不因此改变。
