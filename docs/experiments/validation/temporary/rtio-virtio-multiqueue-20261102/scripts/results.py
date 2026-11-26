#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Parse the matrix's fio, sync and queue evidence without accepting short writes."""
import json
from pathlib import Path
import re
import sys


def mechanism_gates(profiles, queues, hctx_accepted):
    workers = {int(row['worker']): row for row in profiles
               if row['name'] == 'rtio_worker' and row.get('device') == 'vdc'}
    block = {int(row['queue']): row for row in profiles
             if row['name'] == 'block_submission_queue' and row.get('device') == 'vdc'}
    epochs = [row for row in profiles if row['name'] == 'rtio_multi_worker']
    last = epochs[-1] if epochs else {}
    return {
        'workers_use_distinct_queues': set(workers) == set(range(queues)) and all(
            int(row['queue']) == worker and int(row['submitted_bios']) > 0
            for worker, row in workers.items()),
        'queues_drained': set(block) == set(range(queues)) and all(
            row['in_flight_requests'] == '0' and row['staging_depth'] == '0'
            for row in block.values()),
        'owner_queues_drained': bool(workers) and all(
            row.get('data_complete_depth') == '0'
            and row.get('durable_result_depth') == '0'
            and row.get('queued_dirty_pages') == '0'
            and row.get('in_flight_pages') == '0'
            and row.get('completion_pending_pages') == '0'
            and row.get('owned_inodes') == '0'
            for row in workers.values()),
        'one_flush_per_epoch': bool(last) and last['configured'] == str(queues)
            and last['active'] == '0' and int(last['epochs']) > 0
            and last['epochs'] == last['device_flushes'],
        'host_hctx': queues == 1 or hctx_accepted,
    }


def parse_guest(raw, case):
    raw = raw.replace('\r', '')
    raw = re.sub(r'\x1b\[[0-9;]*m', '', raw)
    raw = re.sub(r'\x1b\[(?=\s*(?:\"|-?\d))', '', raw)
    # Kernel serial output can interrupt a userspace write even inside a JSON key.
    raw = re.sub(r'\[CPU-ISOLATION\] role=\w+ cpu=\d+\n', '', raw)
    match = re.search(r'^PRELIM_COMPLETE ' + re.escape(case) + r' (.*)$', raw, re.M)
    if not match:
        raise ValueError('missing guest completion')
    completion = dict(re.findall(r'(\w+)=(\d+)', match[1]))
    if any(completion.get(key) != '0' for key in ('fio_status', 'sync_status', 'umount_status')):
        raise ValueError('fio, sync or unmount failed')
    fio_raw = raw.split(f'PRELIM_FIO_START {case}\n', 1)[1].split(f'PRELIM_FIO_END {case}', 1)[0]
    fio, _ = json.JSONDecoder().raw_decode(fio_raw[fio_raw.index('{'):])
    jobs = fio['jobs']
    if len(jobs) != 1 or jobs[0]['error'] != 0 or jobs[0]['write']['io_bytes'] != 2 * 1024**3:
        raise ValueError('fio failed or did not write exactly 2 GiB')
    write = jobs[0]['write']
    elapsed = (int(completion['sync_end_ns']) - int(completion['start_ns'])) / 1e9
    if elapsed <= 0:
        raise ValueError('invalid elapsed time')
    profiles = []
    for payload in re.findall(r'\[PIPELINE\] (.*)', raw):
        profiles.append(dict(re.findall(r'(\w+)=([^\s]+)', payload)))
    return {'case': case, 'fio_mib_s': write['bw_bytes'] / 1024**2,
            'including_sync_mib_s': 2048 / elapsed,
            'mean_latency_ns': write['lat_ns']['mean'],
            'p99_latency_ns': write['clat_ns'].get('percentile', {}).get('99.000000'),
            'completion': completion, 'profiles': profiles}


def main():
    point = Path(sys.argv[1])
    result = parse_guest((point / 'qemu.log').read_bytes().decode(), point.name)
    if (point / 'e2fsck.status').read_text().strip() != '0':
        raise ValueError('e2fsck failed')
    result['hctx'] = json.loads((point / 'hctx-counts.json').read_text())
    result['mapping'] = json.loads((point / 'mapping.json').read_text())
    result['mechanism_gates'] = mechanism_gates(
        result['profiles'], len(result['mapping']), result['hctx']['accepted'])
    print(json.dumps(result, indent=2))

if __name__ == '__main__':
    main()
