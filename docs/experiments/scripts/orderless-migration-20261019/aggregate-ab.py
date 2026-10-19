import csv
import hashlib
import importlib.util
import json
import re
import statistics
from pathlib import Path


base = Path('/home/zn/asterinas-build-env/orderless-migration-20261019')
root = Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
spec = importlib.util.spec_from_file_location(
    'summarize', root / 'docs/os_com_implementation/test/rnvpce-buffer-compare/summarize.py'
)
summarize = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summarize)
output = base / 'throughput-gated'
rows = []
for folder in sorted(output.glob('round-*/*')):
    version = folder.name
    images = base / 'release-images' / version
    for status_file in sorted(folder.glob('*-status.json')):
        case = status_file.name.removesuffix('-status.json')
        row = summarize.extract(folder / f'{case}.log')
        assert row['outcome'] == 'complete' and row['error'] == 0 and row['io_mib'] == 2048, row
        status = json.loads(status_file.read_text())
        assert status == {'qemu_exit': 33, 'e2fsck_exit': 0, 'guest_complete': True}, status
        manifest = json.loads((folder / f'{case}-manifest.json').read_text())
        assert manifest['requested_disk_journal'] == 'off'
        assert not manifest['expected_runtime_journal']
        assert manifest['iso_sha256'] == hashlib.sha256((images / 'selected.iso').read_bytes()).hexdigest()
        assert manifest['kernel_diff_sha256'] == hashlib.sha256(
            (images / 'kernel-source.diff').read_bytes()
        ).hexdigest()
        features = re.search(
            r'^Filesystem features:\s*(.*)$',
            (folder / f'{case}-filesystem.txt').read_text(),
            re.MULTILINE,
        ).group(1).split()
        assert 'has_journal' not in features
        row.update(round=int(folder.parent.name.split('-')[1]), version=version)
        rows.append(row)

with (output / 'all-results.csv').open('w') as stream:
    writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
    writer.writeheader()
    writer.writerows(rows)

summary = []
for rw in ['randwrite', 'write']:
    for jobs in [1, 16]:
        for version in ['before', 'final']:
            group = [
                row for row in rows
                if (row['rw'], row['jobs'], row['version']) == (rw, jobs, version)
            ]
            assert len(group) == 3
            item = {'rw': rw, 'jobs': jobs, 'version': version, 'repeats': len(group)}
            for key in ['fio_mib_s', 'drained_mib_s', 'mean_us', 'p99_us']:
                values = [row[key] for row in group]
                item[key] = statistics.median(values)
                item[f'{key}_min'] = min(values)
                item[f'{key}_max'] = max(values)
            summary.append(item)
(output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
assert len(rows) == 24
print('validated', len(rows))
for item in summary:
    print(
        item['rw'], item['jobs'], item['version'],
        f"fio={item['fio_mib_s']:.2f}", f"drained={item['drained_mib_s']:.2f}",
    )
