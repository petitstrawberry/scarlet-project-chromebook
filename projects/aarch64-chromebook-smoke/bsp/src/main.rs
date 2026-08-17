#![no_std]
#![no_main]

extern crate scarlet_modules;

// Reserve the post-link kernel-symbol section expected by cargo-scarlet's
// current symbol injection pass.
#[unsafe(link_section = ".scarlet_ksyms")]
#[used]
static KSYM_PLACEHOLDER: [u64; 65536] = [0; 65536];

#[used]
#[unsafe(link_section = ".rodata.boot_anchor")]
static LINUX_IMAGE_ENTRY: extern "C" fn() -> ! =
    scarlet_modules::scarlet::arch::aarch64::boot::linux::image_head;
