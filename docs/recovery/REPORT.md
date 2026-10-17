# rNVPC 缺失文件：证据优先恢复结果

## 当前结论

11 个缺失文件均找到了旧实现、原始候选片段或历史会话代码，已在独立工作树拼接为恢复候选。没有哪个文件需要整体从零重新实现；不完整接缝、版本适配和新增验证代码分别记录。**这些文件不是校验通过的原始目标 blob，也不是已经完成的研究内核。**

工作树：`/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild`，分支 `recovery/rnvpc-rebuild`，基点 `49e7829d6`。原 `os_com_reconstructed` 的跟踪文件保持不变；未提交或合并这些候选。

## 找到的基础

| 文件 | 主要基础 | 来源说明 |
|---|---|---|
| backend.rs / config.rs / dram.rs | 完整旧 nv_pool 实现、完整候选方法、日期明确的会话补丁 | [后端证据](backend-provenance.md) |
| descriptor.rs / index.rs / metadata.rs | 原始结构体与方法片段、最新持久化类型标签、会话字段布局 | [元数据证据](metadata-provenance.md) |
| cow.rs / extend_cow.rs / recover.rs | 可拼接的历史输出、原始候选正文、交叠内容验证 | [逻辑证据](logic-provenance.md) |
| mod.rs / tests.rs | 完整旧入口、最新候选入口、完整旧测试与会话新增测试 | [入口和测试证据](entry-tests-provenance.md) |

候选审计扫描 54,731 个文件，无读取错误；没有找到 11 个文件的精确目标 blob。可遍历的 57 个历史提交中虽有对应旧对象 ID，但其 blob 也不可用。详见外部审计 `/home/zn/asterinas-build-env/rnvpc-candidate-search/REPORT.md` 和 `rnvpc-restoration/historical-blobs.json`。

## 编译与测试

- 使用已部署的官方 Asterinas 0.18.0-20260618 工具镜像及 nightly-2026-04-03；验证容器 `os-com-rnvpc-rebuild`。
- 生产 `make kernel`：失败。第一次揭示 WAL 记账辅助函数的拼接遗漏，已从历史会话补回并复审。第二次剩余 **17 条错误**，均指向尚未恢复的 EXT4 导出/方法。
- 主要缺项：`LATENCY_STATS`、`read_ns`、`LATENCY_ENABLED`、completion 阶段常量与函数、`dump_latency_stats_once`、`dump_nvpage_profile`、`free_blocks_count`、`free_inodes_count`。这是混合版本 EXT4 文件的恢复缺口，不能用空函数或释放块/释放 inode 的接口替代。
- 元数据独立验证：**5 通过、0 失败**；使用恢复模块和仓库真实 Pod derive，覆盖布局/偏移/标签、损坏数据、引用边界和 extent 合法性。测试替代了内核 prelude 等环境接线，因此不证明内核运行或崩溃恢复通过。
- `cargo osdk test rnvpc`：最终也因同一组 **17 条 EXT4 错误**失败，未启动 QEMU。首轮测试的 4 处 reservation 参数不匹配已换回完整匹配版本；1 个依赖尚未恢复的新 API 的后期测试完整保存在 `later-tests/`，没有用空实现让它通过。`tests.rs` 当前纳入 25 个测试，另有模块内测试；不表示这些内核测试已运行通过。
- 11 个恢复文件的最终 rustfmt 检查通过。独立测试 harness 内的 descriptor/metadata/index/id 文件与当前工作树逐字节一致。
- 日志：`/home/zn/asterinas-build-env/rnvpc-restoration/integration/`。

## 不能混同的功能边界

1. 找回的 `cow.rs` 匹配现存 legacy `wal.rs`：DRAM WAL 成本模型、completion 退休。它不满足最新设计的持久化 1024 条 WAL、前台全量 checkpoint。历史记录显示该设计涉及另一路 `pcache_layout` / `pcache_store` / `persistent_btree` 依赖，应继续按版本恢复，不能把旧路径标记成最终实现。
2. `extend_cow.rs` 是兼容路径；最终 rNVPCe 应继续核对现存 RangeWriteBatch、直接 NV 写、EXT4 接入与 rTIO 完成链。
3. 测试仅是可恢复子集，完整 crash/restart、固定 inode、批量写入、迁移和修复矩阵尚未恢复。

## 审阅与保留问题

[独立审阅](review.md) 已确认 WAL 拼接错误补回，但发现继承的高风险行为：不同版本的重复固定 inode 根可能被接受并清理另一根的图；legacy completion 错误退出可能丢失 pending 回收记录并漏掉 WAL 额度释放。另有 extend-only 统计提前返回问题。

本轮将这些问题作为恢复基线的已知缺陷记录，未将推测性算法修复混入来源恢复；因此候选不可宣称 crash-safe，也不应据此运行真实数据实验。下一阶段需要单独的缺陷修复与针对性测试。
