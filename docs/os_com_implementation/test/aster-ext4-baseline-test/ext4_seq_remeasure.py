#!/usr/bin/env python3
"""生成 `nvme/ext4/seqwrite-4k/orderless` 的重测对照 CSV。

排除**不可比批次** `20260921_065946`（SMP=24、10 轮被 timeout 杀掉、参数与后续不同），
其余批次（含 2 条倒序批次）全部纳入。

输出：`plots/<run>/nvme-ext4-seqwrite-remeasure.csv`
列：njobs, n_obs, median_mib_s, min_mib_s, max_mib_s, matrix_single_mib_s, values_mib_s

注意：**不修改** `summary.csv`。矩阵里的 ext4 seqwrite 值本身已接近各点最优值
（1862/1845/.../1170 ≈ best-of-N），所以“消除下降”只能靠伪造——这里只提供对照数据。
"""

import csv
import glob
import os
import statistics as st
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import parse_baseline as pb  # noqa: E402

MATRIX_DIR = "20260921_180514_aster-ext4-baseline"
EXCLUDE = {"20260921_065946"}  # 不可比批次
OUT = os.path.join(HERE, "plots", MATRIX_DIR, "nvme-ext4-seqwrite-remeasure.csv")
TARGET = ("nvme", "ext4", "seqwrite-4k", "orderless")


def main():
    obs = defaultdict(list)
    matrix = {}
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        if "_njmb-" in name or "_njwb-" in name:
            continue
        run = name.split("_aster-ext4-baseline")[0]
        if run in EXCLUDE:
            continue
        log = os.path.join(d, "logs", "baseline_run.log")
        if not os.path.exists(log):
            continue
        for r in pb.parse(log):
            if (r["device"], r["fs"], r["workload"], r["mode"]) != TARGET:
                continue
            if r["valid"] != "1":
                continue
            nj = int(r["njobs"])
            v = float(r["drain_bw_mib_s"])
            obs[nj].append(v)
            if MATRIX_DIR in name:
                matrix[nj] = v

    rows = []
    print(f"{'njobs':>6}{'n':>4}{'中位':>9}{'最小':>9}{'最大':>9}{'矩阵单次':>10}")
    for nj in sorted(obs):
        vals = obs[nj]
        rows.append({
            "njobs": nj,
            "n_obs": len(vals),
            "median_mib_s": round(st.median(vals), 1),
            "min_mib_s": round(min(vals), 1),
            "max_mib_s": round(max(vals), 1),
            "matrix_single_mib_s": matrix.get(nj, ""),
            "values_mib_s": " ".join(f"{v:.1f}" for v in vals),
        })
        print(f"{nj:>6}{len(vals):>4}{st.median(vals):>9.1f}{min(vals):>9.1f}{max(vals):>9.1f}"
              f"{matrix.get(nj, 0):>10.1f}   {rows[-1]['values_mib_s']}")

    with open(OUT, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()), lineterminator="\n")
        w.writeheader()
        w.writerows(rows)
    print(f"\n已写出 → {OUT}")
    print("（summary.csv 未改动；矩阵值≈best-of-N，下降趋势为真实结果）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
