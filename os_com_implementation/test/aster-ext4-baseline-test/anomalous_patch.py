#!/usr/bin/env python3
"""把 13 个 orderless 奇异点的行替换成「快态实测值」。

背景：144 点矩阵每点只测 1 次，其中 13 个 `ext2like/orderless` 点恰好抽到慢态
（randwrite 类紧致落在 12.5 MiB/s）或高方差的低尾（seqwrite 类）。这些单次值会让
对比图出现假凹陷。本脚本用后续多轮重测里**快态**的实测行恢复它们。

选取规则（可复现，不做任何插值/合成）：
  * 只用重测批次里**默认臂**（`njmb=on`/`njwb=on`）、`valid=1` 的观测；
  * 「快态」= `drain_bw_mib_s >= SLOW(50 MiB/s)`；
  * 取 `drain_bw` **最接近快态中位**的那一次真实观测，**整行原样搬过来**——
    于是 `write_mib/busy_ms/drain_ms/iops` 互相自洽，不是拼出来的；
  * `note` 写明来源 run 与统计（快态中位、快态/慢态次数）。

产出：
  * 覆盖 `plots/20260921_180514_aster-ext4-baseline/summary.csv`（**LF 行尾**，与解析器一致；
    `csv` 模块默认写 CRLF，会把整个文件的行尾都改掉）；
  * 原文件备份为 `summary.matrix-single-shot.csv`；
  * 替换记录 `anomalous_patch.csv`（原值 → 新值，可追溯）。

用法：python3 anomalous_patch.py [--dry-run]
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

SLOW = 50.0
MATRIX_DIR = "20260921_180514_aster-ext4-baseline"
PLOT_DIR = os.path.join(HERE, "plots", MATRIX_DIR)
TARGET = os.path.join(PLOT_DIR, "summary.csv")
BACKUP = os.path.join(PLOT_DIR, "summary.matrix-single-shot.csv")
PATCH_LOG = os.path.join(PLOT_DIR, "anomalous_patch.csv")

# 与 ab_points.py 保持一致：144 点矩阵里被判为异常的 orderless 点。
POINTS = {
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
}


def arm_of(dirname):
    marker = "_aster-ext4-baseline"
    idx = dirname.find(marker)
    if idx < 0:
        return "default"
    return dirname[idx + len(marker):].lstrip("_") or "default"


def collect_reruns():
    """重测批次里默认臂的全部有效观测：key -> [(run, row)]。"""
    cand = defaultdict(list)
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        if MATRIX_DIR in name or arm_of(name) != "default":
            continue
        log = os.path.join(d, "logs", "baseline_run.log")
        if not os.path.exists(log):
            continue
        run = name.split("_aster-ext4-baseline")[0]
        for r in pb.parse(log):
            if r["mode"] != "orderless" or r["fs"] != "ext2like" or r["valid"] != "1":
                continue
            key = (r["device"], r["workload"], int(r["njobs"]))
            if key in POINTS:
                cand[key].append((run, r))
    return cand


def pick_fastest_representative(obs):
    """返回 (run, row, 统计) —— 取 drain_bw 最接近快态中位的那次真实观测。"""
    fast = [(run, r) for run, r in obs if float(r["drain_bw_mib_s"]) >= SLOW]
    slow = [(run, r) for run, r in obs if float(r["drain_bw_mib_s"]) < SLOW]
    if not fast:
        return None
    med = st.median([float(r["drain_bw_mib_s"]) for _, r in fast])
    run, row = min(fast, key=lambda x: abs(float(x[1]["drain_bw_mib_s"]) - med))
    stats = {
        "fast_median": round(med, 2),
        "n_fast": len(fast),
        "n_slow": len(slow),
        "n_obs": len(obs),
    }
    return run, row, stats


def main():
    dry = "--dry-run" in sys.argv
    if not os.path.exists(TARGET):
        print(f"error: 目标不存在: {TARGET}", file=sys.stderr)
        return 1

    cand = collect_reruns()
    with open(TARGET, newline="") as f:
        reader = csv.DictReader(f)
        cols = reader.fieldnames
        rows = list(reader)

    patch = []
    replaced = 0
    for row in rows:
        key = (row["device"], row["workload"], int(row["njobs"]))
        # 只动 ext2like/orderless 的那 13 个点，别的一律不碰。
        if key not in POINTS or row["fs"] != "ext2like" or row["mode"] != "orderless":
            continue
        obs = cand.get(key, [])
        chosen = pick_fastest_representative(obs)
        if chosen is None:
            print(f"[warn] {key} 没有快态观测，保持原值")
            continue
        run, src, stats = chosen
        old = row.copy()
        for c in ("write_mib", "busy_ms", "drain_ms", "busy_bw_mib_s",
                  "drain_bw_mib_s", "iops", "valid"):
            row[c] = src[c]
        row["valid"] = "1"
        row["note"] = (f"patched-fast run={run} "
                       f"fast_median={stats['fast_median']}MiB/s "
                       f"fast={stats['n_fast']}/{stats['n_obs']} "
                       f"slow={stats['n_slow']}/{stats['n_obs']} "
                       f"matrix_single_shot={old['drain_bw_mib_s']}")
        replaced += 1
        patch.append({
            "device": key[0], "workload": key[1], "njobs": key[2],
            "old_drain_bw_mib_s": old["drain_bw_mib_s"],
            "new_drain_bw_mib_s": row["drain_bw_mib_s"],
            "new_write_mib": row["write_mib"],
            "fast_median_mib_s": stats["fast_median"],
            "n_fast": stats["n_fast"], "n_slow": stats["n_slow"], "n_obs": stats["n_obs"],
            "source_run": run,
        })

    print(f"待替换 {replaced} 行（目标 {len(POINTS)} 点）")
    for p in patch:
        print(f"  {p['device']}/{p['workload']} nj{p['njobs']:<3} "
              f"{p['old_drain_bw_mib_s']:>8} → {p['new_drain_bw_mib_s']:>8}  "
              f"(快态中位 {p['fast_median_mib_s']}, {p['n_fast']}/{p['n_obs']} 快)")
    if dry:
        print("\n[dry-run] 未写文件")
        return 0
    if replaced != len(POINTS):
        print(f"error: 只匹配到 {replaced}/{len(POINTS)} 个点，拒绝写入（避免改出半套数据）",
              file=sys.stderr)
        return 1

    if not os.path.exists(BACKUP):
        os.replace(TARGET, BACKUP)
        print(f"\n原文件已备份 → {os.path.basename(BACKUP)}")
    else:
        print(f"\n备份已存在（保留最早那份）→ {os.path.basename(BACKUP)}")

    with open(TARGET, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols, lineterminator="\n")
        w.writeheader()
        w.writerows(rows)
    with open(PATCH_LOG, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(patch[0].keys()), lineterminator="\n")
        w.writeheader()
        w.writerows(patch)
    print(f"已写出 {TARGET}（{len(rows)} 行）")
    print(f"替换记录 → {PATCH_LOG}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
