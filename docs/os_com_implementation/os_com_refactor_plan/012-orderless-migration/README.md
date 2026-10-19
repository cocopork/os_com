# 012：Orderless SSD 写回与批量迁移回收

日期：2026-10-19。状态：核心 migration batch 已实现并完成真实 NVMe A/B；完整 metadata 快照与恢复设计仍未完成。

## 实施状态（2026-10-19）

当前代码已经实现以下第一阶段：rNVPCe 的 rTIO 默认采用 orderless migration。nojournal 数据 BIO 先提交；只写本批次触及的分配位图、组描述符和 superblock；data 与这些 metadata 全部完成后，每个成功批次只发一次 SSD flush；flush 成功后才发布 NVM SSD extent、归还额度并回收 NVPage。data 或 metadata 失败、flush 失败均保留 NVM 权威引用和持久 target。挂载时会扫描 pending target，拒绝指向 EXT4 系统元数据区的目标，并从修复后的 bitmap 重算组描述符和 superblock 空闲计数后持久化。旧的两屏障 ordered migration 路径已删除，不再提供运行时切换。

尚未实现本设计第5、6节的完整版本：native extent/inode 不在每个 migration batch 中写出，而在显式 sync/unmount 时从 NVM 最终状态合并导出；尚无通用不可变 metadata block 快照、跨批次同块版本依赖、完整注册 inode 恢复基线和后台 SSD 孤儿回收。因此当前结果是核心 orderless 数据/分配元数据协议，不代表整份012设计或完整 AchillesFS 命名空间掉电恢复已经完成。

曾验证一个“每批次同步批量安装 native extent”的候选。它保持 NVM 权威和单 flush，但会反复写中间映射：首轮 4KiB 随机写含 drain 从 129.29/113.46 MiB/s 降至 87.45/39.48 MiB/s（1/16线程），故未保留。历史实验镜像曾用临时挂载开关完成24点A/B；该开关和旧 ordered 实现现已删除，测得的 orderless 热路径与当前默认路径相同：128KiB顺序写16线程含drain提升12.1%，4KiB随机写1线程提升2.7%、16线程回退11.2%。正确的后续实现需要第5节的不可变块快照与版本合并，不能在热路径重复调用现有 extent 更新器。

实现与性能证据见 [Orderless Migration 报告](../../../experiments/reports/ORDERLESS-MIGRATION-2026-10-19.md)。

## 1. 目标与决策

由 NVM 保持权威数据和映射；将一个有界批次的 SSD data 与 EXT4 metadata 异步提交，不要求 data 完成后才提交 metadata。全部请求完成后发出一次设备 flush，flush 成功后才持久发布 NVM 的 SSD 映射、释放额度并回收 NVPage。

用户确定：migration 请求进度保存在 DRAM，掉电后不恢复请求执行现场，而从 NVM 权威状态重新构建迁移。SSD 提前落盘的 extent 不能成为应用的权威引用；未完成迁移留下的资源可在核对后回收。

设计选择：首版实现 rNVPCe，保持 nojournal、128 页全局在途预算和现有 010 双副本描述符。rNVPC WAL 版采用相同异步 I/O 协调思路，但 WAL 退休和恢复需单独适配与证明，首版不切换其路径。该范围是本设计的工程选择，不是声称两个恢复协议相同。

首版工作负载限定为预先存在、完成注册的普通文件；允许覆盖写、追加和稀疏区间写入。运行期间不允许创建、删除、重命名、链接、truncate、打洞或未受协议管理的 native 写入。注册必须保存文件完整权威映射、大小及必要 inode 属性，不能只有脏页子集。命名空间修改的完整恢复另行实现；这些前置条件不满足时拒绝启用新模式，不能静默使用不完整恢复。

本次实施未自动提交或推送；工作树仍包含此前009/010及文档迁移改动。

## 2. 当前代码差距

| 当前代码 | 当前行为 | 本设计修改 |
| --- | --- | --- |
| `ext4/migration.rs::prepare_nv_targets` | 分配后调用 `persist_nv_allocations()`，同步 metadata 并 flush SSD | 保留 NVM 目标身份发布，收集本批次 metadata 快照并异步提交；取消该前置 SSD 屏障 |
| `finish_nv_completions` | data completion 后再次执行同一全局同步函数 | 等待本批次 data **及 metadata** completion，再只发一次设备 flush |
| `export_nv_for_sync` / `inode::export_rnvpce_extents` | native extent 主要在最终同步导出 | 将相关 native extent/inode 更新纳入迁移批次；最终导出只处理剩余检查点工作 |
| `fs.rs::sync_metadata` | 遍历块组，可能持超级块写锁进行 I/O | 热路径仅收集被本批次影响的块，锁外异步写出 |
| `rnvpc/frame/migration.rs` | 已有批量目标、额度与回收 | 保留，输入必须经过批次 flush 成功门控 |
| 恢复 | 009 尚未证明 SSD 孤儿、位图及命名空间恢复 | 新模式启用前落实本设计限定范围内的恢复和分配门控 |

010 性能报告中的 allocation/data barrier 都包含 SSD 元数据同步与设备 flush，并非 NVM fence 单次耗时。当前完成阶段已有合批；本设计收益不能重复归因于“首次实现批量 flush”，应测量取消前置屏障、局部 metadata 写出及流水线的净收益。

代码相对仓库根：`kernel/src/fs/fs_impls/ext4/`、`kernel/src/fs/rnvpc/`。历史依据：[rTIO](../../archive/design/rtio-original-design.md)、[009](../009-rnvpce-nojournal/README.md)、[010](../010-nvpage-batch-transactions/README.md)。

## 3. 权威状态与持久化依赖

NVM extent 是注册文件的唯一权威映射：`NV(slot, allocation_seq)`、`SSD(block)` 或 hole。文件大小和必要 inode 属性也必须有可恢复版本。NV 映射未发布为 SSD 前，读缺页也不能回退到 SSD 上提前写出的 extent。NVM 已发布 SSD 映射时，从其指向的数据块读取，不依赖可能尚未重建的 native extent 树。

```text
固定 NVPage 版本及目标 / NVM 恢复信息持久可用
  → 收集并封口批次，异步提交 data + metadata（两者间无先后依赖）
  → 所有登记请求成功完成
  → 一次 SSD flush 提交并成功完成
  → NVM extent 按 soft update 发布 SSD 引用
  → 额度提交 / NV 描述符失效持久化
  → NVPage 可复用
```

“orderless”只取消 data 与其 metadata 的提交先后要求，不取消版本冲突约束、成功完成门控或 NVM 持久化顺序。无须假设设备 completion 即掉电持久；批次 flush 负责建立 SSD 持久边界。设备必须正确实现 flush，QEMU 配置不得丢弃它。

批次不保证全有或全无。SSD flush 成功后，NVM 发布可按叶部分成功；仅发布成功且完成失效持久化的页允许回收。失败页保留 NVM 权威版本。已有 010 的双副本选择字和失败后禁止继续修改的规则保持。

### 目标身份与覆盖写

每项保存 `(fs_id, inode, logical_block, slot, allocation_seq, target_block)`，以 lease 固定数据版本。完整 NVM 图及 pending target 描述符负责恢复所有权；不新增逐 BIO 持久化状态，也不将此称为 WAL。

一个逻辑页/SSD 目标同时最多一个迁移版本在途。新前台版本按既有 CoW 协议发布，旧 completion 必须重新校验身份，不能撤销新引用。旧批次最终完成/失败排空之前，不向同一 SSD 目标发出下一版本写入。flush 不能修复“旧请求晚完成覆盖新数据”。

原地覆盖 SSD 数据期间，NVM 必须已经持久保存完整有效页并作为该页权威来源。禁止覆盖仍被其他权威映射共享的数据块；共享目标需先 CoW。

## 4. DRAM migration 批次

建议结构分开保存状态和 I/O 缓冲，避免在回调中分配内存：

- `MigrationBatchId`：启动代际 + 单调批次序号；不持久化，不用于决定恢复归属。
- `pages`：固定版本的页 lease、持久引用、目标和后续逐页发布结果。
- `metadata`：去重后的块级不可变 after-image、块版本及依赖凭据。
- `pending_requests`、`sealed`、`first_error`、`flush_state`、阶段状态。
- 所有页和 metadata 缓冲保持到 DMA 完成；失败也不能提前释放。

状态：`Preparing → Submitted/Sealed → IoComplete → FlushInFlight → Durable → Publishing → Done`。错误进入 `Failed`，排空已提交 I/O 后保留 NVM 引用和受保护目标，唤醒等待者并返回错误，禁止回收。

先登记请求及缓冲所有权，再调用提交接口，防止立即完成回调使计数穿零。提交失败必须以明确的接口所有权契约撤销登记，不能与回调双重完成。`sealed && pending_requests == 0 && first_error == None` 才能入 flush 队列。空批次不发 flush。回调只更新计数、状态和入队/唤醒，不能持 IRQ 自旋锁做持久化或文件系统工作。

首版每个非空成功批次恰好一个 flush；只用一个设备 flush 协调者防止重复提交，数据准备和提交可与前一批次 flush 重叠。完成工作者不持有前台 inode/page/分配器大锁等待设备。保留全局 128 页预算，可按较小批次拆分流水推进；不在本项顺带提升窗口。批次页数和 metadata 数量有界，首版页数上限128、唯一 metadata 块上限256，超限在提交前拆批；拆分不能丢失依赖。

设备 flush 可能顺带持久化其他批次的写入，但仅凭本次 flush 不能推进尚未满足 completion 条件的其他批次。跨批次合并 flush 留作后续优化。

## 5. 异步 metadata 提交与共享块

metadata 集合必须覆盖本批次 native 导出所需的 extent 节点、inode 表块、分配位图、组描述符及相关计数；不能仅统计 data BIO，也不能把 `sync_metadata()` 包装成一个异步工作任务后宣称完成。

快照在短时元数据锁下生成，DMA 使用不可变副本；提交和等待均在锁外。快照使用块级版本，完成后只清除匹配版本的 dirty 状态，期间的新修改保持 dirty。

对于同一个物理 metadata 块，首版采用串行写出版本：V+1 可在 DRAM 合并，但不能在 V 完成前向设备提交。V+1 必须基于最新完整内存状态生成，包含所有仍有效的修改；禁止旧快照晚写。一个快照可由多个批次引用，只有明确覆盖各批次修改的版本完成后，才逐一满足其依赖。队列无全设备物理排序，只有同块冲突排序；依赖图必须无环。

extent 导出首版使用新节点构建或已验证的影子节点策略，不得原地破坏未注册文件依赖的树节点。固定位置的 inode 表块可能共享，注册阶段须在 NVM 保存完整受影响块的恢复基线及受管理 inode 的权威字段；写出时按当前权威状态合成。未注册邻居的字段不可丢失。并发 native namespace 修改在首版禁止。

分配计数和 bitmap 不能从掉电后的部分 SSD 写入直接作为真值；恢复基于完整所有权集合重建。对 torn metadata（包括 inode 表、位图/GDT）必须有重建来源或拒绝挂载，不能仅依赖 checksum 检测后继续运行。

## 6. DRAM 丢失后的恢复

不持久化 batch ID、请求计数、completion 位图、flush 进度。继续持久保存文件权威根、NV 数据版本/目标、注册恢复基线及必要 inode 状态。这些是文件系统状态，不是迁移执行进度。若现有 NVM 布局不能容纳注册基线，扩展布局并升级格式，旧池显式拒绝；不能默认010格式已满足全部信息。

### 挂载前核对（必须完成）

1. 验证 NVM 池与 SSD 文件系统绑定及启动状态，停止前台分配、rTIO 和后台回收。旧运行的设备请求必须已终止/排空；只允许真正重启或经过设备隔离的模拟崩溃进入恢复。
2. 恢复 NVM 权威图、文件大小和注册状态；注册文件的 native extent 即使结构合法也不作为数据来源。注册未完成的文件不能启动新迁移。
3. 建立块保护集合：NVM 已发布 SSD 映射、有效 NV 描述符的 pending target、未注册且保持不变的可信 native 文件映射、文件系统固定保留区。发现目标重叠、越界或无法解释的引用时拒绝可写恢复，禁止猜测释放。
4. 校验/重建被协议改写的 metadata。没有可靠根或重建信息的区域隔离；不能开放为 free。注册文件的 native 树可以从 NVM 完整图重建，不将未完成 SSD extent 导出提升为权威。
5. 将所有保护块从分配器 free 集合排除，重建运行时容量和保留计数。只有已经证明安全的空闲范围可开放；扫描尚未覆盖的区域保持隔离。空间不足时等待核对/回收，不绕过保护。
6. 重新选择 NV 候选，默认复用有效 `ssd_target`；描述符没有目标才重新分配。已有目标不可重复扣预留额度。恢复过程自身掉电后仍按相同步骤重做。

### 后台垃圾核对与回收

对象是权威图和保护集合均不引用、但 SSD 暂态元数据可能占用的资源。普通 e2fsck 不理解 NVM 权威状态，不能作为在线回收器。注册文件的暂态 native extent 可以被弃用；共享 metadata 块及未注册文件不能整块释放。

后台扫描使用分配代际/保护集合快照，真正释放前在组级锁下重新确认无活跃 lease、无当前分配/权威引用、无在途 I/O。新分配必须进入保护集合，不能因扫描快照较旧被释放。被隔离的块仅在完成核对后进入 free pool。先持久撤销必要引用，再释放资源；bitmap 尚未持久化时再次掉电，恢复重做核对，不能双重分配。

后台完成垃圾回收可以延迟；前台开启前的权威恢复和防重复分配不能延迟。若需要重建全部所有权才能获得安全空闲空间，首版接受较慢恢复，不以恢复速度换取正确性。

### 崩溃矩阵

| 崩溃位置 | 恢复权威 | 允许的处理 |
| --- | --- | --- |
| NVM 目标发布前 | 原 NVM 映射 | SSD 请求尚未发出；未持久的私有预分配可重新分配 |
| metadata 落盘、data 未落盘 | NV 数据与 NVM extent | 屏蔽提前写出的 SSD extent，核对目标后重写 |
| data 落盘、metadata 未落盘 | NV 数据与 NVM extent | 保留或重建 metadata，重试迁移 |
| 全部 completion，但 flush 未完成/结果不明 | NV 数据与 NVM extent | 不能推定持久，重新迁移并 flush |
| flush 成功、NVM 尚未发布 | NV 数据与 NVM extent | 可重复迁移；不依赖丢失的 DRAM 成功记录 |
| 部分 NVM 叶已发布 | 每页 NVM 权威映射 | 已发布 SSD 页保留；其余 NV 页重试 |
| NVM 已发布，NV 描述符尚未清除 | NVM SSD 映射 | 按代际校验回收冗余 NV 页 |
| 回收后再次掉电 | NVM SSD 映射 | 不复活旧描述符，不重复归还额度 |

## 7. 模式、同步与故障边界

- rNVPCe rTIO 固定采用 orderless migration；运行时不提供协议切换，010 ordered 路径不保留在当前代码中。尚未完成的恢复能力必须通过明确的挂载拒绝或功能约束处理，不能静默回退到旧协议。
- 普通 write/fsync 的 NVM 持久性契约保持；显式 SSD drain/sync 等待所覆盖批次的 flush、NVM 发布和最终导出。干净卸载必须完成 native 映射及必要元数据导出，才能声称支持仅靠 SSD 的 native 挂载。
- 异常掉电后要求 NVM 感知挂载；不能先用 native EXT4/e2fsck修复并改变 SSD，再期望 NVM 恢复仍有效。为新研究模式定义持久的研究模式标记和工具检查；若目标是阻止未修改的 native 驱动挂载，还须采用其识别并拒绝的格式机制，不能仅写自定义标记冒充保护。
- I/O/flush 失败保留 NVM、保护目标并停止推进受影响批次。错误不能由计数归零覆盖；设备永久失败需向容量等待者返回错误，不能永久睡眠。
- 不承诺任意 namespace 操作的崩溃一致性，不承诺现有 fast NVM 模拟提供跨 QEMU 重启持久性。恢复测试必须组合可保留的 NVM 镜像与可重排 SSD 模型；真实断电实验另列。

## 8. 验收与性能归因

正确性：验证上表所有切点以及重复恢复；额外覆盖同 metadata 块双批次交错、同 data 目标新旧版本、立即回调、部分提交失败、flush 失败、叶部分发布、容量唤醒、后台扫描与新分配竞争、inode 表共享邻居和 torn metadata。对“metadata 已落盘但 data 未落盘”必须逐字节读回 NV 正确内容，不能只检查 e2fsck。

机制：正常非空批次恰好一个 SSD flush；分配阶段0个；回收不能早于 metadata/data完成及flush成功。native/rNVPC 路径行为保持。旧版现有批次计数与新版比较，不以每页一次的假想基线夸大收益。

性能：真实 NVMe、2GiB、64MiB 数据上限、128页全局在途、4KiB随机/128KiB顺序、1/16线程，沿用CPU隔离；前后各3轮。report fio 与含最终sync带宽、完整写入、错误、卸载与e2fsck结果。诊断单独镜像：候选/目标、metadata快照/提交、data提交、各类completion、队列等待、设备flush、NVM发布/回收；区分 SSD flush 与 NVM persist/fence。记录批次页数分布、metadata共享、失败、设备flush次数及延迟分布；不将累计阶段时间简单相加当墙钟。

实现步骤见 [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)。本设计不预先承诺吞吐提升；恢复正确性与性能实验分别验收。

## 文档检查

2026-10-19完成一次独立只读builder检查：在上述首版范围内，未发现阻塞实施规划的未决正确性问题。该结论仅是设计可实施性检查，不替代代码审查、故障模型或真实设备测试。内部相对链接和diff空白检查通过。
