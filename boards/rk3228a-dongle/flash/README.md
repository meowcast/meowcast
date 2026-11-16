# SPI-NOR flashing and rollback

Phase-1 flash tooling for the RK3228A dongle, proven on hardware on 13 November 2026.
The whole procedure for a new unit is in [../README.md](../README.md).

**The one rule:** never write NOR below 0x050000. That region holds the RKFP table, vendor
storage and both IDBlocks (stock DDR init + miniloader). The button-to-MaskROM path is code
in that miniloader. The writer stage cannot write there: the window [0x050000, 0xfe0000) is
compiled in, and `flash_nor.py` refuses images that differ from the stock dump outside it.

## Files

| File | What |
|---|---|
| `stage_spi_write.c` | MaskROM-helper callback: makes flash[start,start+len) equal to DRAM 0x64000000+offset. Bus commands 0x9f/0x05/0x03, plus 0x06/0x20/0x02/0x04 only in WRITE mode. Checks the DRAM CRC first, skips equal sectors, reads back every written sector, then checks the final flash CRC. |
| `flash_nor.py` | Host driver: dry run by default, `--write` to program. Evidence in `runs/<timestamp>/`. |
| `pack-flash.py` | Builds the 16 MiB target image: stock dump + trust slot (`flash_entry.bin` + DTB, vdd_logic 1.10 V as on RAM boots) + kernel slot (KERNEL header + zImage). |
| `rehearse.py` | RAM rehearsal of the flashed boot with the stock DDR init (`load-helper`, `boot`). |
| `make-stock-ddr-loader.py` | nopad RAM loader with CODE471 = stock DDR V1.07 (600 MHz). |
| `build.sh` | Builds the stage, `norflash_memory` (derived from `../rockusb/rockusb_memory.cpp`), and the flash entries (`boards/rk3228a-dongle/boot/linux_entry.S` with `-DFLASH_ENTRY`). |
| `fdt_edit.py` | Same-size DTB property edits without dtc. |

## Data partition (apps work)

Linux (perf kernel with SPI/MTD/JFFS2) mounts **0xc00000-0xfe0000 as /data** (JFFS2, 62 x 64 KiB
erase blocks): app manifests, permission grants and app storage written by appd
(apps). It is the only flash region Linux can reach (DT fixed partition; no
partitioned master; `ramboot.py verify` checks the DTB). Consequences here:

- The kernel slot must end below 0xc00000 (`pack-flash.py` enforces it; ~11.2 MiB).
- `flash_nor.py` writes [0x050000, 0xc00000) by default, so a firmware update keeps /data.
  `--end 0xfe0000` also erases it (fresh /data), and the rollback to stock needs it.
- A full-chip read after a flash differs from the image in 0xc00000-0xfe0000 once Linux has
  written /data; compare [0, 0xc00000) and [0xfe0000, end) instead.

## Flash speed (second version of the stage)

The first flashes took ~7.5 min for [0x050000, 0xfe0000): 457 s, of which the USB upload was 1 s and
455 s were inside the stage. That time was 3984 x 4 KiB sector erases (~45 ms each), three read
passes (compare, per-sector read-back, final chunk CRC) at ~0.5 MB/s, and 38k page programs.
`stage_spi_write.c` now:

| Change | Effect |
| --- | --- |
| 64 KiB block erase (0xd8) when >= 4 sectors of a block need erasing; the whole block is then programmed from the image | 151 block erases instead of 2416 sector erases for a new kernel |
| One verification pass: the final CRC read of each chunk (`--verify-sectors` restores the per-sector read-back) | 1/3 less reading |
| SPI divider from the control block, default 2 (was a fixed 4) | Dry runs bit-exact at 2 and 4 |
| Burst FIFO transfers (FIFO depth probed = 32, at most 32 bytes in flight), word-wide compares | Two read passes over 11.7 MiB: 46.1 s -> 26.3 s |
| Every erase/program address re-checked against [0x050000, 0xfe0000) right before the command | Same window as before |

A table-driven CRC measured *slower* here (memory loads are expensive in this MaskROM setup), so the CRC
stays bitwise.

**Result:** apps build `firmware-apps-1b853ab95d4e` into [0x050000, 0xc00000): **79 s** (2633 sectors
changed, 151 block erases, 42184 pages), every chunk CRC OK. The independent full-chip read (28 s)
equals the image outside /data, and /data (JFFS2) was preserved. The dongle booted it from flash with
apps and grants intact; software reboot -> MaskROM still works.

**Boot the flash without touching the board:** after flashing (helper still loaded) run
`boards/rk3228a-dongle/rockusb/tools/rkdeveloptool/rkdeveloptool rd`. From bare MaskROM, load the helper first
(`rockusb_ram_loader.py --no-ram-padding`), then `rd`.

## DDR reclock in the flash entry

**Why:** the stock IDBlock DDR init (kept; NOR below 0x50000 is never written) leaves DDR3 at
300 MHz in DLL-off mode.

**What the flash entries do:** they are built with `-DDDR_RECLOCK` and link
`../boot/ddr_reclock.c`. Before Linux starts, it switches DDR to the rkbin 528 MHz
configuration through self-refresh, keeping the DRAM contents.

**How it is checked:**
- `build.sh` checks that the copied SRAM section has no NEON/VFP and no absolute image addresses.
- After a boot, `boards/rk3228a-dongle/lab.py sh 'reg 0x67d03000 7'` shows the breadcrumbs: word 6 is
  `0x5280000f` when reclocked, with training passed on all byte lanes.
- `rehearse.py` tests it from the stock DDR init without writing the flash.
- Flashed and verified: `firmware-meow-ddr528-48fe4ec7af41` (82.6 s write, full-chip read
  identical outside `/data`).

## Boot chain once flashed

**DDR3 runs at 528 MHz and vdd_logic at 1.10 V.**
- **How it gets there:** the stock DDR init in the kept IDBlock leaves **300 MHz** (DLL-off mode;
  `dpll_ddr` reads 600 MHz in both states). Our flash entry then
  reclocks to 528 MHz (see "DDR reclock in the flash entry" above).
- **Margin:** 528 MHz was memtest-clean down to vdd_logic 1.05 V on 12 Nov (RAM boot), and 1.10 V is
  what every RAM boot of the perf profile has run since then.


BootROM -> stock IDBlock (DDR3 300 MHz, miniloader 1.24) -> button check (GPIO3_D1) ->
trust slot 0x050000 loaded to 0x68400000 and entered in secure SVC with the MMU and caches
off -> our entry (breadcrumbs at 0x67d03000, DTB copy to 0x68000000, TREFI 39,
LAB flag + watchdog, DDR reclock 300 -> 528 MHz from SRAM) -> zImage at 0x64000000 (kernel slot 0x0d0000, header + image).
A refusal in the entry returns to the miniloader, which then reboots to MaskROM.

## Procedures

The step-by-step guide (tools, reading your unit's stock dump, RAM boot, first flash, rollback)
is in [../README.md](../README.md). The commands below are what `./meow board ...` runs.

Enter MaskROM: hold the button at power-on (HDMI unplugged). From a running LAB image:
`./meow board sh lab-reboot` (MaskROM flag; about 2 s). The helper is loaded by the tools.

Flash a new build:
```sh
./meow board pack                              # builds the stage and entries, prints image name + sha256
./meow board flash flash/images/firmware-XXXX.bin SHA            # dry run, ~27 s
./meow board flash flash/images/firmware-XXXX.bin SHA --write    # ~80 s, keeps /data
```
Then read the whole chip back and compare it with the image outside `/data`
(`[0, 0xc00000)` and `[0xfe0000, end)`); `./meow board dump` shows the read commands.

**Rollback to stock** (your own dump as the image, and the **full** window, because the default
range stops at 0xc00000 to keep /data):
```sh
./meow board rollback            # dry run: shows how many sectors differ
./meow board rollback --write
```
Then do a full read: it must equal your stock dump.

## Proven on hardware

Developed and proven on one unit between 13 and 15 November 2026: three identical reads of the
stock flash, a writer self-test (pattern written and restored), a full rollback to stock that
read back identical, and twelve firmware updates, each followed by an independent full-chip
read that matched the image outside `/data`. Status registers on that unit: SR1=0 SR2=0
SR3=0x60 (unprotected, nothing OTP-locked). A different flash chip or a different stock
loader revision has not been tried: the tools check the JEDEC ID and refuse what they do not
know.
