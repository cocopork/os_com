#!/usr/bin/env python3
"""008 方案 A（nojournal extent 节点批量缓冲）判定性 A/B 的汇总对比。

由 `ab_njmb.sh` 产出的结果目录命名规则：
    <ts>_aster-ext4-baseline          → 臂 = on （默认，当前行为）
    <ts>_aster-ext4-baseline_njmb-off → 臂 = off（挂载选项 nojournal_meta_batch=0）
臂直接从目录名后缀推断，所以不存在「解析器要多认一列」的耦合。

输出每格（njobs）的各次重复值：**单看中位数会被双稳误导**——低态本身就在
12–13 MiB/s 紧致聚集，若只出现一次也会把中位数拉下来，所以每次都列出来。

用法：
    python3 ab_compare.py                 # 自动取最近 6 个结果目录
    python3 ab_compare.py <dir> [dir ...] # 指定目录
"""

import glob
import os
import re
import statistics as st
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import parse_baseline as pb  # noqa: E402

# 判态阈值：基线里慢态紧致落在 12.2-13.0，快态 218-251，中间没有观测值。
# 取 50 MiB/s 作分界（离两侧都足够远），只用于数「几次落在慢态」。
SLOW_FAST_BOUNDARY = 50.0


def arm_of(dirname):
    """臂标签 = 目录名里 `_aster-ext4-baseline` 之后的开关后缀（无后缀 = `default`）。

    这样两个 A/B 轴（`--njmb` 方案 A 作用域、`--njwb` 元数据 write-behind）互不干扰：
    `..._njmb-global` → `njmb-global`，`..._njwb-off` → `njwb-off`，默认臂 → `default`。
    """
    marker = "_aster-ext4-baseline"
    idx = dirname.find(marker)
    if idx < 0:
        return "default"
    return dirname[idx + len(marker):].lstrip("_") or "default"


# 臂的展示顺序：默认臂（= 新实现）排最后，便于与前面的对照臂直读。
def arm_sort_key(arm):
    return (arm != "default", arm)


def main():
    dirs = sys.argv[1:]
    if not dirs:
        # 自动挑选：按 mtime 倒序取最近 6 个**有数据**的结果目录。
        # 跳过 0 轮次目录很重要——中途被杀 / guest 脚本报错的批次会留下空目录，
        # 若不跳过就会挤掉一个有效目录（曾因此让汇总少了一只臂）。
        cands = sorted(
            glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*")),
            key=os.path.getmtime,
            reverse=True,
        )
        picked = []
        for cand in cands:
            log = os.path.join(cand, "logs", "baseline_run.log")
            if not os.path.exists(log) or not pb.parse(log):
                continue
            picked.append(cand)
            if len(picked) >= 6:
                break
        dirs = picked
    dirs = sorted(dirs)
    if not dirs:
        print("没有找到结果目录")
        return 1

    bw = defaultdict(list)   # (arm, njobs) -> [drain_bw, ...]
    wr = defaultdict(list)   # (arm, njobs) -> [write_mib, ...]
    notes = defaultdict(list)
    for d in dirs:
        log = os.path.join(d, "logs", "baseline_run.log")
        if not os.path.exists(log):
            print(f"[skip] 无 baseline_run.log: {os.path.basename(d)}")
            continue
        arm = arm_of(os.path.basename(d))
        rows = pb.parse(log)
        bad = sum(1 for r in rows if r["valid"] != "1")
        print(f"[read] {os.path.basename(d):<50} arm={arm:<3} 轮次={len(rows):<3} 无效={bad}")
        for r in rows:
            k = (arm, r["njobs"])
            ok = r["valid"] == "1"
            bw[k].append(float(r["drain_bw_mib_s"]) if ok else 0.0)
            wr[k].append(float(r["write_mib"]))
            if not ok:
                notes[k].append(r["note"])

    njs = sorted({nj for _, nj in bw})
    arms = sorted({arm for arm, _ in bw}, key=arm_sort_key)
    print()
    print("每格的各次重复（drain_bw MiB/s / write MiB）；慢态 = < %g MiB/s" % SLOW_FAST_BOUNDARY)
    print("=" * 108)
    for nj in njs:
        print(f"\nnjobs={nj}")
        for arm in arms:
            vals = bw.get((arm, nj), [])
            if not vals:
                continue
            med = st.median(vals)
            nslow = sum(1 for v in vals if v < SLOW_FAST_BOUNDARY)
            seq = " ".join(f"{v:7.1f}" for v in vals)
            wseq = " ".join(f"{v:7.0f}" for v in wr[(arm, nj)])
            print(f"  arm={arm:<7} drain_bw[{seq}] 中位={med:7.1f}  慢态 {nslow}/{len(vals)}")
            print(f"          write_mib[{wseq}]")
            for n in notes.get((arm, nj), []):
                print(f"          ! {n}")

    print()
    print("=" * 108)
    print("汇总（全部格子）：")
    for arm in arms:
        allv = [v for (a, _), vals in bw.items() if a == arm for v in vals]
        if not allv:
            continue
        nslow = sum(1 for v in allv if v < SLOW_FAST_BOUNDARY)
        print(f"  arm={arm:<7} n={len(allv):<3} 慢态 {nslow}/{len(allv)}"
              f"  非慢态中位={st.median([v for v in allv if v >= SLOW_FAST_BOUNDARY]):7.1f}"
              f"  总体中位={st.median(allv):7.1f}")

    print()
    print("解读要点：")
    print("  * off 臂慢态 n/n（全部落底线）→ 底线 ≈ nojournal 未合并的同步元数据写代价。")
    print("  * global 臂 部分慢态 → 旧实现：别人的 end 刷掉自己未改完的节点，合并偶发失效。")
    print("  * on 臂 慢态 0/n → owner 分桶修复有效（合并每次都成立）。")
    print("  * 若 on 臂非慢态中位 ≫ global/off，则修复同时抬高了稳定吞吐。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
