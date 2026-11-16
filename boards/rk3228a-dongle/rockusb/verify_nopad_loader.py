#!/usr/bin/env python3
"""Independently check variant RAM payloads, zero padding, and vendor CRC polynomial."""
from pathlib import Path
import hashlib
import json
import struct
from inspect_loader import rc4

root=Path(__file__).resolve().parent
r=root/'tools/rkbin'
p=r/'rk322x_loader_v1.10.256_nopad.bin'
b=p.read_bytes()
results={}
for header,source in [(25,'rk322x_ddr_300MHz_v1.10.bin'),(31,'rk322x_usbplug_v2.56_nopad.bin')]:
    count,table,stride=struct.unpack_from('<BIB',b,header)
    assert count==1 and stride==57
    _,kind,_,begin,length,_=struct.unpack_from('<BI40sIII',b,table)
    decoded=rc4(b[begin:begin+length])
    raw=(r/'bin/rk32'/source).read_bytes()
    assert decoded[:len(raw)]==raw and not any(decoded[len(raw):])
    results[source]={'verified':True,'sha256':hashlib.sha256(raw).hexdigest()}
crc=0
for value in b[:-4]:
    crc^=value<<24
    for _ in range(8):
        crc=((crc<<1)^(0x04c10db7 if crc&0x80000000 else 0))&0xffffffff
assert struct.unpack('<I',b[-4:])[0]==crc
results['package']={'verified_crc':hex(crc),'sha256':hashlib.sha256(b).hexdigest(),'size':len(b)}
(r/'nopad-verification.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results,indent=2))
