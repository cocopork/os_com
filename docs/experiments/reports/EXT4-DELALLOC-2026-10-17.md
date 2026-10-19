# EXT4 journal / nojournal delalloc（2026-10-17）

## 实现范围

基于当前 rebuild 工作树补充原生 EXT4 regular-file buffered write 的延迟分配。默认开启，可用 `nodelalloc` 保留立即分配作为对照，`delalloc` 再次开启，最后一个选项生效。rNVPC / rNVPCe 继续使用各自路径。

前台 `write()` 维护容量预留、ES delayed 状态、PageCache 数据、内存 i_size/时间戳；正常路径不打开 journal handle，不分配物理块或 extent 树节点，不更新 bitmap/GDT/inode-table。仍持有短时容量记账锁、inode 锁和页锁，仍需分配缓存内存。接近满盘且 ES 未命中时，允许只读遍历已有映射以识别不需分配的覆盖写；journal pinned-free ENOSPC 的既有提交后重试机制保留。

## 状态与落盘路径

- `inode/delalloc.rs`：前台写入、短写记账、分批 materialization、可捕获 size。
- `inode/extent_manager/es.rs`：delayed 区间是既有 ES 的不可淘汰状态层；mapping facts 的清空/容量淘汰不丢失延迟所有权。重复覆盖合并去重；维护块数，热路径不扫描全部前序区间。
- `allocator/mod.rs`：每 inode 持有预留账户；reserved → inflight 原子转换，普通/元数据分配不能挪用，失败 claim 返还账户，完成/取消释放剩余额度；与 rNVPCe 共享容量权威。
- writeback/fsync/fdatasync/sync：在 inode 串行化下，每次最多处理64个逻辑页，按需分配 unwritten extent，再转换实际待写范围；后台轮次继续遵守原有页数限制。
- journal：每个 handle/restart 都先登记 ordered data，再改映射并捕获一致的 inode。事务剩余额度不足且操作可在新事务完成时 restart；只有超出整个事务容量才返回 EFBIG。提交线程遇未解决的 hole/unwritten 页保留其 dirty 状态，不在无 handle 时分配。
- nojournal：一次 materialization chunk 内的外部 extent-node after-images 合并写回，失败项保留用于重试，读取优先看待写 after-image；bitmap/GDT/SB 使用既有 dirty metadata funnel。fsync 成功前完成数据、必要元数据和设备屏障。

buffered i_size 与可捕获 i_size 分离。chmod 等属性操作不得提交未分配后缀的大小；即使最后一页已经 written，在该页内追加的新长度也须保留 pending coverage，直到 ordered writeback 接管。已知 written 页的这种覆盖不额外预留物理空间。

mmap 可以绕过 write()：writeback 先快照实际脏页索引，只处理需要映射的脏页，不因宽范围 ordered 登记而分配干净空洞。

## 生命周期与锁序

普通路径：cache pressure admission → inode write lock → journal handle（仅 writeback）→ extent tree → group allocation gate → 短时容量/位图锁。容量锁不覆盖组搜索和 I/O；重启事务时已释放 extent/page/capacity 锁；journal commit 通过 PageCache 刷数据，不取得 inode 锁。

非零 truncate、punch、zero、fallocate、direct I/O、collapse/insert 先 drain delayed 数据再执行已有映射算法。truncate-to-zero、删除回收取消延迟所有权并释放预留。这样保留原有 orphan/ordered 算法；代价是部分异常规操作可能先写出随后丢弃的数据。

## 验证

Docker `os-com-rnvpc-rebuild`，OSDK/QEMU/KVM，SMP=1、4GiB。完整 kernel ktest：**299 passed，0 failed，0 filtered out**，退出码0。构成为118 EXT4 + 23 PageCache +158其他内核测试。

保留原570个 EXT4 场景函数体，审计确认全部恰好执行通过一次；新增2个命名状态/容量场景和10个 delalloc 功能入口。旧精确立即分配场景的 fixture 显式使用 nodelalloc。

新增覆盖包括：两模式前台不分配、缓存读回、重复/部分覆盖、稀疏与预分配、单页分批 writeback、fsync/remount、属性捕获大小、mmap 脏洞、碎片化外部 extent、容量竞争与 ENOSPC、不足容量下冷映射覆盖、只读/abort 拒写、事务 restart、数据及节点/描述符 I/O 失败重试、各类映射变更及删除取消。

独立只读审查发现的 mapped EOF ordered 缺口、restart 误判，以及元数据重试遗漏均已修复并验证 RED→GREEN。原生写入口只读/abort 检查同样有 RED→GREEN 证据。

补充聚焦测试：`ext4_delalloc_bounded_writeback_and_remount` 扩展为在 committed-but-uncheckpointed 状态模拟 NoLogFlush 崩溃并重新挂载，验证 journal replay 只恢复已分配首个页、不发布仍待分配的后缀；**1 passed，0 failed，298 filtered out**。此后仅扩展测试，生产代码与299项完整回归版本一致。

`cargo fmt --all --check`、`git diff --check` 通过。证据目录：`/home/zn/asterinas-build-env/ext4-delalloc-20261017/`，包括任务前源码快照、RED/GREEN 日志、完整日志、审查台账和 `audit.py`。未对真实 NVMe 运行 fio，未格式化设备，也未提交或推送代码。

## 已知边界

1. 首版容量预留保守：未知/可能需映射的每页暂预留27块（1数据＋26 extent 插入/转换增长余量）；完成后释放未用额度。它可能提前 ENOSPC，后续可缩小元数据预算；不能据此声称容量利用率与 Linux 一致。
2. nojournal 不提供掉电原子性；本次保证成功的同步边界和错误传播。真实掉电、完整故障注入矩阵、SMP 并发与用户态 fio 尚未验证。
3. 既有后台 controller 在 I/O 错误后进入 failed 状态；本次验证 writeback/fsync 显式重试，不声称后台线程自动恢复。
4. 仍使用既有 inode 写锁及同步 BIO 等待；未实现 submit/reap 异步流水线、dirty-only 页索引或 Linux PA。本次不声称吞吐提升。
