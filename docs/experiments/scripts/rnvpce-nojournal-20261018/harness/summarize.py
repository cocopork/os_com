#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Retain successful, partial and errored fio results without conflating them."""
import argparse
import csv
import json
import re
from pathlib import Path


def extract(path):
    text = re.sub(r'\x1b\[[0-9;]*[A-Za-z]', '', path.read_text(errors='replace'))
    text = re.sub(r'\[CPU-ISOLATION\] role=\w+ cpu=\d+\s*', '', text)
    start = re.search(r'BUFFER_FIO_START (\S+)', text)
    if not start:
        return None
    name = start.group(1)
    variant, rw, jobs, size = name.split('-')
    count, total = int(jobs[1:]), int(size[:-1])
    end = text.find(f'BUFFER_FIO_END {name}', start.end())
    if end == -1:
        return dict(case=name, variant=variant, rw=rw, jobs=count, total_mib=total,
                    outcome='incomplete_guest', error='', io_mib='', fio_mib_s='',
                    drained_mib_s='', mean_us='', p99_us='', fio_runtime_ms='')
    body = text[start.end():end]
    payload = None
    for match in re.finditer(r'\{', body):
        try:
            value = json.JSONDecoder().raw_decode(body[match.start():])[0]
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and 'jobs' in value:
            payload = value
            break
    if payload is None:
        raise ValueError(f'{name}: missing fio JSON')
    jobs_json = payload['jobs']
    assert len(jobs_json) == 1
    job = jobs_json[0]
    options = job['job options']
    assert options['direct'] == '0' and options['ioengine'] == 'psync'
    op = job['write']
    expected = (total // count) * count * 1048576
    timing = re.search(rf'BUFFER_COMPLETE {name} fio_status=(\d+) start_ns=(\d+) fio_end_ns=(\d+) sync_end_ns=(\d+)', text)
    if timing:
        fio_status, first, _, last = map(int, timing.groups())
        drained = op['io_bytes'] / 1048576 / ((last - first) / 1e9)
    else:
        fio_status, drained = job['error'], ''
    umount = re.search(rf'BUFFER_COMPLETE {name} .*umount_status=(\d+)', text)
    unmount_ok = umount is None or int(umount.group(1)) == 0
    success = unmount_ok and job['error'] == 0 and fio_status == 0 and op['io_bytes'] == expected and timing is not None
    outcome = 'complete' if success else ('fio_error' if job['error'] or fio_status else 'partial_or_drain_incomplete')
    # A partial write or failed checkpoint is not completed-work throughput.
    if not success:
        drained = ''
    return dict(case=name, variant=variant, rw=rw, jobs=count, total_mib=total,
                outcome=outcome, error=job['error'], io_mib=op['io_bytes'] / 1048576,
                fio_mib_s=op['bw_bytes'] / 1048576, drained_mib_s=drained,
                mean_us=op['clat_ns']['mean'] / 1000,
                p99_us=op['clat_ns'].get('percentile', {}).get('99.000000', 0) / 1000,
                fio_runtime_ms=op['runtime'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('artifacts', type=Path)
    args = parser.parse_args()
    rows = []
    for path in sorted(args.artifacts.glob('*-j*-*m.log')):
        row = extract(path)
        if row:
            rows.append(row)
    if rows:
        with (args.artifacts / 'results.csv').open('w') as out:
            writer = csv.DictWriter(out, fieldnames=list(rows[0]))
            writer.writeheader()
            writer.writerows(rows)
    for row in rows:
        print(row['case'], row['outcome'], row['error'], row['io_mib'], row['fio_mib_s'], row['drained_mib_s'])

if __name__ == '__main__':
    main()
