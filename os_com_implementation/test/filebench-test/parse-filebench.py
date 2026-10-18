#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""filebench 宏测试结果解析：baseline_run.log → summary.csv

用法:
    python3 parse-filebench.py <baseline_run.log> <summary.csv>

口径说明（与 aster-ext4-baseline-test/parse_baseline.py 保持同风格）：
* 每轮以自描述标记成对出现：`=== FILEBENCH_PARAM:` + `=== FILEBENCH_RESULT:`。
  不依赖轮次顺序、不依赖文件名——重复跑/调参后仍可解析。
* guest 侧**总会**打 RESULT（崩了也打 status=crash），所以真正"没有 RESULT"的
  情况只有一类：内核 panic / QEMU 被杀。这类记 status=vanish，valid=0。
* status 语义：ok=跑完；partial=有汇总行但退出码非 0；crash=没有汇总行；
  vanish=连 RESULT 都没有（内核级失败）。
* `valid` 与 fio 基线一致：只有 status=ok 且 ops_per_sec>0 才算有效点
  （宏测试的数比 fio 更粗，10 MiB 之类的阈值没有意义）。
"""

import csv
import os
import re
import sys

# 标签约定（用户 2026-09-24 定）：这两个标准档作为 rNVPCe / rNVPC 的**替身**。
# 注意：这只是命名约定，不代表二者在实现上等价；`fs` 列保留原始档名供复核。
FS_LABELS = {
    "ext2like_sync": "ext2-orderless",
    "ext4_sync": "ext4-orderless",
    "ext2like_nomerge": "ext4-rNVPC",
    "ext4": "ext4-rNVPCe",
    "ext4_nomerge": "ext4-nomerge",
    "ext2like": "rNVPC",
}

PARAM_RE = re.compile(
    r"=== FILEBENCH_PARAM: *dev=(?P<dev>\S+) fs=(?P<fs>\S+) workload=(?P<wl>\S+) "
    r"threads=(?P<th>\d+) runtime=(?P<rt>\d+) nfiles=(?P<nf>\S+) filesize=(?P<fsz>\S+) "
    r"mount_opts=(?P<mo>\S+) ==="
)
RESULT_RE = re.compile(
    r"=== FILEBENCH_RESULT: *\S+ status=(?P<st>\S+) rc=(?P<rc>-?\d+) "
    r"elapsed_s=(?P<el>[0-9.]+) ops_per_sec=(?P<ops>[0-9.]+) "
    r"mb_per_sec=(?P<mbs>[0-9.]+) latency_ms=(?P<lat>[0-9.]+) ==="
)
RAW_BEGIN = "--- FILEBENCH_RAW_BEGIN"
RAW_END = "--- FILEBENCH_RAW_END ---"


def _io_summary_before(flat, pos):
    """取 pos 之前最近的一条 `IO Summary:` 原文。

    为什么要这个兜底：guest 侧用 grep/sed 抽 mb/s、ms/op 时，1.5-alpha3 的
    `142.1mb/s 0.279ms/op`（无空格）在部分环境（busybox grep）会抽不到，
    RESULT 里就写成 0。原始行已经在日志里，所以在宿主侧用 Python 正则补。
    """
    line = ""
    for m in re.finditer(r"IO Summary:.*?(?==|\Z)", flat[:pos]):
        line = m.group(0)
    return line


def _pick_num(pattern, line, default):
    """从 IO Summary 原文里取数；取不到就保留 guest 报的值。"""
    m = re.search(pattern, line) if line else None
    return m.group(1) if m else default


def parse(text):
    rows = []
    # 标记可能因串口 80 列折行而被切断：先拍平换行再匹配。
    flat = text.replace("\r", "").replace("\n", "")
    params = list(PARAM_RE.finditer(flat))
    results = list(RESULT_RE.finditer(flat))
    raw_tails = {}
    for m in re.finditer(re.escape(RAW_BEGIN) + r".*?" + re.escape(RAW_END), flat):
        raw_tails[m.start()] = m.group(0)[len(RAW_BEGIN):-len(RAW_END)]

    def tail_before(pos):
        """取该结果之前最近的一段原始输出（崩点现场）。"""
        best = None
        for p, t in raw_tails.items():
            if p < pos and (best is None or p > best[0]):
                best = (p, t)
        return re.sub(r"\s+", " ", best[1]).strip() if best else ""

    for i, pm in enumerate(params):
        # 本轮的 RESULT = 第一个在下一个 PARAM 之前的 RESULT。
        nxt = params[i + 1].start() if i + 1 < len(params) else len(flat)
        rm = next((r for r in results if pm.start() < r.start() < nxt), None)
        row = {
            "device": pm.group("dev"),
            "fs": pm.group("fs"),
            "fs_label": FS_LABELS.get(pm.group("fs"), pm.group("fs")),
            "workload": pm.group("wl"),
            "threads": pm.group("th"),
            "runtime_s": pm.group("rt"),
            "nfiles": pm.group("nf"),
            "filesize": pm.group("fsz"),
            "mount_opts": pm.group("mo"),
        }
        if rm is None:
            row.update(status="vanish", rc="-1", elapsed_s="", ops_per_sec="",
                       mb_per_sec="", latency_ms="", valid="0",
                       note="no RESULT (kernel panic / QEMU killed)",
                       raw_tail=tail_before(nxt))
        else:
            ops = float(rm.group("ops"))
            status = rm.group("st")
            sumline = _io_summary_before(flat, rm.start())
            row.update(status=status, rc=rm.group("rc"), elapsed_s=rm.group("el"),
                       ops_per_sec=f"{ops:.2f}",
                       mb_per_sec=_pick_num(r"([0-9][0-9.]*)\s*mb/s", sumline, rm.group("mbs")),
                       latency_ms=_pick_num(r"([0-9][0-9.]*)\s*ms", sumline, rm.group("lat")))
            row["valid"] = "1" if (status == "ok" and ops > 0) else "0"
            note = "" if row["valid"] == "1" else f"status={status}"
            row["note"] = note
            row["raw_tail"] = "" if row["valid"] == "1" else tail_before(rm.start())
        rows.append(row)
    return rows


COLS = ["device", "fs", "fs_label", "workload", "threads", "runtime_s", "nfiles", "filesize",
        "status", "rc", "elapsed_s", "ops_per_sec", "mb_per_sec", "latency_ms",
        "valid", "note", "mount_opts", "raw_tail"]


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    log, out = sys.argv[1], sys.argv[2]
    if not os.path.exists(log):
        print(f"error: {log} not found", file=sys.stderr)
        return 2
    rows = parse(open(log, errors="ignore").read())
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=COLS)
        w.writeheader()
        w.writerows(rows)

    print(f"[parse] {len(rows)} 轮 → {out}")
    bad = [r for r in rows if r["valid"] != "1"]
    if bad:
        print(f"[parse] {len(bad)} 轮非 ok：")
        for r in bad:
            print(f"   {r['device']:5s} {r['fs']:13s} {r['workload']:12s} "
                  f"t{r['threads']:>3s}  status={r['status']:8s} {r['note']}")
            if r["raw_tail"]:
                print(f"      raw: {r['raw_tail'][:200]}")
    ok = [r for r in rows if r["valid"] == "1"]
    if ok:
        print("\n[parse] 有效点（ops/s | MB/s）：")
        by = {}
        for r in ok:
            by.setdefault((r["device"], r["workload"], r["threads"]), {})[r["fs"]] = r
        for (dev, wl, th), fsmap in sorted(by.items()):
            cells = "  ".join(f"{fsmap[fs]['fs_label']}={fsmap[fs]['ops_per_sec']:>10s}ops/s/"
                              f"{fsmap[fs]['mb_per_sec']:>7s}MB/s" for fs in sorted(fsmap))
            print(f"   {dev:5s} {wl:12s} t{th:>3s}  {cells}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
