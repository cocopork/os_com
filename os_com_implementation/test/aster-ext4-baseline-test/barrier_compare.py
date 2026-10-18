#!/usr/bin/env python3
"""009 可合并 flush 屏障：`--barrier each`（旧行为）vs `merge`（新默认）同内核 A/B。

分组依据（同一内核；`merge` 是默认档，runner 对它**不加**目录后缀，所以要按时间窗 +
旗标一起判）：
  * `*_barrier-each*`                      → `each`（对照组）
  * 目录名无 `_barrier-`/`_njmb-`/`_njwb-` 且 run ≥ SINCE → `merge`（实验组）

另外从每轮的 `qemu-serial-*.log` 解析卸载时打印的诊断行：

    [BARRIER] requests=<N> device_flushes=<M> merged=<K>

`merged > 0` 才说明并发 fsync 真的共享了设备 flush（机制证据，不是吞吐推断）。

用法：python3 barrier_compare.py [device] [fs] [workload] [mode] [--since=YYYYMMDD_HHMM]
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

# 本次 A/B 的起始时间：此后的无后缀批次才算 merge 臂（早于此的无后缀批次是**旧代码**
# 的结果，必须排除——否则会被误当成 merge 臂）。
SINCE = "20260922_1110"
BARRIER_RE = re.compile(
    r"\[BARRIER\] requests=(\d+) device_flushes=(\d+) merged=(\d+)")
# 驱动日志：`###### repeat=N arm=<arm> timestamp=<ts> ...`
ARM_LINE = re.compile(r"###### repeat=\d+ arm=(\S+) timestamp=(\d{8}_\d{6})")


def arms_from_log(path):
    """从驱动日志拿 {timestamp: arm}。

    比目录后缀可靠：runner 的默认档不加后缀、且后缀是否落到最终目录上并不稳定，
    而驱动日志里每只臂的时间戳与臂名是权威的。
    """
    mapping = {}
    if not path or not os.path.exists(path):
        return mapping
    for line in open(path, errors="replace"):
        m = ARM_LINE.search(line)
        if m:
            mapping[m.group(2)] = m.group(1)
    return mapping


def arm_of(name, run, since, log_arms):
    """优先按驱动日志的 timestamp→arm 映射；没有日志时退回目录名推断。"""
    if log_arms:
        arm = log_arms.get(run)
        if arm in ("barrier-each", "barrier-merge"):
            return "each" if arm == "barrier-each" else "merge"
        return None
    if "_barrier-each" in name:
        return "each"
    if "_barrier-" in name or "_njmb-" in name or "_njwb-" in name:
        return None
    return "merge" if run >= since else None


def barrier_counts(logdir):
    """从串口日志取该轮的 (requests, flushes, merged)；可能多行取最后一行的累加值。"""
    out = []
    for f in glob.glob(os.path.join(logdir, "qemu-serial-*.log")):
        try:
            text = open(f, errors="replace").read()
        except OSError:
            continue
        for m in BARRIER_RE.finditer(text):
            out.append(tuple(int(g) for g in m.groups()))
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    since = SINCE
    log_arms = {}
    for a in sys.argv[1:]:
        if a.startswith("--since="):
            since = a.split("=", 1)[1]
        elif a.startswith("--log="):
            log_arms = arms_from_log(a.split("=", 1)[1])
    dev = args[0] if len(args) > 0 else "sata"
    fs = args[1] if len(args) > 1 else "ext2like"
    wl = args[2] if len(args) > 2 else "randwrite-4k"
    mode = args[3] if len(args) > 3 else "fsync"

    bw = defaultdict(list)          # (njobs, arm) -> [drain_bw]
    cnt = defaultdict(lambda: [0, 0, 0])  # arm -> 累加的 (req, flush, merged)
    n_runs = defaultdict(int)
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*"))):
        name = os.path.basename(d)
        log = os.path.join(d, "logs", "baseline_run.log")
        if not os.path.exists(log):
            continue
        run = name.split("_aster-ext4-baseline")[0]
        arm = arm_of(name, run, since, log_arms)
        if arm is None:
            continue
        rows = [r for r in pb.parse(log)
                if (r["device"], r["fs"], r["workload"], r["mode"]) == (dev, fs, wl, mode)]
        if not rows:
            continue
        n_runs[arm] += 1
        for r in rows:
            if r["valid"] != "1":
                continue
            bw[(int(r["njobs"]), arm)].append(float(r["drain_bw_mib_s"]))
        for (rq, fl, mg) in barrier_counts(os.path.join(d, "logs")):
            cnt[arm][0] = max(cnt[arm][0], rq)
            cnt[arm][1] = max(cnt[arm][1], fl)
            cnt[arm][2] = max(cnt[arm][2], mg)

    print(f"=== {dev}/{fs}/{wl}/{mode}：`--barrier each`（旧）vs `merge`（新）同内核 A/B ===")
    tag = f"臂来自驱动日志 {len(log_arms)} 条" if log_arms else f"按目录后缀/时间窗（since={since}）"
    print(f"（判臂依据：{tag}；批次：each={n_runs['each']} merge={n_runs['merge']}）\n")
    print(f"{'njobs':>6}{'each n/中位 MiB/s':>22}{'merge n/中位 MiB/s':>22}"
          f"{'each fsync/s':>15}{'merge fsync/s':>16}{'倍数':>9}")
    for nj in sorted({k[0] for k in bw}):
        e = bw.get((nj, "each"), [])
        m = bw.get((nj, "merge"), [])
        if not e or not m:
            continue
        me, mm = st.median(e), st.median(m)
        # 4 KiB 步长 ⇒ fsync/s = MiB/s × 256
        print(f"{nj:>6}{f'{len(e)}/{me:.2f}':>22}{f'{len(m)}/{mm:.2f}':>22}"
              f"{me * 256:>15.0f}{mm * 256:>16.0f}{mm / me:>8.2f}x")

    print("\n机制证据（每轮卸载时打印的累加计数，取各臂最大值）：")
    print(f"{'臂':>7}{'requests':>12}{'device_flushes':>17}{'merged':>10}{'合并率':>10}")
    for arm in ("each", "merge"):
        rq, fl, mg = cnt[arm]
        rate = f"{mg / rq * 100:.1f}%" if rq else "—"
        print(f"{arm:>7}{rq:>12}{fl:>17}{mg:>10}{rate:>10}")
    print("\n（`each` 臂 device_flushes ≈ requests；`merge` 臂应显著少，且 merged > 0）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
