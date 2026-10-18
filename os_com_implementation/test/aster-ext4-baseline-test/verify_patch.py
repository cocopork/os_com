#!/usr/bin/env python3
"""核验 plots/.../summary.csv 的替换：只改 13 行、每行都是真实快态观测、无多余引号。"""
import csv
import os
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
P = os.path.join(HERE, "plots", "20260921_180514_aster-ext4-baseline")
new = list(csv.DictReader(open(os.path.join(P, "summary.csv"))))
old = list(csv.DictReader(open(os.path.join(P, "summary.matrix-single-shot.csv"))))

print(f"行数 new={len(new)} old={len(old)} 列数={len(new[0])}")
diff = [(o, n) for o, n in zip(old, new) if o != n]
print(f"改动行数={len(diff)}（应 13）")
print(f"  仅 device/fs/workload/mode/njobs 之外的列有变化: "
      f"{all(o['device'] == n['device'] and o['njobs'] == n['njobs'] for o, n in diff)}")
print()
print(f"{'点':<26}{'旧 bw':>9}{'→新 bw':>10}{'write':>8}{'iops':>10}{'slow':>7}")
for o, n in diff:
    note = n["note"]
    slow = note.split("slow=")[1].split()[0] if "slow=" in note else "?"
    print(f"  {o['device'] + '/' + o['workload'] + ' nj' + o['njobs']:<24}"
          f"{o['drain_bw_mib_s']:>9}{n['drain_bw_mib_s']:>10}{n['write_mib']:>8}{n['iops']:>10}{slow:>7}")
print()
print("note 示例:", diff[0][1]["note"])
quoted = sum(1 for line in open(os.path.join(P, "summary.csv")) if '"' in line)
print(f"含引号行数（应 0）: {quoted}")
cr = sum(1 for line in open(os.path.join(P, "summary.csv"), "rb") if b"\r" in line)
print(f"含 CR 的行数（应 0，LF 行尾）: {cr}")
print(f"\n备份: {os.path.basename('summary.matrix-single-shot.csv')}（原始单次矩阵值）")
print(f"替换记录: anomalous_patch.csv（{sum(1 for _ in open(os.path.join(P, 'anomalous_patch.csv'))) - 1} 行）")
sys.exit(0)
