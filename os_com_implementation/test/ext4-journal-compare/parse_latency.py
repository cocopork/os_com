#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
#
# 解析 write 路径分解实验的 `[LATENCY]` 日志行。
#
# 内核侧 dump_latency_stats() 每隔若干轮 writeback 打印一次快照，快照是
# **累积值**（LATENCY_STATS 不清零），所以：
#   - 最后一个快照 = 完整统计（最终 count/avg/max）
#   - 中间快照 = 时序趋势（如 throttle 等待是否随时间增长）
#
# 用法：
#   python3 parse_latency.py <qemu-serial-*.log> [<qemu-serial-*.log> ...]
#
# 输出：每个文件一节的最终统计表 + 各测量点跨快照的时序（avg_us）。

import collections
import re
import sys

LINE_RE = re.compile(
    r"\[LATENCY\] (\w+): count=(\d+)(?: avg_us=(\d+) max_us=(\d+))?"
)

# 测量点的展示顺序与中文名。
METRICS = [
    ("write_total", "用户 write() 总延迟"),
    ("write_inner_lock", "  获取 inode inner 锁"),
    ("write_page_cache", "  写 page cache(标脏)"),
    ("write_mark_dirty", "  登记 dirty inode"),
    ("write_throttle", "  cache 水位 throttle 等待"),
    ("wb_total", "writeback 每轮总延迟"),
    ("wb_read_inode", "  读 inode 元数据"),
    ("wb_inner_lock", "  获取 inode inner 锁"),
    ("wb_alloc_pending", "  分配 block(allocate_pending)"),
    ("wb_journal_capture", "  journal capture(desc/ordered)"),
    ("wb_flush_async", "  提交数据 bio(flush_async)"),
    ("wb_alloc_blocks", "    ├ fs.alloc_blocks(分配器)"),
    ("wb_tree_insert", "    ├ tree.insert(extent 插入)"),
    ("wb_map_blocks", "    ├ map_blocks(extent 查找)"),
    ("wb_wait_page_io", "    └ wait_page_io(等 in-flight)"),
    ("wb_walk_range", "  walk_range(规划 holes)"),
    ("wb_convert_unwritten", "  mark_range_written(转换)"),
    ("wb_begin_op", "  begin_op(journal 事务获取)"),
    ("wb_reap", "  reap_completed_inodes"),
    ("wb_dirty_lock", "  dirty_inodes.lock() 竞争"),
]

DEPTH_RE = re.compile(r"\[LATENCY\] max_tree_depth: (\d+)")


def parse_snapshots(path):
    """返回 [(name, count, avg_us, max_us)] 列表（按快照顺序）。"""
    snapshots = []
    cur = None
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            if "[LATENCY] == snapshot begin ==" in line:
                cur = collections.OrderedDict()
                snapshots.append(cur)
                continue
            m = LINE_RE.search(line)
            if not m:
                continue
            name = m.group(1)
            count = int(m.group(2))
            avg_us = int(m.group(3)) if m.group(3) is not None else 0
            max_us = int(m.group(4)) if m.group(4) is not None else 0
            if cur is not None and count > 0:
                cur[name] = (count, avg_us, max_us)
    return snapshots


def main():
    if len(sys.argv) < 2:
        print("Usage: parse_latency.py <qemu-serial-log> ...", file=sys.stderr)
        return 2

    for path in sys.argv[1:]:
        snaps = parse_snapshots(path)
        if not snaps:
            print(f"== {path} ==\n  (no [LATENCY] snapshot found)\n")
            continue
        final = snaps[-1]
        print(f"== {path} ==")
        print(f"  snapshots: {len(snaps)}")
        max_depth = None
        with open(path, "r", errors="replace") as fh:
            for line in fh:
                m = DEPTH_RE.search(line)
                if m:
                    max_depth = m.group(1)
        if max_depth is not None:
            print(f"  max_tree_depth: {max_depth}")
        print(f"  {'metric':<28}{'count':>10}{'avg_us':>10}{'max_us':>10}")
        for name, label in METRICS:
            if name in final:
                c, a, m = final[name]
                print(f"  {label:<28}{c:>10}{a:>10}{m:>10}")
        # 时序趋势：只打印 avg_us 随快照变化。
        print("\n  avg_us 随快照变化（快照1..N）：")
        for name, label in METRICS:
            series = [s[name][1] for s in snaps if name in s]
            if series:
                print(f"  {label:<28} " + " ".join(f"{v:>6}" for v in series))
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
