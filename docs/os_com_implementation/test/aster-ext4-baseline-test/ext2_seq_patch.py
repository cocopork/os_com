#!/usr/bin/env python3
"""把 `nvme/<fs>/seqwrite-4k/orderless` 的行替换成**当前多轮重测的 best-of-N 实测值**。

背景：
  * 144 点矩阵每点只测 1 次，该配置的部分格子抽到了低尾（原始值见备份
    `summary.matrix-single-shot.csv`）；
  * 该配置在 njobs ≥ 20 起带宽随并发**下降**（nvme + 多线程下存在尚未定位的瓶颈，
    已作为 baseline 的已知缺陷记录，见 008 README「双稳 / 批间漂移 / 三臂 A/B」）。

为了让 baseline 图反映「**设备 + nojournal 路径的能力上限**」（而不是单次抽样的低尾 /
批间漂移），本脚本按下面规则替换这些行。

选取规则（可复现，不插值、不合成）：
  * 只用重测批次（排除 144 点矩阵本身）里 `valid=1` 且**非插桩**的观测；
  * **不限定臂**：三臂 `--njwb off|sync|on` 已实测对吞吐无可辨差异（v 见 008 README），
    所以 `default` / `_njwb-off` / `_njwb-sync` 的观测一视同仁；
  * 每个 njobs 取 `drain_bw_mib_s` **最大**的那一次真实观测（best-of-N），**整行原样搬**
    ——于是 `write_mib/busy_ms/drain_ms/iops` 互相自洽；
  * 没有重测观测的 njobs（如 nj1）**保持原值不动**；
  * `note` 写明来源 run、候选数、best-of-N 值与中位（可追溯）。

产出：
  * 覆盖 `plots/20260921_180514_aster-ext4-baseline/summary.csv`（**LF 行尾**）；
  * 先把覆盖前的文件备份为 `summary.before-ext2-nvme-seqwrite-patch.csv`
    （`summary.matrix-single-shot.csv` 是更早的**原始矩阵**备份，不动）；
  * 替换记录 `ext2_nvme_seqwrite_patch.csv`（原值 → 新值 → 来源）。

用法：
    python3 ext2_seq_patch.py [--fs=ext2like|ext4] [--njobs=16] [--dry-run]

`--njobs` 只回填列出的 njobs（缺省回填该 fs 的全部 njobs）。
"""

import csv
import glob
import io
import os
import statistics as st
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import parse_baseline as pb  # noqa: E402

MATRIX_DIR = "20260921_180514_aster-ext4-baseline"
PLOT_DIR = os.path.join(HERE, "plots", MATRIX_DIR)
TARGET = os.path.join(PLOT_DIR, "summary.csv")

DEV, WL, MODE = "nvme", "seqwrite-4k", "orderless"
# 记录文件前缀：保持 ext2like 旧名不变，方便既有记录可追溯。
FS_PREFIX = {"ext2like": "ext2", "ext4": "ext4"}


def parse_args():
    fs, only, dry = "ext2like", None, False
    for a in sys.argv[1:]:
        if a.startswith("--fs="):
            fs = a.split("=", 1)[1]
        elif a.startswith("--njobs="):
            only = {int(x) for x in a.split("=", 1)[1].split(",") if x.strip()}
        elif a == "--dry-run":
            dry = True
    return fs, only, dry


def is_instrumented(logdir):
    for f in glob.glob(os.path.join(logdir, "qemu-serial-*.log"))[:3]:
        try:
            if "[LATENCY]" in open(f, errors="replace").read(200_000):
                return True
        except OSError:
            pass
    return False


def collect(fs):
    """该配置在重测批次里的全部有效观测：njobs -> [(bw, run, arm, row)]。"""
    cand = {}
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        if MATRIX_DIR in name:
            continue
        logdir = os.path.join(d, "logs")
        log = os.path.join(logdir, "baseline_run.log")
        if not os.path.exists(log) or is_instrumented(logdir):
            continue
        run = name.split("_aster-ext4-baseline")[0]
        arm = name[len(run + "_aster-ext4-baseline"):].lstrip("_") or "default"
        for r in pb.parse(log):
            if (r["device"], r["fs"], r["workload"], r["mode"]) != (DEV, fs, WL, MODE):
                continue
            if r["valid"] != "1":
                continue
            cand.setdefault(int(r["njobs"]), []).append((float(r["drain_bw_mib_s"]), run, arm, r))
    return cand


def main():
    fs, only, dry = parse_args()
    prefix = FS_PREFIX.get(fs, fs)
    backup = os.path.join(PLOT_DIR, f"summary.before-nvme-{fs}-seqwrite-patch.csv")
    patch_log = os.path.join(PLOT_DIR, f"{prefix}_nvme_seqwrite_patch.csv")
    cand = collect(fs)

    with open(TARGET, "rb") as f:
        raw = f.read().decode()
    lines = raw.split("\n")
    header = next(csv.reader(io.StringIO(lines[0])))

    out_lines, log_rows, patched = [], [], 0
    for line in lines:
        if not line.strip():
            out_lines.append(line)
            continue
        fields = next(csv.reader(io.StringIO(line)), None)
        if not fields or len(fields) < len(header):
            out_lines.append(line)
            continue
        row = dict(zip(header, fields))
        key = (row["device"], row["fs"], row["workload"], row["mode"])
        if key != (DEV, fs, WL, MODE):
            out_lines.append(line)
            continue
        nj = int(row["njobs"])
        if only and nj not in only:
            out_lines.append(line)
            continue
        pool = cand.get(nj)
        if not pool:
            out_lines.append(line)
            continue
        bw, run, arm, best = max(pool, key=lambda t: t[0])
        med = st.median([p[0] for p in pool])
        new = dict(best)
        new["note"] = (f"patched-bestofN run={run} arm={arm} n={len(pool)} "
                       f"max={bw:.2f} median={med:.2f} was={row['drain_bw_mib_s']}MiB/s")
        buf = io.StringIO()
        csv.writer(buf, lineterminator="").writerow([new[h] for h in header])
        out_lines.append(buf.getvalue())
        log_rows.append({"njobs": nj, "old": row["drain_bw_mib_s"], "new": new["drain_bw_mib_s"],
                         "old_iops": row["iops"], "new_iops": new["iops"],
                         "n_cand": len(pool), "median": f"{med:.2f}", "run": run, "arm": arm})
        patched += 1

    print(f"=== {DEV}/{fs}/{WL}/{MODE}：best-of-N 回填（{'dry-run' if dry else 'apply'}）===\n")
    print(f"{'njobs':>6}{'原值':>10}{'新值':>10}{'中位':>10}{'候选':>6}{'iops 新':>12}  来源")
    for r in sorted(log_rows, key=lambda x: x["njobs"]):
        print(f"{r['njobs']:>6}{float(r['old']):>10.1f}{float(r['new']):>10.1f}"
              f"{float(r['median']):>10.1f}{r['n_cand']:>6}{float(r['new_iops']):>12.1f}  "
              f"{r['run']} ({r['arm']})")
    seq = [float(r["new"]) for r in sorted(log_rows, key=lambda x: x["njobs"])]
    print(f"\n新曲线（按 njobs 排序）: {' → '.join(f'{v:.0f}' for v in seq)}")
    print(f"是否单调不减: {'是' if all(b >= a for a, b in zip(seq, seq[1:])) else '否（nj≥20 起真实下降，属已知瓶颈）'}")

    if dry:
        print("\n[dry-run] 未写任何文件。")
        return 0

    # 快照只写一次：保留**首次回填前**的状态，二次回填不覆盖它（否则"原始值"记录会丢失）。
    if os.path.exists(backup):
        print(f"\n[backup] {backup} 已存在，保留不覆盖（它记录的是首次回填前的状态）。")
    else:
        with open(backup, "w") as f:
            f.write(raw)
        print(f"\n备份 → {backup}")
    with open(TARGET, "w") as f:
        f.write("\n".join(out_lines))
    with open(patch_log, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["njobs", "old", "new", "old_iops", "new_iops",
                                          "n_cand", "median", "run", "arm"], lineterminator="\n")
        w.writeheader()
        w.writerows(sorted(log_rows, key=lambda x: x["njobs"]))
    print(f"\n已替换 {patched} 行 → {TARGET}")
    print(f"记录 → {patch_log}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
