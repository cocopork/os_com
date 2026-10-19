import subprocess,time,json,sys
from pathlib import Path
root=Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
base=Path('/home/zn/asterinas-build-env/nvpage-batch-20261018')
h=root/'docs/os_com_implementation/test/rnvpce-buffer-compare'
source=Path('/home/zn/asterinas-build-env/rnvpce-nojournal-20261018/final-export')
profile='--profile' in sys.argv
repeats=1 if profile else 3
path=base/('profile' if profile else 'throughput');path.mkdir(exist_ok=True)
order=[]
for repeat in range(1,repeats+1):
    for rw in ['randwrite','write']:
        for jobs in [1,16]:
            for version in (['before','after'] if repeat%2 else ['after','before']):
                images=base/('profile-images' if profile else 'release-images')/version
                art=path/f'round-{repeat}'/version
                art.mkdir(parents=True,exist_ok=True)
                for name in ['selected.iso','ext2.img','exfat.img']:
                    dest=art/name
                    if not dest.exists():
                        src=images/name if name=='selected.iso' else source/name
                        subprocess.run(['cp','--reflink=auto','--sparse=always',str(src),str(dest)],check=True)
                case=dict(round=repeat,version=version,rw=rw,jobs=jobs,profile=profile)
                order.append(case);(path/'order.json').write_text(json.dumps(order,indent=2)+'\n')
                print(time.strftime('%H:%M:%S'),'START',case,flush=True)
                subprocess.run(['python3',str(h/'run.py'),'nvme','--variant','rnvpce','--rw',rw,'--jobs',str(jobs),'--total-mib','2048','--runtime','180','--journal','off','--format-authorized','--kernel-source-diff',str(images/'kernel-source.diff'),'--artifacts',str(art)],cwd=root,check=True)
                subprocess.run(['python3',str(h/'summarize.py'),str(art)],check=True)
                print(time.strftime('%H:%M:%S'),'DONE',case,flush=True)
print('AB_MATRIX_COMPLETE',len(order),flush=True)
