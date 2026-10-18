#!/usr/bin/env python3
"""Parse matched NVPage lifecycle timestamps into additive latency stages."""

import argparse
import csv
import json
import re
import statistics
from pathlib import Path


PHASES = [
    ("write_publish", "User copy start -> NV reference committed"),
    ("resident_wait", "NV reference committed -> migration claim"),
    ("migration_prepare", "Migration claim -> BIO submit"),
    ("bio_latency", "BIO submit -> IRQ completion"),
    ("irq_queue_wait", "IRQ completion -> worker queue drain"),
    ("materialize_queue_wait", "Worker drain -> this page materialization start"),
    ("materialize", "Native EXT4 SSD mapping update"),
    ("batch_wait", "Materialization end -> batch durability start"),
    ("durability", "Batch journal commit or metadata sync + device flush"),
    ("publication_queue_wait", "Batch durable -> this page publication start"),
    ("publication", "NV -> SSD extent publication + reservation release"),
    ("reclaim", "Descriptor clear + reverse removal + NVPage free"),
]


def percentile(values, fraction):
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("result_dir", type=Path)
    args = parser.parse_args()
    log = (args.result_dir / "logs/baseline_run.log").read_text(errors="replace")
    log = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", log)
    pattern = r"\[NVPAGE-LIFECYCLE\] inode=(\d+) page=(\d+) seq=(\d+) times_ns=(\[[\d, ]+\])"
    samples = {}
    rejected = 0
    for inode, page, sequence, raw in re.findall(pattern, log):
        times = json.loads(raw)
        if len(times) not in (13, 14) or any(t == 0 for t in times[:13]) or times[:13] != sorted(times[:13]):
            rejected += 1
            continue
        if len(times) == 14 and not times[4] <= times[13] <= times[5]:
            rejected += 1
            continue
        durations = [b - a for a, b in zip(times[:13], times[1:13])]
        assert sum(durations) == times[12] - times[0]
        key = (int(inode), int(page), int(sequence), times[0])
        samples[key] = (times, durations)
    if not samples:
        raise SystemExit("No valid completed NVPage lifecycle samples found")

    with (args.result_dir / "nvpage-lifecycle-samples.csv").open("w", newline="") as stream:
        writer = csv.writer(stream)
        writer.writerow(["inode", "logical_page", "allocation_seq"] +
                        [f"t{i}_ns" for i in range(14)] +
                        [f"{name}_ms" for name, _ in PHASES] + ["total_ms"])
        for (inode, page, sequence, _), (times, durations) in samples.items():
            writer.writerow([inode, page, sequence] + times + ([0] if len(times) == 13 else []) +
                            [t / 1e6 for t in durations] + [(times[12] - times[0]) / 1e6])

    columns = [(name, [durations[i] / 1e6 for _, durations in samples.values()])
               for i, (name, _) in enumerate(PHASES)]
    totals = [(times[12] - times[0]) / 1e6 for times, _ in samples.values()]
    migrations = [(times[12] - times[2]) / 1e6 for times, _ in samples.values()]
    queue_split = [("irq_worker_busy", [(times[13] - times[4]) / 1e6
                                      for times, _ in samples.values() if len(times) == 14]),
                   ("irq_dispatch_delay", [(times[5] - times[13]) / 1e6
                                          for times, _ in samples.values() if len(times) == 14])]
    mean_total = statistics.mean(totals)
    rows = []
    for name, values in columns + [("total", totals), ("claim_to_reclaim", migrations)] + queue_split:
        if not values:
            continue
        rows.append([name, len(values), statistics.mean(values),
                     percentile(values, .5), percentile(values, .95), max(values),
                     statistics.mean(values) / mean_total * 100])
    with (args.result_dir / "nvpage-lifecycle-summary.csv").open("w", newline="") as stream:
        writer = csv.writer(stream)
        writer.writerow(["phase", "samples", "mean_ms", "p50_ms", "p95_ms", "max_ms", "share_pct"])
        writer.writerows(rows)

    report = [
        "# NVPage lifecycle latency",
        "",
        "NVMe / rNVPCe / random 4 KiB / 16 jobs / KVM / 30 s; "
        "256 MiB file range, 64 MiB NVPool (16 MiB metadata + 48 MiB data).",
        "",
        f"Valid successfully reclaimed samples: {len(samples)}; rejected: {rejected}. "
        "Sampling: every 128th successful page completion; at most 512 samples per dump.",
        "Start: last user-to-NVPage copy start. End: generation-checked NVPage free returns. "
        "Includes NV residency and queue waiting. Excludes write syscall entry, "
        "capacity admission and allocation before the copy. Earlier overwrites are coalesced.",
        "",
        "| Phase | Mean ms | P50 ms | P95 ms | Max ms | Share |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for name, _, mean, p50, p95, maximum, share in rows:
        report.append(f"| {name} | {mean:.4f} | {p50:.4f} | {p95:.4f} | {maximum:.4f} | {share:.2f}% |")
    report += ["", "Phase boundaries:", ""]
    report += [f"- `{name}`: {description}." for name, description in PHASES]
    report += [
        "",
        "The 12 stage means add exactly to the end-to-end mean (before rounding). "
        "Percentiles and maxima cannot be added. Batch durability is shared wall time "
        "experienced by each page, not per-page CPU service time. BIO latency includes "
        "guest block/virtio, QEMU and host/device time. Instrumentation affects throughput.",
        "With completion_leaf_batch=true, publication starts at the inode group's start "
        "for all its pages. Its interval includes shared group work and earlier pages' "
        "reclaim; it is experienced wall time, not additive per-page service. Use "
        "rnvpce_completion_group total divided by group_pages for amortized service.",
        "Only successfully reclaimed pages are represented. Pages still resident at test end "
        "and failed attempts are not independent completed samples; retries may be included "
        "in the residence wait. This is one run, not a device-independent latency guarantee.",
        "`irq_worker_busy` is time awaiting the prior completion batch's end; it includes "
        "that batch's own blocking and preemption. `irq_dispatch_delay` is the remaining "
        "IRQ-to-drain delay after the worker is eligible. The two add to `irq_queue_wait`. "
        "The latter includes enqueue/wake/scheduling and queue-drain overhead; it is not "
        "a pure scheduler or wake-call measurement.",
        "",
        "Final cumulative function counters (includes all instrumented pages/threads, "
        "not restricted to the lifecycle sample; nested counters overlap):",
        "",
        "| Counter | Count | Total ms | Mean us | Max us |",
        "|---|---:|---:|---:|---:|",
    ]
    counters = {}
    for name, count, total, mean, maximum in re.findall(
            r"\[LATENCY\] (\w+): count=(\d+) total_us=(\d+) avg_us=(\d+) max_us=(\d+)", log):
        counters[name] = (int(count), int(total), int(mean), int(maximum))
    for name in ["nvpc_completion_wake", "rnvpce_completion_group", "rnvpce_metadata_reader_wait", "rnvpce_publication_wait",
                 "rnvpce_publication_hold", "rnvpce_completion_inner_wait",
                 "rnvpce_completion_inner_hold", "nvpc_flush_materialize",
                 "nvpc_flush_durability", "nvpc_flush_migrate",
                 "nvpc_flush_reclaim", "nvpc_flush_complete"]:
        if name in counters:
            count, total, mean, maximum = counters[name]
            report.append(f"| {name} | {count} | {total / 1000:.3f} | {mean} | {maximum} |")
    (args.result_dir / "nvpage-lifecycle-analysis.md").write_text("\n".join(report) + "\n")
    print(f"Samples={len(samples)}, total mean={mean_total:.4f} ms, "
          f"P95={percentile(totals, .95):.4f} ms")
    for row in rows:
        print(f"{row[0]}: mean={row[2]:.4f} ms share={row[6]:.2f}%")


if __name__ == "__main__":
    main()
