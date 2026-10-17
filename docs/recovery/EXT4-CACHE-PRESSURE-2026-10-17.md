# 原生 EXT4：脏页背压、后台回写与清洁页回收

## 实现范围

本次从 `8ce7d0bfd30c200ad561f95503fc0e9aace61a37` 的 rebuild 基础补充原生 EXT4 基线能力。挂载时 `rnvpc=off,rnvpc_extend=off,rtio=off`；rNVPC/rNVPCe 不启用这套 admission/worker 策略。公共 PageCache 的失败 BIO 保留脏数据修复同时适用于使用该路径的后端。

| 参数 | 默认值 |
| --- | ---: |
| 文件系统缓存帧高水位 | 512 MiB |
| 清洁回收低水位 | 384 MiB |
| 脏页＋在途回写高水位 | 128 MiB |
| 后台回写触发水位 | 64 MiB |
| 单次 buffered I/O 分片 | 至多 256 KiB |
| 每轮回写 | 至多 1024 页 / 4 MiB |
| 每轮回收 | 至多 4096 页 / 16 MiB |
| 忙／空闲轮询间隔 | 10 ms / 1 s |

这些是初始工程默认值，尚未调优；不是 Linux 的全局 dirty_ratio 策略，也不随可用内存自动调整。

## 关键路径

- `kernel/src/fs/fs_impls/ext4/writeback.rs`：每个文件系统的 accounting、并发预留、等待队列和弱引用 inode 注册表。工作线程轮转 inode，分批回写并回收；等候者在 inode 锁和 journal handle 之外背压。长期无进展时返回 ENOMEM；后台 I/O 错误锁存为 EIO，避免永久重试。
- `kernel/src/fs/fs_impls/ext4/inode/mod.rs`：原生 buffered 读写分片并预留最坏页数，包含非对齐尾页；读预取也计入预留。短读／短写推进真实用户缓冲区游标。目录元数据继续由既有 journal 路径管理。
- `kernel/src/vm/page_cache/accounting.rs`、`cache_page.rs`：按帧生命周期统计 resident/dirty/writeback。RCU 延迟释放和外部持有帧仍计入 resident；回写期间重新弄脏的帧同时计入 dirty 和 writeback，保守限制压力。
- `kernel/src/vm/page_cache/vmo/mod.rs`：有后端的缓存可回收 initialized、clean、无在途 I/O、仅由数组持有的页。非阻塞页锁和 commit 锁排除与 lock-free cursor 克隆的竞态；PTE、BIO、读写者等引用阻止回收。匿名 VMO 不参与。分批 flush 即使部分提交失败也等待已提交 BIO。
- `kernel/src/vm/page_cache/mod.rs`：单页和批量完成路径共用失败回调，失败后原子重新置脏，再清除 writeback；IRQ 回调不获取页锁，防止丢数据或死锁。
- `ostd/src/task/preempt/mod.rs`：空闲 CPU 在 halt 前报告 RCU 静止状态，避免 SMP 空闲核使缓存帧永久延迟释放。仅在退出 bootstrap 且重新检查非 atomic context 后报告。
- `ostd/src/sync/wait.rs`：广播先取走当前等待队列快照，再在锁外逐个唤醒。新入队者留给下一轮，避免并发写者反复入队使 `wake_all` 永不结束。保留先发布 condition、再唤醒的既有协议。

- `osdk/deps/frame-allocator/src/pools/balancing.rs`：空闲池平衡包含 order=0 的离散 4 KiB 页；源池耗尽时立即终止递归，避免回收碎片页触发大规模空搜索并长期占用全局分配锁。

## 验证和反例

测试产物目录：`/home/zn/asterinas-build-env/ext4-writeback-20261017/`。

1. PageCache 测试覆盖生命周期计数、重复置脏、在途重新置脏、失败完成、单页 fallback 失败、外部引用阻止回收、匿名页不可回收。
2. EXT4 测试覆盖小阈值下两个文件持续写入、8 个并发写者各自完整回读、后台错误、短 EOF 读取，以及只读挂载的清洁页回收。EXT4/PageCache 回归合计 585 项通过；OSTD 等待队列 7 项、页分配器 14 项通过。单元环境使用 SMP=1，因为 OSDK 单测不启动 AP；真实设备测试使用 SMP=16。
3. 第一次真实设备顺序写在恰好 512 MiB 返回 ENOMEM，记录于 `wb-seq128k-j1-16g.log`。原因是旧 RCU 只在任务切换报告静止状态，空闲 AP 阻止退休帧释放。补充 idle 报告后 `wb-seq128k-j1-16g-r2.log` 完整写入 16 GiB，并完成 sync/unmount。
4. 真实设备 fio 结果和最终一致性检查见下方结果表。只使用已授权清空的 `/dev/nvme1n1p1`，不操作整盘或其他分区。
5. `final-seq128k-j16-16g.log` 的并发测试停止推进；不能算成功压测。调试复现的 early/late backtrace 和寄存器保存在 `debug-seq16-*.log`：写者在重新 enqueue，广播者的单次 `wake_all` 已累计唤醒 27,849,470 次，设备无 I/O。有限快照修复针对这一活锁，而非仅调整限时或水位。
6. 等待队列新增确定性单测：在真实唤醒后消费通知并重新入队，最多重入 8 次，模拟另一 CPU 的交错。旧算法得到 `left: 9, right: 1`（`wait-broadcast-red-ostd.log`）；快照算法只通知本轮 1 次，下一次 `wake_one` 仍能通知新入队者。`wait-broadcast-green-ostd.log` 为修复后的 7 项测试结果。

7. `snapshot-rand4k-j16-16g.log` 随机写进一步暴露分配器慢路径；`snapshot-rand16-backtrace.log` 定位到释放页时的 `balance_to`。原逻辑最低只尝试 8 KiB，无法转移离散 4 KiB 帧，回收越多反复搜索越严重。修复后的两个测试检查转移数量、帧集合守恒、无重复帧和耗尽早退；旧逻辑失败（`allocator-balance-red-cleanup.log`，0 通过 / 1 失败），新逻辑整个分配器包 11 项通过（`allocator-balance-green.log`）。中断的随机写只作为诊断，不算完成验证。

8. 随机写进一步触发假性 ENOMEM：`complete-rand4k-j16-16g.log` 退出 65，诊断复现 `diagnostic-rand4k-j16-16g.log` 在错误前仍有 `MemFree: 7619336 kB`。`frame-allocator/src/cache.rs` 原先把单页申请升级为批量连续 8 页填充，填充失败便直接返回 None。现在填充失败后按原始请求大小重试；真实 BuddySet 测试覆盖 1/2/3/4 页，旧代码明确返回 None（`allocator-refill-red.log`），新代码整个包 13 项通过（`allocator-refill-green.log`）。空池返回失败另用回调模拟。此前 `complete-*` 结果存档于 `before-refill-fallback/`，最终设备结果以新镜像重跑为准。

9. 单页 fallback 后随机写仍因堆的 32 KiB 申请失败，不能把 fallback 当成整轮问题的充分修复。`pooldiagnostic-rand4k-j16-16g.log` 显示失败时全局空闲池有 4,216,102,912 字节，却只有 order 0/1/2（`global_orders=0x7`），当前 CPU 池同样如此：空闲伙伴页分散在 CPU 池中而无法合并。现用 IRQ SpinLock 保护各 CPU buddy 池，常规申请失败后按单个 local→global 的锁顺序逐池归并，再有限重试；同时重试 balance 刚合并出的块。诊断打印已移除。三池碎片测试归并前失败、归并后可完整分配，`allocator-consolidate-red.log` / `allocator-consolidate-green.log` 保留反例和 14 项全包通过证据。

## 最终真实设备结果

最终镜像为 `ext4-cache-pressure-consolidated.iso`，SHA256 `08d2368c522ee702564c8577e728ffef6615d543dac773a19a332990ef8c0cd9`；`build-release-consolidated.log` 构建退出 0。最终 Rust 文件哈希及快照见产物目录的 `source-manifest.json`、`tracked-source.diff` 和 `source-snapshot/`。

环境：QEMU 10.2.1/KVM，SMP=16，客体内存 8 GiB；fio 3.39，psync、iodepth=1、direct=0、fsync=0，原生 EXT4。每组重新格式化已授权测试分区 `/dev/nvme1n1p1`。随机组先顺序填充全部文件并 sync，再测随机覆盖写；预填充不计入下表。

| 工作负载 | jobs | 实写 GiB | fio MiB/s | IOPS | 含 sync/卸载 MiB/s | 宿主分区写 MiB/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 128 KiB 顺序写 | 16 | 16.00 | 284.17 | 2273 | 280.69 | 284.84 |
| 4 KiB 随机覆盖写 | 16 | 16.00 | 156.67 | 40107 | 155.31 | 157.26 |
| 128 KiB 顺序写 | 1 | 16.00 | 285.33 | 2283 | 281.61 | 290.83 |
| 4 KiB 随机覆盖写 | 1 | 16.00 | 239.54 | 61322 | 237.04 | 238.24 |

四组均完整写入 16 GiB，超过 8 GiB 客体内存及 512 MiB 缓存高水位；每组 fio error=0、完整退出标记、sync/卸载成功、QEMU exit=33、只读 `e2fsck -fn` exit=0。`complete-suite.log` 的套件进程退出 0。e2fsck 检查文件系统结构；本轮 fio 未启用数据校验，数据回读由单元回归覆盖。

“含 sync/卸载”带宽包含尾部排空开销；宿主带宽取计时阶段最初 5 秒之后的分区写扇区增量，包含数据、journal 和元数据，不能单独当作 worker 数据带宽。单线程随机覆盖比 16 线程更快，表明并发路径仍有性能瓶颈；本轮只有单次测量，未归因其具体占比。

最终验证：EXT4/PageCache **585 通过、0 失败**，页分配器 **14 通过、0 失败**，OSTD 等待队列 **7 通过、0 失败**。`rustfmt --check`、`git diff --check` 通过。源码哈希与最终构建快照一致，临时分配器诊断打印已移除。

完整记录与复现脚本：[外部测试报告](/home/zn/asterinas-build-env/ext4-writeback-20261017/REPORT.md)、[原始结果 CSV](/home/zn/asterinas-build-env/ext4-writeback-20261017/results.csv)。`run-complete-suite.py` 会重新格式化该专用分区，测试参数、启动器和 fio 脚本均保留在产物目录。

## 已知边界

- 这是每个文件系统的 buffered regular-file 内存预算，不是全局 VM reclaim。mmap 页被保留引用时不会驱逐；mmap fault 本身尚未接入该 admission。大量 pinned 页可能使 buffered I/O 返回 ENOMEM。
- 仍扫描 resident XArray 寻找脏页；尚无 dirty-only 索引。XArray 空内部节点暂不回收，页框有界不等于全部索引元数据恒定。
- 分配器归并是失败后的同步慢路径，会带来额外延迟；它保持 buddy 页池和已有固定大小的小段缓存，不提供物理页迁移／压缩。
- 不包含 delalloc、后台线程优先级／带宽自适应、全局内存协调，以及完整的 mmap 脏页和 writeback errseq 语义。
- 页缓存回写完成与应用持久化保证不同；应用仍需 fsync/sync。压测同时报告 fio 带宽与包含末尾 sync 的带宽，并在卸载后运行只读 e2fsck。没有进行断电故障注入。
- 实际设备经过 QEMU virtio-blk（单队列，`cache=none,aio=threads`），不能把结果当作主机 Linux 的原生 NVMe 极限带宽。单轮 fio 不代表有统计置信度的性能结论。
