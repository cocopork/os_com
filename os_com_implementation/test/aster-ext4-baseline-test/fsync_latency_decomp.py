#!/usr/bin/env python3
"""fsync 路径的**阶段延迟分解**（配 `LATENCY_ENABLED=true` 的插桩构建使用）。

目的：009 修复 1（可合并 flush 屏障）之后，如果吞吐没变，就要看清"时间到底花在哪一段"，
而不是继续猜。本脚本把每一轮 `[LATENCY]` 快照里的 `fsync_*` 组件摊成
「count / avg_us / 总耗时 / 占比」，并算出**每次 fsync 的设备 flush 次数**与合并率
（从串口日志里的 `[BARRIER]` 计数）。

阶段口径（由 `LATENCY_STATS` 记录，见 `fs.rs`）：
    fsync_total / fsync_lock / fsync_alloc / fsync_data_flush / fsync_capture
    fsync_commit_wait / fsync_barrier / fsync_barrier_flush / fsync_barrier_wait

判读要点：
  * `fsync_barrier_flush` ≪ `fsync_barrier` ⇒ 屏障搭车生效（自己没发设备 flush）；
  * `device_flushes / fsync_count` ≪ 1  ⇒ 并发 fsync 共享了设备 flush；
  * 若上面成立但吞吐不变 ⇒ 瓶颈不在 barrier，而在 `fsync_commit_wait`（提交串行）
    或元数据同步写 / 数据等待，即 009 的修复 2–4。

注意：`LATENCY_ENABLED=true` 会把绝对吞吐压低约 2×（前台每页 5 次计时），
所以插桩数据只能用于比较**相对占比**。

用法：
    python3 fsync_latency_decomp.py [device] [fs] [njobs 逗号分隔] [--run=<run 目录名或路径>]
默认：sata ext2like 1,8,32，自动取**最近一个含 [LATENCY] 的批次**。
"""

import csv
import glob
import os
import re
import statistics as st
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

SNAP = re.compile(
    r"\[LATENCY\] == snapshot begin ==(.*?)\[LATENCY\] == snapshot end ==", re.S)
ONE = re.compile(r"\[LATENCY\] (\w+): count=(\d+)(?: avg_us=(\d+) max_us=(\d+))?")
BARRIER = re.compile(
    r"\[BARRIER\] requests=(\d+) device_flushes=(\d+) merged=(\d+)")

# 展示顺序（按 fsync 时间线），只列这些前缀的组件。
PHASES = [
    ("fsync_lock", "取 inode 写锁"),
    ("fsync_alloc", "分配窗口总计"),
    ("alloc_meta_flush", "└ 方案 A 窗口落盘"),
    ("alloc_desc", "└ inode desc 写回"),
    ("wb_walk_range", "└ 规划 holes（walk）"),
    ("wb_alloc_blocks", "└ 位图/GDT 分配"),
    ("wb_tree_insert", "└ extent 插入/节点写"),
    ("wb_map_blocks", "└ 映射查找"),
    ("wb_convert_unwritten", "└ unwritten 转换"),
    ("fsync_data_flush", "数据页 flush + 等完成"),
    ("fsync_capture", "元数据捕获/写回"),
    ("fsync_commit_wait", "等 journal 提交（含排队）"),
    ("commit_stage_wait", "└└ 排队：请求→stage（含等 handle 关）"),
    ("commit_parked", "└└ 计数：pipeline 让位次数（时长=0）"),
    ("commit_total", "└ 提交服务时间"),
    ("commit_ordered", "└└ ordered-data 段"),
    ("commit_log", "└└ 日志写 + 等完成"),
    ("commit_barrier1", "└└ barrier#1 pre-commit"),
    ("commit_barrier2", "└└ barrier#2 commit 后"),
    ("commit_flush_tail", "└└ 提交体内联排水尾"),
    ("commit_thread_sleep", "提交线程：阻塞等待"),
    ("commit_thread_busy", "提交线程：唤醒后服务"),
    ("commit_thread_age", "└ 其中：age_deadline（取读锁）"),
    ("commit_one_total", "└ 其中：commit_one"),
    ("commit_advance", "└└ advance_pipeline"),
    ("commit_advance_lock", "└└└ 其中：抢 state_write"),
    ("commit_credits_wake", "└└ note_credits_released"),
    ("commit_checkpoint", "└ checkpoint"),
    ("fsync_barrier_flush", "└ 其中：自己下发设备 flush"),
    ("fsync_barrier_wait", "└ 其中：等别人 flush（搭车等待）"),
    ("fsync_barrier", "屏障总计"),
    ("fsync_total", "fsync 总计"),
]


def find_run(explicit):
    if explicit:
        p = explicit if os.path.isdir(explicit) else os.path.join(HERE, "results", explicit)
        return p
    for d in sorted(glob.glob(os.path.join(HERE, "results", "*_aster-ext4-baseline*")),
                    reverse=True):
        logs = os.path.join(d, "logs")
        if not os.path.isdir(logs):
            continue
        for f in glob.glob(os.path.join(logs, "qemu-serial-*.log")):
            try:
                if "[LATENCY]" in open(f, errors="replace").read(200_000):
                    return d
            except OSError:
                pass
    return None


def last_snapshot(logdir, tag_hint):
    """取该轮最后一块快照（累计值），返回 {name: (count, avg_us, max_us)}。"""
    best = {}
    for f in glob.glob(os.path.join(logdir, "qemu-serial-*.log")):
        if tag_hint not in os.path.basename(f):
            continue
        try:
            text = open(f, errors="replace").read()
        except OSError:
            continue
        for block in SNAP.findall(text):
            snap = {}
            for name, cnt, avg, mx in ONE.findall(block):
                snap[name] = (int(cnt), int(avg or 0), int(mx or 0))
            if snap:
                best = snap  # 后面的快照是累计的，覆盖前一个
    return best


def barrier_of(logdir, tag_hint):
    for f in glob.glob(os.path.join(logdir, "qemu-serial-*.log")):
        if tag_hint not in os.path.basename(f):
            continue
        try:
            text = open(f, errors="replace").read()
        except OSError:
            continue
        got = [(int(a), int(b), int(c)) for a, b, c in BARRIER.findall(text)]
        if got:
            return got[-1]
    return None


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    run_arg = None
    for a in sys.argv[1:]:
        if a.startswith("--run="):
            run_arg = a.split("=", 1)[1]
    dev = args[0] if len(args) > 0 else "sata"
    fs = args[1] if len(args) > 1 else "ext2like"
    njobs = [int(x) for x in (args[2] if len(args) > 2 else "1,8,32").split(",")]

    run = find_run(run_arg)
    if not run or not os.path.isdir(run):
        print("找不到含 [LATENCY] 的批次（先跑一次插桩构建的实验）")
        return 1
    print(f"批次：{os.path.basename(run)}  配置：{dev}/{fs}/randwrite-4k/fsync\n")

    summary = {}
    with open(os.path.join(run, "summary.csv")) as fh:
        for r in csv.DictReader(fh):
            summary[(r["device"], r["fs"], r["njobs"])] = r

    for nj in njobs:
        tag = f"{dev}-{fs}-randwrite-4k-fsync-nj{nj}"
        snap = last_snapshot(os.path.join(run, "logs"), tag)
        if not snap:
            print(f"--- nj{nj}: 该轮没有 [LATENCY] 快照\n")
            continue
        total = snap.get("fsync_total", (0, 0, 0))
        n_fsync = snap.get("fsync_count", (total[0],))[0] or total[0]
        row = summary.get((dev, fs, str(nj)))
        dur = float(row["drain_ms"]) if row and row["drain_ms"] not in ("", "0") else 0.0
        rate = (n_fsync / (dur / 1000.0)) if dur else 0.0
        print(f"--- nj{nj}：fsync {n_fsync} 次"
              f"{f'，{dur/1000:.1f} s ⇒ {rate:.0f} fsync/s' if dur else ''}"
              f"，每次 fsync {total[1]} µs")
        print(f"{'阶段':<34}{'count':>10}{'avg_us':>10}{'总耗时 ms':>12}{'占 fsync_total':>16}")
        tot_us = total[0] * total[1]
        for name, label in PHASES:
            if name not in snap:
                continue
            cnt, avg, mx = snap[name]
            ms = cnt * avg / 1000.0
            share = (cnt * avg / tot_us * 100) if tot_us else 0.0
            print(f"{label + ' (' + name + ')':<34}{cnt:>10}{avg:>10}{ms:>12.1f}{share:>15.1f}%")
        b = barrier_of(os.path.join(run, "logs"), tag)
        if b:
            req, fl, mg = b
            print(f"    [BARRIER] requests={req} device_flushes={fl} merged={mg}"
                  f" ⇒ 每次 fsync 设备 flush {fl / n_fsync:.3f} 次（搭车率 {mg / req * 100:.1f}%）")
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
