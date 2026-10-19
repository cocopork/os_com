> **Archived:** original date: 2026-09-22. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# rNVPC 与 rNVPC-extend

本目录保存 rNVPC 和 rNVPC-extend 的权威设计及实验实现映射。

| 文档 | 内容 |
|---|---|
| [design.md](rnvpc-original-design.md) | rNVPC、rNVPC-extend 的数据模型、WAL、soft update、分层与恢复语义 |
| [porting.md](rnvpc-porting.md) | 映射到 Asterinas 的组件、接口、阶段与验证要求 |
| [design-evolution.md](rnvpc-design-evolution.md) | 旧路线与当前路线的逐项对照及废弃内容 |
| [../rTIO/design.md](rtio-original-design.md) | rTIO 的 inode 粒度并发 writeback 设计 |

核心区分：

- **rNVPC**：用现有 DRAM PageCache、per-inode WAL 和 CoW 模拟 Rust + PM PageCache 的性能。
- **rNVPC-extend**：NV 中只保留 NVPage/NVBH 数据面；B-tree、锁、计数器和 handle 在 DRAM
  中运行，并在掉电后扫描重建。**一致性由 soft update（§5.1）提供，不使用 WAL。**
- **rTIO**：按 inode 分配 worker，以多 worker 和多 in-flight BIO 提升 SSD 并发度。

## 实现状态（2026-09-24）

- `rnvpc` 池（P0–P5）与 `ext4-rNVPC`（I1/I2）**已实现并提交**，全量 ktest 零回归。
- `ext4-rNVPCe`（I3）**已接入但实现偏离本设计**：它复用了 rNVPC 的 CoW + per-inode WAL，
  而 §5 要求 rNVPCe **不使用 WAL** ⇒ 重做任务（I5）见
  [../I5-extend-cow-implementation.md](achillesfs-extend-implementation.md)。
- 实现和实验不能把 DRAM 模拟结果表述为真实 PM 的崩溃一致性或硬件性能。
