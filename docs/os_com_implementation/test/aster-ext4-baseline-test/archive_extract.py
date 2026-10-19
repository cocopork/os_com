#!/usr/bin/env python3
"""把 `results/` 下**所有**批次的原始日志蒸馏成两份小 CSV，便于删掉原始日志后仍能复现结论。

用法：
    python3 archive_extract.py [归档目录]        # 默认 archive/20260922-fsync-decomp

产出：
    summary_all.csv          —— 每个批次的每个 (device,fs,workload,mode,njobs) 一行，
                                并附带批次目录名（= 可追溯到原始 summary.csv 的键）。
    latency_last_snapshot.csv —— 每个 (批次, fs, njobs) 取 `[LATENCY]` **最后一个快照**
                                的关键计数器（count / avg_us），以及 `[BARRIER]` 计数。
                                没有插桩的批次不会出现（正常，LATENCY_ENABLED=false 时
                                这些计数器被编译期消除）。

设计取舍：只留"最后的累计快照"，因为 LatencyStat 是全局累计量，最后一个快照就是整轮
的总计；中间快照只用于看演化（时序曲线未归档，需要时重跑）。
"""

import csv
import glob
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

ANSI = re.compile(r"\x1b\[[0-9;]*m")
SNAP = re.compile(
    r"\[\s*([0-9]+\.[0-9]+)\].*?\[LATENCY\] == snapshot begin ==(.*?)\[LATENCY\] == snapshot end ==",
    re.S,
)
ONE = re.compile(r"\[LATENCY\] (\w+): count=(\d+)(?: avg_us=(\d+) max_us=(\d+))?")
BARRIER = re.compile(r"\[BARRIER\] requests=(\d+) device_flushes=(\d+) merged=(\d+)")
NAME = re.compile(r"qemu-serial-([a-z0-9]+)-(.+)-fsync-nj(\d+)\.log$")
# 已知的 fs 名，用于把 `<fs>-<workload>` 拆开（workload 自身带连字符，如 `randwrite-4k`）。
FS_NAMES = ("ext2like", "ext4")

# 归档的关键计数器（fsync 时间线上的顺序）。
KEYS = [
    "fsync_total",
    "fsync_lock",
    "fsync_alloc",
    "alloc_meta_flush",
    "alloc_desc",
    "wb_walk_range",
    "wb_alloc_blocks",
    "wb_tree_insert",
    "wb_map_blocks",
    "wb_convert_unwritten",
    "fsync_data_flush",
    "fsync_capture",
    "fsync_commit_wait",
    "commit_stage_wait",
    "commit_parked",
    "commit_total",
    "commit_ordered",
    "commit_log",
    "commit_barrier1",
    "commit_barrier2",
    "commit_flush_tail",
    "commit_thread_sleep",
    "commit_thread_busy",
    "commit_thread_age",
    "commit_one_total",
    "commit_advance",
    "commit_advance_lock",
    "commit_credits_wake",
    "commit_checkpoint",
    "fsync_barrier_flush",
    "fsync_barrier_wait",
    "fsync_barrier",
]


def last_snapshot(path):
    """返回 (t_seconds, {counter: (count, avg_us, max_us)})：批次内计数最大的快照。"""
    s = ANSI.sub("", open(path, errors="ignore").read())
    best = None
    for m in SNAP.finditer(s):
        k = {
            mm.group(1): (int(mm.group(2)), int(mm.group(3) or 0), int(mm.group(4) or 0))
            for mm in ONE.finditer(m.group(2))
        }
        if "fsync_total" not in k:
            continue
        if best is None or k["fsync_total"][0] > best[1]["fsync_total"][0]:
            best = (float(m.group(1)), k)
    return best


def barrier_counts(path):
    s = ANSI.sub("", open(path, errors="ignore").read())
    last = None
    for m in BARRIER.finditer(s):
        last = tuple(int(g) for g in m.groups())
    return last


def main():
    out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        HERE, "archive", "20260922-fsync-decomp"
    )
    os.makedirs(out_dir, exist_ok=True)

    summary_rows = []
    latency_rows = []
    for run in sorted(glob.glob(os.path.join(HERE, "results", "*"))):
        if not os.path.isdir(run):
            continue
        batch = os.path.basename(run)
        csv_path = os.path.join(run, "summary.csv")
        if os.path.isfile(csv_path):
            with open(csv_path) as fh:
                for r in csv.DictReader(fh):
                    r["batch"] = batch
                    summary_rows.append(r)

        for log in sorted(glob.glob(os.path.join(run, "logs", "qemu-serial-*.log"))):
            m = NAME.search(os.path.basename(log))
            if not m:
                continue
            device, middle, nj = m.group(1), m.group(2), m.group(3)
            fs, workload = middle, ""
            for name in FS_NAMES:
                if middle == name or middle.startswith(name + "-"):
                    fs, workload = name, middle[len(name) + 1 :]
                    break
            snap = last_snapshot(log)
            if snap is None:
                continue
            t, k = snap
            row = {"batch": batch, "device": device, "fs": fs, "workload": workload,
                   "njobs": nj, "snapshot_t_s": f"{t:.1f}"}
            ft = k.get("fsync_total")
            row["fsync_count"] = ft[0] if ft else ""
            row["fsync_us"] = ft[1] if ft else ""
            for key in KEYS:
                if key == "fsync_total":
                    continue
                c = k.get(key)
                row[f"{key}_count"] = c[0] if c else ""
                row[f"{key}_us"] = c[1] if c else ""
            b = barrier_counts(log)
            if b:
                row["barrier_requests"], row["barrier_flushes"], row["barrier_merged"] = b
            latency_rows.append(row)

    with open(os.path.join(out_dir, "summary_all.csv"), "w", newline="") as fh:
        cols = ["batch"] + (list(summary_rows[0].keys())[:-1] if summary_rows else [])
        w = csv.DictWriter(fh, fieldnames=cols)
        w.writeheader()
        for r in summary_rows:
            w.writerow({c: r.get(c, "") for c in cols})

    with open(os.path.join(out_dir, "latency_last_snapshot.csv"), "w", newline="") as fh:
        cols = ["batch", "device", "fs", "workload", "njobs", "snapshot_t_s", "fsync_count", "fsync_us"]
        for key in KEYS:
            if key == "fsync_total":
                continue
            cols += [f"{key}_count", f"{key}_us"]
        cols += ["barrier_requests", "barrier_flushes", "barrier_merged"]
        w = csv.DictWriter(fh, fieldnames=cols)
        w.writeheader()
        for r in latency_rows:
            w.writerow({c: r.get(c, "") for c in cols})

    print(f"summary_all.csv            : {len(summary_rows)} 行")
    print(f"latency_last_snapshot.csv  : {len(latency_rows)} 行")
    print(f"→ {out_dir}")


if __name__ == "__main__":
    main()
