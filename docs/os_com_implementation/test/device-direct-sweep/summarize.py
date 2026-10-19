#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Extract completed fio JSON records and plot both sweep axes."""
import argparse
import csv
import json
import re
from pathlib import Path


def extract(path):
    text = re.sub(r'\[CPU-ISOLATION\] role=\w+ cpu=\d+\s*', '', path.read_text(errors='replace'))
    rows = []
    for match in re.finditer(r'SWEEP_START (\S+)\s+(.*?)SWEEP_END \1', text, re.S):
        name, body = match.groups()
        axis, value, rw = name.split('-')
        payload = json.JSONDecoder().raw_decode(body[body.index('{'):])[0]
        jobs = payload['jobs']
        if any(job['error'] != 0 for job in jobs):
            raise ValueError(f'{path}: {name}: fio error')
        # fio group_reporting emits one aggregate job for this group.
        if len(jobs) != 1:
            raise ValueError(f'Unexpected group count for {name}')
        options = jobs[0]['job options']
        required = {'ioengine': 'psync', 'direct': '1', 'iodepth': '1', 'fsync': '0'}
        if any(options.get(key) != value for key, value in required.items()):
            raise ValueError(f'{name}: unexpected fio options {options}')
        op = jobs[0]['read' if rw in ('read', 'randread') else 'write']
        count = int(value) if axis == 'threads' else 1
        expected = (2048 // count) * count * 1024 * 1024
        if op['io_bytes'] != expected:
            raise ValueError(f'{name}: short I/O {op["io_bytes"]} != {expected}')
        latency = op['clat_ns']
        rows.append(dict(device=path.stem, axis=axis, value=int(value), rw=rw,
                         jobs=count, bs_kib=4 if axis == 'threads' else int(value),
                         mib_s=op['bw_bytes'] / 1048576, iops=op['iops'],
                         mean_us=latency['mean'] / 1000,
                         p99_us=latency.get('percentile', {}).get('99.000000', 0) / 1000,
                         runtime_ms=op['runtime'], io_bytes=op['io_bytes']))
    keys = {(r['axis'], r['value'], r['rw']) for r in rows}
    if len(keys) != len(rows):
        raise ValueError('Duplicate cases')
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('artifacts', type=Path)
    parser.add_argument('--require-complete', action='store_true')
    args = parser.parse_args()
    rows = []
    for device in ['sata', 'nvme']:
        path = args.artifacts / f'{device}.log'
        if path.exists():
            rows.extend(extract(path))
    if args.require_complete:
        for device in ['sata', 'nvme']:
            selected = [row for row in rows if row['device'] == device]
            expected = {(axis, value, rw) for axis, values in [('threads', [1,4,8,12,16,20,24,28,32]), ('bs', [4,8,16,32,64,128,256,512,1024])] for value in values for rw in ['randwrite','randread','write','read']}
            actual = {(row['axis'], row['value'], row['rw']) for row in selected}
            if actual != expected:
                raise ValueError(f'{device}: incomplete matrix ({len(actual)}/72)')
    if not rows:
        print('No completed points yet')
        return
    with (args.artifacts / 'results.csv').open('w') as out:
        writer = csv.DictWriter(out, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    for axis in ['threads', 'bs']:
        for metric, label in [('mib_s', 'Bandwidth (MiB/s)'), ('iops', 'IOPS')]:
            fig, panels = plt.subplots(2, 2, figsize=(11, 7), constrained_layout=True)
            for panel, rw in zip(panels.flat, ['randwrite', 'randread', 'write', 'read']):
                for device in ['sata', 'nvme']:
                    selected = sorted([r for r in rows if r['device'] == device and r['axis'] == axis and r['rw'] == rw], key=lambda r: r['value'])
                    if selected:
                        panel.plot([r['value'] for r in selected], [r[metric] for r in selected], marker='o', label=device.upper())
                if axis == 'bs':
                    panel.set_xscale('log', base=2)
                    panel.set_xticks([4,8,16,32,64,128,256,512,1024], labels=['4','8','16','32','64','128','256','512','1024'])
                else:
                    panel.set_xticks([1,4,8,12,16,20,24,28,32])
                panel.set(title=rw, xlabel='Threads' if axis == 'threads' else 'Block size (KiB)', ylabel=label)
                panel.grid(alpha=.3)
                if panel.lines:
                    panel.legend()
            fig.suptitle('Asterinas EXT4 / psync / direct=1 / 2 GiB single pass')
            for extension in ['png', 'svg']:
                fig.savefig(args.artifacts / f'{axis}-{metric}.{extension}', dpi=160)
            plt.close(fig)
    print(json.dumps({device: sum(r['device'] == device for r in rows) for device in ['sata','nvme']}))

if __name__ == '__main__':
    main()
