# rNVPC 持久 WAL、rNVPCe 与 EXT4 接线

本轮在 `os_com_rnvpc_rebuild` / `recovery/rnvpc-rebuild` 实现，基于 `49e7829d6f6718f0def92231fcdbb5691c883629`。原 `os_com_reconstructed` 跟踪文件未修改；本轮代码与说明随 rebuild 基线提交保存。恢复的候选/会话来源不是缺失历史检查点的逐字节复原。

## 实际实现

- rNVPC 生产上下文使用持久 per-inode WAL、双镜像发布结构和 B-link 树。每 inode 1024 条；第 1025 次写在前台按 LSN 重放完整日志，不合并重复键；树与发布状态持久化后推进 WAL head。SSD 完成不退休 WAL，wal-pinned 页不参与迁移。
- WAL 提交歧义保留被引用数据；恢复重放、树分裂/根发布故障、generation/owner 校验、忙页等待与新读者排斥得到补充。rNVPC 与 rNVPCe 的持久元数据格式分别扫描；共池不同文件系统可共存，现有持久 authority 不允许直接切换模式。
- rNVPCe 保持无 WAL：fixed inode、extent soft updates、pending repair、8-byte reverse locator、count reservation、直接用户前缀复制到 NVPage。完成路径按 leaf 更新，释放 credit 后回收精确 generation。
- EXT4 普通、同步与 O_DIRECT 文件操作均分流到所选模式。rNVPCe 不经过 PageCache；每文件 fsync 不触发 SSD 导出。显式文件系统 sync（包含现有 VFS unmount 同步入口）排空迁移并将 SSD-selected extents 导出到原生 EXT4；恢复后未访问的 fixed inode 也会枚举。
- rTIO 使用两个 worker、128 页上限、物理 NV DMA segment、连续目标合并 BIO；IRQ 回调仅入队/唤醒，持久化、引用发布和回收在进程上下文完成。后台受高水位门控，显式 sync 强制迁移。
- 双 NV 模式拒绝尚未实现 graph-aware 语义的 resize/fallocate/unlink/rename-overwrite；mmap 不暴露旧 PageCache。正常模式保持原生读写。
- rNVPC 先持久化原生 SSD 分配与 extent 映射，再允许 WAL 引用 target；WAL 发布后原生 i_size 持久化，才返回成功前缀。等待前释放 inode 锁与 journal handle，正确 stamp datasync_tid，重试重新读取 mapping/内容，用户前缀只消费一次。这一保守适配增加前台 SSD 同步开销，不能当作原研究性能复原。

## 证据与验证

源文件证据参见 `persistent-wal.md`、`rnvpce-integrity.md`、`ext4-integration.md`、`shared-integration.md`、`integration-frame-sources.json`。独立源码复查见 `integration-review.md`。

测试与构建日志位于 `/home/zn/asterinas-build-env/wal-ext4-integration/`。`run-targeted.py` 提取实际 ktest 函数名，按 OSDK 的精确后缀匹配规则生成 whitelist；没有使用不存在的 substring 过滤能力。

- `build-04.log`：初次整核构建成功（后续修复前的快照）。
- `ktest-01.log`：成功启动但 0 tests，明确无测试覆盖。
- `ktest-02.log`：45 通过、3 失败；重启 fixture 未 format，修正后重跑。
- `ktest-03.log`：50 通过、1 失败；共池 rNVPCe mount 错把 WAL payload 解码为软更新 metadata，修复扫描隔离并新增回归。
- `ktest-04.log`：最终 KVM/OSDK 定向测试 **56 通过、0 失败、678 filtered out**，命令退出 0。包含 9 个持久 WAL/共池恢复测试、5 个 rNVPCe 完整性测试、5 个 EXT4 NV 路径测试、2 个挂载参数测试及其余共享模块回归。测试选择见 `ktest-04-selection.txt`。
- `build-final.log`：当前最终源码 `make kernel` 退出 0，生成 ISO（89712 sectors）。存在编译告警，不声称 warning-free。
- `git diff --check` 通过；当前 32 个变更/新增 kernel Rust 源文件的 SHA-256 见 `integration-source-manifest.json`。

## 尚未解决的边界

1. **rNVPCe migration 的 SSD allocation→NV target 发布存在崩溃 ownership 窗口。** 分配落盘后、descriptor target 发布前断电会泄漏 SSD 块。运行期已经补上发布前可证明私有时的回滚；回滚失败或目标持久化结果歧义会锁存错误、停止再次分配，绝不盲目 free 可能已被持久引用的块。但这不是跨两个介质的持久 ownership ledger，不能声称无泄漏崩溃恢复。
2. NV backend 是 DRAM 模拟；Model restart/fault tests 验证持久镜像协议，不证明真实 PM 掉电原子性、硬件 DMA 持久性或性能。
3. 定向 EXT4 tests 使用隔离 RAM block-device fixtures；不能代替完整用户态文件系统负载、真设备掉电与 journaled NV 模式故障矩阵。原生事务同步新增开销尚未测量。
4. 需要 graph-aware 生命周期实现才能解除 NV 模式的截断/删除等限制；本轮不静默走不安全的原生释放路径。
