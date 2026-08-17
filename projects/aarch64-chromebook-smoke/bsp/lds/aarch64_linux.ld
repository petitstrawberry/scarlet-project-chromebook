OUTPUT_ARCH("aarch64")
ENTRY(_head)

MEMORY {
  RAM (rwx) : ORIGIN = 0x40200000, LENGTH = 1022M
}

SECTIONS {
  . = ORIGIN(RAM);
  __KERNEL_SPACE_START = .;
  PROVIDE(__SCARLET_KSYMS_START = 0);
  PROVIDE(__SCARLET_KSYMS_END = 0);

  .head : {
    KEEP(*(.head.text.header))
    KEEP(*(.head.text.entry))
  } > RAM

  .init : {
    *(.init .init.*)
  } > RAM

  .text : {
    *(.text .text.*)
  } > RAM

  .trampoline ALIGN(0x1000) : ALIGN(0x1000) {
    __TRAMPOLINE_START = .;
    *(.trampoline .trampoline.*)
    . = ALIGN(0x1000);
    __TRAMPOLINE_END = .;
  } > RAM

  .rodata : {
    KEEP(*(.rodata.boot_anchor))
    *(.rodata .rodata.*)
  } > RAM

  . = ALIGN(8);
  .initcall : {
    __INITCALL_START = .;
    __INITCALL_EARLY_START = .;
    *(.initcall.early)
    __INITCALL_EARLY_END = .;
    __INITCALL_DRIVER_START = .;
    *(.initcall.driver)
    __INITCALL_DRIVER_END = .;
    __INITCALL_LATE_START = .;
    *(.initcall.late)
    __INITCALL_END = .;
  } > RAM

  . = ALIGN(8);
  .scarlet_ksyms : {
    __SCARLET_KSYMS_START = .;
    KEEP(*(.scarlet_ksyms))
    __SCARLET_KSYMS_END = .;
  } > RAM

  .data : {
    *(.data .data.*)
  } > RAM

  .bss (NOLOAD) : {
    __BSS_START = .;
    *(.bss .bss.*)
    . = ALIGN(0x1000);
    __BSS_END = .;
  } > RAM

  .fdt_reserved (NOLOAD) : {
    . = ALIGN(0x1000);
    __FDT_RESERVED_START = .;
    . += 2M;
    __FDT_RESERVED_END = .;
  } > RAM

  .lsm_symbols : {
    KEEP(*(.lsm_symbols))
  } > RAM

  __KERNEL_SPACE_END = .;
  __KERNEL_IMAGE_SIZE = __KERNEL_SPACE_END - __KERNEL_SPACE_START;

  /DISCARD/ : {
    *(.eh_frame*)
    *(.note .note.*)
    *(.interp)
  }
}
