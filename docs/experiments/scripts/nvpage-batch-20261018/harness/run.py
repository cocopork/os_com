#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Compares EXT4 buffered writes and rNVPCe on an authorized NVMe partition."""
import argparse
import hashlib
import json
import re
import shlex
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[5]
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
    parser.add_argument('--kernel-source-diff', type=Path,
                        help='Frozen kernel patch used to build selected.iso; required for historical A/B provenance')
    parser.add_argument('--skip-format', action='store_true')
    parser.add_argument('--variant', choices=['ext4', 'rnvpce'], required=True)
    parser.add_argument('--rw', choices=['randwrite', 'write'], required=True)
    parser.add_argument('--jobs', type=int, choices=[1,16], required=True)
    parser.add_argument('--total-mib', type=int, default=2048)
    parser.add_argument('--runtime', type=int, default=60)
    parser.add_argument('--journal', choices=['on', 'off'], default='on',
                        help='On-disk journal feature; rNVPCe always runs without JBD2')
    args = parser.parse_args()
    if not 1 <= args.runtime <= 600:
        parser.error('runtime must be between 1 and 600 seconds')
    if args.total_mib not in [16,32,2048]:
        parser.error('Supported total sizes: 16,32,2048 MiB')
    case = f'{args.variant}-{args.rw}-j{args.jobs}-{args.total_mib}m'
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
    if (art / f'{case}.log').exists():
        raise RuntimeError('Refusing to overwrite an existing run log')
    (art / f'{case}-manifest.json').write_text(json.dumps({
        'device': target, 'lsblk': info, 'commit': subprocess.check_output(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'], text=True).strip(),
        'iso_sha256': hashlib.sha256((art / 'selected.iso').read_bytes()).hexdigest(),
        'cpus': 64, 'memory': '8G', 'queue_depth': 1024,
        'direct': 0, 'engine': 'psync', 'fsync': 0, 'total_mib': args.total_mib,
        'variant': args.variant, 'rw': args.rw, 'jobs': args.jobs, 'runtime_cap_seconds': args.runtime,
        'requested_disk_journal': args.journal,
        'expected_runtime_journal': args.variant == 'ext4' and args.journal == 'on',
        'kernel_diff_sha256': hashlib.sha256(args.kernel_source_diff.read_bytes() if args.kernel_source_diff else subprocess.check_output(['git', '-C', str(ROOT), 'diff', '--binary', '--', 'kernel'])).hexdigest(),
        'kernel_source_diff_path': str(args.kernel_source_diff.resolve()) if args.kernel_source_diff else None,
        'launch_worktree_kernel_diff_sha256': hashlib.sha256(subprocess.check_output(['git', '-C', str(ROOT), 'diff', '--binary', '--', 'kernel'])).hexdigest(),
        'pool_mib':128, 'metadata_reserve_mib':64, 'data_limit_mib':64,
        'size_policy': 'total_mib/jobs MiB per independent inode; runtime cap per runtime_cap_seconds; partial writes retained',
    }, indent=2) + '\n')
    base = ['run', '--rm', '--network', 'none', '--device', f'{target}:/dev/benchmark-target:rwm']
    if not args.skip_format:
        docker([*base, IMAGE, 'mke2fs', '-F', '-q', '-t', 'ext4', '-b', '4096', '-I', '256',
            '-E', 'nodiscard,lazy_itable_init=0,lazy_journal_init=0', '-O',
            'extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^orphan_file,'
            + ('has_journal' if args.journal == 'on' else '^has_journal'), '/dev/benchmark-target'])
    disk_header = docker([*base, IMAGE, 'dumpe2fs', '-h', '/dev/benchmark-target'],
                         capture_output=True, text=True).stdout
    (art / f'{case}-filesystem.txt').write_text(disk_header)
    feature_match = re.search(r'^Filesystem features:\s*(.*)$', disk_header, re.MULTILINE)
    if feature_match is None:
        raise RuntimeError('Unable to verify on-disk filesystem features')
    has_journal = 'has_journal' in feature_match.group(1).split()
    if has_journal != (args.journal == 'on'):
        raise RuntimeError('On-disk journal feature differs from requested --journal mode')
    guest = Path(__file__).with_name('guest.sh')
    cfg = art / 'case.cfg'
    cfg.write_text(f'CASE={case}\nVARIANT={args.variant}\nRW={args.rw}\nBS={"4k" if args.rw == "randwrite" else "128k"}\nJOBS={args.jobs}\nTOTAL_MB={args.total_mib}\nRUNTIME={args.runtime}\n')
    for command in ['rm /rebuild-fio.cfg', f'write {cfg} /rebuild-fio.cfg']:
        subprocess.run(['debugfs', '-w', '-R', command, str(art / 'ext2.img')], check=True, capture_output=True)
    for command in ['rm /rebuild-fio.sh', f'write {guest} /rebuild-fio.sh']:
        subprocess.run(['debugfs', '-w', '-R', command, str(art / 'ext2.img')], check=True, capture_output=True)
    qemu = ['timeout', '--signal=TERM', '--kill-after=30', str(args.runtime + 300), 'qemu-system-x86_64',
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
    with (art / f'{case}.log').open('w') as log:
        result = subprocess.run(['sg', 'docker', '-c', shlex.join(command)], stdout=log, stderr=subprocess.STDOUT)
    text = (art / f'{case}.log').read_text(errors='replace')
    with (art / f'{case}-e2fsck.log').open('w') as log:
        try:
            docker([*base, IMAGE, 'e2fsck', '-fn', '/dev/benchmark-target'], stdout=log, stderr=subprocess.STDOUT)
            fsck_exit = 0
        except subprocess.CalledProcessError as error:
            fsck_exit = error.returncode
    status = {'qemu_exit': result.returncode, 'e2fsck_exit': fsck_exit,
              'guest_complete': f'BUFFER_COMPLETE {case}' in text}
    (art / f'{case}-status.json').write_text(json.dumps(status, indent=2) + '\n')
    if result.returncode not in (0, 33) or not status['guest_complete'] or fsck_exit:
        raise RuntimeError(f'Guest/check incomplete: {status}; inspect serial and e2fsck logs')
    from summarize import extract
    row = extract(art / f'{case}.log')
    if row is None or row['outcome'] != 'complete':
        raise RuntimeError(f'Workload did not complete successfully: {row}')
    print(f'{case}: fio complete, unmount successful and e2fsck clean', flush=True)

if __name__ == '__main__':
    main()
