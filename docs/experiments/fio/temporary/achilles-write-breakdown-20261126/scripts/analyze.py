#!/usr/bin/env python3
"""Extract the Achilles write-path diagnostic snapshots into reproducible CSVs."""

from __future__ import annotations

import csv
import re
from pathlib import Path


REPO = Path(__file__).resolve().parents[6]
OUT = Path(__file__).resolve().parents[1] / "results"
RUNS = Path("/home/zn/asterinas-build-env/achilles-write-breakdown-20261126/results")
RAW = REPO / "docs/experiments/raw-device/formal/raw-device-baseline-20261123/aggregate.csv"
HISTORICAL = REPO / "docs/experiments/fio/temporary/thread-sweep-reconfigured-20261124/aggregate.csv"

CASES = {
    ("append", jobs): RUNS
    / f"e2e-nvme-rnvpce-append-write-threads-reconfigured-{jobs}-r1"
    for jobs in (4, 16, 32)
}
CASES.update(
    {
        ("overwrite", jobs): RUNS
        / f"e2e-nvme-rnvpce-overwrite-randwrite-threads-reconfigured-{jobs}-r1"
        for jobs in (4, 16, 32)
    }
)

METRIC_RE = re.compile(
    r"\[PIPELINE\] name=(?P<name>[a-z0-9_]+) count=(?P<count>\d+) "
    r"total_ns=(?P<total_ns>\d+) max_ns=(?P<max_ns>\d+) units=(?P<units>\d+)"
)
RTIO_RE = re.compile(r"\[PIPELINE\] name=rtio_multi_worker (?P<body>.*)")
QUEUE_RE = re.compile(r"\[PIPELINE\] name=block_submission_queue (?P<body>.*)")
KV_RE = re.compile(r"([a-z_]+)=([^ ]+)")
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def read_csv(path: Path) -> list[dict[str, str]]:
    with path.open(newline="") as source:
        return list(csv.DictReader(source))


def write_csv(path: Path, rows: list[dict[str, object]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as target:
        writer = csv.DictWriter(target, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def parse_log(path: Path):
    metrics: dict[str, dict[str, int]] = {}
    rtio: dict[str, str] = {}
    queues: dict[int, dict[str, str]] = {}
    for line in path.read_text(errors="replace").splitlines():
        line = ANSI_RE.sub("", line)
        match = METRIC_RE.search(line)
        if match:
            values = match.groupdict()
            name = values.pop("name")
            metrics[name] = {key: int(value) for key, value in values.items()}
            continue
        match = RTIO_RE.search(line)
        if match:
            rtio = dict(KV_RE.findall(match.group("body")))
            continue
        match = QUEUE_RE.search(line)
        if match:
            values = dict(KV_RE.findall(match.group("body")))
            queues[int(values["queue"])] = values
    return metrics, rtio, queues


def metric_us(metrics: dict[str, dict[str, int]], name: str, pages: int) -> float:
    return metrics.get(name, {}).get("total_ns", 0) / pages / 1_000


def main() -> None:
    fio_by_case = {row["case"]: row for row in read_csv(RUNS / "results.csv")}
    metric_rows: list[dict[str, object]] = []
    breakdown_rows: list[dict[str, object]] = []
    detailed_rows: list[dict[str, object]] = []
    rtio_rows: list[dict[str, object]] = []
    queue_rows: list[dict[str, object]] = []

    for (workload, jobs), directory in sorted(CASES.items()):
        metrics, rtio, queues = parse_log(directory / "qemu.log")
        fio = fio_by_case[directory.name]
        pages = metrics["nv_foreground_total"]["count"]
        migrated_pages = int(rtio["epoch_pages"])
        assert pages == migrated_pages

        for name, values in sorted(metrics.items()):
            metric_rows.append({"workload": workload, "jobs": jobs, "name": name, **values})

        allocated_pages = metrics["nv_foreground_ssd_allocate"]["units"]
        alloc = 0.0
        if allocated_pages != 0:
            alloc = sum(
                metric_us(metrics, name, pages)
                for name in (
                    "nv_foreground_ssd_allocate",
                    "nv_foreground_allocation_goal",
                    "nv_foreground_allocation_record_publish",
                    "nv_foreground_allocation_record_reclaim",
                )
            )
        capacity = metric_us(metrics, "nv_capacity_wait", pages)
        foreground = metric_us(metrics, "nv_foreground_total", pages)
        foreground_other = max(0.0, foreground - alloc - capacity)
        rtio_software = sum(
            metric_us(metrics, name, pages)
            for name in ("nv_candidate_select", "nv_target_prepare", "nv_completion_total")
        )
        driver = sum(
            metric_us(metrics, name, pages)
            for name in ("nv_async_ready_wait", "nv_device_flush")
        )
        categories = {
            "metadata_allocation": alloc,
            "rtio_software": rtio_software,
            "driver_io": driver,
            "capacity_wait": capacity,
            "foreground_other": foreground_other,
        }
        service_total = sum(categories.values())
        row: dict[str, object] = {
            "workload": workload,
            "jobs": jobs,
            "fio_mib_s": fio["fio_mib_s"],
            "fio_mean_latency_us": fio["mean_latency_us"],
            "written_pages": pages,
            "allocated_pages": allocated_pages,
        }
        for category, value in categories.items():
            row[f"{category}_us_per_page"] = f"{value:.6f}"
            row[f"{category}_pct"] = f"{100 * value / service_total:.3f}"
        row["service_demand_us_per_page"] = f"{service_total:.6f}"
        breakdown_rows.append(row)

        detail_names = (
            "nv_foreground_total",
            "nv_foreground_ssd_allocate",
            "nv_foreground_group_reserve",
            "nv_foreground_block_allocate",
            "nv_foreground_allocation_metadata",
            "nv_foreground_allocation_goal",
            "nv_foreground_allocation_record_publish",
            "nv_foreground_allocation_record_reclaim",
            "nv_foreground_plan",
            "nv_foreground_span_prepare",
            "nv_foreground_persist",
            "nv_foreground_post_commit",
            "nv_capacity_wait",
            "nv_candidate_select",
            "nv_target_prepare",
            "nv_data_submit",
            "nv_async_ready_wait",
            "nv_completion_queue_wait",
            "nv_completion_total",
            "nv_device_flush",
            "nv_extent_publish",
            "nv_reclaim_help",
        )
        detailed_rows.append(
            {
                "workload": workload,
                "jobs": jobs,
                **{
                    name.removeprefix("nv_") + "_us_per_page": f"{metric_us(metrics, name, pages):.6f}"
                    for name in detail_names
                },
            }
        )
        rtio_rows.append({"workload": workload, "jobs": jobs, **rtio})
        for queue, values in sorted(queues.items()):
            queue_rows.append({"workload": workload, "jobs": jobs, **values})

    baseline_rows: list[dict[str, object]] = []
    for row in read_csv(RAW):
        if (
            row["device"] == "nvme"
            and row["system"] == "asterinas"
            and row["axis"] == "threads"
            and row["bs_kib"] == "4"
            and int(row["jobs"]) in (4, 16, 32)
            and row["workload"] in ("append-write", "overwrite-randwrite")
        ):
            baseline_rows.append(
                {
                    "source": "asterinas_raw_device",
                    "workload": "append" if row["workload"] == "append-write" else "overwrite",
                    "jobs": row["jobs"],
                    "fio_mib_s": row["fio_mib_s_mean"],
                    "samples": row["samples"],
                }
            )
    for row in read_csv(HISTORICAL):
        if (
            row["device"] == "nvme"
            and row["mode"] == "rnvpce"
            and int(row["jobs"]) in (4, 16, 32)
            and row["workload"] in ("append-write", "overwrite-randwrite")
        ):
            baseline_rows.append(
                {
                    "source": "historical_achilles",
                    "workload": "append" if row["workload"] == "append-write" else "overwrite",
                    "jobs": row["jobs"],
                    "fio_mib_s": row["fio_mib_s_mean"],
                    "samples": row["samples"],
                }
            )

    write_csv(OUT / "metrics.csv", metric_rows)
    write_csv(OUT / "breakdown.csv", breakdown_rows)
    write_csv(OUT / "stage-details.csv", detailed_rows)
    write_csv(OUT / "rtio.csv", rtio_rows)
    write_csv(OUT / "virtio-queues.csv", queue_rows)
    write_csv(OUT / "baselines.csv", baseline_rows)


if __name__ == "__main__":
    main()
