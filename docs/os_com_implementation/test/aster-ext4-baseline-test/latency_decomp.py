#!/usr/bin/env python3
"""分解某一 (device, fs, workload, mode) 各轮次的**内核侧延迟构成**，并与吞吐对齐。

数据来源：
  * 吞吐：`results/<run>/logs/baseline_run.log`（parse_baseline.py）
  * 内核侧：该轮的 `qemu-serial-<tag>.log` / `qemu-<tag>.log` 里的 `[LATENCY]` 快照；
    `LATENCY_ENABLED = true` 时才有。快照是**累计**的（自 mount 起），每轮一次 mount，
    所以取该轮**最后一块** snapshot 即该轮全程的统计。

用途：回答"同一配置为什么有的轮次 ~1400 MiB/s、有的 ~130 MiB/s"——把慢轮与快轮
并列，看哪个组件（前台 throttle / 后台 per-inode 同步元数据 / 每轮耗时 / 树深）差异最大。

用法：
    python3 latency_decomp.py nvme ext2like seqwrite-4k orderless [--top N]
"""

import glob
import os
import re
import statistics as st
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import parse_baseline as pb  # noqa: E402

SLOW = 50.0
# 关注的关键组件（dump 里的名字）。前台写 + 后台写回的分解 + 每轮耗时 + 树深。
KEY = [
    "write_total", "write_inner_lock", "write_throttle",
    "wb_sync_total", "wb_inner_lock", "wb_alloc_pending",
    "wb_journal_capture", "wb_flush_async", "wb_begin_op",
    "wb_alloc_blocks", "wb_tree_insert", "wb_walk_range", "wb_round",
]
SNAP = re.compile(r"\[LATENCY\] == snapshot begin ==(.*?)\[LATENCY\] == snapshot end ==", re.S)
ONE = re.compile(r"\[LATENCY\] (\w+): count=(\d+)(?: avg_us=(\d+) max_us=(\d+))?")
DEPTH = re.compile(r"\[LATENCY\] max_tree_depth: (\d+)")


def parse_round_snapshot(logdir, tag):
    """取该轮最后一个 snapshot（累计值）。返回 (stats_dict, depth) 或 None。"""
    text = ""
    for pat in (f"qemu-serial-{tag}.log", f"qemu-{tag}.log"):
        p = os.path.join(logdir, pat)
        if os.path.exists(p):
            text += open(p, errors="replace").read()
    blocks = SNAP.findall(text)
    if not blocks:
        return None
    stats = {}
    for name, count, avg, mx in ONE.findall(blocks[-1]):
        stats[name] = {
            "count": int(count),
            "avg_us": int(avg) if avg else 0,
            "max_us": int(mx) if mx else 0,
        }
    depths = DEPTH.findall(text)
    return stats, (int(depths[-1]) if depths else -1)


def main():
    if len(sys.argv) < 5:
        print(__doc__)
        return 1
    dev, fs, wl, mode = sys.argv[1:5]

    rounds = []  # (njobs, bw, write_mib, run, stats, depth)
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        if "_njmb-" in name or "_njwb-" in name:
            continue
        logdir = os.path.join(d, "logs")
        log = os.path.join(logdir, "baseline_run.log")
        if not os.path.exists(log):
            continue
        run = name.split("_aster-ext4-baseline")[0]
        for r in pb.parse(log):
            if (r["device"], r["fs"], r["workload"], r["mode"]) != (dev, fs, wl, mode):
                continue
            if r["valid"] != "1":
                continue
            tag = f"{r['device']}-{r['fs']}-{r['workload']}-{r['mode']}-nj{r['njobs']}"
            snap = parse_round_snapshot(logdir, tag)
            if snap is None:
                continue
            rounds.append((int(r["njobs"]), float(r["drain_bw_mib_s"]),
                           float(r["write_mib"]), run, snap[0], snap[1]))

    if not rounds:
        print("没有找到带 [LATENCY] 快照的轮次（需 LATENCY_ENABLED=true 的构建）")
        return 1

    rounds.sort(key=lambda x: x[1])
    print(f"=== {dev}/{fs}/{wl}/{mode}：{len(rounds)} 轮（按吞吐升序）===")
    hdr = f"{'njobs':>6}{'bw':>9}{'write':>7}{'depth':>6}" + "".join(f"{k[:11]:>12}" for k in KEY)
    print(hdr)
    for nj, bw, wm, run, s, depth in rounds:
        cells = ""
        for k in KEY:
            v = s.get(k, {})
            cnt, avg = v.get("count", 0), v.get("avg_us", 0)
            cells += f"{f'{cnt}/{avg}':>12}"
        print(f"{nj:>6}{bw:>9.1f}{wm:>7.0f}{depth:>6}{cells}")

    slow = [r for r in rounds if r[1] < SLOW]
    fast = [r for r in rounds if r[1] >= SLOW]
    print(f"\n慢轮 {len(slow)} / 快轮 {len(fast)}")
    print(f"{'组件':<20}{'慢轮中位 count/avg_us':>26}{'快轮中位 count/avg_us':>26}{'差异':>10}")
    for k in KEY:
        def med(rs, field):
            vals = [r[4].get(k, {}).get(field, 0) for r in rs]
            return st.median(vals) if vals else 0
        sc, sa = med(slow, "count"), med(slow, "avg_us")
        fc, fa = med(fast, "count"), med(fast, "avg_us")
        ratio = f"{sa / fa:.1f}x" if fa else "n/a"
        print(f"{k:<20}{f'{sc:.0f} / {sa:.0f}':>26}{f'{fc:.0f} / {fa:.0f}':>26}{ratio:>10}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
