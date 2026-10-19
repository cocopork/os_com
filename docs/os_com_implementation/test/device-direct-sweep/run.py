#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Explicitly authorized partition-only guest EXT4 benchmark; saves raw serial output."""
import argparse
import hashlib
import json
import re
import shlex
import subprocess
from pathlib import Path

from summarize import extract

ROOT = Path(__file__).resolve().parents[4]
IMAGE = 'os-com-recovery-tools:20261017'
ALLOWED = {'nvme': '/dev/nvme1n1p1', 'sata': '/dev/sda4'}
EXPECTED_BYTES = {'nvme': 274877906944, 'sata': 274877890560}

def docker(args, **kwargs):
    return subprocess.run(['sg', 'docker', '-c', shlex.join(['docker', *args])], check=True, **kwargs)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('device', choices=ALLOWED)
    parser.add_argument('--artifacts', type=Path, required=True)
    parser.add_argument('--format-authorized', action='store_true')
    parser.add_argument('--skip-format', action='store_true')
    args = parser.parse_args()
    if not args.format_authorized:
        parser.error('Requires prior user authorization and --format-authorized')
    target = ALLOWED[args.device]
    info = json.loads(subprocess.check_output(['lsblk', '-Jb', '-o', 'PATH,TYPE,SIZE,MOUNTPOINTS', target]))['blockdevices'][0]
    if info['type'] != 'part' or info['size'] != EXPECTED_BYTES[args.device] or any(info.get('mountpoints') or []):
        raise RuntimeError(f'Target must be the unmounted 256 GiB partition: {info}')
    if target in Path('/proc/swaps').read_text():
        raise RuntimeError('Target is swap')
    art = args.artifacts.resolve()
    art.mkdir(parents=True, exist_ok=True)
    if (art / f'{args.device}.log').exists():
        raise RuntimeError('Refusing to overwrite an existing run log')
    (art / f'{args.device}-manifest.json').write_text(json.dumps({
        'device': target, 'lsblk': info, 'commit': subprocess.check_output(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'], text=True).strip(),
        'iso_sha256': hashlib.sha256((art / 'selected.iso').read_bytes()).hexdigest(),
        'cpus': 64, 'memory': '8G', 'queue_depth': 1024,
        'direct': 1, 'engine': 'psync', 'fsync': 0, 'total_mib': 2048,
        'size_policy': 'floor(2048/jobs) MiB per independent inode; one pass, runtime cap 600s',
    }, indent=2) + '\n')
    base = ['run', '--rm', '--network', 'none', '--device', f'{target}:/dev/benchmark-target:rwm']
    if not args.skip_format:
        docker([*base, IMAGE, 'mke2fs', '-F', '-q', '-t', 'ext4', '-b', '4096', '-I', '256',
            '-E', 'nodiscard,lazy_itable_init=0,lazy_journal_init=0', '-O',
            'extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^orphan_file,has_journal', '/dev/benchmark-target'])
    guest = Path(__file__).with_name('guest.sh')
    for command in ['rm /rebuild-fio.sh', f'write {guest} /rebuild-fio.sh']:
        subprocess.run(['debugfs', '-w', '-R', command, str(art / 'ext2.img')], check=True, capture_output=True)
    qemu = ['timeout', '--signal=TERM', '--kill-after=30', '14400', 'qemu-system-x86_64',
        '-enable-kvm', '-cpu', 'host', '-machine', 'q35', '-smp', '64', '-m', '8G',
        '-bios', '/root/ovmf/release/OVMF.fd', '-no-reboot', '-display', 'none',
        '-serial', 'stdio', '-monitor', 'none', '-nic', 'none',
        '-device', 'isa-debug-exit,iobase=0xf4,iosize=0x04', '-cdrom', '/benchmark/selected.iso']
    for name, file in [('bootdata', '/benchmark/ext2.img'), ('aux', '/benchmark/exfat.img'), ('target', '/dev/benchmark-target')]:
        suffix = ',cache=none,aio=threads' if name == 'target' else ''
        qemu += ['-drive', f'if=none,format=raw,id={name},file={file}{suffix}']
        device = f'virtio-blk-pci,drive={name},disable-legacy=on'
        if name == 'target':
            device += ',queue-size=1024,num-queues=1,request-merging=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off'
        qemu += ['-device', device]
    command = ['docker', *base, '--name', 'os-com-device-sweep', '--device', '/dev/kvm',
        '-v', f'{ROOT}:/root/asterinas:ro', '-v', f'{art}:/benchmark', IMAGE, *qemu]
    with (art / f'{args.device}.log').open('w') as log:
        result = subprocess.run(['sg', 'docker', '-c', shlex.join(command)], stdout=log, stderr=subprocess.STDOUT)
    text = (art / f'{args.device}.log').read_text(errors='replace')
    if result.returncode not in (0, 33) or 'SWEEP_COMPLETE' not in text:
        raise RuntimeError(f'Incomplete guest run: exit {result.returncode}; inspect serial log')
    rows = extract(art / f'{args.device}.log')
    if len(rows) != 72:
        raise RuntimeError(f'Incomplete fio matrix: {len(rows)}/72')
    with (art / f'{args.device}-e2fsck.log').open('w') as log:
        docker([*base, IMAGE, 'e2fsck', '-fn', '/dev/benchmark-target'], stdout=log, stderr=subprocess.STDOUT)
    print(f'{args.device}: guest complete and e2fsck clean', flush=True)

if __name__ == '__main__':
    main()
