#!/usr/bin/env python3
"""同一 (device, workload, mode, njobs, 挂载选项) 下 **两个 FS profile** 的
writeback（后台回写线程）延迟构成并列对比，回答"为什么 nojournal(ext2like) 比
journal(ext4) 慢"。

与 `latency_decomp.py` 的区别：后者对比**同一 FS 的快轮/慢轮**（双稳性诊断）；本脚本
对比**两个 FS**，且不截断计数器列表，并把每轮快照归一化成

    per_round   = count / 轮数            # 该计数器每轮被触发几次
    us_per_call = avg_us                  # 单次耗时（µs）
    total_ms    = count * avg_us / 1000   # 该轮全程累计线程时间（ms）

数据来源：`results/<run>/logs/{baseline_run.log, qemu-serial-<tag>.log}`，
要求 `LATENCY_ENABLED = true` 的构建。

用法：
    python3 wb_fs_compare.py nvme randwrite-4k orderless 16 [--fs ext2like,ext4]
                            [--min-ts 20260922_221236] [--json out.json]
"""

import argparse
import glob
import json
import os
import re
import statistics as st
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import parse_baseline as pb  # noqa: E402

SNAP = re.compile(r"\[LATENCY\] == snapshot begin ==(.*?)\[LATENCY\] == snapshot end ==", re.S)
ONE = re.compile(r"\[LATENCY\] (\w+): count=(\d+)(?: avg_us=(\d+) max_us=(\d+))?")
DEPTH = re.compile(r"\[LATENCY\] max_tree_depth: (\d+)")

# 前台写路径 / writeback 线程 / 分配窗口子段。顺序即报告顺序。
GROUPS = [
    ("前台 write() 路径", [
        "write_total", "write_inner_lock", "write_page_cache",
        "write_mark_dirty", "write_throttle",
    ]),
    ("writeback 线程：每轮", [
        "wb_round", "wb_collect", "wb_read_inode", "wb_wake",
    ]),
    ("writeback 线程：每 inode 漏斗（wb_sync_total 内部）", [
        "wb_sync_total", "wb_inner_lock", "wb_begin_op", "wb_alloc_pending",
        "wb_alloc_blocks", "wb_walk_range", "wb_tree_insert", "wb_map_blocks",
        "wb_convert_unwritten", "wb_journal_capture", "wb_flush_async",
        "wb_wait_all", "wb_wait_page_io", "wb_reap", "wb_dirty_lock",
    ]),
    ("nojournal 分配窗口 / inode 表（009 定义）", [
        "alloc_meta_flush", "alloc_meta_defer", "alloc_meta_block",
        "alloc_meta_bio", "alloc_meta_reap", "alloc_meta_flusher_write",
        "alloc_desc",
    ]),
    ("journal 提交体（仅 ext4，供参照）", [
        "commit_total", "commit_ordered", "commit_log",
        "commit_barrier1", "commit_barrier2",
    ]),
]


def parse_snapshot(logdir, tag):
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


def collect(dev, fs, wl, mode, njobs, min_ts, suffix=""):
    """返回 [(run, bw, write_mib, stats, depth)]，只取指定配置的轮次。

    `suffix` 非空时：只取目录名含该后缀的轮次（用于 `_njwb-sync` / `_njwb-async`
    这类臂选择），且不再套用默认的"剔除臂目录"规则。
    """
    out = []
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        ts = "_".join(name.split("_")[:2])  # YYYYMMDD_HHMMSS（目录名首两段）
        if ts < min_ts:
            continue
        if suffix:
            if not name.endswith(suffix):
                continue
        elif "_njmb-" in name or "_njwb-" in name or "_barrier-" in name:
            continue
        logdir = os.path.join(d, "logs")
        log = os.path.join(logdir, "baseline_run.log")
        if not os.path.exists(log):
            continue
        for r in pb.parse(log):
            if (r["device"], r["fs"], r["workload"], r["mode"]) != (dev, fs, wl, mode):
                continue
            if int(r["njobs"]) != int(njobs) or r["valid"] != "1":
                continue
            tag = f"{dev}-{fs}-{wl}-{mode}-nj{r['njobs']}"
            snap = parse_snapshot(logdir, tag)
            if snap is None:
                continue
            out.append((name, float(r["drain_bw_mib_s"]), float(r["write_mib"]),
                        snap[0], snap[1]))
    return out


def med(rows, key, field):
    vals = [r[3].get(key, {}).get(field, 0) for r in rows]
    return st.median(vals) if vals else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("device")
    ap.add_argument("workload")
    ap.add_argument("mode")
    ap.add_argument("njobs", type=int)
    ap.add_argument("--fs", default="ext2like,ext4",
                    help="逗号分隔；支持 'fs#目录后缀' 选择带臂后缀的轮次，"
                         "如 ext2like#_njwb-sync,ext2like#_njwb-async")
    ap.add_argument("--min-ts", default="00000000_000000")
    ap.add_argument("--json", default="")
    a = ap.parse_args()

    data = {}
    for spec in a.fs.split(","):
        fs, _, suffix = spec.partition("#")
        rows = collect(a.device, fs, a.workload, a.mode, a.njobs, a.min_ts, suffix)
        if not rows:
            print(f"!! {spec}: 无可用轮次（需 LATENCY_ENABLED=true 的构建）")
            return 1
        data[spec] = rows

    print(f"=== {a.device}/{a.workload}/{a.mode} nj{a.njobs}："
          f"{' vs '.join(data)}（截止 {a.min_ts}）===")
    for fs, rows in data.items():
        bws = ", ".join(f"{r[1]:.1f}" for r in rows)
        print(f"  {fs:<9} 轮数={len(rows)}  drain_bw=[{bws}] MiB/s  "
              f"中位={st.median([r[1] for r in rows]):.1f}  "
              f"写入={st.median([r[2] for r in rows]):.0f} MiB  树深={rows[0][4]}")
    fs_a, fs_b = list(data)[0], list(data)[1]
    ra, rb = data[fs_a], data[fs_b]

    # 每轮触发次数（用于把 count 归一到"每轮"）
    def per_round(rows, key):
        cnt = med(rows, key, "count")
        rounds = med(rows, "wb_round", "count") or 1
        return cnt / rounds

    nround_a = med(ra, "wb_round", "count") or 1
    nround_b = med(rb, "wb_round", "count") or 1

    print(f"\n{'组件':<22}{fs_a + ' µs/次':>18}{fs_b + ' µs/次':>18}"
          f"{'B/A':>7}   │ {'每轮次数 A/B':>16}{'累计 ms A/B':>20}")
    print("-" * 110)
    report = []
    for group, keys in GROUPS:
        printed = False
        for k in keys:
            ca, cb = med(ra, k, "count"), med(rb, k, "count")
            aa, ab = med(ra, k, "avg_us"), med(rb, k, "avg_us")
            if ca == 0 and cb == 0:
                continue
            if not printed:
                print(f"[{group}]")
                printed = True
            ratio = f"{aa / ab:.2f}x" if ab else "-"
            nr = f"{ca / nround_a:.1f}/{cb / nround_b:.1f}"
            tot = (f"{ca * aa / 1000:.0f}/{cb * ab / 1000:.0f}")
            print(f"  {k:<20}{aa:>18}{ab:>18}{ratio:>7}   │ {nr:>16}{tot:>20}")
            report.append({
                "counter": k, "group": group,
                f"{fs_a}_avg_us": aa, f"{fs_b}_avg_us": ab,
                f"{fs_a}_count": int(ca), f"{fs_b}_count": int(cb),
                f"{fs_a}_total_ms": ca * aa / 1000, f"{fs_b}_total_ms": cb * ab / 1000,
            })

    # 漏斗内部合计 vs wb_sync_total：找出未记账部分
    funnel_keys = GROUPS[2][1]
    print(f"\n[每 inode 漏斗的账目闭合（中位值，µs/次）]")
    for fs, rows in data.items():
        inner = [k for k in funnel_keys if k != "wb_sync_total"]
        s = sum(med(rows, k, "avg_us") for k in inner)
        tot = med(rows, "wb_sync_total", "avg_us")
        print(f"  {fs:<9} 子项合计={s:>7}  wb_sync_total={tot:>7}  "
              f"未记账={tot - s:>7}  子项占比={100 * s / tot if tot else 0:.1f}%")

    print(f"\n[每轮账目闭合（中位值，µs/轮）]")
    for fs, rows in data.items():
        inner = [k for k in funnel_keys if k != "wb_sync_total"]
        per = med(rows, "wb_sync_total", "count")
        s = sum(med(rows, k, "avg_us") * med(rows, k, "count") for k in inner)
        tot = med(rows, "wb_sync_total", "avg_us") * per
        rnd = med(rows, "wb_round", "avg_us")
        print(f"  {fs:<9} 漏斗×次数={tot:>9}  子项合计={s:>9}  "
              f"wb_round={rnd:>9}  轮内未记账={rnd - tot:>9}")

    print(f"\n[每轮明细：吞吐 / 关键阶段 µs/次]")
    PER_ROUND = ["write_throttle", "wb_round", "wb_sync_total", "wb_alloc_pending",
                 "alloc_meta_flush", "alloc_meta_defer", "alloc_meta_bio",
                 "alloc_meta_reap", "alloc_meta_block", "alloc_meta_flusher_write",
                 "alloc_desc", "wb_flush_async", "wb_reap"]
    print(f"  {'fs':<9}{'bw':>9}{'write':>7}" +
          "".join(f"{k[:13]:>15}" for k in PER_ROUND))
    for fs, rows in data.items():
        for run, bw, wm, s, _depth in sorted(rows, key=lambda r: r[1]):
            cells = "".join(
                f"{s.get(k, {}).get('avg_us', 0):>15}" for k in PER_ROUND)
            print(f"  {fs:<9}{bw:>9.1f}{wm:>7.0f}{cells}")

    if a.json:
        with open(a.json, "w") as f:
            json.dump({"config": vars(a), "rounds": {
                fs: [{"run": r[0], "bw": r[1], "write_mib": r[2], "stats": r[3]}
                     for r in rows] for fs, rows in data.items()},
                "table": report}, f, indent=2)
        print(f"\n已写出 {a.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
