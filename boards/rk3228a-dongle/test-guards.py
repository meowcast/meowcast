#!/usr/bin/env python3
"""Exercise artifact rejection before any USB session is created."""
import hashlib
import json
from pathlib import Path
import shutil
import struct
import tempfile
import unittest
import ramboot

class ArtifactGuards(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.original = ramboot.OUT
        ramboot.OUT = Path(self.temp.name)
        self.manifest = json.loads((self.original/'manifest.json').read_text())
        for name in self.manifest['files']:
            shutil.copyfile(self.original/name,ramboot.OUT/name)
        self.save()

    def tearDown(self):
        ramboot.OUT = self.original
        self.temp.cleanup()

    def save(self):
        (ramboot.OUT/'manifest.json').write_text(json.dumps(self.manifest))

    def replace_and_rehash(self,name,data):
        (ramboot.OUT/name).write_bytes(data)
        self.manifest['files'][name] = {'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}
        self.save()

    def test_built_image_passes(self):
        ramboot.verify()

    def test_tampered_kernel_rejected(self):
        p=ramboot.OUT/'zImage';data=bytearray(p.read_bytes());data[-1]^=1;p.write_bytes(data)
        with self.assertRaisesRegex(ValueError,'Artifact changed'):
            ramboot.verify()

    def test_wrong_boot_header_rejected_even_with_matching_hash(self):
        data=bytearray((ramboot.OUT/'zImage').read_bytes());data[0x24:0x28]=bytes(4)
        self.replace_and_rehash('zImage',data)
        with self.assertRaisesRegex(ValueError,'zImage'):
            ramboot.verify()

    def test_bad_fdt_size_rejected_even_with_matching_hash(self):
        data=bytearray((ramboot.OUT/'rk3228a-miracast-ram.dtb').read_bytes());data[4:8]=bytes(4)
        self.replace_and_rehash('rk3228a-miracast-ram.dtb',data)
        with self.assertRaisesRegex(ValueError,'FDT'):
            ramboot.verify()

    def test_raw_mtd_device_config_rejected(self):
        data=(ramboot.OUT/'linux.config').read_bytes()
        if b'CONFIG_MTD=y' not in data:
            self.skipTest('kernel without MTD')
        self.replace_and_rehash('linux.config',data.replace(b'# CONFIG_MTD_PARTITIONED_MASTER is not set',b'CONFIG_MTD_PARTITIONED_MASTER=y'))
        with self.assertRaisesRegex(ValueError,'kernel feature: MTD_PARTITIONED_MASTER'):
            ramboot.verify()

    def test_partition_outside_data_window_rejected(self):
        fdt=bytes((ramboot.OUT/'rk3228a-miracast-ram.dtb').read_bytes())
        props=ramboot.fdt_nodes(fdt)
        part=[p for p in props if p.endswith('/data@c00000')]
        if not part:
            self.skipTest('DTB without the data partition')
        reg=struct.pack('>II',0xc00000,0x3e0000)
        self.assertEqual(fdt.count(reg),1)
        bad=fdt.replace(reg,struct.pack('>II',0x40000,0x3e0000))
        with self.assertRaisesRegex(ValueError,'outside the data window'):
            ramboot.check_flash_access('CONFIG_MTD=y',bad)

    def test_flash_driver_config_rejected_outside_perf(self):
        data=(ramboot.OUT/'linux.config').read_bytes().replace(b'# CONFIG_MTD is not set',b'CONFIG_MTD=y')
        self.replace_and_rehash('linux.config',data)
        self.manifest['profile']='graphics'
        self.save()
        with self.assertRaisesRegex(ValueError,'kernel feature: (MTD|SMP|SPI)'):
            ramboot.verify()

if __name__=='__main__':
    unittest.main()
