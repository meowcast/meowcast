/* Trust-slot entry: the stock miniloader reads 0x28c sectors (0x51800 bytes) of the
 * trust partition to 0x68400000 and jumps there. Code + appended DTB must fit. */
ENTRY(stage_entry)
SECTIONS {
  . = 0x68400000;
  .text : {
    KEEP(*(.text.start))
    /* board/ddr_reclock.c: copied to SRAM and run there, so it must be self-contained
     * (its rodata/data/bss sections are checked to be empty below). */
    . = ALIGN(4);
    __ddr_reclock_start = .;
    KEEP(*(.text.ddr_reclock))
    . = ALIGN(4);
    __ddr_reclock_end = .;
    *(.text*) *(.rodata*)
    /* Appended DTB (pack-flash.py): always the end of the binary. */
    KEEP(*(.fdt_tail))
  }
  .data : { *(.data*) }
  .ddr_reclock_forbidden : { *(.text.ddr_reclock.ro) *(.ddr_reclock.forbidden) }
  ASSERT(SIZEOF(.ddr_reclock_forbidden) == 0, "ddr_reclock.c must not use rodata/data/bss (it runs from SRAM)")
  /DISCARD/ : { *(.ARM.exidx*) *(.ARM.extab*) *(.comment*) *(.note*) }
  ASSERT(. <= 0x68404000, "flash entry code exceeds 16 KiB")
}
