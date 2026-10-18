#!/usr/bin/env python3
"""把 filebench / sqlite 两份 summary.csv 汇成 rNVPCe vs rNVPC 对照表。

标签约定（2026-09-24）：ext4 → rNVPCe，ext2like → rNVPC（`fs_label` 列）。

用法：
    python3 test/macro-tests/summarize-macro.py <filebench-summary.csv> [sqlite-summary.csv]

输出：markdown 表格（可直接贴进文档/论文草稿）。
"""
import csv
import os
import sys
from collections import defaultdict

# 列顺序（同时决定“比值 = 前/后”的方向）：ext4= rNVPCe 在前。
LABEL_ORDER = ["rNVPCe", "rNVPC"]


def read_csv(path):
    try:
        with open(path, newline="", encoding="utf-8", errors="replace") as fh:
            rows = list(csv.DictReader(fh))
    except OSError as exc:
        print(f"[warn] 读不到 {path}: {exc}", file=sys.stderr)
        return []
    out = []
    for r in rows:
        # 只保留 valid=1（sqlite 的 valid 由 integrity=ok 决定；filebench 由 status=ok）
        if str(r.get("valid", "1")).strip() not in ("1", "true", "True"):
            continue
        r["_fs_label"] = (r.get("fs_label") or r.get("fs") or "?").strip()
        r["_dev"] = (r.get("device") or "?").strip()
        out.append(r)
    return out


def fnum(row, key):
    try:
        return float(row.get(key, "") or 0)
    except ValueError:
        return 0.0


def pair_table(rows, group_keys, metrics, title, unit_note=""):
    """metrics: [(列名, 显示名)]。行为 group_keys 组合，列按 fs_label 分组成对显示。"""
    groups = defaultdict(dict)
    for r in rows:
        key = tuple(r.get(k, "?") for k in group_keys)
        groups[key][r["_fs_label"]] = r
    labels = [x for x in LABEL_ORDER if x in {r["_fs_label"] for r in rows}]
    labels += sorted({r["_fs_label"] for r in rows} - set(labels))
    if not groups:
        print(f"### {title}\n（无有效数据）\n")
        return
    head = "| " + " | ".join(group_keys) + " | " + " | ".join(
        f"{lab}: {name}" for lab in labels for name, _ in metrics) + " | 比值(rNVPCe/rNVPC) |"
    sep = "|" + "---|" * (len(group_keys) + len(labels) * len(metrics) + 1)
    print(f"### {title}")
    if unit_note:
        print(f"（{unit_note}）")
    print(head)
    print(sep)
    for key in sorted(groups):
        cells = []
        vals = {}
        for lab in labels:
            row = groups[key].get(lab, {})
            for i, (name, keyname) in enumerate(metrics):
                v = fnum(row, keyname)
                cells.append(f"{v:,.3f}" if v else "-")
                if i == 0:          # 比值只取**第一个**指标（吞吐/耗时），不取延迟
                    vals[lab] = v
        ratio = "-"
        if len(labels) == 2:
            a, b = (vals.get(labels[0], 0), vals.get(labels[1], 0))
            if a and b:
                ratio = f"{a / b:.2f}"
        print("| " + " | ".join(str(k) for k in key) + " | " + " | ".join(cells) + f" | {ratio} |")
    print()


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if "--dir" in sys.argv:
        d = sys.argv[sys.argv.index("--dir") + 1]
        fb = sq = None
        for root, _dirs, files in os.walk(d):
            for fn in files:
                if fn != "summary.csv":
                    continue
                p = os.path.join(root, fn)
                if "filebench" in p:
                    fb = p
                elif "sqlite" in p:
                    sq = p
        args = [x for x in (fb, sq) if x]
    if not args:
        print(__doc__)
        return 2

    for path in args:
        rows = read_csv(path)
        name = os.path.basename(os.path.dirname(os.path.abspath(path)))
        if "filebench" in path or "filebench" in name:
            pair_table(rows, ["device", "workload", "threads"],
                       [("ops/s", "ops_per_sec"), ("MB/s", "mb_per_sec"), ("ms/op", "latency_ms")],
                       f"filebench：{name}",
                       "比值列 >1 = rNVPCe 吞吐更高；ms/op 列越小越好（其比值含义相反）")
        else:
            pair_table(rows, ["device", "size"],
                       [("耗时s", "elapsed_s")],
                       f"sqlite speedtest1：{name}",
                       "耗时越小越好；注意 integrity 必须为 ok 才算有效（无效行已被过滤）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
