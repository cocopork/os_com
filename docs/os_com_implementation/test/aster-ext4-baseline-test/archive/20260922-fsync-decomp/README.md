> **Archived:** 2026-09-27. **Source date:** 2026-09-22. **Archive reason:** distilled historical fsync-decomposition data retained after its raw batch logs were deleted. **Replaced by:** the [current experiment acceptance boundary](../../../../EXPERIMENTS.md), which records that no experiment is accepted. **Citation use:** historical methodology and diagnostic context only; this archive cannot support a current formal throughput conclusion.

# fsync 分解实验 · 关键数据归档（2026-09-22）

本目录是 **009-fsync-device-roundtrips** 实验结束后，从 `../results/`（91 个批次、
约 231 MB 原始日志）蒸馏出来的**关键数据**。原始批次日志与 `ab_*.log` 已删除。

## 文件

| 文件 | 行数 | 内容 |
|---|---|---|
| `summary_all.csv` | 679 | 全部批次的吞吐数据：`batch, device, fs, workload, mode, njobs, write_mib, busy_ms, drain_ms, busy_bw_mib_s, drain_bw_mib_s, iops, valid, note` |
| `latency_last_snapshot.csv` | 40 | 每轮 `[LATENCY]` **末次快照**的 31 个关键计数器（`*_count` / `*_us`）+ `[BARRIER]` 计数：`batch, device, fs, workload, njobs, snapshot_t_s, fsync_count, fsync_us, …` |

`LatencyStat` 是**全局累计量**，所以"末次快照"就是该轮的总计（`_us` 是平均值，
总量 = `_count × _us`）。只有插桩构建（`LATENCY_ENABLED=true`）的批次会出现在
`latency_last_snapshot.csv`。

## 重生成 / 扩展

```bash
cd test/aster-ext4-baseline-test
python3 archive_extract.py            # 默认写回本目录
```

脚本会遍历 `results/*/logs/qemu-serial-*.log`，逐轮解析末次快照；
需要中间快照（时序曲线）时改 `last_snapshot()` 或用 `fsync_latency_decomp.py`。

## 关键结论（详见 `../../os_com_refactor_plan/009-fsync-device-roundtrips/README.md`）

1. **修复 1（可合并 flush 屏障）**：journal 路径调用方 barrier → 0；
   nojournal 设备 flush/请求 = 0.739、搭车率 24.2%。
2. **修复 2（日志块异步批量提交）**：nj1 1106 → 840 µs。
3. **修复 3（跨 inode ordered-data 批量）**：中性，保留。
4. **修复 4（等待队列拆分，消除 per-commit 惊群）**：
   `commit_credits_wake` nj8 6290 → 0 µs，nj32 23899 → 19 µs；
   **nj8 7189 → 1886 µs（3.8×），吞吐 3.79 → 14.91 MiB/s**（重复 14.89）。
5. **结论**：剩余成本 = 设备 write + 串行等待（group commit），属天然局限；
   **ext2/ext4 fsync 路径不再继续优化**。
6. **未解项**：nj32 双稳病态态（修复后 5 次采样 < 91 iops），记录在案不再追。

`LATENCY_ENABLED` 已复位为 `false`（插桩代码保留、零开销）。

## 复现命令（当时使用）

```bash
DEV=nvme FS="ext4,ext2like" WORKLOAD=randwrite-4k MODE=fsync \
  NJOBS="1,8,32" ARMS="barrier-merge" REPEATS=1 bash ab_njmb.sh
```

约束：所有实验在 `nvme1n1p2` 上做（sda3 被另一任务占用，SATA 单队列易受干扰）；
每轮会重建内核，因此测量期间不要改内核代码。
