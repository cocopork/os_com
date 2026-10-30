# rNVPCe 128 KiB 写路径分解实验，2026-10-29

## 结论

当前 128 KiB 顺序写的首要瓶颈不是 SSD I/O，也不是 completion 线程唤醒，
而是 **reclaim helper 对同一个 extent token 做逐页、整段重复校验**：
`finish_redirect_page_batches()` 在每页循环内调用
`validate_baseline_token()`，而后者会遍历 token 的全部 generation。长度为 `n` 的
迁移 extent 因此执行 `n` 次加锁和约 `n^2` 次 `BTreeMap` 查询。

该问题在 16 线程时被全局 baseline-generation 自旋锁进一步放大：completion compare
从单线程的 18.88 us/page 增至 291.35 us/page，reclaim helper 从 20.80 us/page
增至 307.73 us/page。真正的 completion 工作者本体只有 0.27--0.33 us/page，设备
flush 只有 0.14--0.17 us/page；二者都不是当前上限。

第二个独立瓶颈是 foreground allocation 与 flusher 共用 block-group ownership。
flusher 先 claim dirty extent，随后才尝试 group reservation；冲突后整段 requeue。
因此每个实际页面在单线程下平均被候选路径处理 4.02 次，16 线程下为 5.86 次。
前台 group reservation 的额外尝试率也从 24.4% 增至 83.2%。

## 实验条件

- 分支：`recovery/rnvpce-page-redirection`
- 设备：`/dev/nvme1n1p1`，每点重新格式化
- Guest：64 vCPU、8 GiB；单 virtio-blk 队列，深度 1024
- rNVPCe：128 MiB pool、64 MiB metadata reserve、64 MiB data limit、fast persist
- fio：buffered `psync`、128 KiB 顺序写、2 GiB、1/16 jobs
- 诊断镜像同时启用 pipeline、latency 和 NVPage batch 统计；默认源码开关已恢复关闭
- 两点均 `fio_status=0`、卸载成功，并通过离线 `e2fsck -fn`

诊断原始日志位于
`/home/zn/asterinas-build-env/foreground-baseline-breakdown-20261029/diagnostic/`。
高频探针会降低绝对吞吐，因此本报告用它定位阶段；默认构建性能仍以
`FOREGROUND-SSD-BASELINE-PERFORMANCE-2026-10-29.md` 为准。

## 端到端结果

| jobs | fio 时间 | 诊断 fio 带宽 | sync 后总时间 | 诊断 drain 带宽 | fio 后排空 |
|---:|---:|---:|---:|---:|---:|
| 1 | 20.77 s | 98.60 MiB/s | 26.42 s | 77.52 MiB/s | 5.65 s |
| 16 | 18.66 s | 109.75 MiB/s | 22.56 s | 90.78 MiB/s | 3.90 s |

对应默认无插桩构建为 110.88/113.60 MiB/s，drain 为 86.23/92.75 MiB/s。
增加线程没有显著提高吞吐，只把前台一次 128 KiB write 的累计延迟从 1.24 ms
放大到 16.79 ms。

## 前台分解

下表均为每次 128 KiB write 的累计 CPU 时间；16 线程数据允许跨线程重叠，不能直接
相加得到墙钟时间。

| 阶段 | j1 us/write | j16 us/write | j16/j1 |
|---|---:|---:|---:|
| foreground total | 1,236.95 | 16,794.78 | 13.58x |
| capacity wait | 68.09 | 11,395.34 | 167.36x |
| SSD allocation（含失败尝试） | 193.65 | 690.13 | 3.56x |
| group select/reserve | 145.20 | 258.62 | 1.78x |
| block bitmap allocation | 0.80 | 1.49 | 1.87x |
| NVM allocation metadata publish | 46.57 | 427.19 | 9.17x |
| persistent transaction | 216.78 | 1,451.17 | 6.69x |
| data init/register/copy body | 159.38 | 1,212.18 | 7.61x |
| extent tree update | 1.95 | 6.62 | 3.39x |
| inode publish（含 ownership record 回收） | 3.85 | 131.41 | 34.13x |

`extent tree update` 已经很小，不再是原 repair-image 时代的主成本。多线程下的
capacity wait 是结果而不是最初原因：前台线程在容量不足时会调用 reclaim helper，
而 helper 正在执行下面的高成本重复验证。

## rTIO 与回收分解

| 阶段 | j1 | j16 | 说明 |
|---|---:|---:|---|
| 成功 flush rounds | 513 | 513 | 平均约 1,022 页/round，批次是满的 |
| flush total | 7.13 us/page | 3.29 us/page | 只统计成功 round |
| target prepare | 0.29 us/page | 0.95 us/page | 不是主项 |
| data BIO submit | 0.78 us/page | 0.93 us/page | 不存在毫秒级提交开销 |
| async I/O ready wait | 1.46 us/page | 1.10 us/page | 设备侧不是上限 |
| completion worker | 0.27 us/page | 0.33 us/page | 只生成 reclaim ticket，足够快 |
| device flush | 0.17 us/page | 0.14 us/page | 513 个批量 flush |
| completion compare | 18.88 us/page | 291.35 us/page | 当前主瓶颈 |
| descriptor clear | 0.83 us/page | 5.13 us/page | 次要共享状态竞争 |
| reclaim helper total | 20.80 us/page | 307.73 us/page | 几乎全部由 compare 解释 |

单线程形成 4,978 个迁移 extent，平均 105.3 页；16 线程形成 16,330 个，平均
32.1 页。多线程物理 NVPage 分配交错，使连续 extent 变短，但仍有足够批量粒度。

## 根因证据

### 1. Token 校验是 O(n^2)

`finish_redirect_page_batches()` 对 extent 的每个 page 执行一次
`pool.validate_baseline_token(token)`。`BaselineGenerationMap::validate()` 每次取得同一个
全局 state spin lock，并遍历 token 内全部 generation。由于一个 extent 中各 page
共享同一个 `Arc<BaselineRangeToken>`，这不是不同页面各自的校验，而是同一整段校验
被重复 `n` 次。

实验吻合这一代码路径：

- 实际只回收 524,288 页，但 j1 completion compare 累计 9.90 s；j16 累计
  152.75 s。
- j16 的 compare 为 9.35 ms/extent，reclaim helper 为 14.92 ms/call。
- publication lock 的实际等待只有 0.25 us/call 左右；不是 inode publication guard
  在阻塞，而是 helper 内部 generation map 的重复加锁和遍历。
- 旧指标名 `nv_extent_publish` 目前包住整个 `finish_redirect_page_batches()`，并不表示
  本轮又写了一次 SSD extent tree；`structural_baseline_mutations=0` 也证明没有后台
  structural baseline 发布。

### 2. 候选页被反复 claim/requeue

| 指标 | j1 | j16 |
|---|---:|---:|
| candidate attempts | 2,059 | 3,003 |
| 成功 flush rounds | 513 | 513 |
| candidate pages / actual pages | 4.02x | 5.86x |
| candidate 累计时间 / actual page | 19.35 us | 16.24 us |
| 前台 group-reserve attempts | 20,378 | 30,012 |
| 实际 writes | 16,384 | 16,384 |
| group-reserve 额外尝试率 | 24.4% | 83.2% |

`migrate_nv_round()` 在 group reservation 前就 claim extent；`prepare_nv_targets()` 又把
已有 SSD target 所属 group 放入 required set。只要 foreground 正持有相同 group，
reservation 返回 `EBUSY`，已 claim 的 extent 被恢复并重新排队。与此同时前台分配
也会因 migration owner 占用 group 而等待，形成双向抖动。

## 与 repair-image 高带宽版本的差距

repair-image 批处理版本的默认 128 KiB 带宽为 j1 289.14 MiB/s、j16
304.26 MiB/s。它的关键优势不只是 extent 更新协议，而是批处理路径没有当前这组
新开销：

1. 当前 foreground extent 更新仅 1.95/6.62 us/write，已经不是约 3 倍带宽差距的
   解释项。
2. 当前 reclaim 新增了 range token，但错误地按 page 重复验证整个 range，重新制造
   了比 repair-image per-page 路径更严重的 O(n^2) 工作和全局锁竞争。
3. 当前 foreground SSD allocation 与 rTIO 共用 group ownership，候选和前台相互
   requeue/retry；repair-image 高带宽版本的后台流水没有这组前台 group 冲突。

## 下一步优先级

1. **P0：把 baseline token 的 generation 校验移出 page 循环。** 每个 dirty extent
   只加锁并验证一次；循环内只检查对应 target。因为迁移 claim 已阻止该范围发生
   baseline mutation，单次 range 校验保持现有一致性语义。预期复杂度从 O(n^2)
   降为 O(n)。应先做这一项 A/B，它有最直接的代码和实测证据。
2. **P1：避免已有 SSD target 的 data-only migration 参与 allocation-group ownership。**
   只有确实分配新 target 或迁移 allocation metadata 时才需要 group reservation；
   否则应在 claim 前做无冲突 admission，避免 claim/requeue。
3. **P2：在 P0 后重新测 descriptor clear。** 若 j16 仍为约 5 us/page，再将 descriptor
   selector 清除和 free-list 发布按连续 slot range 批量化。不要在 P0 前优化 BIO
   深度或 flush 频率；当前 513 个 round 已接近满 1024 页，设备阶段占比很低。

置信度：P0 根因为高（代码直接证明且阶段数据吻合）；P1 为中高（重试计数与锁序
吻合，但还需单独 A/B 量化其独立收益）；设备 I/O 不是主瓶颈为高。
