> **Archived:** original date: not recorded. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# rTIO

rTIO 是 rNVPC/rNVPC-extend 之上的分层 I/O 与 writeback 调度层。它不负责持久对象布局，也不
维护全局物理索引。

权威设计见 [design.md](rnvpc-original-design.md)。相关缓存与持久数据面设计见
[../rNVPC/design.md](rnvpc-original-design.md)，设计变化见
[../rNVPC/design-evolution.md](rnvpc-design-evolution.md)。
