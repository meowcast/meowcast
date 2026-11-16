#!/usr/bin/env python3
"""Retrieve a verified NOR capture from DRAM; never writes the target."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import struct
import subprocess

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--result',required=True)
parser.add_argument('--out',required=True)
args=parser.parse_args()
root=Path(__file__).resolve().parent
result=Path(args.result).resolve()
destination=Path(args.out).resolve()
if not result.is_relative_to(root) or not destination.is_relative_to(root) or destination.exists():
    raise SystemExit('Paths must be within the audit directory; output must be new')
status=result.read_bytes()
if len(status)!=128 or status[:4]!=b'NORR' or status[80:83]!=bytes.fromhex('ef4018'):
    raise SystemExit('Not a verified Winbond NOR capture result')
words=struct.unpack('<32I',status)
size,address=words[25],words[24]
if words[1]!=0x59414b4f or words[2]!=size or words[3] or address!=0x64000000 or size not in (65536,16777216):
    raise SystemExit('Capture status, length or address invalid')
if words[26]!=words[4] or words[27]!=words[7] or words[28]!=words[9]:
    raise SystemExit('SPI/pin restoration not verified')
work=destination.with_name(destination.name+'.chunks')
work.mkdir()
partial=destination.with_name(destination.name+'.partial')
digest=hashlib.sha256()
started=datetime.datetime.now().astimezone().isoformat()
with partial.open('xb') as output, (work/'reads.jsonl').open('x') as log:
    for offset in range(0,size,16384):
        count=min(16384,size-offset)
        chunk=work/f'{offset:08x}.bin'
        p=subprocess.run([str(root/'rockusb_memory'),'read',hex(address+offset),str(count),str(chunk)],
                         capture_output=True,text=True,timeout=25,cwd=root)
        event={'offset':offset,'address':hex(address+offset),'length':count,
               'returncode':p.returncode,'stdout':p.stdout,'stderr':p.stderr}
        if p.returncode:
            log.write(json.dumps(event)+'\n');log.flush()
            raise SystemExit('RAM retrieval failed; partial file retained as incomplete')
        data=chunk.read_bytes()
        if len(data)!=count:raise SystemExit('Short RAM chunk; incomplete file retained')
        event['sha256']=hashlib.sha256(data).hexdigest()
        log.write(json.dumps(event)+'\n');log.flush()
        output.write(data);digest.update(data)
        chunk.unlink() # Solely this script's new temporary chunk; hashes/log retained.
        if offset%1048576==0:print(f'Retrieved {offset+count}/{size} bytes',flush=True)
if partial.stat().st_size!=size:raise SystemExit('Size mismatch')
partial.rename(destination)
event={'started':started,'finished':datetime.datetime.now().astimezone().isoformat(),
       'file':str(destination),'size':size,'sha256':digest.hexdigest(),
       'jedec_id':'ef4018','capture_result':str(result),'capture_result_sha256':hashlib.sha256(status).hexdigest()}
with (root/'firmware-acquisitions.jsonl').open('a') as log:log.write(json.dumps(event)+'\n')
destination.with_name(destination.name+'.json').write_text(json.dumps(event,indent=2)+'\n')
print(json.dumps(event,indent=2),flush=True)
