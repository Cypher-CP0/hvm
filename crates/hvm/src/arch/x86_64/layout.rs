//! Guest physical memory layout for boot-time structures.
//!
//! Everything lives in the first megabyte, below where the kernel is loaded.
//! The addresses follow the conventions used by other Linux-booting VMMs so
//! that register dumps look familiar.

/// Boot GDT (five 8-byte entries).
pub const BOOT_GDT_ADDR: u64 = 0x500;
/// Boot IDT (a single null entry; the guest installs its own).
pub const BOOT_IDT_ADDR: u64 = 0x540;
/// The Linux "zero page": `struct boot_params`, one page.
pub const ZERO_PAGE_ADDR: u64 = 0x7000;
/// Initial stack pointer. The stack grows down from here.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// Boot page tables: one page each.
pub const PML4_ADDR: u64 = 0x9000;
pub const PDPT_ADDR: u64 = 0xa000;
pub const PD_ADDR: u64 = 0xb000;
/// Kernel command line, NUL-terminated.
pub const CMDLINE_ADDR: u64 = 0x2_0000;
/// Upper bound on the command line we will place, including the NUL.
pub const CMDLINE_MAX_SIZE: usize = 0x1_0000;
/// End of conventional memory. On a PC the EBDA, video memory and BIOS ROMs
/// sit between here and 1 MiB, so the range is not reported as RAM.
pub const EBDA_START: u64 = 0x9_fc00;
/// Where guest code or the kernel is loaded. 1 MiB.
pub const HIMEM_START: u64 = 0x10_0000;
/// Guest RAM must end below the 32-bit MMIO hole (APICs, later PCI). RAM
/// above 4 GiB arrives with the PCI work.
pub const MAX_MEM_SIZE: u64 = 0xc000_0000;
