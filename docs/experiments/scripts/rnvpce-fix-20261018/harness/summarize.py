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
    # Serial writes can split an ANSI terminator and leave ESC+[digits directly
    # before a newline. Removing the fragment restores the surrounding fio JSON.
    text = re.sub(r'\x1b\[[0-9;]*(?=\s)', '', text)
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
    # The 64-vCPU UART can split color reset bytes from kernel output into fio's
    # JSON stream. These two forms are invalid JSON and unambiguously fragments.
    body = re.sub(r'(?<=\d)m(?=[,\s])', '', body)
    body = re.sub(r'(?m)^(\s*)[0-9;m\[]+(\s+")', r'\1\2', body)
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
        # The emulated UART occasionally splices fragments of ANSI reset bytes
        # into otherwise valid JSON. Recover only the fixed scalar fields used
        # by this harness; the raw log remains the source of truth.
        def scalar(section, key, number=r'[0-9.]+'):
            match = re.search(rf'"{re.escape(key)}"\s*:\s*({number})', section)
            if not match:
                raise ValueError(f'{name}: missing fio {key}')
            return match.group(1)

        write_match = re.search(r'"write"\s*:\s*\{', body)
        write_start = write_match.start() if write_match else -1
        write_end = body.find('"trim"', write_start)
        if write_start == -1 or write_end == -1:
            raise ValueError(f'{name}: missing fio JSON')
        write = body[write_start:write_end]
        clat_start = write.find('"clat_ns"')
        clat_end = write.find('"lat_ns"', clat_start + 1)
        clat = write[clat_start:clat_end]
        p99 = re.search(r'"99\.000000"\s*:\s*([0-9.]+)', clat)
        job = {
            'error': int(scalar(body, 'error', r'\d+')),
            'job options': {
                'direct': scalar(body, 'direct', r'"?\d+"?').strip('"'),
                'ioengine': re.search(r'"ioengine"\s*:\s*"([^"]+)"', body).group(1),
            },
            'write': {
                'io_bytes': int(scalar(write, 'io_bytes', r'\d+')),
                'bw_bytes': int(scalar(write, 'bw_bytes', r'\d+')),
                'runtime': int(scalar(write, 'runtime', r'\d+')),
                'clat_ns': {
                    'mean': float(scalar(clat, 'mean')),
                    'percentile': {'99.000000': float(p99.group(1)) if p99 else 0},
                },
            },
        }
    else:
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
