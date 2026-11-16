#!/usr/bin/env python3
from pathlib import Path
from datetime import datetime, timezone
import hashlib
import json
import shutil

import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
import paths  # noqa: E402
root = paths.OS
names = ['zImage','rk3228a-miracast-ram.dtb','linux.config','busybox.config',
         'rootfs.cpio','busybox','cpu_probe.bin','linux_entry.bin','ramboot_memory']
manifest = {'linux':'7.3.4','busybox':'1.37.0',
            'profile':(root/'output/profile').read_text().strip(),
            'created':datetime.now(timezone.utc).isoformat(),
            'board':'RK3228A Miracast experimental RAM boot',
            'files':{},'load_addresses':{'zImage':'0x64000000','fdt':'0x68000000','entry':'0x62002000'}}
if (root/'output/features.json').exists():
    manifest['features']=json.loads((root/'output/features.json').read_text())
    if manifest['features'].get('gpu_test'):
        manifest['mesa']='26.2.6'
        for src,dst in [('gpu/gpu-test','gpu-test'),('gpu/runtime-manifest.json','gpu-runtime.json'),('gpu/mesa-buildoptions.json','mesa-buildoptions.json')]:
            shutil.copyfile(root/'output'/src,root/'output'/dst)
            names.append(dst)
for name in names:
    data = (root/'output'/name).read_bytes()
    manifest['files'][name] = {'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}
(root/'output/manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps(manifest,indent=2))
