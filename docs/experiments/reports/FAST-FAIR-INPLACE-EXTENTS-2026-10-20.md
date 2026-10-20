# FAST/FAIR 原地 Extent 实现与崩溃验证

日期：2026-10-20

## 实现范围

- `ExtentRecordEntry` 改为 24B：`logical_block`、`range_tail`、原子 `target`，entry 不再持久化 `allocation_seq`。
- SSD/NV/child 类型和地址合并到一个 8B `ExtentTarget`；稳定 target 替换不再复制整个 extent node。
- SSD range 加载单页到 NVPage 时，先发布等价 SSD 片段，再收缩旧范围，最后用一个 target 字发布 NVPage。
- FAIR split 先持久化右 sibling，再以 8B sibling slot 使其可达，随后缩小左节点 count，最后补 parent route。
- 查询、恢复扫描和 SSD extent 遍历均沿 sibling 链访问 parent 尚未补 route 的节点。

inode root 仍是 16B `PersistentObjectRef`，根切换继续使用既有 pending mutation 协议。本次没有删除 truncate、hole-punch 等其他变更路径仍使用的旧 repair-image 实现。

## 崩溃点验证

测试使用 `DramNvImage` model 后端。在每个指定持久化边界模拟退出，然后重新执行 `Rnvpc::recover`，通过正式查询路径读取映射；NVPage 发布成功时还读取并比较整页内容。

| 操作 | 枚举状态 | 验证结果 |
| --- | ---: | --- |
| 8B target 更新 | 写入前、写入持久化后 | 只能读到旧或新 SSD target，无半写 target |
| 逻辑 extent split | 12 个阶段（0–11） | 8 个逻辑块始终连续可读；最终阶段 NVPage 内容逐字节一致 |
| FAIR 节点 split | 8 个阶段（0–7） | parent route 未完成时由 sibling 到达右节点；恢复后 8 个逻辑块均可读 |

完整内核 ktest：322 passed，0 failed。运行环境为 `os-com-recovery-tools:workspace-20261018`、QEMU/KVM、1 vCPU、4 GiB RAM。
