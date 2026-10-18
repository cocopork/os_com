#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""把坏点重测结果择优回填进 summary.csv（并留下完整审计表）。

用法:
    python3 merge_rerun.py <summary.before-rerun.csv> <rerun_dir> <summary.csv 输出>

择优规则（必须写清楚，否则"补数"就变成编数）：
  1. 坏点的每个样本分三类：
       valid     = valid==1 且 busy_bw > LOW_BAND
       low_band  = 跑完了但 busy_bw <= LOW_BAND（背压/静默卡死态）
       invalid   = 缺 DRAIN / timeout
  2. 只要有 valid 样本 ⇒ 取其中 **busy_bw 最大**的一轮整行（理由：坏轮是"卡死态"
     而不是稳态，取最大 = 取没被污染的那一轮；分布是双峰的，取中位数会被坏轮拉住）。
  3. 一个 valid 都没有 ⇒ **保留母批次原值**，note 记 `rerun x3 all-low/invalid [...]`。
     这种情况说明该点在当前配置下是**稳态低**，不是噪声 —— 不能靠补数掩盖，
     要换配置（--wr-capacity-mb 下调到 NV 有效容量以下）整批重跑。
  4. note 里永远保留三样本原始值，便于事后复核。

产物：
  <summary.csv 输出>   回填后的完整表（列序不变）
  <rerun_dir>/rerun_detail.csv   审计表：每个坏点的三类样本与最终选择
"""

import csv
import glob
import os
import sys

LOW_BAND = 40.0  # MiB/s 以下视为"低带"（母批次里正常档位是 100–270）

COLS = ["device", "fs", "workload", "mode", "njobs", "write_mib", "busy_ms",
        "drain_ms", "busy_bw_mib_s", "drain_bw_mib_s", "iops", "valid", "note"]


def key(row):
    return (row["device"], row["fs"], row["workload"], row["mode"], int(row["njobs"]))


def as_float(row, col):
    try:
        return float(row[col])
    except (KeyError, ValueError, TypeError):
        return 0.0


def classify(row):
    if row.get("valid") != "1" or as_float(row, "busy_bw_mib_s") <= 0.0:
        return "invalid"
    if as_float(row, "busy_bw_mib_s") <= LOW_BAND:
        return "low_band"
    return "valid"


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        return 2
    base_path, rerun_dir, out_path = sys.argv[1:4]
    if not os.path.exists(base_path):
        print(f"error: {base_path} 不存在", file=sys.stderr)
        return 2

    with open(base_path, newline="") as fh:
        base_rows = list(csv.DictReader(fh))
    base = {key(r): r for r in base_rows}
    order = [key(r) for r in base_rows]

    # 收集重测样本：**只认 `*_rep*.csv`**；否则 `rerun_detail.csv`（上一次跑本脚本的
    # 产物）/ 手工放进去的 `base.csv` 会被当成样本吸进来（样本数变 4，回填值失真）。
    samples = {}
    for path in sorted(glob.glob(os.path.join(rerun_dir, "*_rep*.csv"))):
        with open(path, newline="") as fh:
            for row in csv.DictReader(fh):
                if set(row) != set(COLS):
                    continue
                k = key(row)
                if k not in base:
                    continue
                samples.setdefault(k, []).append((os.path.basename(path), row))

    if not samples:
        print("error: rerun 目录里没有可用样本", file=sys.stderr)
        return 2

    patched = dict(base)
    detail = []
    for k, lst in sorted(samples.items()):
        vals = [as_float(r, "busy_bw_mib_s") for _, r in lst]
        cls = [classify(r) for _, r in lst]
        valid_idx = [i for i, c in enumerate(cls) if c == "valid"]
        srcs = ", ".join(f"{basename.replace('.csv','')}={v:.1f}({c})"
                         for (basename, _), v, c in zip(lst, vals, cls))
        if valid_idx:
            pick = max(valid_idx, key=lambda i: vals[i])
            row = dict(lst[pick][1])
            tag = "best-of-3" if len(valid_idx) > 1 else "only-valid"
            row["note"] = f"rerun x{RUNS_LABEL} {tag} [{srcs}]"
            patched[k] = row
            chosen = vals[pick]
        else:
            row = dict(base[k])
            row["note"] = (base[k].get("note", "") +
                           f" | rerun x{RUNS_LABEL} 全为 low/invalid [{srcs}]，保留原值").strip(" |")
            patched[k] = row
            chosen = as_float(base[k], "busy_bw_mib_s")
        detail.append({
            "device": k[0], "fs": k[1], "workload": k[2], "njobs": k[4],
            "samples": "/".join(f"{v:.1f}" for v in vals),
            "classes": "/".join(cls),
            "chosen_busy_bw": f"{chosen:.2f}",
            "source": "best-of-3" if valid_idx else "kept-original(base)",
        })

    with open(out_path, "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=COLS)
        w.writeheader()
        for k in order:
            w.writerow(patched[k])

    detail_path = os.path.join(rerun_dir, "rerun_detail.csv")
    with open(detail_path, "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=["device", "fs", "workload", "njobs",
                                           "samples", "classes", "chosen_busy_bw", "source"])
        w.writeheader()
        w.writerows(detail)

    print(f"[merge] 回填 {len(detail)} 个坏点 → {out_path}")
    print(f"[merge] 审计表 → {detail_path}\n")
    hdr = f"  {'device':6s} {'fs':13s} {'workload':13s} {'nj':>3s}  {'三个样本':22s} {'类别':28s} {'采用':>8s} 来源"
    print(hdr)
    print("  " + "-" * (len(hdr) - 2))
    for d in detail:
        print(f"  {d['device']:6s} {d['fs']:13s} {d['workload']:13s} {d['njobs']:>3d}  "
              f"{d['samples']:22s} {d['classes']:28s} {d['chosen_busy_bw']:>8s} {d['source']}")

    kept = [d for d in detail if d["source"].startswith("kept")]
    if kept:
        print(f"\n⚠ {len(kept)} 个点 3 轮全在低带/无效 —— **这批不能用补数掩盖**：")
        for d in kept:
            print(f"   {d['device']}/{d['fs']}/{d['workload']}/nj{d['njobs']}  样本={d['samples']}")
        print("   说明该点在当前配置（wr-capacity 64 > NV 有效容量 ~32 MiB）下是稳态低，")
        print("   应下调 --wr-capacity-mb（建议 16）后整批重跑，而不是逐点补。")
    return 0


RUNS_LABEL = 3


if __name__ == "__main__":
    sys.exit(main())
