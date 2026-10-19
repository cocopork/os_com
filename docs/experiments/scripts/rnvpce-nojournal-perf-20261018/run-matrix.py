import subprocess,time,json
from pathlib import Path
root=Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
base=Path('/home/zn/asterinas-build-env/rnvpce-nojournal-perf-20261018')
source=Path('/home/zn/asterinas-build-env/rnvpce-nojournal-20261018/final-export')
harness=root/'docs/os_com_implementation/test/rnvpce-buffer-compare'
order=[]
for repeat in range(1,4):
    art=base/f'round-{repeat}'
    art.mkdir(exist_ok=True)
    for name in ['selected.iso','ext2.img','exfat.img']:
        subprocess.run(['cp','--reflink=auto','--sparse=always',str(source/name),str(art/name)],check=True)
    for rw in ['randwrite','write']:
        for jobs in [1,16]:
            variants=['ext4','rnvpce'] if repeat%2 else ['rnvpce','ext4']
            for variant in variants:
                case=dict(round=repeat,variant=variant,rw=rw,jobs=jobs)
                order.append(case)
                (base/'order.json').write_text(json.dumps(order,indent=2)+'\n')
                print(time.strftime('%H:%M:%S'), 'START',case,flush=True)
                subprocess.run(['python3',str(harness/'run.py'),'nvme','--variant',variant,'--rw',rw,'--jobs',str(jobs),'--total-mib','2048','--runtime','180','--journal','off','--format-authorized','--artifacts',str(art)],cwd=root,check=True)
                subprocess.run(['python3',str(harness/'summarize.py'),str(art)],check=True)
                print(time.strftime('%H:%M:%S'),'DONE',case,flush=True)
print('MATRIX_COMPLETE 24',flush=True)
