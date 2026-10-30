# Baseline Token 单次校验优化与验证，2026-10-29

## 结论

本次优化把 reclaim 阶段的 baseline generation 校验从“extent 内每页重新校验整段”
改为“每个 dirty extent 校验整段一次”。一致性语义不变：每页仍检查自己的 SSD
target，且新增 `Arc::ptr_eq` 检查，拒绝一个迁移 extent 混入不同 token。

真实 NVMe 的默认无插桩结果证明优化有效：128 KiB 顺序写 j1 从 110.88 MiB/s
提高到 153.51--179.74 MiB/s；j16 从 113.60 MiB/s 提高到 218.38 MiB/s。
采用较低的 j1 样本计算，分别提升 38.4% 和 92.2%。j16 drain 带宽从
92.75 MiB/s 提高到 148.95 MiB/s，提升 60.6%。

诊断数据确认收益来自预期阶段：`completion_compare` j1 从 18.88 降至
0.46 us/page，j16 从 291.35 降至 1.04 us/page。当前下一瓶颈已经转为
descriptor clear 和候选 extent 的 group-ownership 冲突，不再是 token 校验。

## 实现

`finish_redirect_page_batches()` 现在执行：

1. 从 dirty extent 第一页取得共享 `Arc<BaselineRangeToken>`；
2. 在进入 page 循环前调用一次 `validate_baseline_token()`；
3. 每页确认 token 指针与共享 token 相同，并检查该页 target；
4. 仅当整段 generation 或该页 target 不匹配时，进入原有慢速 mapping fallback。

原实现在第 3 步对每页重新调用第 2 步。`BaselineGenerationMap::validate()` 自身会持有
全局 generation-map 自旋锁并遍历 token 的全部 generation，因此长度为 `n` 的
extent 是 `n` 次加锁和 O(n^2) 次 map 查询。新实现是一次加锁和 O(n) 查询。

## TDD 回归

在真实 EXT4 NV route 场景中写入、迁移、完成并回收一个 4 页 extent。ktest 专用
计数器记录 `BaselineGenerationMap::validate()` 调用次数。

- RED：优化前断言失败，`left: 4, right: 1`。
- GREEN：优化后 `ext4_research_routes` 通过，其中 23 个 NV 场景全部通过。
- 该测试会在校验重新移回 page 循环时失败；它不依赖 mock。

release ktest 构建还暴露了一个与本改动无关的既有问题：三个
`debug_assert_es_coverage()` 调用在 release-ktest 下找不到方法。dev ktest 能正常
编译和执行，因此红绿验证使用 dev ktest；该 release 配置问题没有在本次顺带修改。

## 默认构建性能

条件：`/dev/nvme1n1p1`、64 vCPU、8 GiB、单 virtio queue depth 1024、128 MiB
rNVPCe pool、64 MiB data limit、buffered psync、128 KiB 顺序写、2 GiB。

| 配置 | fio MiB/s | Drain MiB/s | Mean clat | P99 clat |
|---|---:|---:|---:|---:|
| 优化前 j1 | 110.88 | 86.23 | 1.13 ms | 20.84 ms |
| 优化后 j1，第 1 次 | 153.51 | 103.75 | 0.813 ms | 8.45 ms |
| 优化后 j1，第 2 次 | 179.74 | 122.12 | 0.694 ms | 6.85 ms |
| 优化前 j16 | 113.60 | 92.75 | 16.86 ms | 127.40 ms |
| 优化后 j16 | 218.38 | 148.95 | 8.65 ms | 70.78 ms |

三次优化后测试均完整写入 2 GiB、`fio_status=0`、正常卸载。j16 和第二次 j1
分别在运行后通过离线 `e2fsck -fn`；j1 第一次在下一点格式化前未单独执行 e2fsck，
因此不把其正常卸载扩大表述为离线检查证据。

## 诊断阶段 A/B

下表使用同一套高频探针；时间是跨线程累计 CPU 时间，不能直接相加为墙钟时间。

| 指标 | 优化前 j1 | 优化后 j1 | 优化前 j16 | 优化后 j16 |
|---|---:|---:|---:|---:|
| completion compare | 18.88 us/page | 0.46 us/page | 291.35 us/page | 1.04 us/page |
| compare 累计 | 9.90 s | 0.241 s | 152.75 s | 0.546 s |
| reclaim helper | 20.80 us/page | 4.05 us/page | 307.73 us/page | 10.22 us/page |
| reclaim helper 累计 | 10.90 s | 2.12 s | 161.34 s | 5.36 s |
| capacity wait 累计 | 1.12 s | 0.94 s | 186.70 s | 16.40 s |
| foreground total | 1.24 ms/write | 0.89 ms/write | 16.79 ms/write | 7.15 ms/write |

降幅：

- completion compare：j1 97.6%，j16 99.6%；
- reclaim helper：j1 80.5%，j16 96.7%；
- j16 capacity wait：91.2%。

`completion_comparisons` 仍为 524,288，表示每页 target 语义检查没有删除；减少的是
共享 range generation 的重复整段校验。

## 残余瓶颈

### Descriptor clear

优化后 descriptor clear 为 j1 2.11 us/page、j16 5.41 us/page，已超过
completion compare，成为 reclaim 内最大的单项。下一步可检查连续 slot range 是否
仍逐页取得 descriptor/free-list 锁，再决定是否做 range batch clear。

### Candidate claim/requeue

| 指标 | 优化前 j1 | 优化后 j1 | 优化前 j16 | 优化后 j16 |
|---|---:|---:|---:|---:|
| candidate pages / actual pages | 4.02x | 2.69x | 5.86x | 5.79x |
| candidate attempts | 2,059 | 1,380 | 3,003 | 2,964 |
| foreground group attempts | 20,378 | 19,882 | 30,012 | 21,816 |
| group 额外尝试率 | 24.4% | 21.4% | 83.2% | 33.2% |

更快的 reclaim 缩短了冲突窗口，所以前台 group 重试随之下降；但 j16 候选页仍被
处理约 5.8 次，证明 P1 并未被本次改动解决。下一项应让已有 SSD target 的 data-only
migration 不再参与 allocation-group ownership，或在 claim extent 前完成无冲突
admission，避免整段 claim/requeue。

## 证据位置

- 默认构建日志：
  `/home/zn/asterinas-build-env/baseline-token-once-20261029/final/`
- 诊断日志：
  `/home/zn/asterinas-build-env/baseline-token-once-20261029/diagnostic/`
- 优化前分解：`RNVPCE-128K-BREAKDOWN-2026-10-29.md`

诊断开关已恢复为默认关闭，原始镜像和控制台日志不纳入 Git。

## 验证边界

- `cargo fmt --all -- --check`：通过；
- `cargo osdk check --ktests --package aster-kernel`：通过；
- 聚焦 `ext4_research_routes`：通过，`1 passed; 0 failed`，内部 23 个 NV
  场景全部通过；
- 完整 dev ktest 在 QEMU/TCG 中启动 385 个 `aster-kernel` 测试。已执行部分均
  通过，包括新增的 `nv_route_rnvpce_completion_publishes_reclaim_ticket`；20 分钟
  外层超时发生在既有
  `persistent_wal_root_split_faults_recover_from_committed_records` 场景，因此没有取得
  完整测试集的最终汇总，不能据此宣称 385 项全部通过；
- 默认构建的 j16 与重复 j1 性能测试后，离线 `e2fsck -fn` 均通过。
