#!/usr/bin/env python3
import json
import subprocess
import time
from pathlib import Path


ROOT = Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
BASE = Path('/home/zn/asterinas-build-env/orderless-migration-20261019')
HARNESS = ROOT / 'docs/os_com_implementation/test/rnvpce-buffer-compare'
AUX_IMAGES = Path('/home/zn/asterinas-build-env/rnvpce-nojournal-20261018/final-export')


def main():
    output = BASE / 'throughput-gated'
    output.mkdir(exist_ok=True)
    order = []
    for repeat in range(1, 4):
        for rw in ['randwrite', 'write']:
            for jobs in [1, 16]:
                versions = ['before', 'final'] if repeat % 2 else ['final', 'before']
                for version in versions:
                    images = BASE / 'release-images' / version
                    artifacts = output / f'round-{repeat}' / version
                    artifacts.mkdir(parents=True, exist_ok=True)
                    for name in ['selected.iso', 'ext2.img', 'exfat.img']:
                        destination = artifacts / name
                        if destination.exists():
                            continue
                        source = images / name if name == 'selected.iso' else AUX_IMAGES / name
                        subprocess.run(
                            ['cp', '--reflink=auto', '--sparse=always', str(source), str(destination)],
                            check=True,
                        )
                    case = {'round': repeat, 'version': version, 'rw': rw, 'jobs': jobs}
                    order.append(case)
                    (output / 'order.json').write_text(json.dumps(order, indent=2) + '\n')
                    print(time.strftime('%H:%M:%S'), 'START', case, flush=True)
                    subprocess.run(
                        [
                            'python3', str(HARNESS / 'run.py'), 'nvme',
                            '--variant', 'rnvpce', '--rw', rw, '--jobs', str(jobs),
                            '--total-mib', '2048', '--runtime', '180', '--journal', 'off',
                            '--format-authorized', '--kernel-source-diff',
                            str(images / 'kernel-source.diff'), '--artifacts', str(artifacts),
                        ],
                        cwd=ROOT,
                        check=True,
                    )
                    subprocess.run(
                        ['python3', str(HARNESS / 'summarize.py'), str(artifacts)], check=True
                    )
                    print(time.strftime('%H:%M:%S'), 'DONE', case, flush=True)
    print('AB_MATRIX_COMPLETE', len(order), flush=True)


if __name__ == '__main__':
    main()
