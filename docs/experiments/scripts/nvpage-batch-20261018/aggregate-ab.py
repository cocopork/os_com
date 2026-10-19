import csv,json,re,statistics,importlib.util,sys,hashlib
from pathlib import Path
base=Path('/home/zn/asterinas-build-env/nvpage-batch-20261018')
root=Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
spec=importlib.util.spec_from_file_location('summarize',root/'docs/os_com_implementation/test/rnvpce-buffer-compare/summarize.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
for mode in ['throughput','profile']:
    rows=[];diagnostics=[]
    for folder in sorted((base/mode).glob('round-*/*')):
        version=folder.name
        images=base/('release-images' if mode=='throughput' else 'profile-images')/version
        for status_file in sorted(folder.glob('*-status.json')):
            case=status_file.name.removesuffix('-status.json');log=folder/f'{case}.log'
            row=m.extract(log);assert row['outcome']=='complete' and row['error']==0 and row['io_mib']==2048,row
            status=json.loads(status_file.read_text());assert status['guest_complete'] and status['e2fsck_exit']==0 and status['qemu_exit']==33,status
            manifest=json.loads((folder/f'{case}-manifest.json').read_text())
            assert manifest['requested_disk_journal']=='off' and not manifest['expected_runtime_journal']
            assert manifest['iso_sha256']==hashlib.sha256((images/'selected.iso').read_bytes()).hexdigest()
            assert manifest['kernel_diff_sha256']==hashlib.sha256((images/'kernel-source.diff').read_bytes()).hexdigest()
            features=re.search(r'^Filesystem features:\s*(.*)$',(folder/f'{case}-filesystem.txt').read_text(),re.M).group(1).split();assert 'has_journal' not in features
            row.update(round=int(folder.parent.name.split('-')[1]),version=version);rows.append(row)
            if mode=='profile':
                text=log.read_text(errors='replace')
                phases={name:dict(calls=int(calls),sum_ns=int(total),max_ns=int(maximum)) for name,calls,total,maximum in re.findall(r'\[NVPAGE-BATCH\] phase=(\w+) calls=(\d+) sum_ns=(\d+) max_ns=(\d+)',text)}
                events={name:int(count) for name,count in re.findall(r'\[NVPAGE-BATCH\] event=(\w+) count=(\d+)',text)}
                assert len(phases)==7 and len(events)==7,(case,phases,events)
                diagnostics.append(dict(case=case,version=version,rw=row['rw'],jobs=row['jobs'],phases=phases,events=events))
    if rows:
        with (base/mode/'all-results.csv').open('w') as out:
            w=csv.DictWriter(out,fieldnames=list(rows[0]));w.writeheader();w.writerows(rows)
    summary=[]
    for rw in ['randwrite','write']:
        for jobs in [1,16]:
            for version in ['before','after']:
                group=[r for r in rows if (r['rw'],r['jobs'],r['version'])==(rw,jobs,version)]
                if not group:continue
                s=dict(rw=rw,jobs=jobs,version=version,repeats=len(group))
                for key in ['fio_mib_s','drained_mib_s','mean_us','p99_us']:
                    vals=[r[key] for r in group];s[key]=statistics.median(vals);s[key+'_min']=min(vals);s[key+'_max']=max(vals)
                summary.append(s)
    if (base/mode).exists():(base/mode/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
    if diagnostics:(base/mode/'diagnostics.json').write_text(json.dumps(diagnostics,indent=2)+'\n')
    print(mode,'validated',len(rows),flush=True)
    for s in summary:print(s['rw'],s['jobs'],s['version'],s['repeats'],*[f'{s[k]:.2f}' for k in ['fio_mib_s','drained_mib_s']])
    if '--final' in sys.argv:assert len(rows)==(24 if mode=='throughput' else 8)
