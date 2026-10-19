#!/usr/bin/env python3
"""把「144 点矩阵里的 orderless 异常点」在所有批次里的观测汇总出来。

回答两个问题：
  1. 这些点**快起来是多少**（同一配置的重复观测里，非慢态的取值）；
  2. 慢态到底是什么——是每个点的固有属性，还是**与 njobs 相关的偶发状态**。

数据来源：`results/` 下所有 `*_aster-ext4-baseline*` 目录（含最初的 144 点矩阵单次测量、
各批 A/B 的多次重复、以及补齐批次），逐轮解析。

臂的区分：
  * `default`（目录名无后缀）= 默认配置（njmb=on/njwb=on）；
  * `njmb-off` = 关掉方案 A（不缓冲）→ **未合并底线**，单独列出，不混进"快态"统计；
  * `njmb-global` / `njwb-off` = 变体臂（已验证快态与默认等价）。

用法：python3 ab_points.py [结果目录 ...]
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

SLOW = 50.0  # MiB/s；慢态紧致落在 12-14，快态 200-300，中间无观测值

# 144 点矩阵里被判为异常的 orderless 点（device, workload, njobs）
ANOMALOUS = [
    ("sata", "randwrite-4k", 20),
    ("sata", "seqwrite-4k", 4),
    ("sata", "seqwrite-4k", 16),
    ("sata", "seqwrite-4k", 28),
    ("sata", "seqwrite-4k", 32),
    ("nvme", "randwrite-4k", 12),
    ("nvme", "randwrite-4k", 16),
    ("nvme", "randwrite-4k", 20),
    ("nvme", "randwrite-4k", 28),
    ("nvme", "seqwrite-4k", 16),
    ("nvme", "seqwrite-4k", 24),
    ("nvme", "seqwrite-4k", 28),
    ("nvme", "seqwrite-4k", 32),
]

MATRIX_DIR = "20260921_180514_aster-ext4-baseline"

# CSV 输出（写在测试目录里——`results/` 属 root，宿主用户建不了新文件）。
OBS_CSV = os.path.join(HERE, "anomalous_points_observations.csv")
SUM_CSV = os.path.join(HERE, "anomalous_points_summary.csv")


def _run_id(dirname):
    """目录名里的时间戳前缀（≡ 一次 run），用来区分观测来自哪一批。"""
    idx = dirname.find("_aster-ext4-baseline")
    return dirname[:idx] if idx > 0 else dirname


def arm_of(dirname):
    marker = "_aster-ext4-baseline"
    idx = dirname.find(marker)
    if idx < 0:
        return "default"
    return dirname[idx + len(marker):].lstrip("_") or "default"


def main():
    dirs = sys.argv[1:] or sorted(
        glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))
    )

    # (dev, wl, nj) -> arm -> [bw, ...]；同时记 144 点矩阵的单次值
    obs = defaultdict(lambda: defaultdict(list))
    matrix_value = {}
    for d in dirs:
        log = os.path.join(d, "logs", "baseline_run.log")
        if not os.path.exists(log):
            continue
        arm = arm_of(os.path.basename(d))
        is_matrix = MATRIX_DIR in os.path.basename(d)
        for r in pb.parse(log):
            if r["mode"] != "orderless" or r["fs"] != "ext2like":
                continue
            if r["valid"] != "1":
                continue
            key = (r["device"], r["workload"], int(r["njobs"]))
            v = float(r["drain_bw_mib_s"])
            obs[key][arm].append(v)
            if is_matrix:
                matrix_value[key] = v

    print("=" * 100)
    print("一、12 个异常点：所有重复观测（MiB/s）。慢态 = < %g" % SLOW)
    print("=" * 100)
    print(f"{'device/workload':<20}{'njobs':>6}{'矩阵单次':>9}   {'默认臂各次观测':<34}{'慢态':>7}{'快态中位':>9}")
    for dev, wl, nj in ANOMALOUS:
        key = (dev, wl, nj)
        vals = obs.get(key, {}).get("default", [])
        fast = [v for v in vals if v >= SLOW]
        slow = len(vals) - len(fast)
        mv = matrix_value.get(key)
        seq = " ".join(f"{v:6.1f}" for v in vals) if vals else "(无重测数据)"
        med = f"{st.median(fast):8.1f}" if fast else "     --"
        print(f"{dev + '/' + wl:<20}{nj:>6}{('' if mv is None else f'{mv:8.1f}'):>9}   {seq:<34}{f'{slow}/{len(vals)}':>7}{med:>9}")

    print()
    print("二、慢态率随 njobs（默认臂，只看 nvme/randwrite-4k 与 sata/seqwrite-4k 两个连续序列）")
    print("   用来判「慢态是不是每点固有」：若是固有属性，某些点应恒慢；")
    print("   若与 njobs 相关，则会看到某个 njobs 以下从不慢、以上开始偶发。")
    print("-" * 100)
    for dev, wl in [("nvme", "randwrite-4k"), ("sata", "seqwrite-4k"), ("sata", "randwrite-4k")]:
        print(f"\n[{dev}/{wl}]")
        for nj in sorted({k[2] for k in obs if k[0] == dev and k[1] == wl}):
            vals = obs[(dev, wl, nj)].get("default", [])
            if not vals:
                continue
            nslow = sum(1 for v in vals if v < SLOW)
            fast = [v for v in vals if v >= SLOW]
            fmed = f"{st.median(fast):7.1f}" if fast else "     --"
            print(f"  nj{nj:<3} 慢态 {nslow}/{len(vals)}   快态中位 {fmed}   全部: "
                  + " ".join(f"{v:.1f}" for v in vals))

    print()
    print("三、方案 A 关掉（njmb-off）的观测：这条是「未合并底线」，不参与上面统计")
    print("-" * 100)
    for dev, wl in [("nvme", "randwrite-4k"), ("sata", "seqwrite-4k")]:
        for nj in sorted({k[2] for k in obs if k[0] == dev and k[1] == wl}):
            vals = obs[(dev, wl, nj)].get("njmb-off", [])
            if vals:
                print(f"  [{dev}/{wl}] nj{nj:<3} " + " ".join(f"{v:7.1f}" for v in vals))

    write_csvs(obs, matrix_value, dirs)
    return 0


def write_csvs(obs, matrix_value, dirs):
    """把 13 个异常点写成两份 CSV：逐次观测（长表）+ 每点汇总（宽表）。"""
    # 逐次观测：需要知道每条观测属于哪个 run / 哪个臂，所以重扫一遍目录。
    point_set = set(ANOMALOUS)
    rows = []
    for d in dirs:
        log = os.path.join(d, "logs", "baseline_run.log")
        if not os.path.exists(log):
            continue
        name = os.path.basename(d)
        arm = arm_of(name)
        source = "matrix" if MATRIX_DIR in name else "rerun"
        idx = 0
        for r in pb.parse(log):
            if r["mode"] != "orderless" or r["fs"] != "ext2like":
                continue
            key = (r["device"], r["workload"], int(r["njobs"]))
            if key not in point_set:
                continue
            bw = float(r["drain_bw_mib_s"])
            idx += 1
            rows.append({
                "device": key[0], "workload": key[1], "njobs": key[2],
                "arm": arm, "source": source, "run": _run_id(name),
                "obs_in_run": idx,
                "valid": r["valid"],
                "drain_bw_mib_s": bw,
                "write_mib": float(r["write_mib"]),
                "is_slow": "1" if (r["valid"] == "1" and bw < SLOW) else "0",
                "note": r["note"],
            })
    with open(OBS_CSV, "w", newline="") as f:
        cols = ["device", "workload", "njobs", "arm", "source", "run", "obs_in_run",
                "valid", "drain_bw_mib_s", "write_mib", "is_slow", "note"]
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        for row in sorted(rows, key=lambda x: (x["device"], x["workload"], x["njobs"], x["run"], x["obs_in_run"])):
            w.writerow(row)

    # 每点汇总：只统计默认臂的重测（矩阵单次值与 njmb-off 底线单独列）。
    per_point = defaultdict(list)
    for row in rows:
        if row["arm"] == "default" and row["source"] == "rerun" and row["valid"] == "1":
            per_point[(row["device"], row["workload"], row["njobs"])].append(row["drain_bw_mib_s"])
    with open(SUM_CSV, "w", newline="") as f:
        cols = ["device", "workload", "njobs", "matrix_single_mib_s", "n_obs", "n_slow",
                "slow_ratio", "fast_median_mib_s", "fast_min_mib_s", "fast_max_mib_s",
                "fast_values", "slow_values"]
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        for dev, wl, nj in ANOMALOUS:
            vals = per_point.get((dev, wl, nj), [])
            fast = [v for v in vals if v >= SLOW]
            slow = [v for v in vals if v < SLOW]
            w.writerow({
                "device": dev, "workload": wl, "njobs": nj,
                "matrix_single_mib_s": matrix_value.get((dev, wl, nj), ""),
                "n_obs": len(vals), "n_slow": len(slow),
                "slow_ratio": round(len(slow) / len(vals), 3) if vals else "",
                "fast_median_mib_s": round(st.median(fast), 1) if fast else "",
                "fast_min_mib_s": round(min(fast), 1) if fast else "",
                "fast_max_mib_s": round(max(fast), 1) if fast else "",
                "fast_values": " ".join(f"{v:.1f}" for v in fast),
                "slow_values": " ".join(f"{v:.1f}" for v in slow),
            })
    print()
    print(f"CSV 已写出：\n  {OBS_CSV}  ({len(rows)} 行逐次观测)\n  {SUM_CSV}  ({len(ANOMALOUS)} 行汇总)")


if __name__ == "__main__":
    sys.exit(main())
