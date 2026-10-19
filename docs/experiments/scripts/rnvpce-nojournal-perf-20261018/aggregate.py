import csv,json,statistics,sys,importlib.util
from pathlib import Path
base=Path('/home/zn/asterinas-build-env/rnvpce-nojournal-perf-20261018')
root=Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
spec=importlib.util.spec_from_file_location('summarize',root/'docs/os_com_implementation/test/rnvpce-buffer-compare/summarize.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
rows=[]
for art in sorted(base.glob('round-*')):
    for log in sorted(art.glob('*-j*-2048m.log')):
        status=log.with_name(log.stem+'-status.json')
        if not status.exists():continue
        result=m.extract(log)
        assert result and result['outcome']=='complete',result
        check=json.loads(status.read_text())
        assert check['guest_complete'] and check['e2fsck_exit']==0 and check['qemu_exit'] in [0,33],check
        manifest=json.loads(log.with_name(log.stem+'-manifest.json').read_text())
        assert manifest['requested_disk_journal']=='off' and manifest['expected_runtime_journal']==False
        assert manifest['kernel_diff_sha256']=='be221817a32b8e1d289850df15a4febc0aecd539a752b777339e0faca52447a8'
        assert manifest['iso_sha256']=='f7abf7d14f71b9527e3432acb5af3c6e0e5a2ee84e4784a10065804b9346f9cb'
        result['round']=int(art.name.split('-')[1]);rows.append(result)
if rows:
    with (base/'all-results.csv').open('w') as out:
        w=csv.DictWriter(out,fieldnames=list(rows[0]));w.writeheader();w.writerows(rows)
summary=[]
for rw in ['randwrite','write']:
    for jobs in [1,16]:
        for variant in ['ext4','rnvpce']:
            group=[r for r in rows if (r['rw'],r['jobs'],r['variant'])==(rw,jobs,variant)]
            if not group:continue
            s=dict(rw=rw,jobs=jobs,variant=variant,repeats=len(group))
            for key in ['fio_mib_s','drained_mib_s','mean_us','p99_us']:
                vals=[r[key] for r in group]
                s[key]=statistics.median(vals);s[key+'_min']=min(vals);s[key+'_max']=max(vals)
            summary.append(s)
(base/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print('Validated cases:',len(rows))
for s in summary:print(s['rw'],s['jobs'],s['variant'],s['repeats'],*[f'{s[k]:.2f}' for k in ['fio_mib_s','drained_mib_s']])
if '--final' in sys.argv:
    assert len(rows)==24 and all(s['repeats']==3 for s in summary)
    print('VALIDATION_PASS 24/24')
