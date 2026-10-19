#!/usr/bin/env python3
"""给某个 (device, fs, workload, mode) 打逐点观测表：各次取值 + 中位/最小/最大 + 矩阵单次值。

用法：
    python3 sweep_report.py nvme ext4 seqwrite-4k orderless [--all-arms] [--by-run]

默认只用**默认臂**（目录名无 `_njmb-`/`_njwb-` 后缀）的结果目录，避免把 A/B 的
对照臂混进来；`--all-arms` 则全部纳入并在每行标注臂。
"""

import glob
import os
import statistics as st
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import parse_baseline as pb  # noqa: E402

MATRIX_DIR = "20260921_180514_aster-ext4-baseline"


def arm_of(dirname):
    marker = "_aster-ext4-baseline"
    idx = dirname.find(marker)
    if idx < 0:
        return "default"
    return dirname[idx + len(marker):].lstrip("_") or "default"


def main():
    if len(sys.argv) < 5:
        print(__doc__)
        return 1
    dev, fs, wl, mode = sys.argv[1:5]
    all_arms = "--all-arms" in sys.argv
    by_run = "--by-run" in sys.argv
    if by_run:
        # 按批次列出各 njobs 的取值：用来区分「njobs 效应」与「批次/时间效应」——
        # 矩阵/各重复各是一条 run，若每条 run 内部也单调下降，则趋势是 njobs 的；
        # 若只是某些 run 整体低，则是批次效应。
        per_run = defaultdict(dict)
        for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
            name = os.path.basename(d)
            if not all_arms and arm_of(name) != "default":
                continue
            log = os.path.join(d, "logs", "baseline_run.log")
            if not os.path.exists(log):
                continue
            run = name.split("_aster-ext4-baseline")[0]
            for r in pb.parse(log):
                if (r["device"], r["fs"], r["workload"], r["mode"]) != (dev, fs, wl, mode):
                    continue
                if r["valid"] != "1":
                    continue
                per_run[run][int(r["njobs"])] = float(r["drain_bw_mib_s"])
        print(f"=== {dev}/{fs}/{wl}/{mode}：按批次（实际跑测顺序）===")
        for run in sorted(per_run):
            njs = sorted(per_run[run])
            seq = " ".join(f"nj{nj}={per_run[run][nj]:.0f}" for nj in njs)
            print(f"  {run}  ({len(njs)} 点)\n      {seq}")
        return 0

    obs = defaultdict(list)          # njobs -> [(arm, run, bw, write_mib, busy_ms, drain_ms)]
    matrix = {}
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        arm = arm_of(name)
        if not all_arms and arm != "default":
            continue
        log = os.path.join(d, "logs", "baseline_run.log")
        if not os.path.exists(log):
            continue
        run = name.split("_aster-ext4-baseline")[0]
        for r in pb.parse(log):
            if (r["device"], r["fs"], r["workload"], r["mode"]) != (dev, fs, wl, mode):
                continue
            if r["valid"] != "1":
                continue
            nj = int(r["njobs"])
            row = (arm, run, float(r["drain_bw_mib_s"]), float(r["write_mib"]),
                   float(r["busy_ms"]), float(r["drain_ms"]))
            obs[nj].append(row)
            if MATRIX_DIR in name:
                matrix[nj] = row[2]

    print(f"=== {dev}/{fs}/{wl}/{mode}（{'全部臂' if all_arms else '默认臂'}）===")
    print(f"{'njobs':>6}{'n':>4}{'中位':>9}{'最小':>9}{'最大':>9}{'矩阵单次':>10}   各次取值")
    for nj in sorted(obs):
        vals = [x[2] for x in obs[nj]]
        m = f"{matrix[nj]:9.1f}" if nj in matrix else f"{'--':>9}"
        seq = " ".join(f"{v:.1f}" for v in vals)
        print(f"{nj:>6}{len(vals):>4}{st.median(vals):>9.1f}{min(vals):>9.1f}{max(vals):>9.1f}{m}   {seq}")

    # 顺带给出"非慢态"中位（判双稳用）
    fast = [x[2] for nj in obs for x in obs[nj] if x[2] >= 50.0]
    slow = [x[2] for nj in obs for x in obs[nj] if x[2] < 50.0]
    if slow:
        print(f"\n慢态观测 {len(slow)} 次：{sorted(slow)}")
        print(f"非慢态中位 {st.median(fast):.1f} MiB/s（n={len(fast)}）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
