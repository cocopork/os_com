#!/usr/bin/env python3
"""异步元数据写回（async）的**改动前 / 改动后**分组对比。

分组依据：结果目录名里的时间戳。
  * `before` = 20260922_0022 之前的**干净构建**批次（异步化尚未生效）；
  * `after`  = `20260922_0022` 起（含 smoke 轮与两批验证）。

**必须剔除的三类批次**（否则结论会被污染）：
  1. 插桩批次（日志里含 `[LATENCY]`）——插桩会把绝对吞吐压低约 2×；
  2. 变体臂（目录名带 `_njmb-` / `_njwb-`）；
  3. 参数与后续不同的早期批次（`20260921_065946`：SMP=24、10 轮被 timeout 杀掉）。

用法：
    python3 async_compare.py [device] [fs]              # 按时间切分（跨构建）
    python3 async_compare.py [device] [fs] --by-flag    # 按挂载选项旗标切分（**同内核** A/B）

`--by-flag` 只取带显式臂旗标的批次：
  `*_njwb-sync*` = 旧行为（缓冲 + 同步 flush）；`*_njwb-off*` = 无缓冲（原始行为）；
  `SINCE` 之后且目录名**无** `_njwb-` 后缀 = async（新默认，runner 对默认档不加后缀）。
这样两臂来自同一个内核与同一批宿主条件，免疫构建差异与批次漂移。
"""

import glob
import os
import statistics as st
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import parse_baseline as pb  # noqa: E402

SLOW = 50.0
EXCLUDE_RUNS = {"20260921_065946"}
CUTOVER = "20260922_0022"
# --by-flag：此时间点之后的批次才属于「异步版内核」（含 panic 修复后的 smoke 与验证批次）。
SINCE = "20260922_0010"
KEY_POINTS = [("seqwrite-4k", n) for n in (8, 12, 16, 20, 24, 28, 32)] + \
             [("randwrite-4k", n) for n in (8, 12, 16, 20, 24, 32)]


def is_instrumented(logdir):
    for f in glob.glob(os.path.join(logdir, "qemu-serial-*.log"))[:3]:
        try:
            if "[LATENCY]" in open(f, errors="replace").read(200_000):
                return True
        except OSError:
            pass
    return False


def main():
    args = sys.argv[1:]
    by_flag = "--by-flag" in args
    since = SINCE
    for a in list(args):
        if a.startswith("--since="):
            since = a.split("=", 1)[1]
    args = [a for a in args if not a.startswith("--")]
    dev = args[0] if len(args) > 0 else "nvme"
    fs = args[1] if len(args) > 1 else "ext2like"

    def group_of(name, run):
        """返回分组名；None 表示剔除。"""
        if by_flag:
            if "_njwb-sync" in name:
                return "sync"
            if "_njwb-off" in name:
                return "off"
            if "_njwb-" in name or "_njmb-" in name:
                return None
            return "async" if run >= since else None
        if "_njmb-" in name or "_njwb-" in name:
            return None
        return "after" if run >= CUTOVER else "before"

    groups = defaultdict(list)   # (workload, njobs, group) -> [bw]
    skipped = {"instrumented": 0, "variant_arm": 0, "excluded_run": 0, "no_flag": 0,
               "other_fs": 0}
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        logdir = os.path.join(d, "logs")
        log = os.path.join(logdir, "baseline_run.log")
        if not os.path.exists(log):
            continue
        run = name.split("_aster-ext4-baseline")[0]
        group = group_of(name, run)
        if group is None:
            if by_flag:
                skipped["no_flag"] += 1
            else:
                skipped["variant_arm"] += 1
            continue
        if run in EXCLUDE_RUNS:
            skipped["excluded_run"] += 1
            continue
        if is_instrumented(logdir):
            skipped["instrumented"] += 1
            continue
        for r in pb.parse(log):
            if r["device"] != dev or r["fs"] != fs or r["mode"] != "orderless":
                continue
            if r["valid"] != "1":
                continue
            groups[(r["workload"], int(r["njobs"]), group)].append(float(r["drain_bw_mib_s"]))

    if by_flag:
        la, lb = f"sync(旧)", "async(新)"
        ga, gb = "sync", "async"
        ratio_label = "async/sync"
        print(f"=== {dev}/{fs}/orderless：同内核 A/B —— {lb} vs {la}（无后缀批次按 run ≥ {since} 归入 async）===")
    else:
        la, lb = "before", "after"
        ga, gb = "before", "after"
        ratio_label = "倍数"
        print(f"=== {dev}/{fs}/orderless：异步元数据写回 改动前 vs 改动后（跨构建）===")
    extra = f"、无臂旗标批次 {skipped['no_flag']}" if by_flag else ""
    print(f"（已剔除：插桩批次 {skipped['instrumented']}、变体臂 {skipped['variant_arm']}、"
          f"参数不同批次 {skipped['excluded_run']}{extra}）\n")
    print(f"{'workload':<13}{'njobs':>6}{f'{la} n/中位/min/max':>30}{f'{lb} n/中位/min/max':>30}{'慢态 旧→新':>18}")
    for wl, nj in KEY_POINTS:
        b = groups.get((wl, nj, ga), [])
        a = groups.get((wl, nj, gb), [])
        if not b and not a:
            continue
        bs = sum(1 for v in b if v < SLOW)
        as_ = sum(1 for v in a if v < SLOW)
        btxt = f"{len(b)}/{(st.median(b) if b else 0):.0f}/{(min(b) if b else 0):.0f}/{(max(b) if b else 0):.0f}"
        atxt = f"{len(a)}/{(st.median(a) if a else 0):.0f}/{(min(a) if a else 0):.0f}/{(max(a) if a else 0):.0f}"
        print(f"{wl:<13}{nj:>6}{btxt:>30}{atxt:>30}{f'{bs}/{len(b)} → {as_}/{len(a)}':>18}")

    # 分离"快态"（≥SLOW）做更干净的中位对比：底线态会污染中位。
    print("\n只看**非慢态**观测（避免底线态污染中位）：")
    print(f"{'workload':<13}{'njobs':>6}{f'{la} 快态 n/中位':>22}{f'{lb} 快态 n/中位':>22}{ratio_label:>11}")
    for wl, nj in KEY_POINTS:
        b = [v for v in groups.get((wl, nj, ga), []) if v >= SLOW]
        a = [v for v in groups.get((wl, nj, gb), []) if v >= SLOW]
        if not b or not a:
            continue
        mb, ma = st.median(b), st.median(a)
        print(f"{wl:<13}{nj:>6}{f'{len(b)}/{mb:.0f}':>22}{f'{len(a)}/{ma:.0f}':>22}{ma / mb:>10.2f}x")

    if by_flag:
        # 三臂：T0 off（无缓冲）/ T1 sync（缓冲 + 同步 flush）/ T2 async（缓冲 + 异步）
        print("\n三臂快态中位（同内核；T0=off 无缓冲、T1=sync、T2=async）：")
        print(f"{'workload':<13}{'njobs':>6}{'off(T0) n/中位':>20}{'sync(T1) n/中位':>18}"
              f"{'async(T2) n/中位':>19}{'T1/T0':>9}{'T2/T1':>9}")
        for wl, nj in KEY_POINTS:
            o = [v for v in groups.get((wl, nj, "off"), []) if v >= SLOW]
            s = [v for v in groups.get((wl, nj, "sync"), []) if v >= SLOW]
            a = [v for v in groups.get((wl, nj, "async"), []) if v >= SLOW]
            if not (o or s or a):
                continue
            mo = st.median(o) if o else 0.0
            ms = st.median(s) if s else 0.0
            ma = st.median(a) if a else 0.0
            r1 = f"{ms / mo:.2f}x" if o and s else "-"
            r2 = f"{ma / ms:.2f}x" if s and a else "-"
            print(f"{wl:<13}{nj:>6}{f'{len(o)}/{mo:.0f}':>20}{f'{len(s)}/{ms:.0f}':>18}"
                  f"{f'{len(a)}/{ma:.0f}':>19}{r1:>9}{r2:>9}")
        print("\n慢态（<50 MiB/s）计数 off / sync / async：")
        for wl, nj in KEY_POINTS:
            o = groups.get((wl, nj, "off"), [])
            s = groups.get((wl, nj, "sync"), [])
            a = groups.get((wl, nj, "async"), [])
            if not (o or s or a):
                continue
            sc = lambda v: sum(1 for x in v if x < SLOW)
            print(f"{wl:<13}{nj:>6}    {sc(o)}/{len(o)}   {sc(s)}/{len(s)}   {sc(a)}/{len(a)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
