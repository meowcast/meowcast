#!/usr/bin/env python3
"""Verified RK3228 RAM-only probe/boot. No persistent-storage commands."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import tempfile

import sys
ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import paths  # noqa: E402
OUT = paths.OUT
ENTRY = 0x62002000
CALLBACK = 0x63014c38
SCRATCH = 0x62000000
KERNEL = 0x64000000
FDT = 0x68000000

def sha(data):
    return hashlib.sha256(data).hexdigest()

def check_fdt(fdt):
    magic, size = struct.unpack_from('>II', fdt)
    if magic != 0xd00dfeed or size != len(fdt) or len(fdt) > 0x10000:
        raise ValueError('Invalid FDT')

# Linux may write only the SPI-NOR data partition: [0xc00000, 0xfe0000). Below it are the
# RKFP table, vendor storage, both IDBlocks (the button->MaskROM path), trust and kernel.
DATA_WINDOW = (0xc00000, 0xfe0000)

def fdt_nodes(fdt):
    """Return {path: {prop: bytes}} for a flattened device tree."""
    _, total, off_struct, off_strings = struct.unpack_from('>IIII', fdt)
    nodes, path, pos = {}, [], off_struct
    while True:
        token, = struct.unpack_from('>I', fdt, pos); pos += 4
        if token == 1:
            end = fdt.index(b'\0', pos); path.append(fdt[pos:end].decode()); pos = (end + 4) & ~3
            nodes.setdefault('/'.join(path) or '/', {})
        elif token == 2:
            path.pop()
        elif token == 3:
            length, nameoff = struct.unpack_from('>II', fdt, pos); pos += 8
            s = off_strings + nameoff
            nodes['/'.join(path) or '/'][fdt[s:fdt.index(b'\0', s)].decode()] = fdt[pos:pos + length]
            pos = (pos + length + 3) & ~3
        elif token == 4:
            continue
        elif token == 9:
            return nodes
        else:
            raise ValueError('Invalid FDT token')

def check_flash_access(config, fdt):
    for symbol in ['MTD_BLOCK', 'MTD_PARTITIONED_MASTER', 'MTD_SPI_NOR_USE_4K_SECTORS', 'MTD_UBI', 'MTD_CMDLINE_PARTS']:
        if f'CONFIG_{symbol}=y' in config:
            raise ValueError('Unexpected enabled kernel feature: '+symbol)
    nodes = fdt_nodes(fdt)
    flashes = [p for p, props in nodes.items() if b'jedec,spi-nor' in props.get('compatible', b'').split(b'\0')
               and props.get('status', b'okay\0') in (b'okay\0', b'ok\0')]
    for flash in flashes:
        tables = [p for p in nodes if p.startswith(flash + '/') and p.count('/') == flash.count('/') + 1
                  and b'fixed-partitions' in nodes[p].get('compatible', b'').split(b'\0')]
        if len(tables) != 1:
            raise ValueError('SPI-NOR without exactly one fixed-partitions table: '+flash)
        parts = [p for p in nodes if p.startswith(tables[0] + '/') and p.count('/') == tables[0].count('/') + 1]
        if not parts:
            raise ValueError('SPI-NOR partition table is empty')
        for part in parts:
            start, size = struct.unpack('>II', nodes[part]['reg'])
            if not (DATA_WINDOW[0] <= start and start + size <= DATA_WINDOW[1]) and 'read-only' not in nodes[part]:
                raise ValueError(f'Writable SPI-NOR partition outside the data window: {part} {start:#x}+{size:#x}')

def verify():
    manifest = json.loads((OUT/'manifest.json').read_text())
    for name, record in manifest['files'].items():
        if Path(name).name != name:
            raise ValueError('Invalid manifest filename')
        data = (OUT/name).read_bytes()
        if sha(data) != record['sha256'] or len(data) != record['bytes']:
            raise ValueError('Artifact changed: '+name)
    image = (OUT/'zImage').read_bytes()
    magic, start, end = struct.unpack_from('<III', image, 0x24)
    if magic != 0x016f2818 or start != 0 or end != len(image) or not 0 < len(image) <= 0x2000000:
        raise ValueError('Invalid zImage header or load extent')
    check_fdt((OUT/'rk3228a-miracast-ram.dtb').read_bytes())
    config = (OUT/'linux.config').read_text()
    # SMP is allowed only in the perf-lab kernel (BootROM mailbox bring-up).
    # MTD/SPI only in perf, and only for the DT "data" partition (check_flash_access).
    perf = manifest['profile'] == 'perf'
    forbidden = ['MMC','DEVMEM','MODULES'] + ([] if perf else ['SMP','MTD','SPI'])
    if 'CONFIG_MTD=y' in config:
        check_flash_access(config, (OUT/'rk3228a-miracast-ram.dtb').read_bytes())
    for symbol in forbidden:
        if f'# CONFIG_{symbol} is not set' not in config:
            raise ValueError('Unexpected enabled kernel feature: '+symbol)
    for symbol in ['ARCH_ROCKCHIP','BINFMT_ELF','AEABI','USB_DWC2','USB_CONFIGFS','USB_CONFIGFS_ACM','USB_CONFIGFS_ECM','PACKET']:
        if f'CONFIG_{symbol}=y' not in config:
            raise ValueError('Missing kernel feature: '+symbol)
    # LAB images must be paired with the LAB_RECOVERY handoff (and vice versa),
    # otherwise auto-recovery silently disappears (see LAB-RECOVERY.md).
    entry_lab = OUT/'linux_entry.lab'
    if entry_lab.exists() and 'lab' in manifest.get('features', {}):
        if (entry_lab.read_text().strip() == '1') != manifest['features']['lab']:
            raise ValueError('linux_entry.bin LAB flag does not match the image (rebuild with the same LAB)')
    for name in ['cpu_probe.bin', 'linux_entry.bin']:
        if not 0 < (OUT/name).stat().st_size <= 8192:
            raise ValueError('Invalid stage size')
    return manifest

class Session:
    def __init__(self):
        stamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
        self.run = paths.LOGS/('hardware-'+stamp)
        self.run.mkdir()
        self.index = 0
        print('Evidence: '+str(self.run), flush=True)

    def command(self, *args, required=True):
        try:
            p = subprocess.run([str(OUT/'ramboot_memory'), *map(str,args)],
                               capture_output=True, text=True, timeout=25)
            record = {'returncode':p.returncode,'stdout':p.stdout,'stderr':p.stderr}
        except subprocess.TimeoutExpired as error:
            record = {'returncode':124,'stdout':(error.stdout or b'').decode(errors='replace'),
                      'stderr':(error.stderr or b'').decode(errors='replace'), 'host_timeout':True}
        record.update(time=datetime.now(timezone.utc).isoformat(),args=list(map(str,args)))
        with (self.run/'usb.jsonl').open('a') as log:
            log.write(json.dumps(record)+'\n')
        if required and record['returncode']:
            raise RuntimeError('USB step failed: '+json.dumps(record))
        return record

    def read(self, address, size):
        self.index += 1
        path = self.run/f'read-{self.index:05d}.bin'
        self.command('read',hex(address),size,path)
        data = path.read_bytes()
        if len(data) != size:
            raise RuntimeError('Short read')
        return data

    def write(self, address, data):
        if not data or len(data) > 16384 or address & 3:
            raise ValueError('Invalid write size/alignment')
        self.index += 1
        path = self.run/f'write-{self.index:05d}.bin'
        path.write_bytes(data)
        self.command('write-ram',hex(address),path)
        if self.read(address,len(data)) != data:
            raise RuntimeError('RAM readback mismatch at '+hex(address))

    def guard(self):
        vendor = (paths.RKBIN/'bin/rk32/rk322x_usbplug_v2.56.bin').read_bytes()
        if sha(vendor) != '273eb4ec41a6ef6bb2b9bbc3ba3fc3bd21a668fdefeff4e0242fe48ebcde424e':
            raise RuntimeError('Vendor helper reference changed')
        offset = 0x6300231c-(0x63001000-0x3bc)
        if self.read(0x6300231c,40) != vendor[offset:offset+40]:
            raise RuntimeError('Unexpected resident helper dispatcher')
        if self.read(0x63002a58,8)[2:6] != bytes.fromhex('00bf00bf'):
            raise RuntimeError('Precise RAM-write patch absent')
        if self.read(CALLBACK,4) != bytes(4):
            raise RuntimeError('Resident callback occupied')

    def probe(self):
        self.guard()
        code = (OUT/'cpu_probe.bin').read_bytes()
        before_code = self.read(ENTRY,len(code))
        before_output = self.read(SCRATCH,80)
        installed = False
        try:
            self.write(SCRATCH,b'WAIT'+bytes(76))
            self.write(ENTRY,code)
            installed = True
            self.write(CALLBACK,struct.pack('<I',ENTRY))
            self.command('execute-scratch','0x62001000')
            data = self.read(SCRATCH,80)
            (self.run/'cpu-probe.bin').write_bytes(data)
            words = struct.unpack('<20I',data)
            if data[:8] != b'RCPUOKAY':
                raise RuntimeError('CPU probe did not complete')
            state = dict(zip(['cpsr','sctlr','midr','mpidr','clidr','id_pfr1','cntfrq',
                              'ttbr0','ttbr1','ttbcr'],words[2:12]))
            state['svc_mode'] = state['cpsr'] & 31 == 0x13
            state['mmu_enabled'] = bool(state['sctlr'] & 1)
            state['dcache_enabled'] = bool(state['sctlr'] & 4)
            state['icache_enabled'] = bool(state['sctlr'] & 4096)
            state['cortex_a7'] = (state['midr'] >> 4) & 0xfff == 0xc07
            state['boot_core'] = state['mpidr'] & 0xffffff
            state['translation_par'] = dict(zip(['entry','kernel','fdt','usb','sgrf'],map(hex,words[12:17])))
            addresses = [ENTRY,KERNEL,FDT,0x30040000,0x10150000]
            state['identity_mapping'] = all(not (par & 1) and par & 0xfffff000 == address & 0xfffff000
                                            for par,address in zip(words[12:17],addresses))
            state['sgrf_soc_con0'] = hex(words[17])
            state['ddr_secure_region_enabled'] = bool(words[17] & 0x4000)
            state['cru_misc_con'] = hex(words[18])
            state['hdmi_phy_clock_bypassed'] = bool(words[18] & 0x2000)
            state['translation_par']['cru'] = hex(words[19])
            state['identity_mapping'] = state['identity_mapping'] and not(words[19] & 1) and words[19] & 0xfffff000 == 0x110e0000
            (self.run/'cpu-probe.json').write_text(json.dumps(state,indent=2)+'\n')
            print(json.dumps(state,indent=2), flush=True)
            return state
        finally:
            # Restore before reporting readiness; an unresponsive helper raises.
            if installed:
                self.write(CALLBACK,bytes(4))
            self.write(ENTRY,before_code)
            self.write(SCRATCH,before_output)

    def boot(self, dtb=None, entry=None):
        state = self.probe()
        if not state['svc_mode'] or not state['identity_mapping'] or not state['cortex_a7'] or state['boot_core'] != 0xf00:
            raise RuntimeError('Entry state requires investigation; Linux was NOT launched')
        for name, address in [('zImage',KERNEL),('rk3228a-miracast-ram.dtb',FDT)]:
            data = (OUT/name).read_bytes()
            if name.endswith('.dtb') and dtb:
                # Lab DTB variant (e.g. CPUs disabled); same header checks.
                data = Path(dtb).read_bytes()
                check_fdt(data)
                (self.run/'dtb-override.json').write_text(json.dumps({'path':str(dtb),'sha256':sha(data)})+'\n')
                print('DTB override: '+str(dtb),flush=True)
            print(f'Uploading and verifying {name}: {len(data)} bytes at {address:#x}',flush=True)
            for offset in range(0,len(data),16384):
                self.write(address+offset,data[offset:offset+16384])
                if offset % 1048576 == 0:
                    print(f'  {offset}/{len(data)} bytes',flush=True)
            print('Verified SHA-256: '+sha(data),flush=True)
        self.guard()
        stage = (OUT/'linux_entry.bin').read_bytes()
        if entry:
            # Lab handoff variant (A/B tests); same size limit as verify().
            stage = Path(entry).read_bytes()
            if not 0 < len(stage) <= 8192:
                raise ValueError('Invalid entry stage size')
            (self.run/'entry-override.json').write_text(json.dumps({'path':str(entry),'sha256':sha(stage)})+'\n')
            print('Entry override: '+str(entry),flush=True)
        self.write(ENTRY,stage)
        # Callback installed last. From here Linux takes ownership of all RAM.
        self.write(CALLBACK,struct.pack('<I',ENTRY))
        print('Launching Linux; loss of Rockusb/status timeout is expected, not proof of boot.',flush=True)
        record = self.command('execute-scratch','0x62001000',required=False)
        (self.run/'handoff.json').write_text(json.dumps(record,indent=2)+'\n')
        if record['returncode'] == 0:
            raise RuntimeError('Callback returned instead of handing off; inspect CPU state')
        print('Handoff sent. Verify new USB serial enumeration and kernel output.',flush=True)

def main():
    global OUT
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('action',choices=['verify','probe','boot'])
    p.add_argument('--dir',default=str(OUT),help='artifact directory with manifest.json (default: output)')
    p.add_argument('--dtb',help='boot this DTB instead of the manifest one (lab variants)')
    p.add_argument('--entry',help='boot this handoff stage instead of linux_entry.bin (lab variants)')
    args = p.parse_args()
    OUT = Path(args.dir).resolve()
    manifest = verify()
    print(f'Artifacts verified: Linux {manifest["linux"]}; {manifest["files"]["zImage"]["bytes"]} byte zImage',flush=True)
    if args.action != 'verify':
        session = Session()
        if args.action == 'probe':
            session.probe()
        else:
            session.boot(args.dtb, args.entry)

if __name__ == '__main__':
    main()
