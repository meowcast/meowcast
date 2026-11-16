#!/usr/bin/env python3
"""Run a hash-specified reviewed RAM stage through the verified RK322x callback."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import struct
import subprocess

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--code',required=True)
parser.add_argument('--sha256',required=True)
parser.add_argument('--output-bytes',type=int,default=16)
parser.add_argument('--entry',choices=['0x62001000','0x62002000'],default='0x62002000')
args=parser.parse_args()
root=Path(__file__).resolve().parent
code=Path(args.code).resolve()
payload=code.read_bytes()
if not code.is_relative_to(root) or hashlib.sha256(payload).hexdigest()!=args.sha256:
    raise SystemExit('Unexpected code path/hash')
if not 0<len(payload)<=8192 or not 16<=args.output_bytes<=4096:
    raise SystemExit('Invalid code/output size')
run=root/('ram-callback-'+datetime.datetime.now().astimezone().strftime('%Y%m%dT%H%M%S%z'))
run.mkdir()
print('Evidence directory: '+str(run),flush=True)
tool=root/'rockusb_memory'

def step(*command,required=True):
    event={'timestamp':datetime.datetime.now().astimezone().isoformat(),'args':list(map(str,command))}
    try:
        p=subprocess.run([str(tool),*map(str,command)],cwd=root,capture_output=True,text=True,
                         timeout=75 if command[0]=='execute-scratch' else 25)
        event.update(returncode=p.returncode,stdout=p.stdout,stderr=p.stderr)
    except subprocess.TimeoutExpired:
        event.update(returncode=124,error='Host timeout')
    with (run/'operations.jsonl').open('a') as f:f.write(json.dumps(event)+'\n')
    print(json.dumps(event),flush=True)
    if required and event['returncode']:raise RuntimeError('Failed USB step')
    return event['returncode']==0

# Match the exact resident callback-dispatch code, not merely a generic VID/PID.
step('read','0x6300231c','40',run/'dispatcher.bin')
vendor=(root/'tools/rkbin/bin/rk32/rk322x_usbplug_v2.56.bin').read_bytes()
offset=0x6300231c-(0x63001000-0x3bc)
if (run/'dispatcher.bin').read_bytes()!=vendor[offset:offset+40]:
    raise SystemExit('Resident dispatcher differs; no write issued')
step('read','0x63002a58','8',run/'write-padding-guard.bin')
if (run/'write-padding-guard.bin').read_bytes()[2:6]!=bytes.fromhex('00bf00bf'):
    raise SystemExit('RAM-write padding patch absent; no write issued')
step('read','0x63014c38','4',run/'callback-before.bin')
if (run/'callback-before.bin').read_bytes()!=bytes(4):
    raise SystemExit('Callback already occupied; no write issued')
step('read','0x62000000',str(args.output_bytes),run/'output-before.bin')
step('read',args.entry,str(len(payload)),run/'code-before.bin')
(run/'wait.bin').write_bytes(b'WAIT'+bytes(args.output_bytes-4))
(run/'callback-new.bin').write_bytes(struct.pack('<I',int(args.entry,0)))
installed=False
execution_returned=False
cleanup_ok=True
try:
    step('write-ram','0x62000000',run/'wait.bin')
    step('write-ram',args.entry,code)
    step('read',args.entry,str(len(payload)),run/'code-readback.bin')
    if (run/'code-readback.bin').read_bytes()!=payload:raise RuntimeError('Code readback mismatch')
    installed=True
    step('write-ram','0x63014c38',run/'callback-new.bin')
    step('read','0x63014c38','4',run/'callback-readback.bin')
    if (run/'callback-readback.bin').read_bytes()!=(run/'callback-new.bin').read_bytes():
        raise RuntimeError('Callback readback mismatch')
    step('execute-scratch','0x62001000')
    execution_returned=True
    step('read','0x62000000',str(args.output_bytes),run/'output-after.bin')
    print('Stage output prefix: '+(run/'output-after.bin').read_bytes()[:64].hex(),flush=True)
finally:
    # Always remove the callback before restoring its code or scratch buffers.
    if installed:cleanup_ok=step('write-ram','0x63014c38',run/'callback-before.bin',required=False)
    if cleanup_ok:
        cleanup_ok=step('write-ram',args.entry,run/'code-before.bin',required=False)
        cleanup_ok=step('write-ram','0x62000000',run/'output-before.bin',required=False) and cleanup_ok
        if cleanup_ok:
            step('read','0x63014c38','4',run/'callback-restored.bin')
            step('read',args.entry,str(len(payload)),run/'code-restored.bin')
            step('read','0x62000000',str(args.output_bytes),run/'output-restored.bin')
            cleanup_ok=all((run/(name+'-before.bin')).read_bytes()==(run/(name+'-restored.bin')).read_bytes()
                           for name in ['callback','code','output'])
    (run/'result.json').write_text(json.dumps({'code':str(code),'sha256':args.sha256,
        'execution_command_returned':execution_returned,'scratch_and_callback_restored':cleanup_ok},indent=2)+'\n')
    print(json.dumps({'execution_command_returned':execution_returned,'scratch_and_callback_restored':cleanup_ok}),flush=True)
