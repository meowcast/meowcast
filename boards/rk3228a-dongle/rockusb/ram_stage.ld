ENTRY(stage_entry)
SECTIONS {
  . = 0x62002000;
  .text : { KEEP(*(.text.start)) *(.text*) *(.rodata*) }
  .data : { *(.data*) }
  .bss (NOLOAD) : { *(.bss*) *(COMMON) }
  /DISCARD/ : { *(.ARM.exidx*) *(.ARM.extab*) *(.comment*) *(.note*) }
  ASSERT(. <= 0x62004000, "RAM stage exceeds reserved 8 KiB")
}
