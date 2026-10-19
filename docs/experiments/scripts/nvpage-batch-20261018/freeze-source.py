import subprocess,sys,json,hashlib
from pathlib import Path
root=Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
out=Path('/home/zn/asterinas-build-env/nvpage-batch-20261018')/sys.argv[1];out.mkdir(parents=True,exist_ok=True)
patch=subprocess.check_output(['git','diff','--binary','--','kernel'],cwd=root)
for name in subprocess.check_output(['git','ls-files','--others','--exclude-standard','kernel'],cwd=root,text=True).splitlines():
    r=subprocess.run(['git','diff','--binary','--no-index','/dev/null',name],cwd=root,capture_output=True)
    assert r.returncode in (0,1);patch+=r.stdout
(out/'kernel-source.diff').write_bytes(patch)
print(hashlib.sha256(patch).hexdigest(),out/'kernel-source.diff')
