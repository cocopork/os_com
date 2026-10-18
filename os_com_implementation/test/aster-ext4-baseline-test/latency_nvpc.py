#!/usr/bin/env python3
"""提取 rNVPC/rNVPCe 延迟分解快照，按「总耗时 = count x avg_us」给出占比。

数据来源：`results/*_aster-ext4-baseline/logs/qemu-{,serial-}<tag>.log` 里的
`[LATENCY] == snapshot begin/end ==` 块（需 `LATENCY_ENABLED=true` 的构建）。
快照是自 mount 起**累计**的，每轮一次 mount ⇒ 取该轮**最后一块**。

用法:
    python3 latency_nvpc.py [--dir-prefix 2026092] [--tags-end nj1,nj8]
                            [--device nvme] [--fs rnvpc_extend]
                            [--workload randwrite-4k|seqwrite-4k] [--mode orderless]
"""
import argparse
import glob
import os
import re

SNAP = re.compile(r"\[LATENCY\] == snapshot begin ==(.*?)\[LATENCY\] == snapshot end ==", re.S)
ONE = re.compile(r"\[LATENCY\] (\w+): count=(\d+)(?: avg_us=(\d+) max_us=(\d+))?")

# 前台写(A) + 后台回刷(B) + 参照量
A_FIELDS = [
    "nvpc_retire_total", "nvpc_retire_lock", "nvpc_wal", "nvpc_old_read",
    "nvpc_stage", "nvpc_transition", "nvpc_publish", "nvpc_index",
    "nvpc_wal_append", "nvpc_dirty_mark", "nvpc_extend_publish",
    # 2026-09-24：控制面元数据（NVBH）分配单独计时 + 容量等待按类别拆分
    # （数据面 staging vs 控制面 NVBH）——用于判定"前台是否卡在元数据上"。
    "nvpc_alloc_nvbh", "extend_data_wait", "extend_meta_wait",
    # I5：rNVPCe（soft update，无 WAL）专属段。extend_* 与 nvpc_wal* 同屏出现时，
    # 前者非零、后者恒 0 就是“rNVPCe 不经过 WAL”的直接证据。
    "extend_retire_total", "extend_in_flight_wait", "extend_commit", "extend_complete",
]
B_FIELDS = [
    "nvpc_flush_round", "nvpc_flush_gather", "nvpc_flush_read",
    # 2026-09-24：回刷改成**完全异步提交**（去掉 `io_batch.wait_all()`）后，
    # `nvpc_flush_wait`（= 等设备）这个口径作废，改成 `nvpc_flush_budget_wait`
    # （= 等回刷在飞预算）。两者**不可比**，旧结果里的 flush_wait 不要当同一个量读。
    "nvpc_flush_submit", "nvpc_flush_budget_wait", "nvpc_flush_complete",
]
DEVICE_FIELDS = ["nvpc_flush_device"]
COMPLETION_FIELDS = [
    "nvpc_flush_materialize",
    "nvpc_flush_durability",
    "nvpc_flush_migrate",
    "nvpc_flush_reclaim",
]
C_FIELDS = ["nvpc_eagain", "nvpc_pages_flushed", "nvpc_flush_done_pages",
            "nvpc_flush_bios", "nvpc_flush_skip"]  # 纯事件计数
REF_FIELDS = ["write_total", "write_throttle", "write_inner_lock", "wb_round", "wb_sync_total",
              "wb_flush_async", "wb_alloc_pending"]


def last_snapshot(logdir, tag):
    text = ""
    for pat in (f"qemu-{tag}.log", f"qemu-serial-{tag}.log"):
        p = os.path.join(logdir, pat)
        if os.path.exists(p):
            text += open(p, errors="replace").read()
    blocks = SNAP.findall(text)
    if not blocks:
        return None, 0
    return dict((m[0], (int(m[1]), int(m[2] or 0), int(m[3] or 0)))
                for m in ONE.findall(blocks[-1])), len(blocks)


def section(st, names, title, total_us=None):
    rows = [(n, st[n]) for n in names if n in st]
    if not rows:
        return
    print(f"\n  -- {title}")
    for n, (c, avg, mx) in rows:
        us = c * avg
        pct = f"{100.0 * us / total_us:6.1f}%" if total_us else "     -"
        print(f"    {n:24s} count={c:9d}  avg={avg:7d}us  max={mx:9d}us  total={us/1e6:9.3f}s  {pct}")


def phase_rows(st, names):
    """Return additive serial-phase rows with percentages of their own sum."""
    raw = [(name, *st[name], st[name][0] * st[name][1]) for name in names if name in st]
    denominator = sum(row[4] for row in raw)
    return [(*row, 100.0 * row[4] / denominator if denominator else 0.0) for row in raw]


def phase_section(st, names, title):
    rows = phase_rows(st, names)
    if not rows:
        return
    print(f"\n  -- {title}")
    for name, count, avg, maximum, total, percentage in rows:
        print(
            f"    {name:24s} count={count:9d}  avg={avg:7d}us  "
            f"max={maximum:9d}us  total={total/1e6:9.3f}s  {percentage:6.1f}%"
        )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir-prefix", default="2026")
    ap.add_argument("--tags-end", default="nj1,nj8")
    # 轮次 tag 形如 `<device>-<fs>-<workload>-<mode>-<njobs>`；不要写死 workload
    # （randwrite-4k 之外还有 seqwrite-4k，两者路径形态差别很大）。
    ap.add_argument("--device", default="nvme")
    ap.add_argument("--fs", dest="fs_name", default="rnvpc_extend")
    ap.add_argument("--workload", default="randwrite-4k")
    ap.add_argument("--mode", default="orderless")
    args = ap.parse_args()

    here = os.path.dirname(os.path.abspath(__file__))
    for d in sorted(glob.glob(os.path.join(here, "results", f"{args.dir_prefix}*_aster-ext4-baseline"))):
        for end in args.tags_end.split(","):
            tag = f"{args.device}-{args.fs_name}-{args.workload}-{args.mode}-{end}"
            st, nblocks = last_snapshot(os.path.join(d, "logs"), tag)
            if not st:
                continue
            total = st.get("nvpc_retire_total")
            total_us = total[0] * total[1] if total else None
            print(f"\n{'=' * 78}\n{d.split('/')[-1]}   {end}   snapshots={nblocks}")
            if total:
                print(f"  前台 retire 总计 {total[0]} 次, avg {total[1]}us, 合计 {total_us/1e6:.3f}s")
            section(st, A_FIELDS, "A 前台 write() -> 页退休到 NV", total_us)
            section(st, B_FIELDS, "B 后台 rTIO 回刷")
            phase_section(st, COMPLETION_FIELDS, "B1 completion 串行阶段占比")
            section(st, DEVICE_FIELDS, "B2 异步 BIO 提交到设备完成（可并行，不计入 B1 分母）")
            section(st, C_FIELDS, "事件计数（只看 count）")
            # 合并倍率：连续页合并大 BIO 的直接量化（新构建才有 nvpc_flush_bios）。
            pages, bios = st.get("nvpc_pages_flushed"), st.get("nvpc_flush_bios")
            if pages and bios and bios[0]:
                print(f"    {'合并倍率':22s} {pages[0] / bios[0]:.2f} 页/BIO"
                      f"  （{pages[0]} 页 / {bios[0]} 条 BIO）")
            # 完全异步后“已下发”与“已完成”是两个量：差值就是采样那一刻还在飞的页数
            # （只在末快照有意义，非 0 且有界 = 流水正常；无界增长 = 预算失效）。
            done = st.get("nvpc_flush_done_pages")
            if done and pages:
                print(f"    {'下发/完成(页)':22s} {pages[0]} / {done[0]}"
                      f"  （未完成 {pages[0] - done[0]} 页在飞）")
            section(st, REF_FIELDS, "参照（ext4 侧）")


if __name__ == "__main__":
    main()
