> **Archived:** original date: 2026-09-23. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# rTIO 设计

rTIO 管理 DRAM/NV page 切换，并通过 inode 粒度的多 worker writeback 提高块设备并发度。其
核心目标是产生足够多的 in-flight BIO，而不是从全局物理视图寻找连续页。

## 1. 边界

| rTIO 负责 | rTIO 不负责 |
|---|---|
| DRAM/NV residency 决策 | NVPage/NVBH 的持久布局 |
| inode 候选选择与 worker ownership | rNVPC per-inode WAL 格式 |
| 页收集、BIO 构造和异步提交 | 全局 SSD/NV 统一编址 |
| completion 状态推进与重试 | 跨 inode 崩溃事务 |
| 并发度、水线和公平性 | 跨 inode 物理连续页 packing |

rTIO 通过 runtime descriptor 获取每页的 SSD backing address。地址属于 page，而不是某个全局
physical-to-NV index。

## 2. 前台读写路径的主动分层

### 2.1 读路径

```text
B-tree lookup
  -> Dram: 直接读取
  -> Nv:   从 NVPage 读取
  -> Miss: 从 SSD 读取，分配并登记 DramClean page
```

读路径可以根据命中和状态主动选择 DRAM 或 NV，不需要等待后台线程搬迁页面。

### 2.2 写路径

```text
DramClean write
  -> 预留容量
  -> 分配 NVPage/NVBH
  -> CoW 写入 NVPage
  -> 按 soft update 顺序发布 metadata
  -> descriptor 切换为 NvDirty

NvDirty write
  -> 分配新版本
  -> CoW update
  -> 发布新版本
```

前台路径完成 NV 层发布后即可按定义返回。后台 SSD writeback 不应长期持有应用 write 所需的
inode 写锁、page 写锁或全局文件系统锁。

## 3. Inode 粒度 writeback

每个 worker 一次 claim 一个 inode：

```text
dirty inode queue
  -> claim inode lease
  -> collect dirty pages of this inode
  -> map each page to its SSD backing blocks
  -> build and submit multiple BIOs asynchronously
  -> process completions
  -> release/requeue inode lease
```

约束：

1. 同一 inode 同时最多有一个 writeback lease；
2. 不同 inode 可以由不同 worker 并行处理；
3. worker 不等待每个 BIO 完成后才提交下一个 BIO；
4. worker 达到自己的 in-flight budget 后再等待 completion；
5. inode 内可以按逻辑页或 extent 顺序收集，并合并自然连续的 SSD blocks；
6. 不跨 inode 为了物理连续性重新排序 dirty pages。

这种所有权使单 inode 的 soft-update 顺序保持简单，同时让多 inode workload 能把设备队列填满。

## 4. 调度和并发控制

### 4.1 候选队列

inode 在首次出现 dirty NVPage、WAL 达到高水线或显式 sync 时进入候选队列。重复入队需要合并，
避免一个 inode 占据大量队列节点。

选择优先级可使用简单、可解释的组合：

1. WAL 已满或容量处于临界水线的 inode；
2. 显式 sync/fsync 等待的 inode；
3. 最早变脏或最久未服务的 inode；
4. 普通后台回收候选。

物理地址连续性不参与跨 inode 排序。

### 4.2 并发预算

系统至少区分：worker 数、每 worker 最大 in-flight BIO、单次 inode 收集页数和全局 in-flight
上限。默认值必须通过设备实验确定，而不是由 virtio queue size 直接推导。

队列深度的有效值是尚未完成的设备请求数。仅把 virtio descriptor ring 从 64 增到 1024，不会
自动提高并发；若 worker 在 submit 后同步等待，实际深度仍接近 1。

### 4.3 公平性

一个大 inode 达到页数、字节数或时间片预算后应释放 lease，并在仍有 dirty pages 时重新入队。
这防止大文件长期占用 worker，同时不要求多个 worker 同时处理一个 inode。

## 5. Completion 状态推进

BIO submit 与 completion 是两个边界。两条路线的 completion 职责**不同**，不得混用：

```text
submit success:
    NvDirty -> NvWriteback
    保留 NVPage/NVBH（rNVPC 额外保留 WAL record）

completion success:
    page -> NvReclaimable
    rNVPC :  退休覆盖该 page 的 WAL records
    rNVPCe:  减少在飞账本（in-flight NV 数据页数），无 WAL
    释放 NV 容量并唤醒等待者

completion failure:
    保留 NV 内容
    page -> NvDirty/retry
    WAL record 不退休（rNVPC）/ 在飞计数不减（rNVPCe）
```

| | rNVPC（`rnvpc=on`） | rNVPCe（`rnvpc_extend=on`） |
|---|---|---|
| 回刷记账 | per-inode WAL（1024 op-log） | **未落 SSD 的 NV 数据页数**（in-flight budget） |
| 回压触发 | WAL 满 | in-flight 超预算 |
| completion 副作用 | 退休 WAL records | 减 in-flight + 驱动旧页回收 |

成功完成的 clean NVPage 直接回收，不转换成 DRAM page。后续读 miss 从 SSD 新建 `DramClean`。

completion worker 只能短时获取状态锁；它不能与 submit worker 组成两个挂载级串行阶段，也不能
在等待 I/O 时持有 inode 写锁、page 写锁或全局文件系统锁。

## 6. Sync 与容量压力

- **后台水线触发**：选择普通候选，异步推进，不阻塞前台。
- **WAL-full**（仅 rNVPC）：提升对应 inode 优先级；只阻塞该 inode 的新 update，直到 completion
  释放记录。
- **in-flight 超预算**（仅 rNVPCe）：以未落 SSD 的 NV 数据页数为额度；同样只阻塞该 inode，
  等 completion 减计数。**回压等待用自轮询（`spin_loop` + `yield_now`），不用带超时的等待。**
- **NV 容量临界**：允许多个 worker 并行回收不同 inode。
- **fsync/sync**：等待目标范围的 completion；不要求全局按物理地址排序。

容量等待必须发生在取得下层 inode/page/NVBH 锁之前，避免前台持锁等待后台回收形成闭环。

## 7. 不变量

1. 同一 inode 同时至多一个 writeback worker owner。
2. 不同 inode 的 worker 可以并行提交 I/O。
3. BIO submit 后、completion 前，NV 数据保持有效（rNVPC 额外：对应 WAL 记录也不得退休）。
4. completion 成功是 NVPage 回收的必要条件；对 rNVPC 同时是 WAL 退休的必要条件。
5. 失败 completion 不丢失 dirty 状态，并可重新提交。
6. 后台 I/O 等待不持有前台 write 所需的粗粒度锁。
7. DRAM/NV 切换由 page descriptor 原子发布，读者不会看到无 backing 的状态。
8. 不建立跨 inode 的全局物理排序依赖。
9. **rNVPC 与 rNVPCe 不得共用回刷记账机制**：rNVPCe 路径上不得出现 per-inode WAL。

## 8. 最小实验与消融

| 实验 | 目的 |
|---|---|
| 1 worker、每次只允许 1 个 in-flight BIO | 串行基线 |
| 多 worker、每 inode 1 个 in-flight BIO | 分离 inode 并发收益 |
| 多 worker、每 worker 多 in-flight BIO | 验证设备队列利用率 |
| 禁用 inode 内 BIO merge | 区分并发收益与合并收益 |
| 禁用主动 DRAM/NV 切换 | 测量分层策略成本 |
| WAL 1024 与受控小容量 WAL（**仅 rNVPC**） | 测量 WAL-full 回压 |
| in-flight 预算扫描（**仅 rNVPCe**） | 测量在飞额度回压，对照 WAL-full |

至少观测吞吐、IOPS、P50/P99 write 延迟、回压等待（rNVPC = WAL 等待；rNVPCe = in-flight
等待）、dirty-page collect 时间、BIO 构造时间、submit-to-completion 延迟、平均/峰值
in-flight BIO、每 worker 利用率和每 inode 服务公平性。

## 9. 非目标

- 不追求跨 inode 的全局物理连续写回；
- 不让 queue size 配置替代真实异步并发；
- 不在 completion 后保留或转换 clean NVPage；
- 不通过持久化 worker/queue/lock 来恢复运行时控制面；
- 不以 DRAM 模拟结果声称真实 PM 或设备的硬件性能。
