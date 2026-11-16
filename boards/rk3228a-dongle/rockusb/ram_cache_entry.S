/* Stable entry sequence, first installed at a previously unused code address.
 * Invalidate stale instructions/branch predictions after USB DMA code writes. */
.syntax unified
.arm
.section .text.start,"ax",%progbits
.global stage_entry
.type stage_entry,%function
stage_entry:
    mov r12, #0
    mcr p15, 0, r12, c7, c5, 0
    mcr p15, 0, r12, c7, c5, 6
    dsb sy
    isb sy
    b stage_main
.size stage_entry, .-stage_entry
