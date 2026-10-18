#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
#
# 解析 ext4 journal-vs-nojournal 的 fio JSON 输出，生成 summary.csv，
# 并计算相对裸设备（raw device）带宽的比例。
#
# 用法：
#   python3 parse_summary.py <journal_compare_run.log> <baseline_dir> <backend> <out.csv>
#
#   <baseline_dir> : 裸设备基线目录（含 sata/ 与 nvme/ 子目录），如
#                    research1/motivation/test_dir/vm-raw-device-baseline/results/.../asterinas
#   <backend>      : sata | nvme（与测试盘对应，决定读取哪个基线子目录）
#
# 解析对象：
#   - <log> 里每轮的 "=== FIO_RESULT: ... ===" / "=== END_FIO_RESULT ===" 之间的
#     fio JSON（guest 脚本以 --output-format=json 输出）。
#   - 裸设备基线各子目录的 fio.log 里 "WRITE: bw=..." 汇总行。

import csv
import json
import os
import re
import sys

# "=== FIO_RESULT: randwrite_4K_nj1_journal_buffer ==="
# 或带后缀：randwrite_4K_nj1_journal_buffer_pa8_nf16
FIO_RESULT_RE = re.compile(
    r"^=== FIO_RESULT:\s*([^_\s]+)_([^_\s]+)_nj(\d+)_([^_\s]+)_([^_\s]+?)(?:_pa(\d+))?(?:_nf(\d+))?\s*===\s*$"
)


def parse_raw_bw_mib(text: str):
    """从 fio 文本汇总行 'WRITE: bw=21.4MiB/s (22.4MB/s)' 提取 MiB/s 带宽。"""
    m = re.search(r"WRITE:\s+bw=([0-9.]+)(KiB/s|MiB/s|GiB/s)", text)
    if not m:
        return None
    val = float(m.group(1))
    mult = {"KiB/s": 1.0 / 1024, "MiB/s": 1.0, "GiB/s": 1024.0}[m.group(2)]
    return val * mult


def normalize_bs(bs: str) -> str:
    """'4k'/'4K' → '4K'；'128k'/'128K' → '128K'。"""
    return bs.upper()


def load_baseline(baseline_dir: str, backend: str):
    """建立 {(bs_norm, numjobs) → raw_bw_mib} 映射。

    基线子目录命名形如 asterinas-sata-bs-bs4k-nj1-qd1 或
    asterinas-sata-numjobs-bs4k-nj4-qd1，其中 bs 与 nj 用 -bs<N>k?-nj<N>- 编码。
    """
    mapping = {}
    root = os.path.join(baseline_dir, backend)
    if not os.path.isdir(root):
        return mapping
    # 同一 (bs, nj) 可能同时出现在 bs 序列与 numjobs 序列两个子目录中，
    # 累加后取平均，避免目录遍历顺序影响结果。
    sums = {}
    for name in sorted(os.listdir(root)):
        m = re.search(r"-bs(\d+k?)-nj(\d+)-", name)
        if not m:
            continue
        bs_norm = normalize_bs(m.group(1))
        nj = m.group(2)
        fio_log = os.path.join(root, name, "fio.log")
        if not os.path.isfile(fio_log):
            continue
        with open(fio_log, "r", errors="replace") as fh:
            text = fh.read()
        bw = parse_raw_bw_mib(text)
        if bw is None:
            continue
        key = (bs_norm, nj)
        if key in sums:
            sums[key][0] += bw
            sums[key][1] += 1
        else:
            sums[key] = [bw, 1]
    return {key: v[0] / v[1] for key, v in sums.items()}


def parse_rounds(log_text: str):
    """按 FIO_RESULT/END_FIO_RESULT 标记切分每轮，返回 (rw, bs, nj, jm, wt, pa, nf, raw_json_text)。"""
    rounds = []
    lines = log_text.splitlines()
    i = 0
    n = len(lines)
    while i < n:
        m = FIO_RESULT_RE.match(lines[i].strip())
        if m:
            rw, bs, nj, jm, wt = m.group(1, 2, 3, 4, 5)
            pa = m.group(6) or "0"
            nf = m.group(7) or "1"
            buf = []
            i += 1
            while i < n and "=== END_FIO_RESULT ===" not in lines[i]:
                buf.append(lines[i])
                i += 1
            rounds.append((rw, bs, nj, jm, wt, pa, nf, "\n".join(buf)))
        i += 1
    return rounds


def extract_write(json_text: str):
    """从 fio JSON 文本提取写带宽（bytes/s）与 IOPS（所有 job 求和）。

    fio JSON 根对象为 {"jobs":[{"write":{"bw":KiB/s,"bw_bytes":bytes/s,"iops":...}}...]}。
    返回 (bw_bytes_total, iops_total)，失败返回 None。
    """
    # fio JSON 是输出末尾的一整段；journal abort 等 ERROR 日志（`Error { ...}`）
    # 会夹带 `{` 干扰定位。从后往前找每个 `{`，取第一个能解析出含 `jobs`
    # 数组的根对象。
    starts = [i for i, ch in enumerate(json_text) if ch == "{"]
    for start in reversed(starts):
        try:
            obj, _ = json.JSONDecoder().raw_decode(json_text[start:])
        except (json.JSONDecodeError, ValueError):
            continue
        jobs = obj.get("jobs") if isinstance(obj, dict) else None
        if not isinstance(jobs, list) or not jobs:
            continue
        break
    else:
        return None

    bw_bytes_total = 0
    iops_total = 0.0
    found = False
    for job in jobs:
        w = job.get("write") or {}
        if not w:
            continue
        bw_bytes = w.get("bw_bytes")
        bw_kib = w.get("bw")
        if bw_bytes is None and bw_kib is not None:
            bw_bytes = float(bw_kib) * 1024.0
        if bw_bytes is None:
            continue
        bw_bytes_total += float(bw_bytes)
        iops = w.get("iops")
        if iops is not None:
            iops_total += float(iops)
        found = True

    if not found:
        return None
    return (bw_bytes_total, iops_total)


def main():
    if len(sys.argv) < 5:
        print(
            "Usage: parse_summary.py <log> <baseline_dir> <backend> <out.csv>",
            file=sys.stderr,
        )
        return 2

    log_path, baseline_dir, backend, out_csv = sys.argv[1:5]

    with open(log_path, "r", errors="replace") as fh:
        log_text = fh.read()

    mapping = load_baseline(baseline_dir, backend)

    rows = []
    for rw, bs, nj, jm, wt, pa, nf, raw in parse_rounds(log_text):
        ex = extract_write(raw)
        if ex is None:
            # JSON 缺失/解析失败（如轮次超时被杀）：留空带宽/IOPS/比例。
            rows.append((rw, bs, nj, jm, wt, pa, nf, None, None, None, None))
            continue
        bw_bytes, iops = ex
        ext4_mib = bw_bytes / (1024 * 1024)
        raw_bw = mapping.get((normalize_bs(bs), nj))
        ratio = ext4_mib / raw_bw if raw_bw else None
        rows.append((rw, bs, nj, jm, wt, pa, nf, ext4_mib, iops, raw_bw, ratio))

    with open(out_csv, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(
            [
                "rw",
                "bs",
                "numjobs",
                "journal_mode",
                "writetype",
                "prealloc_blocks",
                "nrfiles",
                "bw_mib_per_s",
                "iops",
                "raw_bw_mib_per_s",
                "ratio_to_raw",
            ]
        )
        for rw, bs, nj, jm, wt, pa, nf, ext4_mib, iops, raw_bw, ratio in rows:
            w.writerow(
                [
                    rw,
                    bs,
                    nj,
                    jm,
                    wt,
                    pa,
                    nf,
                    f"{ext4_mib:.2f}" if ext4_mib is not None else "",
                    f"{iops:.1f}" if iops is not None else "",
                    f"{raw_bw:.2f}" if raw_bw is not None else "",
                    f"{ratio:.3f}" if ratio is not None else "",
                ]
            )

    print(f"[parse] {len(rows)} rounds → {out_csv}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
