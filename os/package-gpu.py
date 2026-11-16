#!/usr/bin/env python3
"""Copy the explicitly built ARM graphics stack and its ELF dependency closure."""
from pathlib import Path
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys

root=Path(sys.argv[1])
mesa=Path('/build/mesa-install/usr/lib')
search=[mesa,Path('/usr/lib/arm-linux-gnueabihf'),Path('/lib/arm-linux-gnueabihf'),Path('/usr/arm-linux-gnueabihf/lib')]
records={}
seen=set()

def copy_elf(src,dst):
    src=src.resolve()
    target=root/str(dst).lstrip('/')
    target.parent.mkdir(parents=True,exist_ok=True)
    shutil.copy2(src,target)
    records[str(dst)]={'bytes':target.stat().st_size,'sha256':hashlib.sha256(target.read_bytes()).hexdigest()}
    if src in seen:return
    seen.add(src)
    header=subprocess.check_output(['arm-linux-gnueabihf-readelf','-h',str(src)],text=True)
    if 'ARM' not in header or 'ELF32' not in header:raise RuntimeError('Not a 32-bit ARM ELF: '+str(src))
    dynamic=subprocess.check_output(['arm-linux-gnueabihf-readelf','-d',str(src)],text=True)
    for soname in re.findall(r'\(NEEDED\).*\[([^]]+)\]',dynamic):
        dep=next((p/soname for p in search if (p/soname).exists()),None)
        if dep is None:raise RuntimeError('Missing ARM dependency '+soname)
        location=Path('/usr/lib') if dep.parent==mesa else Path('/lib')
        copy_elf(dep,location/soname)

copy_elf(Path('output/gpu/gpu-test'),Path('/bin/gpu-test'))
copy_elf(Path('output/gpu/membw'),Path('/bin/membw'))
copy_elf(Path('output/gpu/canary'),Path('/bin/canary'))
copy_elf(Path('output/gpu/memtest'),Path('/bin/memtest'))
if os.environ.get('UI')=='1':
    copy_elf(Path('deploy/ui/bin/miracast-ui'),Path('/usr/bin/miracast-ui'))
if os.environ.get('APPD')=='1':
    copy_elf(Path('deploy/appd/bin/appd'),Path('/usr/bin/appd'))
for soname in ['libEGL.so.1','libGLESv2.so.2','libgbm.so.1','libgallium-26.2.6.so']:
    copy_elf(mesa/soname,Path('/usr/lib')/soname)
copy_elf(mesa/'gbm/dri_gbm.so',Path('/usr/lib/gbm/dri_gbm.so'))
loader=next(p/'ld-linux-armhf.so.3' for p in search if (p/'ld-linux-armhf.so.3').exists())
copy_elf(loader,Path('/lib/ld-linux-armhf.so.3'))
manifest={'mesa':'26.2.6','gallium_drivers':['lima'],'software_renderer_built':False,'files':records}
Path('output/gpu/runtime-manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
print('Packaged GPU runtime:',sum(x['bytes'] for x in records.values()),'bytes',len(records),'ELF files')
