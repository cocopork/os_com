#!/usr/bin/env python3
"""打印某一轮的全部 `[LATENCY]` 组件：count / avg_us / max_us（含 max 与 avg 的比值）。

`max_us / avg_us` 是区分两种膨胀的关键：
  * 两者同阶 → **每次调用都慢**（稳定开销/设备等待均摊到每次）；
  * max ≫ avg（比如 20×）→ **少数几次极慢**（偶发长停顿：分配器长停顿、设备长尾、被抢占）。

用法：python3 latency_round.py <results 目录名或绝对路径> <round tag>
例：  python3 latency_round.py 20260921_233xxx_aster-ext4-baseline nvme-ext2like-seqwrite-4k-orderless-nj32
"""

import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SNAP = re.compile(r"\[LATENCY\] == snapshot begin ==(.*?)\[LATENCY\] == snapshot end ==", re.S)
ONE = re.compile(r"\[LATENCY\] (\w+): count=(\d+)(?: avg_us=(\d+) max_us=(\d+))?")


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 1
    rd = sys.argv[1]
    if not os.path.isabs(rd):
        rd = os.path.join(HERE, "results", rd)
    tag = sys.argv[2]
    text = ""
    for pat in (f"qemu-serial-{tag}.log", f"qemu-{tag}.log"):
        p = os.path.join(rd, "logs", pat)
        if os.path.exists(p):
            text += open(p, errors="replace").read()
    blocks = SNAP.findall(text)
    if not blocks:
        print(f"{tag}: 该轮没有 [LATENCY] 快照")
        return 1
    print(f"=== {os.path.basename(rd)} / {tag}（快照 {len(blocks)} 块，末块为该轮累计）===")
    print(f"{'组件':<22}{'count':>9}{'avg_us':>10}{'max_us':>10}{'max/avg':>9}")
    for name, count, avg, mx in ONE.findall(blocks[-1]):
        c, a, m = int(count), int(avg or 0), int(mx or 0)
        r = f"{m / a:.1f}x" if a else "n/a"
        print(f"{name:<22}{c:>9}{a:>10}{m:>10}{r:>9}")
    depths = re.findall(r"\[LATENCY\] max_tree_depth: (\d+)", text)
    if depths:
        print(f"{'max_tree_depth':<22}{depths[-1]:>9}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
