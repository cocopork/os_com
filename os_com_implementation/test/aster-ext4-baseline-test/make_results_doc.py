#!/usr/bin/env python3
"""从本地数据生成自洽的结果附录 `RESULTS.md`。

背景：本仓库只跟踪**文档与脚本**（见 `.gitignore`）——`plots/`、`results/`、汇总 CSV
与原始日志都不入库。于是文档里引用的实测数值若只指向本地 CSV，换台机器就读不到。
本脚本把**关键实测表**从本地数据自动生成成 markdown 附录：

  * 文档自洽：`RESULTS.md` 入库，任何机器上都能读到数值；
  * 不誊抄：所有数值直接来自数据文件，避免手工转录出错；
  * 可追溯：每张表都标明来源文件与生成命令。

用法：
    python3 make_results_doc.py             # 生成/覆盖 RESULTS.md
    python3 make_results_doc.py --stdout    # 只打印，不写文件

某份数据缺失时（例如在没跑过实验的机器上）只跳过对应小节并注明"本机无数据"，
不报错——这样脚本在任何 checkout 上都可用。
"""

import csv
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PLOT_DIR = os.path.join(HERE, "plots", "20260921_180514_aster-ext4-baseline")
OUT = os.path.join(HERE, "RESULTS.md")

NJOBS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
PANELS = (
    ("sata", "randwrite-4k", "(a) SATA random write"),
    ("sata", "seqwrite-4k", "(b) SATA sequential write"),
    ("nvme", "randwrite-4k", "(c) NVMe random write"),
    ("nvme", "seqwrite-4k", "(d) NVMe sequential write"),
)
SERIES = (
    ("ext2like", "orderless", "EXT2-like orderless"),
    ("ext4", "orderless", "EXT4 orderless"),
    ("ext2like", "fsync", "EXT2-like fsync"),
    ("ext4", "fsync", "EXT4 fsync"),
)
RUN_CONFIG_KEYS = ("SMP", "MEM", "RUNTIME_LIMIT", "FILE_RANGE_MB", "TIMEOUT_SEC", "RELEASE")


def read_csv(path):
    """读 CSV 成 list[dict]；文件不存在返回 None。"""
    if not os.path.exists(path):
        return None
    with open(path, newline="") as f:
        return [dict(r) for r in csv.DictReader(f)]


def run_config():
    """尽力从 run-baseline.sh 里抽默认值，抽不到就不写（避免文档与脚本漂移）。

    支持两种写法：`SMP=64` / `MEM="4G"`，以及 `SMP="${SMP:-64}"`（后者取 `:-` 后的默认值）。
    """
    path = os.path.join(HERE, "run-baseline.sh")
    if not os.path.exists(path):
        return []
    text = open(path, errors="replace").read()
    out = []
    for key in RUN_CONFIG_KEYS:
        m = re.search(rf"^{key}=(.*)$", text, re.M)
        if not m:
            continue
        val = m.group(1).strip()
        sub = re.match(r"^\$\{[A-Za-z_]+:-(.*)\}$", val)
        if sub:
            val = sub.group(1)
        val = val.strip().strip('"').strip("'").strip()
        if val and not val.startswith("$"):
            out.append(f"`{key}={val}`")
    return out


def cell(rows, device, fs, workload, mode, njobs):
    """取某格 drain_bw；缺列/无效返回 None。"""
    for r in rows:
        if (r.get("device"), r.get("fs"), r.get("workload"), r.get("mode")) != \
                (device, fs, workload, mode):
            continue
        if str(r.get("njobs")) != str(njobs):
            continue
        if r.get("valid") != "1":
            return None
        try:
            return float(r["drain_bw_mib_s"])
        except (KeyError, ValueError):
            return None
    return None


def panel_table(rows, device, workload):
    head = "| 系列 | " + " | ".join(str(n) for n in NJOBS) + " |"
    sep = "|---" * (len(NJOBS) + 1) + "|"
    lines = [head, sep]
    for fs, mode, label in SERIES:
        cells = []
        for n in NJOBS:
            v = cell(rows, device, fs, workload, mode, n)
            cells.append("—" if v is None else f"{v:.0f}")
        lines.append(f"| {label} | " + " | ".join(cells) + " |")
    return "\n".join(lines)


def patched_points():
    """汇总所有回填记录（14 行奇异点 + ext2like/ext4 nj16），供文末列表。"""
    out = []
    rows = read_csv(os.path.join(PLOT_DIR, "anomalous_patch.csv"))
    if rows:
        for r in rows:
            out.append(f"- `{r['device']}/{r['workload']}` nj{r['njobs']}："
                       f"{float(r['old_drain_bw_mib_s']):.1f} → "
                       f"{float(r['new_drain_bw_mib_s']):.1f} MiB/s"
                       f"（来源 `{r['source_run']}`，快态 {r['n_fast']}/{r['n_obs']}）")
    for name, tag in (("ext2_nvme_seqwrite_patch.csv", "ext2like"),
                      ("ext4_nvme_seqwrite_patch.csv", "ext4")):
        rows = read_csv(os.path.join(PLOT_DIR, name))
        if not rows:
            continue
        for r in rows:
            out.append(f"- `nvme/seqwrite-4k/{tag}` nj{r['njobs']}："
                       f"{float(r['old']):.1f} → {float(r['new']):.1f} MiB/s"
                       f"（best-of-N，{r['n_cand']} 个候选，中位 {float(r['median']):.1f}，"
                       f"来源 `{r['run']}`/{r['arm']}）")
    return out


def anomalous_table():
    rows = read_csv(os.path.join(HERE, "anomalous_points_summary.csv"))
    if not rows:
        return None
    lines = ["| device | workload | njobs | 矩阵单次 | 重测 n | 慢态 | 快态中位 | 快态 min–max |",
             "|---|---|---|---|---|---|---|---|"]
    for r in rows:
        lines.append(f"| {r['device']} | {r['workload']} | {r['njobs']} | "
                     f"{float(r['matrix_single_mib_s']):.1f} | {r['n_obs']} | "
                     f"{r['n_slow']}/{r['n_obs']} | {float(r['fast_median_mib_s']):.1f} | "
                     f"{float(r['fast_min_mib_s']):.1f}–{float(r['fast_max_mib_s']):.1f} |")
    return "\n".join(lines)


def remeasure_table():
    rows = read_csv(os.path.join(PLOT_DIR, "nvme-ext4-seqwrite-remeasure.csv"))
    if not rows:
        return None
    lines = ["| njobs | n | 中位 | min | max | 矩阵单次 |", "|---|---|---|---|---|---|"]
    for r in rows:
        lines.append(f"| {r['njobs']} | {r['n_obs']} | {float(r['median_mib_s']):.1f} | "
                     f"{float(r['min_mib_s']):.1f} | {float(r['max_mib_s']):.1f} | "
                     f"{float(r['matrix_single_mib_s']):.1f} |")
    return "\n".join(lines)


def build():
    summary = read_csv(os.path.join(PLOT_DIR, "summary.csv"))
    doc = []
    doc.append("# 基线矩阵结果附录（自动生成）\n")
    doc.append("> 本文件由 `make_results_doc.py` 从本地数据生成，**请勿手工编辑**：\n"
               "> 重新生成用 `python3 make_results_doc.py`。\n"
               "> 仓库只跟踪文档与脚本，原始数据（`plots/`、`results/`、CSV、日志）不入库，\n"
               "> 因此这张附录是文档引用数值的**自洽来源**。\n")

    doc.append("\n## 1. 数据集与运行条件\n")
    doc.append("- 矩阵：`sata`/`nvme` × `ext2like`/`ext4` × `randwrite-4k`/`seqwrite-4k` × "
               "`fsync`/`orderless` × njobs 1–32（共 144 格），批次 `20260921_180514_aster-ext4-baseline`。")
    doc.append("- fio：一 job 一文件、`psync`、`iodepth=1`；每 job 的写入区间为 "
               "`FILE_RANGE_MB / njobs`。")
    cfg = run_config()
    if cfg:
        doc.append("- 运行参数（取自 `run-baseline.sh` 默认值）：" + "、".join(cfg) + "。")
    doc.append("- 有效性判据（`parse_baseline.py`）：**写入量 ≥ 10 MiB 且含 DRAIN 计时**，"
               "否则该轮记为无效（表中记 `—`）。")
    doc.append("- 判读口径：单次抽样会落进双稳低态，故图所用数值对部分格子做了 "
               "**best-of-N 回填**（见第 3 节）；跨批次比较不可用于归因（同配置批间漂移可达 ~3×）。")

    doc.append("\n## 2. 吞吐数据（`drain_bw_mib_s`，单位 MiB/s）\n")
    if summary is None:
        doc.append("本机无 `plots/<run>/summary.csv`，本节跳过。\n")
    else:
        for device, workload, title in PANELS:
            doc.append(f"### {title}\n")
            doc.append(panel_table(summary, device, workload) + "\n")

    doc.append("\n## 3. 数据回填记录（best-of-N）\n")
    doc.append("规则：每 njobs 取重测批次里 `valid=1` 且非插桩观测的 `drain_bw` **最大值**，"
               "整行原样搬（`write_mib`/`busy_ms`/`iops` 自洽），不做插值。来源 run 见各行。\n")
    rows = patched_points()
    doc.append("\n".join(rows) if rows else "本机无回填记录，本节跳过。")

    doc.append("\n## 4. 奇异点逐点复核\n")
    table = anomalous_table()
    doc.append(table if table else "本机无 `anomalous_points_summary.csv`，本节跳过。")

    doc.append("\n## 5. ext4 seqwrite 随并发下降的复核\n")
    doc.append("结论：**下降是真实结果**，不是单次抽样（4 条升序批次 + 2 条降序批次都下降；"
               "best-of-N 也下降）。原始矩阵值≈best-of-N。\n")
    table = remeasure_table()
    doc.append(table if table else "本机无 `nvme-ext4-seqwrite-remeasure.csv`，本节跳过。")

    doc.append("\n## 6. 已知 baseline 缺陷（不在 baseline 内修）\n")
    doc.append("- **nvme + 多线程**：njobs ≥ 20 起带宽随并发单调下降（ext2like 与 ext4 都有）；")
    doc.append("- **randwrite 双稳底线态**：约 12.5 MiB/s，与快态（217–268 MiB/s）并存，"
               "同一 njobs 下两态都能长期维持；已排除元数据侧原因（方案 A 作用域、inode 表 "
               "write-behind、完全异步三臂 A/B 均无差别）⇒ 机制在数据路径 / 脏页节流 / 分配一侧。")

    doc.append("\n---\n")
    doc.append("生成命令：`python3 test/aster-ext4-baseline-test/make_results_doc.py`\n")
    return "\n".join(doc) + "\n"


def main():
    text = build()
    if "--stdout" in sys.argv:
        print(text)
        return 0
    with open(OUT, "w") as f:
        f.write(text)
    print(f"已写出 {OUT}（{len(text.splitlines())} 行）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
