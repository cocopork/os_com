> **Archived:** original date: 2026-09-24. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# 读写路径 / writeback 变化总结（fio 前确认件）

> ## ⚠️ 历史快照（勿当现状读）
>
> 本文件写于 **I1 之前**，用于“跑端到端 fio 前确认当前走哪条路”。
> 它描述的“rNVPC/rTIO 尚未接入、fio 只能测到原生 ext4”**已经全部作废**。
>
> **当前现状请看**：[README.md](../../rNVPC-rTIO/README.md)（状态总表）与
> [ext4-io-integration.md](achillesfs-ext4-io-integration.md)（接入规格）。
> 当前唯一待办是 **I5**（[I5-extend-cow-implementation.md](achillesfs-extend-implementation.md)）。
>
> 本文件的价值仅在于：记录“接入前”的零件清单与设计意图对照。

- 结论版本：T2.2b（身份迁移到 `index::PageKey`）之后、T2.3/P4/P5 接入之前
- 分支：`os_com@nvpool-port`，本文件对应 HEAD `e34997b9e` + 未提交的 T2.2b 工作区改动
- 目的：在跑端到端 fio 之前，把“当前读写路径到底走哪条路”讲清楚，等确认后再烧机

## 0.0 当时 vs 现在（对齐用）

| 项 | 本文件当时 | 现在（2026-09-24） |
|---|---|---|
| ext4 是否引用 rnvpc | 零引用，`#[expect(dead_code)]` | ✅ I1/I2 已接入 `extent_manager` 块 I/O 边界 |
| fio 能测到什么 | 只有原生 ext4 | ✅ 三个对象（native / `rnvpc` / `rnvpc_extend`）均可跑 |
| T2.2b 状态 | “待提交” | ✅ 已提交 |
| rTIO | 未接入 | ✅ v0 已落地（单线程回刷）；多 worker 待做 |
| 待确认项（§5） | 走法 A/B/C | ✅ 已选 C，并已实际跑出两代对比 |

> **阅读提示**：下面 §1–§3 里所有“**尚未接入**”的标注都指**当时**（I1 之前）。
> 现在的接入情况见 §0.0 表与 [ext4-io-integration.md](achillesfs-ext4-io-integration.md) §2。
> §1.2/§2.2/§2.3 的“设计意图”部分本身**仍然有效**（是 design.md 的转述）。

---

## 0. 最重要的一条事实（请先看这里）

> 本节描述的是**当时**的事实，已作废；保留原文以对照“接入前”的起点。

| 项 | 现状 | 证据 |
|---|---|---|
| rNVPC/rTIO 是否已接入 I/O 路径 | **没有**。三个开关只被解析和校验，`RnvpcModeConfig` 在 ext4 里是 `#[expect(dead_code)]` | `kernel/src/fs/fs_impls/ext4/fs.rs:645-646` |
| ext4 侧对 `rnvpc` 的引用 | 只有挂载选项解析 + 一处开关矩阵单测，**零数据路径引用** | `grep -rn rnvpc kernel/src/fs/fs_impls/` |
| rnvpc 模块是否被 ext4 调用 | 无。`kernel/src/fs/rnvpc/`（18 文件）当前只被自己的 ktest 驱动 | `kernel/src/fs/rnvpc/tests.rs` |
| 因此当时跑 fio 测到的是 | **原生 ext4 路径**（Batch A 优化版，含 B1 O_DIRECT 一致性修复） | 见 §2/§3 |

> 含义：**端到端 fio 现在不会测到 rNVPC / rTIO 的任何行为**。要测到，必须先做 T2.3（状态机接池生命周期）+ P4（soft update 发布顺序）+ P5（rTIO worker 池）并真正接入 ext4 的 PageCache 读写路径。这一点决定了 4. 里三种走法怎么选。

---

## 1. 读路径

### 1.1 现状（原生 ext4，fio 实际测到的）

```text
VFS read → PageCache 查找 → 命中：直接拷回用户缓冲
                          → 未命中：ext4 read → bio → virtio-blk → 设备
O_DIRECT read → 绕过 PageCache，但先做一致性处理（见下）
```

本分支上真正改变读行为的是 B1（`51e60ae20`）：

- O_DIRECT 读之前，把与该区间重叠的 **buffered 写区间物化并转 written**，避免"读到旧的磁盘内容"。
- 影响：`fio/dio_coherence`（`5861874fa` 新增）两种模式实测 PASS；这也是唯一一个已用 VM 真实设备验证过的读写路径改动。

### 1.2 rNVPC 目标读路径（设计意图，**尚未接入**）

```text
PageCache/上层读 → DRAM B-tree 查 PageKey(fs_id, inode, page_index)
  ├─ Residency::Dram → 读 DRAM page（DramClean，可随时丢弃）
  ├─ Residency::Nv   → NV 是当前数据源（NvDirty / NvWriteback），从 NV 读
  └─ miss            → 从 SSD 读入，分配新的 DramClean page
```

关键不变量（`design.md §5.2`）：**clean NVPage writeback 成功后只标 `NvReclaimable` 并直接回收，不复制/迁移成 DRAM page**。

已就位的零件：
- `index::PageKey` / `Residency{Dram,Nv}` / `RuntimeHandle{Dram(u64),Nv(NvPageId)}` / `PageEntry{residency,handle,ssd_backing_block,state}` + `rebuild_from_descriptors`（只重建 `Nv` 项）——`e34997b9e` + T2.2b
- `descriptor::{NvDescriptor, NvObjectKind{NvPage,Nvbh}, NvObjectState{DramClean,NvDirty,NvWriteback,NvReclaimable}}` + `can_transition_to`（拒绝 `NvReclaimable→DramClean`）——`41238a0f4`

---

## 2. 写路径

### 2.1 现状（原生 ext4，fio 实际测到的）

buffered write 链路（Batch A 之后）：

```text
VFS write → PageCache 标记 dirty
          → 脏水位节流（73d7b2d79）
          → writeback：per-inode delalloc 预分配（691f62b4c）+ 后台异步 submit-then-reap（4d22b1947）
          → nojournal 元数据异步落盘（9ba9deb6b / f304badd5 / bc0699641）
fsync → 裁剪设备往返（c59270bef）+ nojournal 分配窗口异步转交
O_DIRECT write → 直通设备（B1 只改读侧一致性）
```

其他相关：superblock 全局锁从块分配路径上剥离（`26ef8b66f`）、virtio-blk 队列深度 1024（`1fcb2d996`）、writeback 跳过未分配空洞而不是中止 journal（`90c609235`）。

### 2.2 rNVPC 目标写路径：update = WAL 额度 + CoW（设计意图，**尚未接入**）

`design.md §3.3` 的 6 步，落到实现上：

```text
1. 锁目标 inode 的 update 上下文
2. 从 per-inode WAL（WAL_CAPACITY_PER_INODE=1024 op-log）预留一条
3. append WAL record 并计入 PM 容量
4. 分配新 page，复制旧内容 + 应用本次 update
5. PageCache 索引切到新 page
6. 旧 page 与 op-log 等 writeback 完成后回收
```

硬约束：
- **没有 WAL 额度就不许改 PageCache**；1024 条未退休 → 触发该 inode writeback 并等至少一条可回收；一个 inode 的 WAL 耗尽只阻塞自己（不借不还）。
- **禁止原地修改**（CoW 语义）；不模拟 group commit / 日志压缩 / 跨 inode 事务。

已就位的零件：`wal::InodeWalRuntime` / `InodeWalTable`（`WAL_RECORD_BYTES=32`，completion 才退休）、`cow::InodeCowContext` + `CowCounters`、`capacity::PoolClass::Wal` 与 `RnvpcStats::wal_pages`、`frame::{charge_wal_pages, release_wal_pages}`——`502791279`。

### 2.3 rNVPC-extend 目标写路径：soft update 发布顺序（设计意图，**尚未接入**）

`design.md §5.1` 的 7 步顺序 + 5 条不变量（NVBH 不得先引用未 ready 的 NVPage；旧 NVPage 不得在引用撤销前回收；已发布对象只能 CoW；同 inode publish 串行 / 不同 inode 并行；容量等待发生在取低层锁**之前**）。这是 P4 的范围，目前只有零件（描述符 + 状态机），没有协议实现。

---

## 3. writeback 变化

### 3.1 现状（原生 ext4）

- 单线程后台 writeback daemon（`8231030af`），`wb_*` 计数器可观测。
- 前台写受脏水位约束（`73d7b2d79`），后台提交后不等待、延后 reap（`4d22b1947`）。
- nojournal 元数据：批量提交 + 可配 in-flight 深度（`NOJOURNAL_META_IO_DEPTH`），011 系列计数器（`69d500047` / `bc0699641` / `f304badd5` / `7cd5b279d`）。
- 卸载：排空脏 inode，不再 Weak-upgrade（`3717d22d7`）；排空遇到"不可刷回 inode"报错一次并放弃，不再无限重试（`cf9ce1ae9`）；ktest fixture 卸载前先 `sync_all` 对齐真实卸载顺序（`2d32451a4`）。

### 3.2 rNVPC 的 writeback（设计意图，**尚未接入**）

- WAL 退休**只依据 completion 边界**推进——BIO 已提交不算数（`design.md §3.4`）。
- rNVPC 的 writeback 只负责"WAL 空间回收 + 数据落盘"；**不使用全局物理索引**，不跨 inode 找连续 SSD 地址。

### 3.3 rTIO 的 writeback（设计意图，**尚未接入**）

- 对外只暴露：inode 级候选、每页 SSD target、page 状态、容量水线（`design.md §6`）。
- worker 以 inode 为**所有权粒度**并发：lease 单 owner，提交后不等待，达到 in-flight budget 才等 completion，completion 只短持状态锁（P5 / `rTIO/design.md`）。
- completion 三条硬规则：SSD 写成功前不得进 reclaimable；失败保留 NV 数据与原状态可重试；成功的数据页**直接回收、不迁移为 DRAM page**。
- 升级路径：现有单线程 daemon → 多 worker 池；`NOJOURNAL_META_IO_DEPTH` 那套"批量提交 + 延后 reap"是提交侧的现成参考。

---

## 4. 端到端 fio 的三种走法与建议

| 走法 | 测到什么 | 代价 | 适用 |
|---|---|---|---|
| **A. 现在就跑** | 原生 ext4（Batch A + B1），用于**锁定回归基线** | 最低（~10 min/轮） | 想先确认 B1/T0/T2 这批提交没扰动原生性能 |
| **B. 先接 T2.3+P4+P5 再跑** | rNVPC / rTIO 的真实效果 | 高（三阶段实现 + 接入 ext4 读写路径） | 目标就是"组件跑起来看效果" |
| **C. A 先跑一轮锁定基线 → 再 B** | 两代对比有共同基底 | 中 | **推荐**：fio 只需 ~10 min，却能让后面 rNVPC 的效果有可比基线 |

已知不能作为结论的使用方式：
- 用 fio 去验证"rNVPC 提升了并发"——现在 rNVPC 不在路径上，`in-flight` 深度必须是 rTIO 接入后**实测**的值（任务书 §T5.x 明确要求），不能靠主张。
- 把 fio 结果与 011 基线做严格等式比较——nj16 orderless 存在 3–10× 双稳，≥3 次重复取中位才可比。

---

## 5. 待确认项（请逐条给结论）

1. 采用 A / B / C 哪种走法？（建议 C）
2. 若选 C：端到端 fio 的 job 与参数用哪一套？
   - `fio/dio_coherence`（已验证 PASS，最快，1 轮即可）
   - `fio/aster_ext4_baseline`（回归对比，建议 ≥3 repeats）
   - 011 的随机写矩阵（成本最高）
3. rNVPC 效果的目标场景是否确认按 `design.md` 的"inode 内合并大 I/O + 多 worker"（即 P5 的兜底可选项）？

---

## 6. 本文件对应的代码事实清单

| 组件 | 位置 | commit |
|---|---|---|
| P0 改名 | `rnvpc/`（原 `nv_pool/`） | `503a256b6` |
| P0 cmdline 前缀 + 兼容 | `rnvpc/config.rs` | `4742032a2` |
| P0 三互斥开关 | `rnvpc/mode.rs`、`ext4/fs.rs` | `3a3b0e581` |
| P1 WAL/CoW/容量 | `rnvpc/{wal,cow,capacity,frame}.rs` | `502791279` |
| P2 描述符与状态机 | `rnvpc/descriptor.rs` | `41238a0f4` |
| T2.2a 持久格式自描述 | `rnvpc/{layout,persist,frame,recover}.rs` | `65e541838` |
| T2.2b 身份 → `PageKey` | `rnvpc/{frame,recover,tests}.rs` | 本工作区（待提交） |
| P3 DRAM B-tree | `rnvpc/index.rs` | `e34997b9e` |
| B1 O_DIRECT 一致性 | `ext4/` + benchmark job | `51e60ae20` / `5861874fa` |
