#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""SQLite 宏测试结果解析：baseline_run.log → summary.csv

用法:
    python3 parse-sqlite.py <baseline_run.log> <summary.csv>

口径：
* 一轮 = 一个 (device, fs, size, run_i)。guest 打 PAMAM/RUN/INTEGRITY/RESULT 四组标记。
* status: ok=跑完且 integrity=ok；partial=部分轮跑完；crash=全崩（Asterinas 当前
  的**预期**结果，见 guest 脚本头的 P9 台账说明）；vanish=连 RESULT 都没有（内核级失败）。
* 崩点（末 15 行原始输出）会落到 `crash_tail` 列 —— 对 rNVPC 来说**崩点本身就是
  结论**（P9 before 值），不是噪声。
"""

import csv
import os
import re
import sys

PARAM_RE = re.compile(
    r"=== SQLITE_PARAM: *dev=(?P<dev>\S+) fs=(?P<fs>\S+) size=(?P<size>\d+) "
    r"runs=(?P<runs>\d+) mount_opts=(?P<mo>\S+) ==="
)
# 标签约定（用户 2026-09-24 定）：ext4 → rNVPCe，ext2like → rNVPC（替身命名）。
FS_LABELS = {
    "ext2like_sync": "ext2-orderless",
    "ext4_sync": "ext4-orderless",
    "ext2like_nomerge": "ext4-rNVPC",
    "ext4": "ext4-rNVPCe",
    "ext4_nomerge": "ext4-nomerge",
    "ext2like": "rNVPC",
}
RUN_RE = re.compile(
    r"=== SQLITE_RUN: *\S+ run=(?P<n>\d+) status=(?P<st>\S+) rc=(?P<rc>-?\d+) "
    r"elapsed_s=(?P<el>[0-9.]+) ==="
)
IG_RE = re.compile(r"=== SQLITE_INTEGRITY: *\S+ run=(?P<n>\d+) result=(?P<ig>\S+) ===")
RES_RE = re.compile(
    r"=== SQLITE_RESULT: *\S+ status=(?P<st>\S+) runs_ok=(?P<ok>\d+)/(?P<runs>\d+) "
    r"total_elapsed_s=(?P<tot>[0-9.]+) integrity=(?P<ig>\S+) ==="
)
RAW_RE = re.compile(r"--- SQLITE_RAW_BEGIN run=(\d+) rc=(-?\d+) ---(.*?)--- SQLITE_RAW_END run=\1 ---")


def parse(text):
    flat = text.replace("\r", "").replace("\n", "")
    rows = []
    raw_by_run = {int(m.group(1)): re.sub(r"\s+", " ", m.group(3)).strip()
                  for m in RAW_RE.finditer(flat)}

    params = list(PARAM_RE.finditer(flat))
    for i, pm in enumerate(params):
        nxt = params[i + 1].start() if i + 1 < len(params) else len(flat)
        seg = flat[pm.start():nxt]
        res = RES_RE.search(seg)
        runs = RUN_RE.findall(seg)
        igs = {int(n): r for n, r in IG_RE.findall(seg)}
        base = {
            "device": pm.group("dev"),
            "fs": pm.group("fs"),
            "fs_label": FS_LABELS.get(pm.group("fs"), pm.group("fs")),
            "size": pm.group("size"),
            "mount_opts": pm.group("mo"),
        }
        if not runs and not res:
            rows.append({**base, "run": "0", "status": "vanish", "rc": "-1",
                         "elapsed_s": "", "integrity": "n/a", "valid": "0",
                         "note": "no RESULT (kernel panic / QEMU killed)",
                         "crash_tail": ""})
            continue
        for n, st, rc, el in runs:
            integ = igs.get(int(n), "n/a")
            valid = "1" if (st == "ok" and integ == "ok") else "0"
            note = "" if valid == "1" else f"status={st} integrity={integ}"
            rows.append({**base, "run": n, "status": st, "rc": rc, "elapsed_s": el,
                         "integrity": integ, "valid": valid, "note": note,
                         "crash_tail": "" if valid == "1" else raw_by_run.get(int(n), "")})
        if res and not runs:
            rows.append({**base, "run": "0", "status": res.group("st"), "rc": "-1",
                         "elapsed_s": res.group("tot"), "integrity": res.group("ig"),
                         "valid": "0", "note": "RESULT without per-run lines",
                         "crash_tail": ""})
    return rows


COLS = ["device", "fs", "fs_label", "size", "run", "status", "rc", "elapsed_s", "integrity",
        "valid", "note", "mount_opts", "crash_tail"]


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

    print(f"[parse] {len(rows)} 行 → {out}")
    print("\n[parse] 每轮结果（elapsed_s 越小越好；crash 需要看 crash_tail 定位）")
    hdr = f"  {'device':6s} {'fs':13s} {'size':>6s} {'run':>3s} {'status':9s} {'rc':>4s} {'elapsed_s':>10s} {'integrity':9s}"
    print(hdr)
    print("  " + "-" * (len(hdr) - 2))
    for r in rows:
        print(f"  {r['device']:6s} {r['fs']:13s} {r['size']:>6s} {r['run']:>3s} "
              f"{r['status']:9s} {r['rc']:>4s} {r['elapsed_s']:>10s} {r['integrity']:9s}")

    crash = [r for r in rows if r["status"] == "crash"]
    if crash:
        print(f"\n[parse] ⚠ {len(crash)} 轮 crash —— 崩点现场（末段原始输出）：")
        seen = set()
        for r in crash:
            key = (r["fs"], r["crash_tail"][-120:])
            if r["crash_tail"] and key not in seen:
                seen.add(key)
                print(f"   [{r['device']}/{r['fs']}/size{r['size']}/run{r['run']}] ...{r['crash_tail'][-200:]}")
    ok = [r for r in rows if r["valid"] == "1"]
    if ok:
        print(f"\n[parse] 有效点 {len(ok)} 个；按 (device,fs,size) 汇总平均耗时：")
        agg = {}
        for r in ok:
            agg.setdefault((r["device"], r["fs"], r["size"]), []).append(float(r["elapsed_s"]))
        for k in sorted(agg):
            v = agg[k]
            print(f"   {k[0]:5s} {k[1]:13s} size={k[2]:>5s}  n={len(v)}  avg={sum(v)/len(v):.2f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
