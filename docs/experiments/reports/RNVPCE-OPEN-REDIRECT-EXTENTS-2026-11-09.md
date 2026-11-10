# rNVPCe 开放式重定向 extent 优化验证

日期：2026-11-09

## 1. 目标

本轮优化解决 4 KiB 顺序覆盖写中两个重复持久化问题：

1. 相邻的 `write()` 不再各自创建一个持久化 redirect extent；满足逻辑地址、NV slot 和 SSD block 连续性时，后一次写扩展前一个记录。
2. 覆盖写未改变 extent root、文件大小和块数时，不再发布内容完全相同的新 inode 记录。

开放式 redirect extent 仍是持久化结构。DRAM 只保存扩展游标和运行时状态；`write()` 返回前，新数据和扩展后的 `page_count` 都必须完成持久化。

## 2. 一致性协议

扩展顺序为：

1. 精确预留原 redirect extent 尾部之后的 NV slot；
2. 写入并持久化新增数据页；
3. 原子更新所选持久化记录副本中的 8 字节头部并持久化；
4. 将 DRAM entry 的已提交页数和迁移队列计数扩展到新范围。

步骤 3 之前掉电时，恢复仍采用旧范围，未引用的尾部页由恢复过程回收。步骤 3 之后掉电时，恢复采用完整新范围。扩展期间 entry 处于 `Extending` 状态，迁移不能获得该 entry。

精确预留只允许发生在同一个 NV 分配域内。若尾部已占用、跨 per-CPU 分配域、entry 正在迁移或已达到 128 页，事务退化为发布独立 redirect entry，不能把普通分配得到的数值相邻区间误判为可扩展区间。

## 3. 正确性验证

以下定向 ktest 均通过：

| 测试 | 覆盖内容 |
| --- | --- |
| `rnvpce_adjacent_overwrite_syscalls_extend_one_redirect_entry` | 相邻 4 KiB 系统调用形成一个 redirect entry |
| `rnvpce_redirect_extent_runtime_extension_keeps_one_entry` | `Dirty -> Extending -> Dirty`、范围所有权和队列计数 |
| `rnvpce_aborted_redirect_extension_restores_migration_readiness` | worker 在扩展期间消费旧 ready 项后，中止扩展会恢复调度 |
| `rnvpce_redirect_extent_extension_recovery_boundaries` | 头部发布前后两个掉电恢复边界 |
| `rnvpce_existing_baseline_overwrite_uses_no_extent_update_session` | 纯覆盖写复用 inode 引用和 generation，并可恢复读取 |
| `rnvpce_slot_allocator` | 精确尾部预留及跨分配域退化 |

release 内核构建成功。真实设备每轮测试结束后的 `e2fsck -f` 状态均为 0。
最终审查还关闭了三个并发/错误路径问题：精确预留的 `EAGAIN` 重新进入原容量等待路径；成功扩展在 entry-map 锁内先切回 `Dirty` 再入队；安全失败会重新入队旧 entry 并释放未发布尾页。

实现过程中，未标记普通预留与精确尾部预留的早期版本在跨分配域时触发：

```text
NV parallel reclaim failed: EINVAL "NV slot range crosses allocation domains"
```

修复后，只有 `try_mark_allocated_after_predecessor` 成功的预留才允许扩展；真实设备五轮测试未再出现该错误。失败数据保留在：

```text
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/failed-cross-domain-results
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/failed-unmarked-fallback-results
```

## 4. 性能环境

- 设备：`/dev/nvme1n1p2`，256 GiB；每个测试点重新格式化为 EXT4。
- VM：64 vCPU，8 GiB DRAM。
- NV 池：128 MiB；metadata reserve 64 MiB；data limit 64 MiB。
- 数据集：预先创建的 2 GiB、低碎片文件。
- fio：4 KiB，1 job，buffered overwrite；分别测试 `write` 和 `randwrite`。
- 每个工作负载运行 5 轮。
- 五轮性能镜像 SHA-256：`6962c6013789550eb48bab5aadd4d8c400b8474b9525c3e5d51f4f3759b7d078`。
- 审查修复后的最终镜像 SHA-256：`e807b2390b7ddee6d861bd3c2149a16b36cde083a0fca0a0d26d62f26c2eb0d0`。
- 测试树状态：分支 `optimize/rnvpce-allocation-goal`，基准提交
  `fe6f09ff7c02294232c5e24e19ef0b7f7d0038ec`，包含本轮及此前尚未提交的工作树修改。

核心执行命令：

```bash
docker exec -e CARGO_NET_OFFLINE=true -e SMP=64 -e MEM=8G \
  -w /root/asterinas/kernel os-com-rnvpc-rebuild \
  cargo osdk build --release --init-args='/ext2/rebuild-fio.sh' \
  --kcmd-args='console=ttyS0 ostd.log_level=error rnvpc=off rnvpc_extend=off rtio=off rnvpc_size=128M rnvpc_meta_reserve=64M rnvpc_data_limit=64M rnvpc_sim_mode=fast rtio_cpu_isolation=on rtio_cpu_trace=on'

ROOT_ART=<round-dir> SOURCE_ART=<source-dir> TARGET=/dev/nvme1n1p2 \
  CASES='overwrite-rnvpce-threads-write-1 overwrite-rnvpce-threads-randwrite-1' \
  bash docs/experiments/scripts/ext4-rnvpce-overwrite-sweep-20261105/run-sweep.sh
```

原始数据：

```text
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/round-1
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/round-2
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/round-3
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/round-4
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/round-5
/home/zn/asterinas-build-env/rnvpce-open-redirect-20261109/final-smoke
```

## 5. 结果

修改前数据取自 2026-11-05 overwrite sweep 的同配置单轮结果。修改后为五轮均值。

| 工作负载 | 修改前 fio 带宽 | 修改后 fio 带宽 | 带宽变化 | 修改前平均延迟 | 修改后平均延迟 | 修改前 p99 | 修改后 p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB 顺序覆盖写 | 183.69 MiB/s | 630.80 MiB/s | +243.4% | 21.00 us | 5.96 us | 40.70 us | 13.56 us |
| 4 KiB 随机覆盖写 | 182.06 MiB/s | 189.22 MiB/s | +3.9% | 21.12 us | 20.32 us | 40.70 us | 38.55 us |

顺序覆盖写五轮 fio 带宽分别为 `648.92`、`653.90`、`646.46`、`628.80`、`575.93 MiB/s`；随机覆盖写分别为 `183.53`、`198.41`、`191.26`、`184.24`、`188.69 MiB/s`。

审查修复后用最终镜像重跑一个双点烟雾测试，顺序覆盖写为 `622.11 MiB/s`、平均延迟 `6.03 us`；随机覆盖写为 `191.44 MiB/s`、平均延迟 `20.06 us`。结果落在前述五轮范围内，且两点 `e2fsck` 状态均为 0。

## 6. 结论与边界

优化有效。顺序 4 KiB 系统调用能够复用开放式 redirect extent，主要的持久化元数据成本被摊薄，带宽提高约 3.43 倍，平均和 p99 延迟分别下降约 71.6% 和 66.7%。随机覆盖几乎无法满足三种地址连续性，因此结果基本保持原水平，这也验证收益来自目标机制，而不是设备或 fio 配置变化。

本报告验证的是 1 job 热路径和崩溃协议，不替代完整的 1/4/8/12/16 jobs 多线程扫描。单 job 下宿主多 `hctx` 接受检查仍会报告请求集中，这是该检查的预期限制，不影响 fio 完成状态和文件系统一致性结果。
