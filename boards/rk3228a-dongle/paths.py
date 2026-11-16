"""Where this board's tools find things. Imported by the scripts in this directory."""
import hashlib
import os
from pathlib import Path
import sys

BOARD = Path(__file__).resolve().parent
REPO = BOARD.parents[1]
OS = REPO / 'os'
OUT = OS / 'output'            # the OS build's artifacts and this board's host tools
ROCKUSB = BOARD / 'rockusb'
RKBIN = ROCKUSB / 'tools/rkbin'        # fetch-tools.sh puts the pinned checkouts here
RKDEV = ROCKUSB / 'tools/rkdeveloptool/rkdeveloptool'
LOGS = BOARD / 'logs'
LOGS.mkdir(exist_ok=True)
# Full-chip reads of stock firmware whose boot chain these tools were proven against.
KNOWN_STOCK = {'e00c28551eea7b309cb2b0601afe74d52683a215eae2f3d660c19c054fd96002'}


def config():
    """config.env at the repository root (or $MEOW_CONFIG), overridden by the environment."""
    values = {}
    path = Path(os.environ.get('MEOW_CONFIG', REPO / 'config.env'))
    if path.is_file():
        for line in path.read_text().splitlines():
            line = line.strip()
            if line and not line.startswith('#') and '=' in line:
                key, value = line.split('=', 1)
                values[key.strip()] = value.strip().strip('"\'')
    values.update({k: v for k, v in os.environ.items() if k.startswith('MEOW_')})
    return values


def stock_dump():
    """(path, bytes) of this unit's own 16 MiB stock flash dump, checked against its pinned hash.

    MEOW_STOCK_DUMP names the file (default: stock/nor-stock.bin here) and MEOW_STOCK_SHA256
    its SHA-256 (default: the first word of <file>.sha256). Every unit has its own dump; it is
    never part of the repository.
    """
    cfg = config()
    path = Path(cfg.get('MEOW_STOCK_DUMP') or BOARD / 'stock/nor-stock.bin').expanduser()
    if not path.is_absolute():
        path = REPO / path
    if not path.is_file():
        sys.exit(f'No stock flash dump at {path}: read your unit first (docs/FLASHING.md), '
                 'or set MEOW_STOCK_DUMP in config.env')
    sha = cfg.get('MEOW_STOCK_SHA256')
    side = Path(str(path) + '.sha256')
    if not sha and side.is_file():
        sha = side.read_text().split()[0]
    if not sha:
        sys.exit(f'No pinned hash for {path}: set MEOW_STOCK_SHA256 or write {side.name} '
                 'after two full reads gave identical files')
    data = path.read_bytes()
    if len(data) != 0x1000000 or hashlib.sha256(data).hexdigest() != sha.lower():
        sys.exit('Stock dump size/hash mismatch')
    if sha.lower() not in KNOWN_STOCK:
        print(f'WARNING: stock firmware {sha[:12]} is not one these tools were proven with. '
              'Run the RAM rehearsal (flash/rehearse.py) before the first write.', file=sys.stderr)
    return path, data
