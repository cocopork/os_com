#!/usr/bin/env python3
"""
修正版 baseline 解析器。

修复 run-baseline.sh 内嵌解析的两个缺陷：

  1) 段边界有误：原实现用 `log.find("=== END_FIO_RESULT ===", seg_start)` 作为该轮
     段尾。当某轮因 `set -e` + `timeout 180` 提前退出、没有输出 END 标记时，段会
     一直延伸到下一轮的 END，把下一轮的 fio JSON 也算进本轮 → write_mib 出现
     "假值"（典型症状：相邻两轮 write_mib / drain_bw 完全相同）。
     修正：段边界用"下一个 FIO_RESULT 标记"。

  2) DRAIN 查找无上界：原实现 `re.search(..., log[seg_end:])` 会一直向后搜，缺
     DRAIN 的轮次会匹配到下一轮的 DRAIN → drain_bw 错配。
     修正：DRAIN 只在该轮段内查找，缺失记为空。

  3) 标签正则的 fs 分组是 `[a-z0-9]+`，**吃不下下划线** ⇒ `nvme_rnvpc_extend_*`
     这类标签整个匹配不上（字符类不含 `_`，回溯救不了）。后果不是"少一行"那么
     简单：段尾是"下一个 FIO_RESULT 标记"，被漏掉的轮次没有自己的标记 ⇒ 它的
     fio JSON 被并进**前一轮**的段 ⇒ write_mib 翻倍，且该轮 DRAIN 去配两轮的
     字节 → 产出"不对应任何单轮"的带宽（实测 2×512 MiB / 17.06 s = 60.02，
     而真值是 30.01 / 31.74）。
     修正：fs 分组改 `[a-z0-9_]+?`（lazy，由后面的 workload 关键字锚住边界）。

输出 CSV 增加 `valid` / `note` 列：`write_mib >= MIN_VALID_MIB` 记为有效。
fsync 档吞吐本身很低（~5 MiB/s，30s 只写几十 MiB）属**正常现象**，不算无效；
只有写量 < 10 MiB（实际基本是 0，即 fio 根本没产出数据）才判为无效。

轮次标记：新格式**自描述**（`<dev>_<fs>_<workload>_<mode>_nj<N>`，设备与 fs 都入标签）；
旧格式（`<workload>_<mode>_nj<N>`）仍可解析，设备按轮次顺序推断（每 36 轮一个设备）、
fs 记为 ext4。

用法:
    python3 parse_baseline.py <baseline_run.log> [out.csv]
"""

import re
import sys

DEVICES = ("sata", "nvme")
WORKLOADS = ("randwrite-4k", "seqwrite-4k")
MODES = ("orderless", "fsync")
NJOBS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
PER_DEVICE = len(WORKLOADS) * len(MODES) * len(NJOBS)

# write_mib 小于该值视为「未产出数据」。fsync 档吞吐本身很低（~5 MiB/s，30s 只写
# 几十 MiB），属正常现象不算无效；只有写量接近 0（fio 没返回/没产出 JSON）才无效。
MIN_VALID_MIB = 10.0

# 轮次标记。新格式是**自描述**的（设备与 fs 都入标签，宿主不再猜）：
#   === FIO_RESULT: <dev>_<fs>_<workload>_<mode>_nj<N> ===
FIO_RE = re.compile(
    r"=== FIO_RESULT: ([a-z0-9]+)_([a-z0-9_]+?)_(randwrite-4k|seqwrite-4k)_(orderless|fsync)_nj(\d+) ==="
)
# 旧格式（无 dev/fs）：=== FIO_RESULT: <workload>_<mode>_nj<N> ===
# 设备只能按轮次顺序推断（每 PER_DEVICE 轮一个设备），fs 记为 ext4——
# 仅用于重解析历史日志（如 20260921_065946）。
FIO_LEGACY_RE = re.compile(
    r"=== FIO_RESULT: (randwrite-4k|seqwrite-4k)_(orderless|fsync)_nj(\d+) ==="
)
WRITE_RE = re.compile(r'"write"\s*:\s*\{(.*?)\n\s*\}', re.S)
IOB_RE = re.compile(r'"io_bytes"\s*:\s*(\d+)')
IOPS_RE = re.compile(r'"iops"\s*:\s*([\d.]+)')
DRAIN_RE = re.compile(r"DRAIN: busy_ns=(\d+) drain_ns=(\d+)")


def _build_row(dev, fs, wl, mode, nj, seg):
    """从一轮的日志段构造结果行（含有效性判定与无效原因）。"""
    wb = 0.0
    iops = 0.0
    for jm in WRITE_RE.finditer(seg):
        body = jm.group(1)
        b = IOB_RE.search(body)
        ii = IOPS_RE.search(body)
        if b:
            wb += float(b.group(1))
        if ii:
            iops += float(ii.group(1))

    dm = DRAIN_RE.search(seg)
    busy = int(dm.group(1)) if dm else 0
    drain = int(dm.group(2)) if dm else 0

    write_mib = round(wb / 1048576, 1)
    too_small = write_mib < MIN_VALID_MIB
    no_drain = drain == 0
    # 有效性要求「写量够」**且**「有 DRAIN 计时」：缺 DRAIN 的轮次 drain_bw 会算成 0.0，
    # 若仍记 valid=1，任何聚合（中位数等）都会被 0 污染。这类轮次基本都是被宿主
    # `timeout` 杀掉的（fio 已返回、最终 `sync` 没跑完）。
    valid = (not too_small) and (not no_drain)
    if too_small:
        if "terminating on signal" in seg:
            note = "timeout: make run_kernel 超时被杀，fio 未返回"
        elif wb <= 0:
            note = "no-fio-json: 本轮未产出 fio JSON"
        else:
            note = f"write<{MIN_VALID_MIB:g}MiB"
    elif no_drain:
        note = "no-drain: 缺 DRAIN 行（多为被 timeout 杀掉），drain_bw 不可用"
    else:
        note = ""

    return {
        "device": dev,
        "fs": fs,
        "workload": wl,
        "mode": mode,
        "njobs": nj,
        "write_mib": write_mib,
        "busy_ms": round(busy / 1e6, 1),
        "drain_ms": round(drain / 1e6, 1),
        "busy_bw_mib_s": round(wb / 1048576 / (busy / 1e9), 2) if busy else 0.0,
        "drain_bw_mib_s": round(wb / 1048576 / (drain / 1e9), 2) if drain else 0.0,
        "iops": round(iops, 1),
        "valid": "1" if valid else "0",
        "note": note,
    }


def parse(log_path):
    log = open(log_path, errors="replace").read()

    # 新格式：dev/fs 直接从标签取。
    marks = list(FIO_RE.finditer(log))
    if marks:
        rows = []
        for i, m in enumerate(marks):
            e = marks[i + 1].start() if i + 1 < len(marks) else len(log)
            rows.append(_build_row(
                m.group(1), m.group(2), m.group(3), m.group(4), int(m.group(5)),
                log[m.end():e],
            ))
        return rows

    # 旧格式：设备按轮次顺序推断。
    marks = list(FIO_LEGACY_RE.finditer(log))
    rows = []
    for i, m in enumerate(marks):
        e = marks[i + 1].start() if i + 1 < len(marks) else len(log)
        dev_idx = i // PER_DEVICE
        dev = DEVICES[dev_idx] if dev_idx < len(DEVICES) else f"dev{dev_idx}"
        rows.append(_build_row(dev, "ext4", m.group(1), m.group(2), int(m.group(3)),
                               log[m.end():e]))
    return rows


def dump_csv(rows, path):
    cols = ("device", "fs", "workload", "mode", "njobs", "write_mib", "busy_ms", "drain_ms",
            "busy_bw_mib_s", "drain_bw_mib_s", "iops", "valid", "note")
    with open(path, "w") as f:
        f.write(",".join(cols) + "\n")
        for r in rows:
            f.write(",".join(str(r[c]) for c in cols) + "\n")


def dump_table(rows):
    devs = sorted({r["device"] for r in rows})
    fss = sorted({r["fs"] for r in rows})
    for dev in devs:
        for fs in fss:
            for wl in WORKLOADS:
                for mode in MODES:
                    grp = [
                        r for r in rows
                        if r["device"] == dev and r["fs"] == fs
                        and r["workload"] == wl and r["mode"] == mode
                    ]
                    if not grp:
                        continue
                    grp.sort(key=lambda r: r["njobs"])
                    hdr = "".join(f"{r['njobs']:>9}" for r in grp)
                    bw = "".join(
                        f"{r['drain_bw_mib_s']:>9.1f}" if r["valid"] == "1" else f"{'--':>9}"
                        for r in grp
                    )
                    wm = "".join(f"{r['write_mib']:>9.0f}" for r in grp)
                    print(f"\n[{dev}/{fs}] {wl} / {mode}")
                    print(f"  {'njobs':<16}{hdr}")
                    print(f"  {'drain_bw MiB/s':<16}{bw}")
                    print(f"  {'write MiB':<16}{wm}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    rows = parse(sys.argv[1])
    fss = sorted({r["fs"] for r in rows}) or ["-"]
    devs = sorted({r["device"] for r in rows}) or ["-"]
    print(f"解析到 {len(rows)} 轮（device={','.join(devs)}  fs={','.join(fss)}）")
    bad = [r for r in rows if r["valid"] != "1"]
    print(f"无效轮次（写量 < {MIN_VALID_MIB:g} MiB，或缺 DRAIN 计时）: {len(bad)}")
    for r in bad:
        print(f"  [{r['device']}/{r['fs']}] {r['workload']}/{r['mode']}/nj{r['njobs']:<3} {r['note']}")
    dump_table(rows)
    if len(sys.argv) > 2:
        dump_csv(rows, sys.argv[2])
        print(f"\nCSV 已写出 → {sys.argv[2]}")
